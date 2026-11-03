//! provider documentation bundleの形式検証と静的HTML生成。

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::io::{BufRead, Cursor, Read, Write as IoWrite};
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ammonia::{Builder, UrlRelative};
use comrak::adapters::{HeadingAdapter, HeadingMeta};
use comrak::nodes::{Node, Sourcepos};
use comrak::options::Plugins;
use comrak::{format_html_with_plugins, parse_document, Arena, Options};
use finl_unicode::categories::CharacterCategories;
use flate2::bufread::GzDecoder;
use flate2::write::GzEncoder;
use flate2::Compression;
use percent_encoding::percent_decode_str;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::process::Command;
use uuid::Uuid;

mod coordinate;
mod highlight;

pub use coordinate::{
    validate_docs_version, validate_name_component, validate_publish_version,
    CoordinateValidationError, DocsCoordinate, NameComponentError,
};

use highlight::{highlighting, CARINA_DARK_THEME, CRN_GRAMMAR, SYNTECT_CRATE_VERSION};
pub use highlight::{theme_css, CodeLanguage, HighlightError, HighlightedCode};

/// Paths shared by rendered templates and registry startup asset publication.
pub mod presentation {
    pub const DOCS_SCRIPT_KEY: &str = "providers/_assets/docs.js";
    pub const DOCS_TOKENS_KEY: &str = "providers/_assets/tokens.css";
    pub const DOCS_STYLESHEET_KEY: &str = "providers/_assets/docs.css";
    pub const DOCS_HIGHLIGHT_STYLESHEET_KEY: &str = "providers/_assets/highlight.css";
}
use presentation::{
    DOCS_HIGHLIGHT_STYLESHEET_KEY, DOCS_SCRIPT_KEY, DOCS_STYLESHEET_KEY, DOCS_TOKENS_KEY,
};

pub const DOCS_ARCHIVE_MAX_BYTES: u64 = 16 * 1024 * 1024;
pub const DEFAULT_DOCS_RENDER_MEMORY_LIMIT_BYTES: u64 = 384 * 1024 * 1024;
pub const DEFAULT_DOCS_RENDER_CPU_SECONDS: u64 = 20;
pub const DEFAULT_DOCS_RENDER_TIMEOUT_SECONDS: u64 = 30;

#[cfg(test)]
const HTML_CONTENT_TYPE: &str = "text/html; charset=utf-8";
#[cfg(test)]
const JSON_CONTENT_TYPE: &str = "application/json";

const BLOCK_BYTES: usize = 512;
const MAX_ENTRIES: usize = 4096;
const MAX_FILE_BYTES: u64 = 256 * 1024;
const MAX_TOTAL_FILE_BYTES: u64 = 64 * 1024 * 1024;
const MAX_PAX_EXTENDED_HEADER_BYTES: u64 = 64 * 1024;
const MAX_TAR_STREAM_BYTES: u64 = 80 * 1024 * 1024;
const MAX_TABLE_CELL_BOUND: usize = 250_000;
const MAX_PARSE_TREE_DEPTH: usize = 64;
const MAX_RENDERED_PAGE_HTML_BYTES: usize = 2 * 1024 * 1024;
const MAX_RENDERED_HTML_BYTES: usize = 64 * 1024 * 1024;
const MAX_WORKER_COORDINATE_BYTES: usize = 1024;
const MAX_WORKER_ERROR_BYTES: usize = 16 * 1024;
const MAX_WORKER_STDERR_BYTES: usize = 16 * 1024;
const WORKER_STACK_BYTES: usize = 64 * 1024 * 1024;
const WORKER_REQUEST_MAGIC: &[u8; 8] = b"CRDOCREQ";
const WORKER_RESPONSE_MAGIC: &[u8; 8] = b"CRDOCRSP";
const WORKER_PROTOCOL_VERSION: u8 = 1;
const RESOURCE_LIMIT_FAILURE_DETAIL: &str =
    "documentation rendering exceeded the registry's resource limits or failed";

#[derive(Debug, Clone, Copy)]
struct EmbeddedProviderNavigationMetadata {
    namespace: &'static str,
    name: &'static str,
    bytes: &'static [u8],
}

// Keep this table explicit: adding a metadata file without registering its provider must not
// silently change runtime navigation.
const EMBEDDED_PROVIDER_NAVIGATION_METADATA: &[EmbeddedProviderNavigationMetadata] = &[
    EmbeddedProviderNavigationMetadata {
        namespace: "carina-rs",
        name: "aws",
        bytes: include_bytes!("../docs-navigation/carina-rs/aws.json"),
    },
    EmbeddedProviderNavigationMetadata {
        namespace: "carina-rs",
        name: "awscc",
        bytes: include_bytes!("../docs-navigation/carina-rs/awscc.json"),
    },
    EmbeddedProviderNavigationMetadata {
        namespace: "carina-rs",
        name: "render-reference-grouped",
        bytes: REFERENCE_PROVIDER_NAVIGATION_METADATA.as_bytes(),
    },
];

#[cfg(any(test, feature = "test-support"))]
pub mod test_support;

/// 同一binaryでdocs render workerを起動する隠しsubcommand。
pub const DOCS_RENDER_WORKER_COMMAND: &str = "render-docs-worker";
/// worker自身の準備失敗を親へ伝える専用exit code。
pub const DOCS_RENDER_WORKER_INFRASTRUCTURE_EXIT_CODE: i32 = 70;
const DOCS_RENDER_WORKER_ABNORMAL_EXIT_CODE: i32 = 1;

/// worker processが終了するときの失敗分類。
#[derive(Debug)]
pub struct DocsRenderWorkerError {
    source: anyhow::Error,
    exit_code: i32,
}

impl DocsRenderWorkerError {
    fn infrastructure(source: impl Into<anyhow::Error>) -> Self {
        Self {
            source: source.into(),
            exit_code: DOCS_RENDER_WORKER_INFRASTRUCTURE_EXIT_CODE,
        }
    }

    fn infrastructure_io(context: &'static str, source: std::io::Error) -> Self {
        Self::infrastructure(anyhow::Error::new(source).context(context))
    }

    fn abnormal(source: impl Into<anyhow::Error>) -> Self {
        Self {
            source: source.into(),
            exit_code: DOCS_RENDER_WORKER_ABNORMAL_EXIT_CODE,
        }
    }

    pub fn exit_code(&self) -> i32 {
        self.exit_code
    }
}

impl fmt::Display for DocsRenderWorkerError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{:#}", self.source)
    }
}

impl std::error::Error for DocsRenderWorkerError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.source.as_ref())
    }
}

/// docs render subprocessへ渡すOS・wall-clock上限。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DocsRenderLimits {
    memory_limit_bytes: u64,
    cpu_seconds: u64,
    timeout: Duration,
}

impl DocsRenderLimits {
    pub fn new(memory_limit_bytes: u64, cpu_seconds: u64, timeout_seconds: u64) -> Self {
        Self::with_timeout(
            memory_limit_bytes,
            cpu_seconds,
            Duration::from_secs(timeout_seconds),
        )
    }

    pub fn with_timeout(memory_limit_bytes: u64, cpu_seconds: u64, timeout: Duration) -> Self {
        Self {
            memory_limit_bytes,
            cpu_seconds,
            timeout,
        }
    }

    fn memory_limit_bytes(self) -> u64 {
        self.memory_limit_bytes
    }

    fn cpu_seconds(self) -> u64 {
        self.cpu_seconds
    }

    fn timeout(self) -> Duration {
        self.timeout
    }
}

impl Default for DocsRenderLimits {
    fn default() -> Self {
        Self::new(
            DEFAULT_DOCS_RENDER_MEMORY_LIMIT_BYTES,
            DEFAULT_DOCS_RENDER_CPU_SECONDS,
            DEFAULT_DOCS_RENDER_TIMEOUT_SECONDS,
        )
    }
}

/// Owned archive bytes paired with validated output and opaque companion metadata.
///
/// Only a renderer can construct this evidence:
/// ```compile_fail
/// use carina_docs_bundle::ValidatedBundle;
/// let forged = ValidatedBundle { archive: Vec::<u8>::new(), rendered: todo!() };
/// ```
/// A default value cannot bypass validation:
/// ```compile_fail
/// use carina_docs_bundle::ValidatedBundle;
/// let forged: ValidatedBundle<Vec<u8>> = Default::default();
/// ```
/// Serialized fields cannot confer validation evidence:
/// ```compile_fail
/// use carina_docs_bundle::ValidatedBundle;
/// let forged: ValidatedBundle<Vec<u8>> = serde_json::from_str("{}").unwrap();
/// ```
/// The associated archive cannot be replaced:
/// ```compile_fail
/// use carina_docs_bundle::ValidatedBundle;
/// fn replace(bundle: &mut ValidatedBundle<Vec<u8>>) {
///     *bundle.archive() = b"unvalidated".to_vec();
/// }
/// ```
/// Output is read-only while associated with the archive:
/// ```compile_fail
/// use carina_docs_bundle::ValidatedBundle;
/// fn replace(bundle: &mut ValidatedBundle<Vec<u8>>) {
///     bundle.rendered_pages().rendered_pages.clear();
/// }
/// ```
/// An owner that switches its exposed bytes cannot supply the archive:
/// ```compile_fail
/// use carina_docs_bundle::{DocsCoordinate, SubprocessDocsRenderer};
/// use std::sync::{Arc, atomic::{AtomicBool, Ordering}};
/// struct SwitchingBytes(Arc<AtomicBool>);
/// impl AsRef<[u8]> for SwitchingBytes {
///     fn as_ref(&self) -> &[u8] {
///         if self.0.load(Ordering::SeqCst) { b"changed" } else { b"original" }
///     }
/// }
/// async fn substitute(renderer: &SubprocessDocsRenderer) {
///     let switch = Arc::new(AtomicBool::new(false));
///     let coordinate = DocsCoordinate::new("local", "provider", "0.0.0").unwrap();
///     let checked = renderer.validate_and_render(
///         SwitchingBytes(Arc::clone(&switch)), &coordinate,
///     ).await.unwrap();
///     switch.store(true, Ordering::SeqCst);
///     let _ = checked.archive();
/// }
/// ```
pub struct ValidatedBundle<M = ()> {
    archive: Vec<u8>,
    metadata: M,
    rendered: RenderedPages,
}

impl<M> ValidatedBundle<M> {
    pub fn archive(&self) -> &[u8] {
        &self.archive
    }

    pub fn metadata(&self) -> &M {
        &self.metadata
    }

    pub fn rendered_pages(&self) -> &RenderedPages {
        &self.rendered
    }

    pub fn into_parts(self) -> (Vec<u8>, M, RenderedPages) {
        (self.archive, self.metadata, self.rendered)
    }
}

impl ValidatedBundle {
    /// Carry host evidence alongside the archive without changing its validated bytes.
    pub fn with_metadata<M>(self, metadata: M) -> ValidatedBundle<M> {
        ValidatedBundle {
            archive: self.archive,
            metadata,
            rendered: self.rendered,
        }
    }
}

/// Validated objects belonging to one version root.
/// ```compile_fail
/// use carina_docs_bundle::RenderedPages;
/// let forged = RenderedPages { version_root: todo!(), rendered_pages: Vec::new() };
/// ```
pub struct RenderedPages {
    version_root: DocsVersionRoot,
    rendered_pages: Vec<RenderedPage>,
}

impl RenderedPages {
    pub fn version_root(&self) -> &str {
        self.version_root.host_path()
    }

    pub fn iter(&self) -> impl ExactSizeIterator<Item = (String, &[u8])> {
        self.rendered_pages
            .iter()
            .map(|page| (page.key(), page.body.as_slice()))
    }
}

impl IntoIterator for RenderedPages {
    type Item = RenderedPage;
    type IntoIter = std::vec::IntoIter<RenderedPage>;

    fn into_iter(self) -> Self::IntoIter {
        self.rendered_pages.into_iter()
    }
}

/// Media kind of a rendered object; the host supplies its own MIME mapping.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RenderedKind {
    Html,
    Json,
}

/// Host MIME mapping shared by publication and render fingerprint assembly.
#[derive(Debug, Clone, Copy)]
pub struct RenderContentTypes {
    pub html: &'static str,
    pub json: &'static str,
}

impl RenderContentTypes {
    pub fn for_kind(self, kind: RenderedKind) -> &'static str {
        match kind {
            RenderedKind::Html => self.html,
            RenderedKind::Json => self.json,
        }
    }
}

/// validation時のprovider座標からだけ作るversioned docs root。
#[derive(Debug, Clone)]
struct DocsVersionRoot {
    namespace: Arc<str>,
    name: Arc<str>,
    version: Arc<str>,
    host_path: Arc<str>,
}

impl DocsVersionRoot {
    fn new(namespace: &str, name: &str, version: &str) -> Self {
        let provider_root = docs_provider_root(namespace, name);
        Self {
            namespace: namespace.into(),
            name: name.into(),
            version: version.into(),
            host_path: format!("/{provider_root}/{version}").into(),
        }
    }

    fn host_path(&self) -> &str {
        &self.host_path
    }

    fn shared_host_path(&self) -> Arc<str> {
        Arc::clone(&self.host_path)
    }

    fn object_key(&self, path: &RenderedObjectPath) -> String {
        format!(
            "{}{}",
            self.host_path.trim_start_matches('/'),
            path.key_suffix()
        )
    }

    fn page_title(&self, page: &NamedDocsPage) -> String {
        format!(
            "{} ({}) · {}/{} {}",
            page.title.as_str(),
            page.kind().label().to_ascii_lowercase(),
            self.namespace,
            self.name,
            self.version
        )
    }

    fn index_page_title(&self) -> String {
        format!("{}/{} {}", self.namespace, self.name, self.version)
    }
}

/// version renderが生成するobject位置。公開時に必要なmetadataもこの型から決まる。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum RenderedObjectPath {
    Page(DocsPagePath),
    Navigation,
}

impl RenderedObjectPath {
    fn key_suffix(&self) -> String {
        match self {
            Self::Page(path) => path.key_suffix(),
            Self::Navigation => "/nav.json".to_string(),
        }
    }

    fn kind(&self) -> RenderedKind {
        match self {
            Self::Page(_) => RenderedKind::Html,
            Self::Navigation => RenderedKind::Json,
        }
    }
}

pub fn docs_provider_root(namespace: &str, name: &str) -> String {
    format!("providers/{namespace}/{name}")
}

pub fn docs_versions_path(namespace: &str, name: &str) -> String {
    format!("{}/versions.json", docs_provider_root(namespace, name))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum DocsPageKind {
    Resource,
    DataSource,
}

impl DocsPageKind {
    fn label(self) -> &'static str {
        match self {
            Self::Resource => "Resource",
            Self::DataSource => "Data source",
        }
    }

    fn css_class(self) -> &'static str {
        match self {
            Self::Resource => "docs-page-kind-resource",
            Self::DataSource => "docs-page-kind-data-source",
        }
    }
}

/// 検証済みslugからだけ作られるresourceまたはdata sourceの位置。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct NamedPagePath {
    kind: DocsPageKind,
    slug: String,
}

impl NamedPagePath {
    fn new(kind: DocsPageKind, slug: impl Into<String>) -> Self {
        Self {
            kind,
            slug: slug.into(),
        }
    }

    fn archive_path(&self) -> String {
        match self.kind {
            DocsPageKind::Resource => format!("resources/{}.md", self.slug),
            DocsPageKind::DataSource => format!("data-sources/{}.md", self.slug),
        }
    }

    fn key_suffix(&self) -> String {
        match self.kind {
            DocsPageKind::Resource => format!("/resources/{}", self.slug),
            DocsPageKind::DataSource => format!("/data-sources/{}", self.slug),
        }
    }
}

/// 検証済みpage位置。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum DocsPagePath {
    Index,
    Named(NamedPagePath),
}

impl DocsPagePath {
    fn archive_path(&self) -> String {
        match self {
            Self::Index => "index.md".to_string(),
            Self::Named(path) => path.archive_path(),
        }
    }

    fn key_suffix(&self) -> String {
        match self {
            Self::Index => String::new(),
            Self::Named(path) => path.key_suffix(),
        }
    }
}

mod page_title {
    use super::DocsBundleError;

    /// resource/data-source Markdownの先頭行から検証したDSL名。
    #[derive(Debug, Clone, PartialEq, Eq, Hash)]
    pub(super) struct PageTitle(String);

    impl PageTitle {
        pub(super) fn as_str(&self) -> &str {
            &self.0
        }

        pub(super) fn type_name(&self) -> &str {
            self.0
                .rsplit_once('.')
                .expect("validated page title has at least two segments")
                .1
        }
    }

    /// `PageTitle`を構築する唯一の入口。Markdownの先頭行だけを検査する。
    // public/providers/_assets/docs.jsの対応する検証と同期を保つこと。
    pub(super) fn parse(archive_path: &str, markdown: &str) -> Result<PageTitle, DocsBundleError> {
        let first_line_end = markdown
            .as_bytes()
            .iter()
            .position(|byte| matches!(byte, b'\r' | b'\n'))
            .unwrap_or(markdown.len());
        let first_line = &markdown[..first_line_end];
        let Some(title) = first_line.strip_prefix("# ") else {
            return Err(DocsBundleError::new(format!(
                "{} must begin with an ATX H1 title",
                archive_path
            )));
        };
        if title.len() > 128 {
            return Err(DocsBundleError::new(format!(
                "{} title exceeds the 128-byte limit",
                archive_path
            )));
        }
        let valid_segment = |segment: &str| {
            !segment.is_empty()
                && segment
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
        };
        if !(title.contains('.') && title.split('.').all(valid_segment)) {
            return Err(DocsBundleError::new(format!(
                "{} title must be dot-separated ASCII identifier segments",
                archive_path
            )));
        }
        Ok(PageTitle(title.to_string()))
    }
}

use page_title::PageTitle;

/// 検証済みpage。resource/data sourceは必ず検証済みDSL名を持つ。
#[derive(Debug, Clone, PartialEq, Eq)]
enum ValidatedDocsPage {
    Index,
    Named(NamedDocsPage),
}

impl ValidatedDocsPage {
    fn parse(path: DocsPagePath, markdown: &str) -> Result<Self, DocsBundleError> {
        let archive_path = path.archive_path();
        match path {
            DocsPagePath::Index => Ok(Self::Index),
            DocsPagePath::Named(path) => Ok(Self::Named(NamedDocsPage {
                path,
                title: page_title::parse(&archive_path, markdown)?,
            })),
        }
    }

    fn path(&self) -> DocsPagePath {
        match self {
            Self::Index => DocsPagePath::Index,
            Self::Named(page) => DocsPagePath::Named(page.path.clone()),
        }
    }

    fn archive_path(&self) -> String {
        match self {
            Self::Index => "index.md".to_string(),
            Self::Named(page) => page.path.archive_path(),
        }
    }

    fn key_suffix(&self) -> String {
        match self {
            Self::Index => String::new(),
            Self::Named(page) => page.path.key_suffix(),
        }
    }

    fn named(&self) -> Option<&NamedDocsPage> {
        match self {
            Self::Index => None,
            Self::Named(page) => Some(page),
        }
    }
}

/// resourceまたはdata sourceであることと、検証済みDSL名を型で結び付けたpage。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct NamedDocsPage {
    path: NamedPagePath,
    title: PageTitle,
}

impl NamedDocsPage {
    fn path(&self) -> &NamedPagePath {
        &self.path
    }

    fn key_suffix(&self) -> String {
        self.path.key_suffix()
    }

    fn slug(&self) -> &str {
        &self.path.slug
    }

    fn kind(&self) -> DocsPageKind {
        self.path.kind
    }
}

mod validated_docs_archive {
    use super::ValidatedDocsPage;

    /// 1回のarchive検証で確定した、navigation導出前のpage集合。
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub(super) struct ValidatedDocsArchive {
        pages: Vec<ValidatedDocsPage>,
    }

    impl ValidatedDocsArchive {
        pub(super) fn pages(&self) -> &[ValidatedDocsPage] {
            &self.pages
        }
    }

    pub(super) fn validated_docs_archive(pages: Vec<ValidatedDocsPage>) -> ValidatedDocsArchive {
        ValidatedDocsArchive { pages }
    }
}

use validated_docs_archive::{validated_docs_archive, ValidatedDocsArchive};

/// provider navigation metadataの適用可否で排他的に決まる、検証済みnavigation。
#[derive(Debug, Clone, PartialEq, Eq)]
enum DocsNavigation {
    Flat {
        resources: Vec<NamedDocsPage>,
        data_sources: Vec<NamedDocsPage>,
    },
    Grouped(GroupedNavigation),
}

/// version内の全pageのservice所属を導出時に確定したgrouped navigation。
#[derive(Debug, Clone, PartialEq, Eq)]
struct GroupedNavigation {
    services: Vec<DocsService>,
    service_of: HashMap<NamedPagePath, usize>,
}

impl GroupedNavigation {
    fn service_for(&self, page: &NamedPagePath) -> Option<&DocsService> {
        self.service_of
            .get(page)
            .and_then(|index| self.services.get(*index))
    }
}

/// version内の全pageのexactly-once所属を確認した後にだけ構築されるservice。
#[derive(Debug, Clone, PartialEq, Eq)]
struct DocsService {
    id: String,
    title: String,
    resources: Vec<NamedDocsPage>,
    data_sources: Vec<NamedDocsPage>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProviderNavigationDocument {
    services: Vec<ServiceDeclaration>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ServiceDeclaration {
    id: String,
    title: String,
    resources: Vec<String>,
    data_sources: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ProviderNavigationMetadata {
    services: Vec<ProviderServiceMetadata>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ProviderServiceMetadata {
    id: String,
    title: String,
    resources: Vec<String>,
    data_sources: Vec<String>,
}

#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
#[error("invalid provider navigation metadata: {detail}")]
struct ProviderNavigationMetadataError {
    detail: String,
}

impl ProviderNavigationMetadataError {
    fn new(detail: impl Into<String>) -> Self {
        Self {
            detail: detail.into(),
        }
    }

    #[cfg(test)]
    fn detail(&self) -> &str {
        &self.detail
    }
}

/// 読み取り面へ書く、検証済みobject位置とbody。
/// ```compile_fail
/// use carina_docs_bundle::RenderedPage;
/// let forged = RenderedPage { version_root: todo!(), path: todo!(), body: Vec::new() };
/// ```
#[derive(Debug)]
pub struct RenderedPage {
    version_root: DocsVersionRoot,
    path: RenderedObjectPath,
    body: Vec<u8>,
}

impl RenderedPage {
    fn key(&self) -> String {
        self.version_root.object_key(&self.path)
    }

    pub fn into_parts(self) -> (String, Vec<u8>, RenderedKind) {
        let key = self.key();
        let kind = self.path.kind();
        (key, self.body, kind)
    }
}

/// docs bundleが規範形式を満たさない理由。
#[derive(Debug, thiserror::Error)]
#[error("invalid documentation bundle: {detail}")]
pub struct DocsBundleError {
    detail: String,
}

impl DocsBundleError {
    pub fn new(detail: impl Into<String>) -> Self {
        Self {
            detail: detail.into(),
        }
    }

    pub fn detail(&self) -> &str {
        &self.detail
    }
}

/// docs renderer境界が返す、publisher起因とregistry起因を分けた失敗。
#[derive(Debug, thiserror::Error)]
pub enum DocsRendererError {
    #[error(transparent)]
    InvalidBundle(#[from] DocsBundleError),
    #[error("invalid documentation bundle: {}", RESOURCE_LIMIT_FAILURE_DETAIL)]
    RenderAborted(RenderAbort),
    #[error("documentation renderer failed: {detail}")]
    Infrastructure {
        detail: String,
        #[source]
        source: Option<std::io::Error>,
    },
}

/// 隔離したdocs rendererが応答を完了できなかった原因。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RenderAbort {
    TimedOut,
    Signaled(i32),
    Exited(i32),
    Io,
    MalformedResponse,
}

impl DocsRendererError {
    pub fn invalid(detail: impl Into<String>) -> Self {
        Self::InvalidBundle(DocsBundleError::new(detail))
    }

    pub fn infrastructure(detail: impl Into<String>) -> Self {
        Self::Infrastructure {
            detail: detail.into(),
            source: None,
        }
    }

    fn infrastructure_io(context: impl fmt::Display, source: std::io::Error) -> Self {
        Self::Infrastructure {
            detail: format!("{context}: {source}"),
            source: Some(source),
        }
    }

    pub fn detail(&self) -> &str {
        match self {
            Self::InvalidBundle(error) => error.detail(),
            Self::RenderAborted(_) => RESOURCE_LIMIT_FAILURE_DETAIL,
            Self::Infrastructure { detail, .. } => detail,
        }
    }
}

/// 単体テスト向けにrendererを同一processのblocking threadで動かす実装。
#[cfg(any(test, feature = "test-support"))]
#[derive(Debug, Default)]
pub struct InProcessDocsRenderer;

#[cfg(any(test, feature = "test-support"))]
impl InProcessDocsRenderer {
    pub async fn validate_and_render(
        &self,
        archive: Vec<u8>,
        coordinate: &DocsCoordinate,
    ) -> Result<ValidatedBundle, DocsRendererError> {
        let coordinate = coordinate.clone();
        tokio::task::spawn_blocking(move || validate_and_render_docs_bundle(archive, &coordinate))
            .await
            .map_err(|error| {
                DocsRendererError::infrastructure(format!(
                    "in-process documentation renderer task failed: {error}"
                ))
            })?
            .map_err(DocsRendererError::InvalidBundle)
    }
}

/// 本番でrendererをresource-limited child processへ隔離する実装。
#[derive(Debug, Clone)]
pub struct SubprocessDocsRenderer {
    executable: PathBuf,
    limits: DocsRenderLimits,
}

impl SubprocessDocsRenderer {
    pub fn new(executable: impl Into<PathBuf>, limits: DocsRenderLimits) -> Self {
        Self {
            executable: executable.into(),
            limits,
        }
    }
}

impl SubprocessDocsRenderer {
    pub async fn validate_and_render(
        &self,
        archive: Vec<u8>,
        coordinate: &DocsCoordinate,
    ) -> Result<ValidatedBundle, DocsRendererError> {
        let (archive, expected_archive) = tokio::task::spawn_blocking(move || {
            let expected_archive = visit_docs_archive(&archive, |_, _| Ok(()))?;
            Ok::<_, DocsBundleError>((archive, expected_archive))
        })
        .await
        .map_err(|error| {
            DocsRendererError::infrastructure(format!(
                "documentation archive validation task failed: {error}"
            ))
        })?
        .map_err(DocsRendererError::InvalidBundle)?;
        let response = self.render_in_subprocess(&archive, coordinate).await?;
        let pages = match response {
            WorkerResponse::Rendered(pages) => pages,
            WorkerResponse::Invalid(detail) => {
                return Err(DocsRendererError::invalid(detail));
            }
        };
        build_validated_bundle_from_worker(archive, coordinate, &expected_archive, pages).map_err(
            |error| {
                tracing::error!(%error, "documentation renderer output did not match the archive");
                DocsRendererError::infrastructure(format!(
                    "documentation renderer output did not match the archive: {error}"
                ))
            },
        )
    }
}

impl SubprocessDocsRenderer {
    async fn render_in_subprocess(
        &self,
        archive: &[u8],
        coordinate: &DocsCoordinate,
    ) -> Result<WorkerResponse, DocsRendererError> {
        let mut command = Command::new(&self.executable);
        command
            .arg(DOCS_RENDER_WORKER_COMMAND)
            .arg(self.limits.memory_limit_bytes().to_string())
            .arg(self.limits.cpu_seconds().to_string())
            .env_clear()
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let mut child = command.spawn().map_err(|error| {
            DocsRendererError::infrastructure_io(
                format!(
                    "could not start documentation renderer {}",
                    self.executable.display()
                ),
                error,
            )
        })?;
        let stdin = child.stdin.take().ok_or_else(|| {
            DocsRendererError::infrastructure("documentation renderer stdin was not piped")
        })?;
        let stdout = child.stdout.take().ok_or_else(|| {
            DocsRendererError::infrastructure("documentation renderer stdout was not piped")
        })?;
        let stderr = child.stderr.take().ok_or_else(|| {
            DocsRendererError::infrastructure("documentation renderer stderr was not piped")
        })?;

        let communication = async {
            tokio::join!(
                write_worker_request(
                    stdin,
                    coordinate.namespace(),
                    coordinate.name(),
                    coordinate.version(),
                    archive,
                ),
                read_worker_response(stdout),
                read_stderr(stderr),
                child.wait(),
            )
        };
        let (write_result, stdout_result, stderr_result, status_result) =
            match tokio::time::timeout(self.limits.timeout(), communication).await {
                Ok(results) => results,
                Err(_) => {
                    let kill_result = child.kill().await;
                    let wait_result = child.wait().await;
                    let cause = RenderAbort::TimedOut;
                    tracing::warn!(
                        executable = %self.executable.display(),
                        ?cause,
                        ?kill_result,
                        ?wait_result,
                        "documentation renderer exceeded its wall-clock limit"
                    );
                    return Err(DocsRendererError::RenderAborted(cause));
                }
            };

        let stderr = stderr_result
            .unwrap_or_else(|error| format!("<failed to read renderer stderr: {error}>"));
        let status = match status_result {
            Ok(status) => status,
            Err(error) => {
                let cause = RenderAbort::Io;
                tracing::warn!(?cause, %error, %stderr, "could not wait for documentation renderer");
                return Err(DocsRendererError::RenderAborted(cause));
            }
        };
        if status.code() == Some(DOCS_RENDER_WORKER_INFRASTRUCTURE_EXIT_CODE) {
            tracing::error!(
                %status,
                write_error = ?write_result.as_ref().err(),
                stdout_error = ?stdout_result.as_ref().err(),
                %stderr,
                "documentation renderer setup failed"
            );
            return Err(DocsRendererError::infrastructure(format!(
                "documentation renderer setup failed with {status}"
            )));
        }
        let cause = if !status.success() {
            Some(render_abort_from_status(&status))
        } else if write_result.is_err() {
            Some(RenderAbort::Io)
        } else {
            stdout_result.as_ref().err().map(response_error_abort)
        };
        if let Some(cause) = cause {
            tracing::warn!(
                %status,
                ?cause,
                write_error = ?write_result.as_ref().err(),
                stdout_error = ?stdout_result.as_ref().err(),
                %stderr,
                "documentation renderer ended abnormally"
            );
            return Err(DocsRendererError::RenderAborted(cause));
        }
        if !stderr.is_empty() {
            tracing::warn!(%stderr, "documentation renderer wrote to stderr");
        }
        match stdout_result {
            Ok(stdout) => Ok(stdout),
            Err(error) => Err(DocsRendererError::RenderAborted(response_error_abort(
                &error,
            ))),
        }
    }
}

fn render_abort_from_status(status: &std::process::ExitStatus) -> RenderAbort {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;

        if let Some(signal) = status.signal() {
            return RenderAbort::Signaled(signal);
        }
    }
    status.code().map_or(RenderAbort::Io, RenderAbort::Exited)
}

fn response_error_abort(error: &std::io::Error) -> RenderAbort {
    match error.kind() {
        std::io::ErrorKind::InvalidData | std::io::ErrorKind::UnexpectedEof => {
            RenderAbort::MalformedResponse
        }
        _ => RenderAbort::Io,
    }
}

#[derive(Debug)]
struct WorkerPage {
    path: DocsPagePath,
    body: Vec<u8>,
}

#[derive(Debug)]
enum WorkerResponse {
    Rendered(Vec<WorkerPage>),
    Invalid(String),
}

fn build_validated_bundle_from_worker(
    archive: Vec<u8>,
    coordinate: &DocsCoordinate,
    expected_archive: &ValidatedDocsArchive,
    pages: Vec<WorkerPage>,
) -> Result<ValidatedBundle, &'static str> {
    let expected_count = expected_archive.pages().len();
    if pages.len() != expected_count {
        return Err("renderer returned the wrong page count");
    }
    let mut total_bytes = 0_usize;
    let mut by_path = HashMap::with_capacity(pages.len());
    for page in pages {
        total_bytes = total_bytes
            .checked_add(page.body.len())
            .ok_or("renderer page size overflowed")?;
        if total_bytes > MAX_RENDERED_HTML_BYTES {
            return Err("renderer returned too much page data");
        }
        if std::str::from_utf8(&page.body).is_err() {
            return Err("renderer returned a non-UTF-8 page");
        }
        if by_path.insert(page.path, page.body).is_some() {
            return Err("renderer returned a duplicate page path");
        }
    }
    let version_root = DocsVersionRoot::new(
        coordinate.namespace(),
        coordinate.name(),
        coordinate.version(),
    );
    let mut rendered_pages = Vec::with_capacity(expected_count + 1);
    for page in expected_archive.pages() {
        let path = page.path();
        let body = by_path
            .remove(&path)
            .ok_or("renderer omitted an archive page")?;
        rendered_pages.push(RenderedPage {
            version_root: version_root.clone(),
            path: RenderedObjectPath::Page(path),
            body,
        });
    }
    if !by_path.is_empty() {
        return Err("renderer returned a page outside the archive");
    }
    // nav.json is always derived in the parent from validated pages (including titles) and
    // registry-owned metadata.
    let navigation = docs_navigation_for_version(expected_archive.pages(), &version_root, true);
    append_navigation_document(
        &mut rendered_pages,
        &navigation,
        &version_root,
        MAX_RENDERED_HTML_BYTES,
    )
    .map_err(|_| "renderer navigation exceeded its output limit")?;
    // 隔離対象はDoSとcrashでありchildは同じbinaryなので、親ではHTMLを再sanitizeしない。
    Ok(ValidatedBundle {
        archive,
        metadata: (),
        rendered: RenderedPages {
            version_root,
            rendered_pages,
        },
    })
}

async fn write_worker_request<W>(
    mut writer: W,
    namespace: &str,
    name: &str,
    version: &str,
    archive: &[u8],
) -> std::io::Result<()>
where
    W: AsyncWrite + Unpin,
{
    let payload_len = worker_request_payload_len(namespace, name, version, archive)?;
    writer.write_all(&payload_len.to_le_bytes()).await?;
    writer.write_all(WORKER_REQUEST_MAGIC).await?;
    writer.write_u8(WORKER_PROTOCOL_VERSION).await?;
    write_async_string(&mut writer, namespace).await?;
    write_async_string(&mut writer, name).await?;
    write_async_string(&mut writer, version).await?;
    writer.write_u64_le(archive.len() as u64).await?;
    writer.write_all(archive).await?;
    writer.shutdown().await
}

fn worker_request_payload_len(
    namespace: &str,
    name: &str,
    version: &str,
    archive: &[u8],
) -> std::io::Result<u64> {
    let mut length = (WORKER_REQUEST_MAGIC.len() + 1 + 8) as u64;
    for value in [namespace, name, version] {
        let value_len = u64::try_from(value.len()).map_err(|_| invalid_worker_data())?;
        length = length
            .checked_add(4)
            .and_then(|length| length.checked_add(value_len))
            .ok_or_else(invalid_worker_data)?;
    }
    length
        .checked_add(u64::try_from(archive.len()).map_err(|_| invalid_worker_data())?)
        .ok_or_else(invalid_worker_data)
}

async fn write_async_string<W>(writer: &mut W, value: &str) -> std::io::Result<()>
where
    W: AsyncWrite + Unpin,
{
    let length = u32::try_from(value.len()).map_err(|_| invalid_worker_data())?;
    writer.write_u32_le(length).await?;
    writer.write_all(value.as_bytes()).await
}

async fn read_worker_response<R>(mut reader: R) -> std::io::Result<WorkerResponse>
where
    R: AsyncRead + Unpin,
{
    let frame_len = reader.read_u64_le().await?;
    if frame_len > max_worker_response_payload_bytes() as u64 {
        return Err(worker_protocol_error("renderer response exceeds its limit"));
    }
    let mut frame = reader.take(frame_len);
    let mut magic = [0_u8; 8];
    frame.read_exact(&mut magic).await?;
    if &magic != WORKER_RESPONSE_MAGIC {
        return Err(worker_protocol_error("renderer response magic is invalid"));
    }
    if frame.read_u8().await? != WORKER_PROTOCOL_VERSION {
        return Err(worker_protocol_error(
            "renderer response protocol version is unsupported",
        ));
    }
    let response = match frame.read_u8().await? {
        0 => {
            let count = frame.read_u32_le().await? as usize;
            if count > MAX_ENTRIES {
                return Err(worker_protocol_error("renderer returned too many pages"));
            }
            let mut pages = Vec::with_capacity(count);
            let mut total_bytes = 0_usize;
            for _ in 0..count {
                let kind = frame.read_u8().await?;
                let slug = read_async_bytes(&mut frame, 128).await?;
                let path = decode_worker_path(kind, &slug).map_err(worker_protocol_error)?;
                let body_len = frame.read_u64_le().await?;
                if body_len > frame.limit() {
                    return Err(worker_protocol_error("renderer page body is truncated"));
                }
                let body_len = usize::try_from(body_len)
                    .map_err(|_| worker_protocol_error("renderer page body is too large"))?;
                total_bytes = total_bytes
                    .checked_add(body_len)
                    .ok_or_else(|| worker_protocol_error("renderer page size overflowed"))?;
                if total_bytes > MAX_RENDERED_HTML_BYTES {
                    return Err(worker_protocol_error(
                        "renderer page bodies exceed their total limit",
                    ));
                }
                let mut body = vec![0_u8; body_len];
                frame.read_exact(&mut body).await?;
                pages.push(WorkerPage { path, body });
            }
            WorkerResponse::Rendered(pages)
        }
        1 => {
            let detail = read_async_bytes(&mut frame, MAX_WORKER_ERROR_BYTES).await?;
            let detail = String::from_utf8(detail)
                .map_err(|_| worker_protocol_error("renderer error detail is not valid UTF-8"))?;
            WorkerResponse::Invalid(detail)
        }
        _ => return Err(worker_protocol_error("renderer response status is invalid")),
    };
    if frame.limit() != 0 {
        return Err(worker_protocol_error(
            "renderer response contains trailing frame bytes",
        ));
    }
    let mut reader = frame.into_inner();
    let mut trailing = [0_u8; 1];
    if reader.read(&mut trailing).await? != 0 {
        return Err(worker_protocol_error(
            "renderer response contains trailing stdout bytes",
        ));
    }
    Ok(response)
}

async fn read_async_bytes<R>(reader: &mut R, limit: usize) -> std::io::Result<Vec<u8>>
where
    R: AsyncRead + Unpin,
{
    let length = reader.read_u32_le().await? as usize;
    if length > limit {
        return Err(worker_protocol_error(
            "renderer response field exceeds its limit",
        ));
    }
    let mut bytes = vec![0_u8; length];
    reader.read_exact(&mut bytes).await?;
    Ok(bytes)
}

async fn read_stderr<R>(mut reader: R) -> std::io::Result<String>
where
    R: AsyncRead + Unpin,
{
    let mut captured = Vec::new();
    let mut buffer = [0_u8; 4096];
    let mut truncated = false;
    loop {
        let count = reader.read(&mut buffer).await?;
        if count == 0 {
            break;
        }
        let remaining = MAX_WORKER_STDERR_BYTES.saturating_sub(captured.len());
        captured.extend_from_slice(&buffer[..count.min(remaining)]);
        truncated |= count > remaining;
    }
    let mut message = String::from_utf8_lossy(&captured).into_owned();
    if truncated {
        message.push_str(" [truncated]");
    }
    Ok(message)
}

fn max_worker_response_payload_bytes() -> usize {
    WORKER_RESPONSE_MAGIC.len() + 2 + 4 + MAX_ENTRIES * (1 + 4 + 128 + 8) + MAX_RENDERED_HTML_BYTES
}

fn decode_worker_path(kind: u8, slug: &[u8]) -> Result<DocsPagePath, &'static str> {
    match kind {
        0 if slug.is_empty() => Ok(DocsPagePath::Index),
        1 | 2 => {
            let mut archive_path = if kind == 1 {
                b"resources/".to_vec()
            } else {
                b"data-sources/".to_vec()
            };
            archive_path.extend_from_slice(slug);
            archive_path.extend_from_slice(b".md");
            classify_file(&archive_path).map_err(|_| "response page slug is invalid")
        }
        _ => Err("response page path kind is invalid"),
    }
}

fn worker_protocol_error(detail: impl Into<String>) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, detail.into())
}

fn read_sync_bytes<R: Read>(reader: &mut R, limit: usize) -> std::io::Result<Vec<u8>> {
    let length = read_sync_u32(reader)? as usize;
    if length > limit {
        return Err(invalid_worker_data());
    }
    let mut bytes = vec![0_u8; length];
    reader.read_exact(&mut bytes)?;
    Ok(bytes)
}

fn read_sync_u8<R: Read>(reader: &mut R) -> std::io::Result<u8> {
    let mut bytes = [0_u8; 1];
    reader.read_exact(&mut bytes)?;
    Ok(bytes[0])
}

fn read_sync_u32<R: Read>(reader: &mut R) -> std::io::Result<u32> {
    let mut bytes = [0_u8; 4];
    reader.read_exact(&mut bytes)?;
    Ok(u32::from_le_bytes(bytes))
}

fn read_sync_u64<R: Read>(reader: &mut R) -> std::io::Result<u64> {
    let mut bytes = [0_u8; 8];
    reader.read_exact(&mut bytes)?;
    Ok(u64::from_le_bytes(bytes))
}

fn invalid_worker_data() -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        "documentation renderer protocol value is too large",
    )
}

struct WorkerRequest {
    coordinate: DocsCoordinate,
    archive: Vec<u8>,
}

/// hidden worker subcommandを同期実行する。server runtimeや設定の初期化前に呼ぶ。
pub fn run_docs_render_worker<I>(arguments: I) -> Result<(), DocsRenderWorkerError>
where
    I: IntoIterator<Item = OsString>,
{
    let mut arguments = arguments.into_iter();
    let memory_limit_bytes = parse_worker_limit("memory limit", arguments.next().as_deref())
        .map_err(DocsRenderWorkerError::infrastructure)?;
    let cpu_seconds = parse_worker_limit("CPU limit", arguments.next().as_deref())
        .map_err(DocsRenderWorkerError::infrastructure)?;
    if arguments.next().is_some() {
        return Err(DocsRenderWorkerError::infrastructure(anyhow::anyhow!(
            "documentation renderer worker received extra arguments"
        )));
    }
    apply_worker_resource_limits(memory_limit_bytes, cpu_seconds).map_err(|error| {
        DocsRenderWorkerError::infrastructure_io(
            "documentation renderer could not apply resource limits",
            error,
        )
    })?;

    let stdin = std::io::stdin();
    let request = read_worker_request(&mut stdin.lock()).map_err(|error| {
        DocsRenderWorkerError::infrastructure_io(
            "documentation renderer could not decode its request",
            error,
        )
    })?;
    let version_root = DocsVersionRoot::new(
        request.coordinate.namespace(),
        request.coordinate.name(),
        request.coordinate.version(),
    );
    let archive = request.archive;
    let render_thread = std::thread::Builder::new()
        .name("docs-renderer".to_string())
        .stack_size(WORKER_STACK_BYTES)
        .spawn(move || {
            render_archive_pages(&archive, &version_root).map(|(_, rendered_pages)| rendered_pages)
        })
        .map_err(|error| {
            DocsRenderWorkerError::infrastructure_io(
                "documentation renderer could not spawn its render thread",
                error,
            )
        })?;
    let rendered = render_thread.join().map_err(|_| {
        DocsRenderWorkerError::abnormal(anyhow::anyhow!("documentation renderer thread panicked"))
    })?;
    let response = match rendered {
        Ok(pages) => WorkerResponse::Rendered(into_worker_pages(pages)),
        Err(error) => WorkerResponse::Invalid(error.detail().to_string()),
    };
    let stdout = std::io::stdout();
    write_worker_response(&mut stdout.lock(), &response).map_err(|error| {
        DocsRenderWorkerError::abnormal(
            anyhow::Error::new(error)
                .context("documentation renderer could not encode its response"),
        )
    })?;
    Ok(())
}

fn parse_worker_limit(name: &str, value: Option<&OsStr>) -> anyhow::Result<u64> {
    let value = value
        .and_then(OsStr::to_str)
        .ok_or_else(|| anyhow::anyhow!("documentation renderer worker has no {name}"))?;
    let value = value.parse::<u64>().map_err(|error| {
        anyhow::anyhow!("documentation renderer worker has invalid {name}: {error}")
    })?;
    if value == 0 {
        anyhow::bail!("documentation renderer worker {name} must be positive");
    }
    Ok(value)
}

#[cfg(unix)]
fn apply_worker_resource_limits(memory_limit_bytes: u64, cpu_seconds: u64) -> std::io::Result<()> {
    let memory_limit =
        libc::rlim_t::try_from(memory_limit_bytes).map_err(|_| invalid_worker_data())?;
    let cpu_limit = libc::rlim_t::try_from(cpu_seconds).map_err(|_| invalid_worker_data())?;

    macro_rules! set_limit {
        ($resource:expr, $value:expr) => {{
            let limit = libc::rlimit {
                rlim_cur: $value,
                rlim_max: $value,
            };
            // SAFETY: 有効なresource定数と初期化済みrlimitへの参照だけを渡す。
            let result = unsafe { libc::setrlimit($resource, &limit) };
            if result != 0 {
                return Err(std::io::Error::last_os_error());
            }
        }};
    }

    #[cfg(target_os = "linux")]
    set_limit!(libc::RLIMIT_AS, memory_limit);
    // ponytail: macOSのRLIMIT_ASはEINVALになるか強制されないため、開発環境は親のwall timeoutで補完する。
    #[cfg(not(target_os = "linux"))]
    let _ = memory_limit;
    set_limit!(libc::RLIMIT_CPU, cpu_limit);
    set_limit!(libc::RLIMIT_CORE, 0);
    Ok(())
}

#[cfg(not(unix))]
fn apply_worker_resource_limits(
    _memory_limit_bytes: u64,
    _cpu_seconds: u64,
) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "documentation renderer resource limits require Unix",
    ))
}

fn read_worker_request<R: Read>(reader: &mut R) -> std::io::Result<WorkerRequest> {
    let frame_len = read_sync_u64(reader)?;
    if frame_len > max_worker_request_payload_bytes() as u64 {
        return Err(invalid_worker_data());
    }
    let request = {
        let mut frame = reader.take(frame_len);
        let mut magic = [0_u8; 8];
        frame.read_exact(&mut magic)?;
        if &magic != WORKER_REQUEST_MAGIC {
            return Err(invalid_worker_data());
        }
        if read_sync_u8(&mut frame)? != WORKER_PROTOCOL_VERSION {
            return Err(invalid_worker_data());
        }
        let namespace = read_sync_string(&mut frame, MAX_WORKER_COORDINATE_BYTES)?;
        let name = read_sync_string(&mut frame, MAX_WORKER_COORDINATE_BYTES)?;
        let version = read_sync_string(&mut frame, MAX_WORKER_COORDINATE_BYTES)?;
        let coordinate = DocsCoordinate::new(&namespace, &name, &version)
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
        let archive_len = read_sync_u64(&mut frame)?;
        if archive_len > DOCS_ARCHIVE_MAX_BYTES {
            return Err(invalid_worker_data());
        }
        let archive_len = usize::try_from(archive_len).map_err(|_| invalid_worker_data())?;
        let mut archive = vec![0_u8; archive_len];
        frame.read_exact(&mut archive)?;
        if frame.limit() != 0 {
            return Err(invalid_worker_data());
        }
        WorkerRequest {
            coordinate,
            archive,
        }
    };
    let mut trailing = [0_u8; 1];
    if reader.read(&mut trailing)? != 0 {
        return Err(invalid_worker_data());
    }
    Ok(request)
}

fn max_worker_request_payload_bytes() -> usize {
    WORKER_REQUEST_MAGIC.len()
        + 1
        + 3 * (4 + MAX_WORKER_COORDINATE_BYTES)
        + 8
        + DOCS_ARCHIVE_MAX_BYTES as usize
}

fn read_sync_string<R: Read>(reader: &mut R, limit: usize) -> std::io::Result<String> {
    let bytes = read_sync_bytes(reader, limit)?;
    String::from_utf8(bytes).map_err(|_| invalid_worker_data())
}

fn write_worker_response<W: IoWrite>(
    writer: &mut W,
    response: &WorkerResponse,
) -> std::io::Result<()> {
    let payload_len = worker_response_payload_len(response)?;
    writer.write_all(&payload_len.to_le_bytes())?;
    writer.write_all(WORKER_RESPONSE_MAGIC)?;
    writer.write_all(&[WORKER_PROTOCOL_VERSION])?;
    match response {
        WorkerResponse::Rendered(pages) => {
            writer.write_all(&[0])?;
            write_sync_u32(writer, pages.len())?;
            for page in pages {
                let (kind, slug) = encode_worker_path(&page.path);
                writer.write_all(&[kind])?;
                write_sync_bytes(writer, slug)?;
                writer.write_all(&(page.body.len() as u64).to_le_bytes())?;
                writer.write_all(&page.body)?;
            }
        }
        WorkerResponse::Invalid(detail) => {
            writer.write_all(&[1])?;
            write_sync_bytes(writer, detail.as_bytes())?;
        }
    }
    writer.flush()
}

fn worker_response_payload_len(response: &WorkerResponse) -> std::io::Result<u64> {
    let mut length = (WORKER_RESPONSE_MAGIC.len() + 2) as u64;
    match response {
        WorkerResponse::Rendered(pages) => {
            length = length.checked_add(4).ok_or_else(invalid_worker_data)?;
            for page in pages {
                let (_, slug) = encode_worker_path(&page.path);
                length = length
                    .checked_add(1 + 4 + 8)
                    .and_then(|length| length.checked_add(slug.len() as u64))
                    .and_then(|length| length.checked_add(page.body.len() as u64))
                    .ok_or_else(invalid_worker_data)?;
            }
        }
        WorkerResponse::Invalid(detail) => {
            if detail.len() > MAX_WORKER_ERROR_BYTES {
                return Err(invalid_worker_data());
            }
            length = length
                .checked_add(4 + detail.len() as u64)
                .ok_or_else(invalid_worker_data)?;
        }
    }
    Ok(length)
}

fn encode_worker_path(path: &DocsPagePath) -> (u8, &[u8]) {
    match path {
        DocsPagePath::Index => (0, &[]),
        DocsPagePath::Named(path) => match path.kind {
            DocsPageKind::Resource => (1, path.slug.as_bytes()),
            DocsPageKind::DataSource => (2, path.slug.as_bytes()),
        },
    }
}

fn write_sync_u32<W: IoWrite>(writer: &mut W, value: usize) -> std::io::Result<()> {
    let value = u32::try_from(value).map_err(|_| invalid_worker_data())?;
    writer.write_all(&value.to_le_bytes())
}

fn write_sync_bytes<W: IoWrite>(writer: &mut W, bytes: &[u8]) -> std::io::Result<()> {
    write_sync_u32(writer, bytes.len())?;
    writer.write_all(bytes)
}

fn into_worker_pages(pages: Vec<RenderedPage>) -> Vec<WorkerPage> {
    pages
        .into_iter()
        .map(|page| {
            let RenderedObjectPath::Page(path) = page.path else {
                unreachable!("render worker must return HTML pages only");
            };
            WorkerPage {
                path,
                body: page.body,
            }
        })
        .collect()
}

/// Test-only in-process rendering; archive and output validation still apply.
#[cfg(any(test, feature = "test-support"))]
pub fn validate_and_render_docs_bundle(
    archive: Vec<u8>,
    coordinate: &DocsCoordinate,
) -> Result<ValidatedBundle, DocsBundleError> {
    let version_root = DocsVersionRoot::new(
        coordinate.namespace(),
        coordinate.name(),
        coordinate.version(),
    );
    let (expected_archive, rendered_pages) = render_archive_pages(&archive, &version_root)?;
    build_validated_bundle_from_worker(
        archive,
        coordinate,
        &expected_archive,
        into_worker_pages(rendered_pages),
    )
    .map_err(DocsBundleError::new)
}

/// 上限適用済みの圧縮bytesを検証し、HTML pageだけを生成するworker内部境界。
fn render_archive_pages(
    archive: &[u8],
    version_root: &DocsVersionRoot,
) -> Result<(ValidatedDocsArchive, Vec<RenderedPage>), DocsBundleError> {
    let validated = visit_docs_archive(archive, |_, _| Ok(()))?;
    // The child needs the same navigation for server-rendered sidebars, but only the parent emits
    // diagnostics so one incomplete version produces one warning.
    let navigation = docs_navigation_for_version(validated.pages(), version_root, false);
    let rendered_pages = render_validated_archive(
        archive,
        &validated,
        version_root,
        &navigation,
        MAX_RENDERED_HTML_BYTES,
    )?;
    Ok((validated, rendered_pages))
}

/// gzipとtarを先頭から読み、各fileを一つずつ検証してvisitorへ渡す。
fn visit_docs_archive<F>(
    archive: &[u8],
    visit_file: F,
) -> Result<ValidatedDocsArchive, DocsBundleError>
where
    F: FnMut(&ValidatedDocsPage, String) -> Result<(), DocsBundleError>,
{
    if archive.len() as u64 > DOCS_ARCHIVE_MAX_BYTES {
        return Err(DocsBundleError::new(
            "compressed documentation archive exceeds the 16 MiB limit",
        ));
    }
    let compressed = Cursor::new(archive);
    let decoder = GzDecoder::new(compressed);
    let mut tar = CappedReader::new(decoder, MAX_TAR_STREAM_BYTES);
    let validated = parse_tar(&mut tar, visit_file)?;
    let decoder = tar.into_inner();
    let mut compressed = decoder.into_inner();
    if !compressed.fill_buf().map_err(io_error)?.is_empty() {
        return Err(DocsBundleError::new(
            "gzip stream contains trailing data or a second member",
        ));
    }
    Ok(validated)
}

fn parse_tar<R, F>(
    reader: &mut CappedReader<R>,
    mut visit_file: F,
) -> Result<ValidatedDocsArchive, DocsBundleError>
where
    R: Read,
    F: FnMut(&ValidatedDocsPage, String) -> Result<(), DocsBundleError>,
{
    let mut paths = HashSet::new();
    let mut has_index = false;
    let mut pending_pax = PaxOverrides::default();
    let mut entry_count = 0_usize;
    let mut total_file_bytes = 0_u64;
    let mut pages = Vec::new();

    loop {
        let header = read_block(reader)?;
        if is_zero_block(&header) {
            if !is_zero_block(&read_block(reader)?) {
                return Err(DocsBundleError::new(
                    "tar end marker is not two zero blocks",
                ));
            }
            if !pending_pax.is_empty() {
                return Err(DocsBundleError::new(
                    "pax extended header has no following entry",
                ));
            }
            ensure_zero_to_end(reader)?;
            break;
        }

        let parsed = parse_header(&header)?;
        match parsed.kind {
            b'g' => {
                return Err(DocsBundleError::new("pax global headers are not permitted"));
            }
            b'x' => {
                if parsed.size > MAX_PAX_EXTENDED_HEADER_BYTES {
                    return Err(DocsBundleError::new(
                        "pax extended header exceeds the 64 KiB limit",
                    ));
                }
                let body = read_body(reader, parsed.size)?;
                pending_pax.present = true;
                pending_pax.apply(&body)?;
                continue;
            }
            0 | b'0' | b'5' => {}
            b'L' | b'K' => {
                return Err(DocsBundleError::new(
                    "GNU long-name and long-link entries are not permitted",
                ));
            }
            kind => {
                return Err(DocsBundleError::new(format!(
                    "tar entry type {kind:?} is not a regular file or directory"
                )));
            }
        }

        let is_directory = parsed.kind == b'5';
        let path = pending_pax.path.take().unwrap_or(parsed.path);
        let size = pending_pax.size.take().unwrap_or(parsed.size);
        pending_pax.present = false;
        if is_directory && size != 0 {
            return Err(DocsBundleError::new(
                "directory entry declares a nonzero size",
            ));
        }
        let Some(path) = normalize_path(path, is_directory)? else {
            skip_body(reader, size)?;
            continue;
        };

        entry_count = entry_count
            .checked_add(1)
            .ok_or_else(|| DocsBundleError::new("entry count overflow"))?;
        if entry_count > MAX_ENTRIES {
            return Err(DocsBundleError::new("archive exceeds 4096 entries"));
        }
        if !paths.insert(path.clone()) {
            return Err(DocsBundleError::new("archive contains a duplicate path"));
        }

        if is_directory {
            if path != b"resources" && path != b"data-sources" {
                return Err(DocsBundleError::new(
                    "archive contains a directory outside the allowed roots",
                ));
            }
            skip_body(reader, size)?;
            continue;
        }

        if size > MAX_FILE_BYTES {
            return Err(DocsBundleError::new("file exceeds the 256 KiB limit"));
        }
        total_file_bytes = total_file_bytes
            .checked_add(size)
            .ok_or_else(|| DocsBundleError::new("total file size overflow"))?;
        if total_file_bytes > MAX_TOTAL_FILE_BYTES {
            return Err(DocsBundleError::new(
                "archive files exceed the 64 MiB total limit",
            ));
        }
        let destination = classify_file(&path)?;
        let body = read_body(reader, size)?;
        let markdown = String::from_utf8(body)
            .map_err(|_| DocsBundleError::new("documentation file is not UTF-8"))?;
        validate_table_cell_bound(&markdown)?;
        if matches!(destination, DocsPagePath::Index) {
            has_index = true;
        }
        let page = ValidatedDocsPage::parse(destination, &markdown)?;
        visit_file(&page, markdown)?;
        pages.push(page);
    }

    if !has_index {
        return Err(DocsBundleError::new("index.md is missing"));
    }
    Ok(validated_docs_archive(pages))
}

fn validate_provider_navigation_metadata(
    bytes: &[u8],
) -> Result<ProviderNavigationMetadata, ProviderNavigationMetadataError> {
    let ProviderNavigationDocument { services } =
        serde_json::from_slice(bytes).map_err(|error| {
            ProviderNavigationMetadataError::new(format!("JSON is invalid: {error}"))
        })?;
    if services.is_empty() {
        return Err(ProviderNavigationMetadataError::new(
            "services must be a non-empty array",
        ));
    }

    let mut ids = HashSet::with_capacity(services.len());
    let mut titles = HashSet::with_capacity(services.len());
    let mut assigned_resources = HashSet::new();
    let mut assigned_data_sources = HashSet::new();
    let mut validated_services = Vec::with_capacity(services.len());

    for declaration in services {
        let ServiceDeclaration {
            id,
            title,
            resources,
            data_sources,
        } = declaration;
        if !valid_service_id_or_slug(&id) {
            return Err(ProviderNavigationMetadataError::new(
                "service id does not match the required form",
            ));
        }
        if title.is_empty()
            || title.chars().count() > 128
            || title.chars().any(invalid_service_title_character)
        {
            return Err(ProviderNavigationMetadataError::new(
                "service title is invalid",
            ));
        }
        if !ids.insert(id.clone()) {
            return Err(ProviderNavigationMetadataError::new(
                "contains a duplicate service id",
            ));
        }
        if !titles.insert(title.clone()) {
            return Err(ProviderNavigationMetadataError::new(
                "contains a duplicate service title",
            ));
        }
        if resources.is_empty() && data_sources.is_empty() {
            return Err(ProviderNavigationMetadataError::new(
                "contains a service with no pages",
            ));
        }

        let mut resources = validate_service_slugs(resources, "resource", &mut assigned_resources)?;
        let mut data_sources =
            validate_service_slugs(data_sources, "data source", &mut assigned_data_sources)?;
        resources.sort_unstable();
        data_sources.sort_unstable();
        validated_services.push(ProviderServiceMetadata {
            id,
            title,
            resources,
            data_sources,
        });
    }

    validated_services.sort_unstable_by(|left, right| left.id.cmp(&right.id));
    Ok(ProviderNavigationMetadata {
        services: validated_services,
    })
}

fn embedded_provider_navigation_metadata_bytes(
    namespace: &str,
    name: &str,
) -> Option<&'static [u8]> {
    EMBEDDED_PROVIDER_NAVIGATION_METADATA
        .iter()
        .find(|metadata| metadata.namespace == namespace && metadata.name == name)
        .map(|metadata| metadata.bytes)
}

struct DocsNavigationDerivation {
    navigation: DocsNavigation,
    unassigned_resources: Vec<String>,
    unassigned_data_sources: Vec<String>,
    duplicate_type_name_sections: Vec<String>,
}

/// validated pagesとvalidated provider metadataからnavigationを構築する唯一の入口。
fn derive_docs_navigation(
    pages: &[ValidatedDocsPage],
    metadata: Option<&ProviderNavigationMetadata>,
) -> DocsNavigationDerivation {
    let (resources, data_sources) = sorted_page_sections(pages);
    let Some(metadata) = metadata else {
        return DocsNavigationDerivation {
            navigation: DocsNavigation::Flat {
                resources,
                data_sources,
            },
            unassigned_resources: Vec::new(),
            unassigned_data_sources: Vec::new(),
            duplicate_type_name_sections: Vec::new(),
        };
    };

    let pages_by_path = pages
        .iter()
        .filter_map(ValidatedDocsPage::named)
        .map(|page| (page.path(), page))
        .collect::<HashMap<_, _>>();
    let mut assigned = HashSet::with_capacity(pages_by_path.len());
    let mut services = Vec::with_capacity(metadata.services.len());
    for metadata_service in &metadata.services {
        let resources = metadata_service
            .resources
            .iter()
            .filter_map(|slug| {
                let path = NamedPagePath::new(DocsPageKind::Resource, slug);
                let page = *pages_by_path.get(&path)?;
                assigned.insert(page.path());
                Some(page.clone())
            })
            .collect::<Vec<_>>();
        let data_sources = metadata_service
            .data_sources
            .iter()
            .filter_map(|slug| {
                let path = NamedPagePath::new(DocsPageKind::DataSource, slug);
                let page = *pages_by_path.get(&path)?;
                assigned.insert(page.path());
                Some(page.clone())
            })
            .collect::<Vec<_>>();
        if resources.is_empty() && data_sources.is_empty() {
            continue;
        }
        services.push(DocsService {
            id: metadata_service.id.clone(),
            title: metadata_service.title.clone(),
            resources,
            data_sources,
        });
    }

    let mut duplicate_type_name_sections = Vec::new();
    for service in &services {
        if has_duplicate_type_names(&service.resources) {
            duplicate_type_name_sections.push(format!("{}.resources", service.id));
        }
        if has_duplicate_type_names(&service.data_sources) {
            duplicate_type_name_sections.push(format!("{}.data_sources", service.id));
        }
    }

    let unassigned_resources = resources
        .iter()
        .filter(|page| !assigned.contains(page.path()))
        .map(|page| page.slug().to_string())
        .collect::<Vec<_>>();
    let unassigned_data_sources = data_sources
        .iter()
        .filter(|page| !assigned.contains(page.path()))
        .map(|page| page.slug().to_string())
        .collect::<Vec<_>>();
    if !unassigned_resources.is_empty()
        || !unassigned_data_sources.is_empty()
        || !duplicate_type_name_sections.is_empty()
    {
        return DocsNavigationDerivation {
            navigation: DocsNavigation::Flat {
                resources,
                data_sources,
            },
            unassigned_resources,
            unassigned_data_sources,
            duplicate_type_name_sections,
        };
    }

    let mut service_of = HashMap::with_capacity(pages_by_path.len());
    for (index, service) in services.iter().enumerate() {
        for page in service.resources.iter().chain(&service.data_sources) {
            service_of.insert(page.path().clone(), index);
        }
    }
    DocsNavigationDerivation {
        navigation: DocsNavigation::Grouped(GroupedNavigation {
            services,
            service_of,
        }),
        unassigned_resources,
        unassigned_data_sources,
        duplicate_type_name_sections,
    }
}

fn has_duplicate_type_names(pages: &[NamedDocsPage]) -> bool {
    let mut type_names = HashSet::with_capacity(pages.len());
    pages
        .iter()
        .any(|page| !type_names.insert(page.title.type_name()))
}

fn docs_navigation_for_metadata_bytes(
    pages: &[ValidatedDocsPage],
    version_root: &DocsVersionRoot,
    metadata_bytes: Option<&[u8]>,
    emit_diagnostics: bool,
) -> DocsNavigation {
    let metadata = match metadata_bytes.map(validate_provider_navigation_metadata) {
        Some(Ok(metadata)) => Some(metadata),
        Some(Err(error)) => {
            if emit_diagnostics {
                tracing::error!(
                    namespace = %version_root.namespace,
                    name = %version_root.name,
                    version = %version_root.version,
                    %error,
                    "embedded provider navigation metadata is invalid; using flat navigation"
                );
            }
            None
        }
        None => None,
    };
    let derivation = derive_docs_navigation(pages, metadata.as_ref());
    if emit_diagnostics
        && metadata.is_some()
        && (!derivation.unassigned_resources.is_empty()
            || !derivation.unassigned_data_sources.is_empty()
            || !derivation.duplicate_type_name_sections.is_empty())
    {
        tracing::warn!(
            namespace = %version_root.namespace,
            name = %version_root.name,
            version = %version_root.version,
            unassigned_resources = ?derivation.unassigned_resources,
            unassigned_data_sources = ?derivation.unassigned_data_sources,
            duplicate_type_name_sections = ?derivation.duplicate_type_name_sections,
            "provider navigation metadata does not cover documentation version; using flat navigation"
        );
    }
    derivation.navigation
}

fn docs_navigation_for_version(
    pages: &[ValidatedDocsPage],
    version_root: &DocsVersionRoot,
    emit_diagnostics: bool,
) -> DocsNavigation {
    docs_navigation_for_metadata_bytes(
        pages,
        version_root,
        embedded_provider_navigation_metadata_bytes(&version_root.namespace, &version_root.name),
        emit_diagnostics,
    )
}

// public/providers/_assets/docs.jsの対応する検証およびRegistry Protocol（carina-rs/carinaのsite/src/content/reference/registry/protocol.md）と同期を保つこと。
fn invalid_service_title_character(character: char) -> bool {
    character.is_control()
        || matches!(
            character,
            '\u{00ad}'
                | '\u{061c}'
                | '\u{180e}'
                | '\u{200b}'..='\u{200f}'
                | '\u{2028}'..='\u{202e}'
                | '\u{2060}'..='\u{206f}'
                | '\u{feff}'
                | '\u{fff9}'..='\u{fffb}'
        )
}

fn validate_service_slugs(
    slugs: Vec<String>,
    kind: &str,
    assigned: &mut HashSet<String>,
) -> Result<Vec<String>, ProviderNavigationMetadataError> {
    let mut validated = Vec::with_capacity(slugs.len());
    for slug in slugs {
        if !valid_service_id_or_slug(&slug) {
            return Err(ProviderNavigationMetadataError::new(format!(
                "{kind} slug does not match the required form"
            )));
        }
        if !assigned.insert(slug.clone()) {
            return Err(ProviderNavigationMetadataError::new(format!(
                "assigns a {kind} slug more than once"
            )));
        }
        validated.push(slug);
    }
    Ok(validated)
}

fn valid_service_id_or_slug(value: &str) -> bool {
    let bytes = value.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= 128
        && matches!(bytes[0], b'a'..=b'z' | b'0'..=b'9')
        && bytes
            .iter()
            .all(|byte| matches!(byte, b'a'..=b'z' | b'0'..=b'9' | b'_'))
}

fn validate_table_cell_bound(markdown: &str) -> Result<(), DocsBundleError> {
    let bound = table_cell_bound(markdown)?;
    if bound > MAX_TABLE_CELL_BOUND {
        return Err(DocsBundleError::new(
            "documentation file pipe-by-line product exceeds the 250,000 limit",
        ));
    }
    Ok(())
}

fn table_cell_bound(markdown: &str) -> Result<usize, DocsBundleError> {
    let bytes = markdown.as_bytes();
    let mut largest_pipe_count = 0_usize;
    let mut current_pipe_count = 0_usize;
    let mut line_count = 0_usize;
    let mut index = 0_usize;

    while index < bytes.len() {
        match bytes[index] {
            b'|' => current_pipe_count += 1,
            b'\r' => {
                largest_pipe_count = largest_pipe_count.max(current_pipe_count);
                current_pipe_count = 0;
                line_count += 1;
                if bytes.get(index + 1) == Some(&b'\n') {
                    index += 1;
                }
            }
            b'\n' => {
                largest_pipe_count = largest_pipe_count.max(current_pipe_count);
                current_pipe_count = 0;
                line_count += 1;
            }
            _ => {}
        }
        index += 1;
    }

    if bytes
        .last()
        .is_some_and(|byte| !matches!(byte, b'\r' | b'\n'))
    {
        largest_pipe_count = largest_pipe_count.max(current_pipe_count);
        line_count += 1;
    }
    let bound = largest_pipe_count
        .checked_add(1)
        .and_then(|columns| columns.checked_mul(line_count))
        .ok_or_else(|| DocsBundleError::new("documentation table-cell bound overflows"))?;
    Ok(bound)
}

fn render_validated_archive(
    archive: &[u8],
    validated_archive: &ValidatedDocsArchive,
    version_root: &DocsVersionRoot,
    navigation: &DocsNavigation,
    max_rendered_bytes: usize,
) -> Result<Vec<RenderedPage>, DocsBundleError> {
    let targets = Arc::new(
        validated_archive
            .pages()
            .iter()
            .map(|page| (page.archive_path(), page.path()))
            .collect::<HashMap<_, _>>(),
    );
    let mut rendered_pages = Vec::with_capacity(validated_archive.pages().len());
    let mut total_bytes = 0_usize;
    let mut expected_pages = validated_archive.pages().iter();

    let rendered_archive = visit_docs_archive(archive, |page, markdown| {
        if expected_pages.next() != Some(page) {
            return Err(DocsBundleError::new(
                "documentation archive changed between validation and rendering",
            ));
        }
        let page = render_documentation_page(
            page,
            &markdown,
            version_root,
            navigation,
            Arc::clone(&targets),
        )?;
        drop(markdown);
        let body_len = page.body.len();
        total_bytes = total_bytes
            .checked_add(body_len)
            .ok_or_else(|| DocsBundleError::new("rendered documentation size overflow"))?;
        if total_bytes > max_rendered_bytes {
            return Err(DocsBundleError::new(
                "rendered documentation exceeds the 64 MiB total limit",
            ));
        }
        rendered_pages.push(page);
        Ok(())
    })?;

    if expected_pages.next().is_some() || rendered_archive != *validated_archive {
        return Err(DocsBundleError::new(
            "documentation archive changed between validation and rendering",
        ));
    }
    Ok(rendered_pages)
}

fn sorted_page_sections(
    validated_pages: &[ValidatedDocsPage],
) -> (Vec<NamedDocsPage>, Vec<NamedDocsPage>) {
    let mut resources = validated_pages
        .iter()
        .filter_map(ValidatedDocsPage::named)
        .filter(|page| page.kind() == DocsPageKind::Resource)
        .cloned()
        .collect::<Vec<_>>();
    let mut data_sources = validated_pages
        .iter()
        .filter_map(ValidatedDocsPage::named)
        .filter(|page| page.kind() == DocsPageKind::DataSource)
        .cloned()
        .collect::<Vec<_>>();
    resources.sort_unstable_by(|left, right| left.slug().cmp(right.slug()));
    data_sources.sort_unstable_by(|left, right| left.slug().cmp(right.slug()));
    (resources, data_sources)
}

fn append_navigation_document(
    rendered_pages: &mut Vec<RenderedPage>,
    navigation: &DocsNavigation,
    version_root: &DocsVersionRoot,
    max_rendered_bytes: usize,
) -> Result<(), DocsBundleError> {
    let rendered_bytes = rendered_pages.iter().try_fold(0_usize, |total, page| {
        total
            .checked_add(page.body.len())
            .ok_or_else(|| DocsBundleError::new("rendered documentation size overflow"))
    })?;
    let navigation = render_navigation_document(version_root, navigation)?;
    let total_bytes = rendered_bytes
        .checked_add(navigation.body.len())
        .ok_or_else(|| DocsBundleError::new("rendered documentation size overflow"))?;
    if total_bytes > max_rendered_bytes {
        return Err(DocsBundleError::new(
            "rendered documentation exceeds the 64 MiB total limit",
        ));
    }
    rendered_pages.push(navigation);
    Ok(())
}

fn render_documentation_page(
    page: &ValidatedDocsPage,
    markdown: &str,
    version_root: &DocsVersionRoot,
    navigation: &DocsNavigation,
    targets: Arc<HashMap<String, DocsPagePath>>,
) -> Result<RenderedPage, DocsBundleError> {
    let rendered = render_markdown(
        markdown,
        &page.archive_path(),
        targets,
        version_root.shared_host_path(),
    )?;
    let title = match page {
        ValidatedDocsPage::Index => version_root.index_page_title(),
        ValidatedDocsPage::Named(page) => version_root.page_title(page),
    };
    let body =
        render_page_template(&title, version_root, page, navigation, &rendered)?.into_bytes();
    Ok(RenderedPage {
        version_root: version_root.clone(),
        path: RenderedObjectPath::Page(page.path()),
        body,
    })
}

#[derive(Serialize)]
struct FlatNavigationDocument<'a> {
    overview: &'a str,
    resources: Vec<NavigationEntry<'a>>,
    data_sources: Vec<NavigationEntry<'a>>,
}

#[derive(Serialize)]
struct GroupedNavigationDocument<'a> {
    overview: &'a str,
    services: Vec<NavigationService<'a>>,
}

#[derive(Serialize)]
struct NavigationService<'a> {
    id: &'a str,
    title: &'a str,
    resources: Vec<NavigationEntry<'a>>,
    data_sources: Vec<NavigationEntry<'a>>,
}

#[derive(Serialize)]
struct NavigationEntry<'a> {
    slug: &'a str,
    title: &'a str,
    href: String,
}

fn render_navigation_document(
    version_root: &DocsVersionRoot,
    navigation: &DocsNavigation,
) -> Result<RenderedPage, DocsBundleError> {
    let body = match navigation {
        DocsNavigation::Flat {
            resources,
            data_sources,
        } => serde_json::to_vec(&FlatNavigationDocument {
            overview: version_root.host_path(),
            resources: navigation_entries(version_root, resources),
            data_sources: navigation_entries(version_root, data_sources),
        }),
        DocsNavigation::Grouped(grouped) => serde_json::to_vec(&GroupedNavigationDocument {
            overview: version_root.host_path(),
            services: grouped
                .services
                .iter()
                .map(|service| NavigationService {
                    id: &service.id,
                    title: &service.title,
                    resources: navigation_entries(version_root, &service.resources),
                    data_sources: navigation_entries(version_root, &service.data_sources),
                })
                .collect(),
        }),
    };
    let mut body = body.map_err(|error| {
        DocsBundleError::new(format!("navigation JSON rendering failed: {error}"))
    })?;
    body.push(b'\n');
    Ok(RenderedPage {
        version_root: version_root.clone(),
        path: RenderedObjectPath::Navigation,
        body,
    })
}

fn navigation_entries<'a>(
    version_root: &DocsVersionRoot,
    pages: &'a [NamedDocsPage],
) -> Vec<NavigationEntry<'a>> {
    pages
        .iter()
        .map(|page| NavigationEntry {
            slug: page.slug(),
            title: page.title.as_str(),
            href: format!("{}{}", version_root.host_path(), page.key_suffix()),
        })
        .collect()
}

/// 現在のrenderer出力を表すfingerprintを、信頼済みの組み込みbundleから生成する。
pub fn assemble_render_fingerprint(
    index_outputs: impl IntoIterator<Item = (String, String, Vec<u8>)>,
    content_types: RenderContentTypes,
) -> Result<String, DocsBundleError> {
    render_reference_fingerprint_inputs(index_outputs, content_types).map(|inputs| {
        fingerprint_with_highlighting_inputs(
            &inputs,
            CRN_GRAMMAR.as_bytes(),
            CARINA_DARK_THEME.as_bytes(),
            SYNTECT_CRATE_VERSION.as_bytes(),
            content_types.json,
        )
    })
}

const REFERENCE_INDEX: &str = concat!(
    "# Reference provider\n\n",
    "## Render features\n\n",
    "| left | center | right |\n",
    "| :--- | :----: | ----: |\n",
    "| alpha | beta | gamma |\n\n",
    "```rust\nlet reference = true;\n```\n\n",
    "```crn\nlet region = \"ap-northeast-1\" # reference\n```\n\n",
    "```json\n{\"reference\": true}\n```\n\n",
    "```sh\necho \"reference\"\n```\n\n",
    "## Duplicate heading\n\n",
    "[Fragment only](#duplicate-heading)\n\n",
    "## Duplicate heading\n\n",
    "[Resource fragment](resources/reference_resource.md#arguments)\n\n",
    "[Mail](mailto:docs@example.com)\n\n",
    "[JavaScript link](javascript:alert(1))\n\n",
    "[Missing file](missing.md)\n\n",
    "[Above root](../outside.md)\n\n",
    "~~deprecated~~\n\n",
    "Author <em id=\"author-written-id\">inline HTML</em>.\n\n",
    "![Reference image](https://example.com/reference.png)\n\n",
    "- [x] task syntax stays text\n\n",
    "footnote[^1]\n\n",
    "[^1]: definition\n\n",
    "<script>raw-html-must-be-dropped</script>\n\n",
    "<https://example.com/reference>\n",
);
const REFERENCE_RESOURCE: &str = "# reference.Resource\n\n## Arguments\n\n`name` configures it.\n";
const REFERENCE_DATA_SOURCE: &str = "# reference.DataSource\n\n## Result\n\nReturns a value.\n";
const REFERENCE_PROVIDER_NAVIGATION_METADATA: &str = concat!(
    "{\"services\":[{\"id\":\"reference\",\"title\":\"Reference Service\",",
    "\"resources\":[\"reference_resource\"],",
    "\"data_sources\":[\"reference_data\"]}]}"
);

fn render_reference_docs_bundle() -> Result<Vec<RenderedPage>, DocsBundleError> {
    let sources = [
        (DocsPagePath::Index, REFERENCE_INDEX),
        (
            DocsPagePath::Named(NamedPagePath::new(
                DocsPageKind::Resource,
                "reference_resource",
            )),
            REFERENCE_RESOURCE,
        ),
        (
            DocsPagePath::Named(NamedPagePath::new(
                DocsPageKind::DataSource,
                "reference_data",
            )),
            REFERENCE_DATA_SOURCE,
        ),
    ];
    let pages = sources
        .iter()
        .map(|(path, markdown)| {
            validate_table_cell_bound(markdown)?;
            ValidatedDocsPage::parse(path.clone(), markdown)
        })
        .collect::<Result<Vec<_>, _>>()?;
    let navigation = derive_docs_navigation(&pages, None).navigation;
    let targets = Arc::new(
        pages
            .iter()
            .map(|page| (page.archive_path(), page.path()))
            .collect::<HashMap<_, _>>(),
    );
    let version_root = DocsVersionRoot::new("carina-rs", "render-reference", "1.0.0");

    let mut rendered = sources
        .iter()
        .zip(&pages)
        .map(|((_, markdown), page)| {
            render_documentation_page(
                page,
                markdown,
                &version_root,
                &navigation,
                Arc::clone(&targets),
            )
        })
        .collect::<Result<Vec<_>, _>>()?;
    append_navigation_document(
        &mut rendered,
        &navigation,
        &version_root,
        MAX_RENDERED_HTML_BYTES,
    )?;
    Ok(rendered)
}

fn render_grouped_reference_docs_bundle() -> Result<Vec<RenderedPage>, DocsBundleError> {
    let archive = reference_docs_archive_bytes()?;
    let version_root = DocsVersionRoot::new("carina-rs", "render-reference-grouped", "1.0.0");
    let (validated, mut rendered) = render_archive_pages(&archive, &version_root)?;
    let navigation = docs_navigation_for_version(validated.pages(), &version_root, true);
    append_navigation_document(
        &mut rendered,
        &navigation,
        &version_root,
        MAX_RENDERED_HTML_BYTES,
    )?;
    Ok(rendered)
}

pub fn reference_docs_archive_bytes() -> Result<Vec<u8>, DocsBundleError> {
    let mut tar = Vec::new();
    append_reference_tar_entry(&mut tar, b"./resources/", b'5', b"");
    append_reference_tar_entry(&mut tar, b"./data-sources/", b'5', b"");
    append_reference_tar_entry(&mut tar, b"./index.md", b'0', REFERENCE_INDEX.as_bytes());
    append_reference_tar_entry(
        &mut tar,
        b"./resources/reference_resource.md",
        b'0',
        REFERENCE_RESOURCE.as_bytes(),
    );
    append_reference_tar_entry(
        &mut tar,
        b"./data-sources/reference_data.md",
        b'0',
        REFERENCE_DATA_SOURCE.as_bytes(),
    );
    tar.extend([0_u8; BLOCK_BYTES * 2]);

    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    encoder
        .write_all(&tar)
        .map_err(|error| DocsBundleError::new(format!("reference bundle gzip failed: {error}")))?;
    encoder
        .finish()
        .map_err(|error| DocsBundleError::new(format!("reference bundle gzip failed: {error}")))
}

fn append_reference_tar_entry(tar: &mut Vec<u8>, path: &[u8], kind: u8, body: &[u8]) {
    debug_assert!(path.len() <= 100);
    let mut header = [0_u8; BLOCK_BYTES];
    header[..path.len()].copy_from_slice(path);
    write_reference_tar_octal(&mut header[100..108], 0o644);
    write_reference_tar_octal(&mut header[108..116], 0);
    write_reference_tar_octal(&mut header[116..124], 0);
    write_reference_tar_octal(&mut header[124..136], body.len() as u64);
    write_reference_tar_octal(&mut header[136..148], 0);
    header[148..156].fill(b' ');
    header[156] = kind;
    header[257..263].copy_from_slice(b"ustar\0");
    header[263..265].copy_from_slice(b"00");
    let checksum = header.iter().map(|byte| u64::from(*byte)).sum();
    write_reference_tar_checksum(&mut header[148..156], checksum);
    tar.extend(header);
    tar.extend(body);
    let padding = (BLOCK_BYTES - body.len() % BLOCK_BYTES) % BLOCK_BYTES;
    tar.resize(tar.len() + padding, 0);
}

fn write_reference_tar_octal(field: &mut [u8], value: u64) {
    let digits = format!("{:0width$o}", value, width = field.len() - 1);
    field.copy_from_slice(format!("{digits}\0").as_bytes());
}

fn write_reference_tar_checksum(field: &mut [u8], value: u64) {
    let digits = format!("{value:06o}");
    field[..6].copy_from_slice(digits.as_bytes());
    field[6] = 0;
    field[7] = b' ';
}

#[derive(Debug, Clone)]
struct FingerprintInput {
    key: String,
    content_type: String,
    body: Vec<u8>,
}

fn render_reference_fingerprint_inputs(
    index_outputs: impl IntoIterator<Item = (String, String, Vec<u8>)>,
    content_types: RenderContentTypes,
) -> Result<Vec<FingerprintInput>, DocsBundleError> {
    let mut pages = render_reference_docs_bundle()?;
    pages.extend(render_grouped_reference_docs_bundle()?);
    let mut inputs = pages
        .into_iter()
        .map(|page| {
            let (key, body, kind) = page.into_parts();
            let content_type = content_types.for_kind(kind);
            FingerprintInput {
                key,
                content_type: content_type.to_string(),
                body,
            }
        })
        .collect::<Vec<_>>();
    inputs.extend(
        index_outputs
            .into_iter()
            .map(|(key, content_type, body)| FingerprintInput {
                key,
                content_type,
                body,
            }),
    );
    Ok(inputs)
}

fn fingerprint_with_highlighting_inputs(
    outputs: &[FingerprintInput],
    crn_grammar: &[u8],
    theme: &[u8],
    default_syntax_identity: &[u8],
    json_content_type: &str,
) -> String {
    let navigation_metadata_digest = embedded_provider_navigation_metadata_digest();
    fingerprint_with_navigation_metadata_digest(
        outputs,
        crn_grammar,
        theme,
        default_syntax_identity,
        &navigation_metadata_digest,
        json_content_type,
    )
}

fn fingerprint_with_navigation_metadata_digest(
    outputs: &[FingerprintInput],
    crn_grammar: &[u8],
    theme: &[u8],
    default_syntax_identity: &[u8],
    navigation_metadata_digest: &[u8],
    json_content_type: &str,
) -> String {
    let highlighting_inputs: [(&str, &str, &[u8]); 3] = [
        (
            "highlight/crn.tmLanguage.json",
            json_content_type,
            crn_grammar,
        ),
        ("highlight/carina-dark.theme.json", json_content_type, theme),
        (
            "syntect-crate-version",
            "text/plain; charset=utf-8",
            default_syntax_identity,
        ),
    ];
    let navigation_metadata_input = [(
        "docs-navigation-metadata-digest",
        "application/octet-stream",
        navigation_metadata_digest,
    )];
    fingerprint_outputs(
        outputs
            .iter()
            .map(|input| {
                (
                    input.key.as_str(),
                    input.content_type.as_str(),
                    input.body.as_slice(),
                )
            })
            .chain(highlighting_inputs)
            .chain(navigation_metadata_input),
    )
}

fn embedded_provider_navigation_metadata_digest() -> [u8; 32] {
    provider_navigation_metadata_digest(
        EMBEDDED_PROVIDER_NAVIGATION_METADATA
            .iter()
            .map(|metadata| (metadata.namespace, metadata.name, metadata.bytes)),
    )
}

fn provider_navigation_metadata_digest<'a>(
    metadata: impl IntoIterator<Item = (&'a str, &'a str, &'a [u8])>,
) -> [u8; 32] {
    let mut metadata = metadata.into_iter().collect::<Vec<_>>();
    metadata.sort_unstable_by(|left, right| (left.0, left.1).cmp(&(right.0, right.1)));
    let mut digest = Sha256::new();
    for (namespace, name, bytes) in metadata {
        let key = format!("docs-navigation/{namespace}/{name}.json");
        hash_framed_bytes(&mut digest, key.as_bytes());
        hash_framed_bytes(&mut digest, bytes);
    }
    digest.finalize().into()
}

fn fingerprint_outputs<K, C, B>(outputs: impl IntoIterator<Item = (K, C, B)>) -> String
where
    K: AsRef<str>,
    C: AsRef<str>,
    B: AsRef<[u8]>,
{
    let mut digest = Sha256::new();
    for (key, content_type, body) in outputs {
        hash_framed_bytes(&mut digest, key.as_ref().as_bytes());
        hash_framed_bytes(&mut digest, content_type.as_ref().as_bytes());
        hash_framed_bytes(&mut digest, body.as_ref());
    }
    format!("{:x}", digest.finalize())
}

fn hash_framed_bytes(digest: &mut Sha256, bytes: &[u8]) {
    let length = u64::try_from(bytes.len()).expect("renderer output length must fit u64");
    digest.update(length.to_le_bytes());
    digest.update(bytes);
}

fn render_markdown(
    markdown: &str,
    current_archive_path: &str,
    targets: Arc<HashMap<String, DocsPagePath>>,
    version_root: Arc<str>,
) -> Result<String, DocsBundleError> {
    let nonce = format!("carina-heading-{}-", Uuid::new_v4());
    let heading_adapter = DocumentationHeadingAdapter::new(nonce.clone());
    let mut options = Options::default();
    options.extension.table = true;
    options.extension.strikethrough = true;
    options.extension.autolink = true;
    options.render.r#unsafe = false;
    let mut plugins = Plugins::default();
    plugins.render.heading_adapter = Some(&heading_adapter);
    let syntax_highlighter = highlighting().map_err(|error| {
        DocsBundleError::new(format!("syntax highlighter initialization failed: {error}"))
    })?;
    plugins.render.codefence_syntax_highlighter = Some(syntax_highlighter);
    let arena = Arena::new();
    let root = parse_document(&arena, markdown, &options);
    validate_parse_tree_depth(root)?;
    let mut rendered_html = CappedRenderedHtml::new(MAX_RENDERED_PAGE_HTML_BYTES);
    format_html_with_plugins(root, &options, &mut rendered_html, &plugins)
        .map_err(|_| DocsBundleError::new("rendered documentation page exceeds the 2 MiB limit"))?;
    let rendered_html = rendered_html.into_string();

    Ok(sanitize_html(
        &rendered_html,
        current_archive_path,
        targets,
        version_root,
        nonce,
    ))
}

fn validate_parse_tree_depth(root: Node<'_>) -> Result<(), DocsBundleError> {
    let mut pending = vec![(root, 0_usize)];
    while let Some((node, depth)) = pending.pop() {
        if depth > MAX_PARSE_TREE_DEPTH {
            return Err(DocsBundleError::new(
                "documentation parse tree exceeds the 64-level depth limit",
            ));
        }
        pending.extend(node.children().map(|child| (child, depth + 1)));
    }
    Ok(())
}

fn sanitize_html(
    html: &str,
    current_archive_path: &str,
    targets: Arc<HashMap<String, DocsPagePath>>,
    version_root: Arc<str>,
    nonce: String,
) -> String {
    let tags = [
        "h1",
        "h2",
        "h3",
        "h4",
        "h5",
        "h6",
        "p",
        "br",
        "hr",
        "blockquote",
        "ul",
        "ol",
        "li",
        "pre",
        "code",
        "em",
        "strong",
        "del",
        "a",
        "table",
        "thead",
        "tbody",
        "tr",
        "th",
        "td",
        "span",
    ]
    .into_iter()
    .collect::<HashSet<_>>();
    let clean_content_tags = [
        "script", "style", "iframe", "object", "embed", "template", "textarea", "svg", "math",
    ]
    .into_iter()
    .collect::<HashSet<_>>();
    let tag_attributes = [
        ("a", ["href"].into_iter().collect()),
        ("h1", ["id"].into_iter().collect()),
        ("h2", ["id"].into_iter().collect()),
        ("h3", ["id"].into_iter().collect()),
        ("h4", ["id"].into_iter().collect()),
        ("h5", ["id"].into_iter().collect()),
        ("h6", ["id"].into_iter().collect()),
        ("code", ["class"].into_iter().collect()),
        ("span", ["class"].into_iter().collect()),
        ("th", ["align"].into_iter().collect()),
        ("td", ["align"].into_iter().collect()),
    ]
    .into_iter()
    .collect::<HashMap<_, HashSet<_>>>();
    let url_schemes = ["http", "https", "mailto"]
        .into_iter()
        .collect::<HashSet<_>>();
    let current_archive_path = current_archive_path.to_string();
    let mut sanitizer = Builder::new();
    sanitizer
        .tags(tags)
        .clean_content_tags(clean_content_tags)
        .generic_attributes(HashSet::new())
        .tag_attributes(tag_attributes)
        .url_schemes(url_schemes)
        .url_relative(UrlRelative::PassThrough)
        .link_rel(None)
        .strip_comments(true)
        .attribute_filter(
            move |element, attribute, value| match (element, attribute) {
                ("a", "href") => {
                    rewrite_href(value, &current_archive_path, &targets, &version_root)
                }
                ("h1" | "h2" | "h3" | "h4" | "h5" | "h6", "id") => value
                    .strip_prefix(&nonce)
                    .map(|slug| Cow::Owned(format!("user-content-{slug}"))),
                ("code", "class") if valid_code_class(value) => Some(Cow::Borrowed(value)),
                ("span", "class") if valid_highlight_classes(value) => Some(Cow::Borrowed(value)),
                ("th" | "td", "align") => Some(Cow::Borrowed(value)),
                _ => None,
            },
        );
    sanitizer.clean(html).to_string()
}

struct CappedRenderedHtml {
    html: String,
    limit: usize,
}

impl CappedRenderedHtml {
    fn new(limit: usize) -> Self {
        Self {
            html: String::new(),
            limit,
        }
    }

    fn into_string(self) -> String {
        self.html
    }
}

impl fmt::Write for CappedRenderedHtml {
    fn write_str(&mut self, value: &str) -> fmt::Result {
        if self
            .html
            .len()
            .checked_add(value.len())
            .is_none_or(|length| length > self.limit)
        {
            return Err(fmt::Error);
        }
        self.html.push_str(value);
        Ok(())
    }
}

struct DocumentationHeadingAdapter {
    nonce: String,
    slugs: Mutex<HeadingSlugs>,
}

#[derive(Default)]
struct HeadingSlugs {
    used: HashSet<String>,
    next_suffix: HashMap<String, usize>,
}

impl HeadingSlugs {
    fn unique(&mut self, base: String) -> String {
        let mut suffix = self.next_suffix.get(&base).copied().unwrap_or(0);
        loop {
            let candidate = if suffix == 0 {
                base.clone()
            } else {
                format!("{base}-{suffix}")
            };
            let following_suffix = suffix
                .checked_add(1)
                .expect("heading slug suffix must fit usize");
            if self.used.insert(candidate.clone()) {
                self.next_suffix.insert(base, following_suffix);
                return candidate;
            }
            suffix = following_suffix;
        }
    }
}

impl DocumentationHeadingAdapter {
    fn new(nonce: String) -> Self {
        Self {
            nonce,
            slugs: Mutex::new(HeadingSlugs::default()),
        }
    }
}

impl HeadingAdapter for DocumentationHeadingAdapter {
    fn enter(
        &self,
        output: &mut dyn fmt::Write,
        heading: &HeadingMeta,
        _sourcepos: Option<Sourcepos>,
    ) -> fmt::Result {
        let base_slug = heading_slug(&heading.content);
        let slug = self
            .slugs
            .lock()
            .expect("heading slug lock poisoned")
            .unique(base_slug);
        write!(output, "<h{} id=\"{}{}\">", heading.level, self.nonce, slug)
    }

    fn exit(&self, output: &mut dyn fmt::Write, heading: &HeadingMeta) -> fmt::Result {
        writeln!(output, "</h{}>", heading.level)
    }
}

fn heading_slug(text: &str) -> String {
    text.chars()
        .flat_map(char::to_lowercase)
        .filter_map(|character| {
            if character.is_letter()
                || character.is_number_decimal()
                || matches!(character, ' ' | '-' | '_')
            {
                Some(if character == ' ' { '-' } else { character })
            } else {
                None
            }
        })
        .collect()
}

fn rewrite_href<'a>(
    value: &'a str,
    current_archive_path: &str,
    targets: &HashMap<String, DocsPagePath>,
    version_root: &str,
) -> Option<Cow<'a, str>> {
    if let Some(fragment) = value.strip_prefix('#') {
        return Some(Cow::Owned(format!("#user-content-{fragment}")));
    }
    if absolute_href_is_allowed(value) {
        return Some(Cow::Borrowed(value));
    }

    let (path, fragment) = match value.split_once('#') {
        Some((path, fragment)) => (path, Some(fragment)),
        None => (value, None),
    };
    if path.contains('?') {
        return None;
    }
    let decoded = percent_decode_str(path).decode_utf8().ok()?;
    let resolved = resolve_bundle_path(current_archive_path, decoded.as_ref())?;
    let target = targets.get(&resolved)?;
    let mut rewritten = format!("{version_root}{}", target.key_suffix());
    if let Some(fragment) = fragment {
        rewritten.push_str("#user-content-");
        rewritten.push_str(fragment);
    }
    Some(Cow::Owned(rewritten))
}

fn absolute_href_is_allowed(value: &str) -> bool {
    // ammoniaも先にschemeを検査するが、http(s)のauthority有無はここで原値から固定する。
    let Ok(url) = ammonia::Url::parse(value) else {
        return false;
    };
    match url.scheme() {
        "http" | "https" => {
            value
                .get(url.scheme().len()..)
                .is_some_and(|suffix| suffix.starts_with("://"))
                && url.host_str().is_some()
        }
        "mailto" => value
            .get(url.scheme().len()..)
            .is_some_and(|suffix| suffix.starts_with(':')),
        _ => false,
    }
}

fn resolve_bundle_path(current_archive_path: &str, relative_path: &str) -> Option<String> {
    if matches!(relative_path.rsplit('/').next(), Some("." | "..")) {
        return None;
    }
    let mut resolved = current_archive_path
        .rsplit_once('/')
        .map_or_else(Vec::new, |(directory, _)| {
            directory.split('/').map(str::to_string).collect()
        });
    for component in relative_path.split('/') {
        match component {
            "." => {}
            ".." => {
                resolved.pop()?;
            }
            component => resolved.push(component.to_string()),
        }
    }
    Some(resolved.join("/"))
}

fn valid_code_class(value: &str) -> bool {
    value.strip_prefix("language-").is_some_and(|language| {
        !language.is_empty()
            && language.bytes().all(|byte| {
                byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"+-".contains(&byte)
            })
    })
}

fn valid_highlight_classes(value: &str) -> bool {
    let mut classes = value.split_ascii_whitespace();
    let Some(first) = classes.next() else {
        return false;
    };
    std::iter::once(first).chain(classes).all(|class| {
        class.strip_prefix("hl-").is_some_and(|suffix| {
            !suffix.is_empty()
                && suffix
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        })
    })
}

pub const TOPBAR_HTML: &str = concat!(
    "<header class=\"topbar\">\n",
    "<div class=\"topbar-inner\">\n",
    "<a href=\"https://carina-rs.dev/\" class=\"brand\" aria-label=\"Carina home\">",
    "<span class=\"brand-star\" aria-hidden=\"true\">★</span>",
    "<span class=\"brand-name\">Carina</span></a>\n",
    "<nav class=\"topbar-nav\" aria-label=\"Primary\">",
    "<a href=\"https://carina-rs.dev/reference/dsl/syntax/\" class=\"external\">Reference →</a>",
    "<a href=\"/providers/index.html\" aria-current=\"page\">Providers</a>",
    "<a href=\"https://github.com/carina-rs/carina\" class=\"external\">GitHub →</a>",
    "</nav>\n",
    "</div>\n",
    "</header>\n",
);

fn render_page_template(
    title: &str,
    version_root: &DocsVersionRoot,
    current: &ValidatedDocsPage,
    navigation: &DocsNavigation,
    main: &str,
) -> Result<String, DocsBundleError> {
    let provider_root = docs_provider_root(&version_root.namespace, &version_root.name);
    let versions_path = docs_versions_path(&version_root.namespace, &version_root.name);
    let page_kind = current.named().map_or_else(String::new, |page| {
        let kind = page.kind();
        format!(
            "<p class=\"docs-page-kind {}\">{}</p>\n",
            escape_html(kind.css_class()),
            escape_html(kind.label())
        )
    });
    let sidebar = render_sidebar(
        version_root,
        current,
        navigation,
        &provider_root,
        &versions_path,
    )?;
    let highlight_stylesheet =
        format!("<link rel=\"stylesheet\" href=\"/{DOCS_HIGHLIGHT_STYLESHEET_KEY}\">\n");
    Ok(format!(
        concat!(
            "<!doctype html>\n",
            "<html>\n",
            "<head>\n",
            "<meta charset=\"utf-8\">\n",
            "<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n",
            "<title>{}</title>\n",
            "<link rel=\"stylesheet\" href=\"/{}\">\n",
            "<link rel=\"stylesheet\" href=\"/{}\">\n",
            "{}",
            "<script src=\"/{}\" defer></script>\n",
            "</head>\n",
            "<body>\n",
            "{}",
            "<div class=\"docs-layout\">\n",
            "{}",
            "<main class=\"docs-content\">\n",
            "{}",
            "{}",
            "</main>\n",
            "</div>\n",
            "</body>\n",
            "</html>\n",
        ),
        escape_html(title),
        DOCS_TOKENS_KEY,
        DOCS_STYLESHEET_KEY,
        highlight_stylesheet,
        DOCS_SCRIPT_KEY,
        TOPBAR_HTML,
        sidebar,
        page_kind,
        main,
    ))
}

fn render_sidebar(
    version_root: &DocsVersionRoot,
    current: &ValidatedDocsPage,
    navigation: &DocsNavigation,
    provider_root: &str,
    versions_path: &str,
) -> Result<String, DocsBundleError> {
    let mut links = String::new();
    push_overview_link(
        &mut links,
        version_root,
        matches!(current, ValidatedDocsPage::Index),
    );
    match navigation {
        DocsNavigation::Flat {
            resources,
            data_sources,
        } => push_flat_sidebar_links(&mut links, version_root, current, resources, data_sources),
        DocsNavigation::Grouped(grouped) => {
            push_grouped_sidebar_links(&mut links, version_root, current, grouped)?;
        }
    }
    let current_path = format!("{}{}", version_root.host_path(), current.key_suffix());
    let navigation_path = format!("{}/nav.json", version_root.host_path());

    Ok(format!(
        concat!(
            "<aside class=\"docs-sidebar\">\n",
            "<input class=\"sidebar-toggle\" type=\"checkbox\" id=\"sidebar-toggle\">\n",
            "<label class=\"sidebar-toggle-label\" for=\"sidebar-toggle\">",
            "Provider navigation</label>\n",
            "<nav class=\"sidebar-nav\" aria-label=\"Provider documentation\" ",
            "data-nav-url=\"{}\" data-current=\"{}\">\n",
            "<div class=\"version-switcher\">",
            "<select id=\"version-switcher\" hidden data-versions-url=\"{}\" ",
            "data-version-base=\"{}\" data-current=\"{}\" ",
            "aria-label=\"Version\"></select>",
            "</div>\n",
            "<div class=\"sidebar-links\">\n",
            "{}",
            "</div>\n",
            "</nav>\n",
            "</aside>\n",
        ),
        escape_html(&navigation_path),
        escape_html(&current_path),
        escape_html(&format!("/{versions_path}")),
        escape_html(&format!("/{provider_root}")),
        escape_html(&version_root.version),
        links,
    ))
}

fn push_flat_sidebar_links(
    links: &mut String,
    version_root: &DocsVersionRoot,
    current: &ValidatedDocsPage,
    resources: &[NamedDocsPage],
    data_sources: &[NamedDocsPage],
) {
    match current {
        ValidatedDocsPage::Index => {
            // indexだけに全一覧を置き、JavaScriptなしでも全pageへ到達できるようにする。
            push_sidebar_section(links, "Resources", resources, version_root, None);
            push_sidebar_section(links, "Data Sources", data_sources, version_root, None);
        }
        ValidatedDocsPage::Named(current) => match current.kind() {
            DocsPageKind::Resource => push_sidebar_section(
                links,
                "Resources",
                std::slice::from_ref(current),
                version_root,
                Some(current),
            ),
            DocsPageKind::DataSource => push_sidebar_section(
                links,
                "Data Sources",
                std::slice::from_ref(current),
                version_root,
                Some(current),
            ),
        },
    }
}

fn push_grouped_sidebar_links(
    links: &mut String,
    version_root: &DocsVersionRoot,
    current: &ValidatedDocsPage,
    grouped: &GroupedNavigation,
) -> Result<(), DocsBundleError> {
    let current = match current {
        ValidatedDocsPage::Index => {
            for service in &grouped.services {
                push_sidebar_service(
                    links,
                    service,
                    &service.resources,
                    &service.data_sources,
                    false,
                    version_root,
                    None,
                );
            }
            return Ok(());
        }
        ValidatedDocsPage::Named(current) => current,
    };
    let (resources, data_sources) = match current.kind() {
        DocsPageKind::Resource => (std::slice::from_ref(current), &[][..]),
        DocsPageKind::DataSource => (&[][..], std::slice::from_ref(current)),
    };
    let service = grouped.service_for(current.path()).ok_or_else(|| {
        DocsBundleError::new("grouped navigation does not assign the current documentation page")
    })?;
    push_sidebar_service(
        links,
        service,
        resources,
        data_sources,
        true,
        version_root,
        Some(current),
    );
    Ok(())
}

fn push_sidebar_service(
    html: &mut String,
    service: &DocsService,
    resources: &[NamedDocsPage],
    data_sources: &[NamedDocsPage],
    open: bool,
    version_root: &DocsVersionRoot,
    current: Option<&NamedDocsPage>,
) {
    html.push_str("<details class=\"sidebar-service\"");
    if open {
        html.push_str(" open");
    }
    html.push_str(">\n<summary><h2 class=\"sidebar-service-title\">");
    html.push_str(&escape_html(&service.title));
    html.push_str("</h2></summary>\n");
    push_grouped_sidebar_section(html, "Resources", resources, version_root, current);
    push_grouped_sidebar_section(html, "Data Sources", data_sources, version_root, current);
    html.push_str("</details>\n");
}

fn push_grouped_sidebar_section(
    html: &mut String,
    heading: &str,
    pages: &[NamedDocsPage],
    version_root: &DocsVersionRoot,
    current: Option<&NamedDocsPage>,
) {
    if pages.is_empty() {
        return;
    }
    html.push_str("<section class=\"sidebar-section\">\n<h3 class=\"sidebar-section-title\">");
    html.push_str(heading);
    html.push_str("</h3>\n<ul class=\"sidebar-list\">\n");
    push_sidebar_list_items(
        html,
        pages,
        version_root,
        current,
        SidebarPageTitle::TypeName,
    );
    html.push_str("</ul>\n</section>\n");
}

fn push_sidebar_section(
    html: &mut String,
    heading: &str,
    pages: &[NamedDocsPage],
    version_root: &DocsVersionRoot,
    current: Option<&NamedDocsPage>,
) {
    if pages.is_empty() {
        return;
    }
    html.push_str("<section class=\"sidebar-section\">\n<h2 class=\"sidebar-section-title\">");
    html.push_str(heading);
    html.push_str("</h2>\n<ul class=\"sidebar-list\">\n");
    push_sidebar_list_items(html, pages, version_root, current, SidebarPageTitle::Full);
    html.push_str("</ul>\n</section>\n");
}

fn push_sidebar_list_items(
    html: &mut String,
    pages: &[NamedDocsPage],
    version_root: &DocsVersionRoot,
    current: Option<&NamedDocsPage>,
    title_style: SidebarPageTitle,
) {
    for page in pages {
        html.push_str("<li>");
        push_sidebar_page_link(html, version_root, page, title_style, current == Some(page));
        html.push_str("</li>\n");
    }
}

#[derive(Clone, Copy)]
enum SidebarPageTitle {
    Full,
    TypeName,
}

impl SidebarPageTitle {
    fn label(self, title: &PageTitle) -> &str {
        match self {
            Self::Full => title.as_str(),
            Self::TypeName => title.type_name(),
        }
    }
}

fn push_overview_link(html: &mut String, version_root: &DocsVersionRoot, current: bool) {
    push_sidebar_link_start(html, version_root.host_path(), current);
    html.push_str("Overview</a>\n");
}

fn push_sidebar_page_link(
    html: &mut String,
    version_root: &DocsVersionRoot,
    page: &NamedDocsPage,
    title_style: SidebarPageTitle,
    current: bool,
) {
    let href = format!("{}{}", version_root.host_path(), page.key_suffix());
    push_sidebar_link_start(html, &href, current);
    html.push_str(&escape_html(title_style.label(&page.title)));
    html.push_str("</a>\n");
}

fn push_sidebar_link_start(html: &mut String, href: &str, current: bool) {
    html.push_str("<a class=\"sidebar-link\" href=\"");
    html.push_str(&escape_html(href));
    if current {
        html.push_str("\" aria-current=\"page");
    }
    html.push_str("\">");
}

pub fn escape_html(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&#39;"),
            character => escaped.push(character),
        }
    }
    escaped
}

struct ParsedHeader {
    path: Vec<u8>,
    size: u64,
    kind: u8,
}

fn parse_header(header: &[u8; BLOCK_BYTES]) -> Result<ParsedHeader, DocsBundleError> {
    if &header[257..263] != b"ustar\0" || &header[263..265] != b"00" {
        return Err(DocsBundleError::new("tar entry is not POSIX ustar or pax"));
    }

    parse_octal(&header[100..108], "mode")?;
    parse_octal(&header[108..116], "uid")?;
    parse_octal(&header[116..124], "gid")?;
    let size = parse_octal(&header[124..136], "size")?;
    parse_octal(&header[136..148], "mtime")?;
    let expected_checksum = parse_octal(&header[148..156], "checksum")?;
    parse_octal(&header[329..337], "device major")?;
    parse_octal(&header[337..345], "device minor")?;
    let actual_checksum = header
        .iter()
        .enumerate()
        .map(|(index, byte)| {
            if (148..156).contains(&index) {
                u64::from(b' ')
            } else {
                u64::from(*byte)
            }
        })
        .sum::<u64>();
    if expected_checksum != actual_checksum {
        return Err(DocsBundleError::new("tar header checksum does not match"));
    }

    let name = field_bytes(&header[..100]);
    let prefix = field_bytes(&header[345..500]);
    let mut path = Vec::with_capacity(prefix.len() + usize::from(!prefix.is_empty()) + name.len());
    if !prefix.is_empty() {
        path.extend_from_slice(prefix);
        path.push(b'/');
    }
    path.extend_from_slice(name);

    Ok(ParsedHeader {
        path,
        size,
        kind: header[156],
    })
}

fn field_bytes(field: &[u8]) -> &[u8] {
    let length = field
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(field.len());
    &field[..length]
}

fn parse_octal(field: &[u8], name: &str) -> Result<u64, DocsBundleError> {
    if field.first().is_some_and(|byte| byte & 0x80 != 0) {
        return Err(DocsBundleError::new(format!(
            "GNU base-256 {name} is not POSIX tar"
        )));
    }
    let Some(start) = field.iter().position(|byte| *byte != b' ' && *byte != 0) else {
        return Ok(0);
    };
    let end = field
        .iter()
        .rposition(|byte| *byte != b' ' && *byte != 0)
        .expect("a non-padding byte exists");
    let digits = &field[start..=end];
    if !digits.iter().all(|byte| matches!(byte, b'0'..=b'7')) {
        return Err(DocsBundleError::new(format!(
            "tar {name} is not an octal number"
        )));
    }
    digits.iter().try_fold(0_u64, |value, digit| {
        value
            .checked_mul(8)
            .and_then(|value| value.checked_add(u64::from(*digit - b'0')))
            .ok_or_else(|| DocsBundleError::new(format!("tar {name} overflows")))
    })
}

fn normalize_path(
    mut path: Vec<u8>,
    is_directory: bool,
) -> Result<Option<Vec<u8>>, DocsBundleError> {
    if path.starts_with(b"./") {
        path.drain(..2);
    }
    if is_directory && path.ends_with(b"/") {
        path.pop();
    }
    if path.is_empty() {
        return if is_directory {
            Ok(None)
        } else {
            Err(DocsBundleError::new("file entry path is empty"))
        };
    }
    if path
        .split(|byte| *byte == b'/')
        .any(|component| component.is_empty() || component == b"." || component == b"..")
    {
        return Err(DocsBundleError::new(
            "entry path contains an empty, dot, or dot-dot component",
        ));
    }
    Ok(Some(path))
}

fn classify_file(path: &[u8]) -> Result<DocsPagePath, DocsBundleError> {
    if path == b"index.md" {
        return Ok(DocsPagePath::Index);
    }
    let mut components = path.split(|byte| *byte == b'/');
    let root = components.next().unwrap_or_default();
    let filename = components.next().unwrap_or_default();
    if components.next().is_some() || filename.is_empty() {
        return Err(DocsBundleError::new(
            "file is outside an allowed documentation location",
        ));
    }
    let kind = match root {
        b"resources" => DocsPageKind::Resource,
        b"data-sources" => DocsPageKind::DataSource,
        _ => {
            return Err(DocsBundleError::new(
                "file is outside an allowed documentation location",
            ));
        }
    };
    let Some(slug) = filename.strip_suffix(b".md") else {
        return Err(DocsBundleError::new("documentation file must end in .md"));
    };
    let slug = std::str::from_utf8(slug)
        .map_err(|_| DocsBundleError::new("documentation slug does not match the required form"))?;
    if !valid_service_id_or_slug(slug) {
        return Err(DocsBundleError::new(
            "documentation slug does not match the required form",
        ));
    }
    Ok(DocsPagePath::Named(NamedPagePath::new(kind, slug)))
}

#[derive(Default)]
struct PaxOverrides {
    present: bool,
    path: Option<Vec<u8>>,
    size: Option<u64>,
}

impl PaxOverrides {
    fn is_empty(&self) -> bool {
        !self.present
    }

    fn apply(&mut self, body: &[u8]) -> Result<(), DocsBundleError> {
        let mut offset = 0_usize;
        while offset < body.len() {
            let relative_space = body[offset..]
                .iter()
                .position(|byte| *byte == b' ')
                .ok_or_else(|| DocsBundleError::new("pax record has no length separator"))?;
            let space = offset + relative_space;
            let length_text = std::str::from_utf8(&body[offset..space])
                .map_err(|_| DocsBundleError::new("pax record length is not ASCII"))?;
            let length = length_text
                .parse::<usize>()
                .map_err(|_| DocsBundleError::new("pax record length is invalid"))?;
            let end = offset
                .checked_add(length)
                .filter(|end| *end <= body.len())
                .ok_or_else(|| DocsBundleError::new("pax record exceeds its header body"))?;
            if length <= relative_space + 2 || body.get(end - 1) != Some(&b'\n') {
                return Err(DocsBundleError::new("pax record framing is invalid"));
            }
            let record = &body[space + 1..end - 1];
            let equals = record
                .iter()
                .position(|byte| *byte == b'=')
                .ok_or_else(|| DocsBundleError::new("pax record has no value separator"))?;
            let key = &record[..equals];
            let value = &record[equals + 1..];
            match key {
                b"path" => self.path = Some(value.to_vec()),
                b"size" => {
                    let value = std::str::from_utf8(value)
                        .map_err(|_| DocsBundleError::new("pax size is not ASCII"))?;
                    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
                        return Err(DocsBundleError::new("pax size is invalid"));
                    }
                    self.size = Some(
                        value
                            .parse::<u64>()
                            .map_err(|_| DocsBundleError::new("pax size overflows"))?,
                    );
                }
                _ => {}
            }
            offset = end;
        }
        Ok(())
    }
}

fn read_block<R: Read>(reader: &mut CappedReader<R>) -> Result<[u8; BLOCK_BYTES], DocsBundleError> {
    let mut block = [0_u8; BLOCK_BYTES];
    reader.read_exact(&mut block).map_err(io_error)?;
    Ok(block)
}

fn read_body<R: Read>(reader: &mut CappedReader<R>, size: u64) -> Result<Vec<u8>, DocsBundleError> {
    let (padding, _) = body_layout(reader, size)?;
    let size = usize::try_from(size)
        .map_err(|_| DocsBundleError::new("entry size is not representable"))?;
    let mut body = vec![0_u8; size];
    reader.read_exact(&mut body).map_err(io_error)?;
    discard(reader, padding)?;
    Ok(body)
}

fn skip_body<R: Read>(reader: &mut CappedReader<R>, size: u64) -> Result<(), DocsBundleError> {
    let (_, consumed) = body_layout(reader, size)?;
    discard(reader, consumed)
}

fn body_layout<R>(reader: &CappedReader<R>, size: u64) -> Result<(u64, u64), DocsBundleError> {
    let padding = (BLOCK_BYTES as u64 - size % BLOCK_BYTES as u64) % BLOCK_BYTES as u64;
    let consumed = size
        .checked_add(padding)
        .ok_or_else(|| DocsBundleError::new("entry size overflows"))?;
    if consumed > reader.remaining() {
        return Err(DocsBundleError::new(
            "entry exceeds the decompressed tar stream limit",
        ));
    }
    Ok((padding, consumed))
}

fn discard<R: Read>(reader: &mut CappedReader<R>, mut bytes: u64) -> Result<(), DocsBundleError> {
    let mut buffer = [0_u8; 8192];
    while bytes > 0 {
        let length = usize::try_from(bytes.min(buffer.len() as u64))
            .map_err(|_| DocsBundleError::new("entry size is not representable"))?;
        reader.read_exact(&mut buffer[..length]).map_err(io_error)?;
        bytes -= length as u64;
    }
    Ok(())
}

fn ensure_zero_to_end<R: Read>(reader: &mut CappedReader<R>) -> Result<(), DocsBundleError> {
    let mut buffer = [0_u8; 8192];
    loop {
        let length = reader.read(&mut buffer).map_err(io_error)?;
        if length == 0 {
            return Ok(());
        }
        if buffer[..length].iter().any(|byte| *byte != 0) {
            return Err(DocsBundleError::new(
                "non-zero data follows the tar end marker",
            ));
        }
    }
}

fn is_zero_block(block: &[u8; BLOCK_BYTES]) -> bool {
    block.iter().all(|byte| *byte == 0)
}

fn io_error(error: std::io::Error) -> DocsBundleError {
    DocsBundleError::new(format!("archive stream is invalid: {error}"))
}

struct CappedReader<R> {
    inner: R,
    consumed: u64,
    limit: u64,
}

impl<R> CappedReader<R> {
    fn new(inner: R, limit: u64) -> Self {
        Self {
            inner,
            consumed: 0,
            limit,
        }
    }

    fn remaining(&self) -> u64 {
        self.limit - self.consumed
    }

    fn into_inner(self) -> R {
        self.inner
    }
}

impl<R: Read> Read for CappedReader<R> {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }
        if self.consumed == self.limit {
            let mut probe = [0_u8; 1];
            return match self.inner.read(&mut probe)? {
                0 => Ok(0),
                _ => Err(std::io::Error::other(
                    "decompressed tar stream exceeds the 80 MiB limit",
                )),
            };
        }
        let allowed = usize::try_from(self.remaining().min(buffer.len() as u64))
            .expect("buffer length fits usize");
        let read = self.inner.read(&mut buffer[..allowed])?;
        self.consumed += read as u64;
        Ok(read)
    }
}

#[cfg(test)]
mod tests {
    use std::io;

    use super::*;
    use crate::test_support::{
        gzip_test_bytes as gzip,
        refresh_test_tar_first_header_checksum as refresh_first_header_checksum,
        test_pax_record as pax_record, TestTarBuilder as TarBuilder,
    };

    #[test]
    fn validated_bytes_stay_owned_and_unchanged_when_companion_metadata_changes() {
        use std::sync::atomic::{AtomicBool, Ordering};

        let archive = valid_bundle();
        let original_pointer = archive.as_ptr();
        let expected_digest = Sha256::digest(&archive);
        let switch = Arc::new(AtomicBool::new(false));
        let coordinate = DocsCoordinate::new("local", "provider", "0.0.0").unwrap();
        let checked = super::validate_and_render_docs_bundle(archive, &coordinate)
            .unwrap()
            .with_metadata(Arc::clone(&switch));

        switch.store(true, Ordering::SeqCst);

        assert!(checked.metadata().load(Ordering::SeqCst));
        assert_eq!(checked.archive().as_ptr(), original_pointer);
        assert_eq!(Sha256::digest(checked.archive()), expected_digest);
        let (archive, metadata, pages) = checked.into_parts();
        assert_eq!(archive.as_ptr(), original_pointer);
        assert_eq!(Sha256::digest(&archive), expected_digest);
        assert!(Arc::ptr_eq(&metadata, &switch));
        assert_eq!(pages.iter().len(), 4);
    }

    #[tokio::test]
    async fn subprocess_spawn_failure_retains_its_io_source_and_message() {
        use std::error::Error;

        let executable = std::env::temp_dir().join(format!("missing-renderer-{}", Uuid::new_v4()));
        let renderer = SubprocessDocsRenderer::new(&executable, DocsRenderLimits::default());
        let coordinate = DocsCoordinate::new("local", "provider", "0.0.0").unwrap();
        let error = renderer
            .render_in_subprocess(b"", &coordinate)
            .await
            .expect_err("missing executable must fail to spawn");
        let source = error
            .source()
            .expect("spawn failure must preserve its source")
            .downcast_ref::<io::Error>()
            .expect("spawn source must be an I/O error");

        assert_eq!(source.kind(), io::ErrorKind::NotFound);
        assert!(source.raw_os_error().is_some());
        assert_eq!(
            error.to_string(),
            format!(
                "documentation renderer failed: could not start documentation renderer {}: {source}",
                executable.display()
            )
        );
    }

    #[test]
    fn worker_setup_io_failures_retain_their_sources_and_messages() {
        use std::error::Error;

        for context in [
            "documentation renderer could not apply resource limits",
            "documentation renderer could not decode its request",
            "documentation renderer could not spawn its render thread",
        ] {
            let source = io::Error::from_raw_os_error(libc::EACCES);
            let expected = format!("{context}: {source}");
            let error = DocsRenderWorkerError::infrastructure_io(context, source);
            let source = std::iter::successors(error.source(), |&error| error.source())
                .find_map(|error| error.downcast_ref::<io::Error>())
                .expect("worker setup must retain its I/O error");

            assert_eq!(source.kind(), io::ErrorKind::PermissionDenied);
            assert_eq!(source.raw_os_error(), Some(libc::EACCES));
            assert_eq!(
                error.exit_code(),
                DOCS_RENDER_WORKER_INFRASTRUCTURE_EXIT_CODE
            );
            assert_eq!(error.to_string(), expected);
            assert_eq!(format!("{error:#}"), expected);
        }
    }

    // Fixture adapter keeps the original test bodies independent of server evidence.
    struct ValidatedDocsBundle {
        version_root: DocsVersionRoot,
        rendered_pages: Vec<RenderedPage>,
    }

    fn fetched_docs_for_bytes(bytes: &[u8]) -> Vec<u8> {
        bytes.to_vec()
    }

    fn validate_and_render_docs_bundle(
        archive: Vec<u8>,
        namespace: &str,
        name: &str,
        version: &str,
    ) -> Result<ValidatedDocsBundle, DocsBundleError> {
        let coordinate = DocsCoordinate::new(namespace, name, version).unwrap();
        super::validate_and_render_docs_bundle(archive, &coordinate).map(|bundle| {
            let (_, _, rendered) = bundle.into_parts();
            ValidatedDocsBundle {
                version_root: rendered.version_root,
                rendered_pages: rendered.rendered_pages,
            }
        })
    }

    const BLOCK: usize = 512;
    const MAX_FILE: usize = 256 * 1024;
    const MAX_TAR_STREAM: usize = 80 * 1024 * 1024;
    const TEST_GROUPED_NAVIGATION_METADATA: &str = concat!(
        "{\"services\":[",
        "{\"id\":\"ec2\",\"title\":\"EC2\",",
        "\"resources\":[\"ec2_instance\"],\"data_sources\":[\"ec2_ami\"]},",
        "{\"id\":\"acm\",\"title\":\"ACM & <Certificates>\",",
        "\"resources\":[\"acm_pca\",\"acm_certificate\"],\"data_sources\":[]}]}",
    );

    #[derive(Clone)]
    struct SharedLogBuffer(Arc<Mutex<Vec<u8>>>);

    impl io::Write for SharedLogBuffer {
        fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buffer);
            Ok(buffer.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn worker_response_rejects_oversized_truncated_or_bad_headers() {
        assert_eq!(WORKER_PROTOCOL_VERSION, 1);
        let oversized = (max_worker_response_payload_bytes() as u64 + 1)
            .to_le_bytes()
            .to_vec();
        assert_worker_response_error(oversized, "renderer response exceeds its limit").await;

        let mut truncated = 14_u64.to_le_bytes().to_vec();
        truncated.extend_from_slice(b"CRD");
        assert!(read_worker_response(Cursor::new(truncated)).await.is_err());

        let mut bad_magic = encoded_worker_response(&WorkerResponse::Rendered(Vec::new()));
        bad_magic[8] = b'X';
        assert_worker_response_error(bad_magic, "renderer response magic is invalid").await;

        let mut bad_version = encoded_worker_response(&WorkerResponse::Rendered(Vec::new()));
        bad_version[16] = WORKER_PROTOCOL_VERSION + 1;
        assert_worker_response_error(
            bad_version,
            "renderer response protocol version is unsupported",
        )
        .await;
    }

    #[tokio::test]
    async fn worker_response_rejects_a_body_longer_than_its_remaining_frame() {
        let response = raw_rendered_response(0, b"", 1, b"");

        assert_worker_response_error(response, "renderer page body is truncated").await;
    }

    #[tokio::test]
    async fn worker_response_rejects_a_bad_slug() {
        let response = raw_rendered_response(1, b"_bad", 0, b"");

        assert_worker_response_error(response, "response page slug is invalid").await;
    }

    #[tokio::test]
    async fn worker_response_rejects_trailing_frame_or_stdout_bytes() {
        let mut frame_trailing = encoded_worker_response(&WorkerResponse::Rendered(Vec::new()));
        frame_trailing.extend_from_slice(b"x");
        let payload_len = u64::from_le_bytes(frame_trailing[..8].try_into().unwrap()) + 1;
        frame_trailing[..8].copy_from_slice(&payload_len.to_le_bytes());
        assert_worker_response_error(
            frame_trailing,
            "renderer response contains trailing frame bytes",
        )
        .await;

        let mut stdout_trailing = encoded_worker_response(&WorkerResponse::Rendered(Vec::new()));
        stdout_trailing.extend_from_slice(b"x");
        assert_worker_response_error(
            stdout_trailing,
            "renderer response contains trailing stdout bytes",
        )
        .await;
    }

    #[test]
    fn validated_worker_bundle_rejects_extra_missing_and_duplicate_pages() {
        let archive = bundle_with_files(&[
            (b"index.md", b"# Index\n"),
            (b"resources/x.md", b"# test.compute.X\n"),
        ]);
        let expected_archive = visit_docs_archive(&archive, |_, _| Ok(())).unwrap();

        let extra = vec![
            worker_page(DocsPagePath::Index, b"index"),
            worker_page(docs_page_path(DocsPageKind::Resource, "x"), b"x"),
            worker_page(docs_page_path(DocsPageKind::DataSource, "extra"), b"extra"),
        ];
        assert_eq!(
            build_worker_bundle(&archive, &expected_archive, extra)
                .err()
                .unwrap(),
            "renderer returned the wrong page count"
        );

        let missing = vec![worker_page(DocsPagePath::Index, b"index")];
        assert_eq!(
            build_worker_bundle(&archive, &expected_archive, missing)
                .err()
                .unwrap(),
            "renderer returned the wrong page count"
        );

        let duplicate = vec![
            worker_page(DocsPagePath::Index, b"first"),
            worker_page(DocsPagePath::Index, b"second"),
        ];
        assert_eq!(
            build_worker_bundle(&archive, &expected_archive, duplicate)
                .err()
                .unwrap(),
            "renderer returned a duplicate page path"
        );
    }

    #[test]
    fn validated_worker_bundle_rejects_a_non_utf8_body() {
        let archive = bundle_with_files(&[(b"index.md", b"# Index\n")]);
        let expected_archive = visit_docs_archive(&archive, |_, _| Ok(())).unwrap();
        let pages = vec![WorkerPage {
            path: DocsPagePath::Index,
            body: vec![0xff],
        }];

        assert_eq!(
            build_worker_bundle(&archive, &expected_archive, pages)
                .err()
                .unwrap(),
            "renderer returned a non-UTF-8 page"
        );
    }

    #[tokio::test]
    async fn worker_response_cannot_inject_or_replace_navigation_document() {
        let response = raw_rendered_response(
            3,
            b"",
            b"{\"overview\":\"/attacker-controlled\"}\n".len() as u64,
            b"{\"overview\":\"/attacker-controlled\"}\n",
        );

        assert_worker_response_error(response, "response page path kind is invalid").await;
    }

    #[test]
    fn accepts_pax_bundle_with_dot_prefixes_directories_and_root_entry() {
        let archive = valid_bundle();

        assert!(validate(&archive).is_ok());
    }

    #[test]
    fn rendered_pages_omit_raw_html_before_sanitizing() {
        let markdown = br#"
# Safe

ordinary <em onclick="alert(1)">kept-inline-text</em> text and
<a href="javascript:alert(1)" onmouseover="alert(1)">unsafe link</a>

<script>drop-script</script>
<style>drop-style</style>
<iframe>drop-iframe</iframe>
<object>drop-object</object>
<embed>drop-embed</embed>
<EMBED>drop-upper-embed</EMBED>
<template>drop-template</template>
<textarea>drop-textarea</textarea>
<base href="https://evil.example/">
<meta http-equiv="refresh" content="0">
<svg><text>drop-svg</text></svg>
<math><mi>drop-math</mi></math>
<div>
drop-block-text
</div>

<!-- drop-comment -->
<p style="color:red" onclick="alert(1)">safe paragraph</p>
"#;

        let bundle = render_bundle(&bundle_with_files(&[(b"index.md", markdown)]));
        let html = page_html(&bundle, "");

        for absent in [
            "drop-script",
            "<style",
            "drop-style",
            "<iframe",
            "drop-iframe",
            "<object",
            "drop-object",
            "<embed",
            "<template",
            "drop-template",
            "<textarea",
            "drop-textarea",
            "<base",
            "<meta http-equiv",
            "<svg",
            "drop-svg",
            "<math",
            "drop-math",
            "drop-block-text",
            "drop-comment",
            "raw HTML omitted",
            "style=",
            "onclick=",
            "onmouseover=",
            "javascript:",
            "safe paragraph",
        ] {
            assert!(!html.contains(absent), "unexpected {absent:?} in {html}");
        }
        assert_eq!(html.matches("<script").count(), 1);
        assert_eq!(html.matches("<link").count(), 3);
        assert!(html.contains("<link rel=\"stylesheet\" href=\"/providers/_assets/tokens.css\">"));
        assert!(html.contains("<link rel=\"stylesheet\" href=\"/providers/_assets/docs.css\">"));
        assert!(
            html.contains("<link rel=\"stylesheet\" href=\"/providers/_assets/highlight.css\">")
        );
        assert!(html.contains("<script src=\"/providers/_assets/docs.js\" defer></script>"));
        assert!(html.contains("ordinary kept-inline-text text and"));
        assert!(!html.contains("<em"));
        assert!(html.contains("unsafe link"));
        assert!(!html.contains(">unsafe link</a>"));
    }

    #[test]
    fn sanitizer_defense_in_depth_drops_hostile_html_and_filters_attributes() {
        let targets = Arc::new(HashMap::new());
        let version_root: Arc<str> = "/providers/carina-rs/aws/1.2.3".into();
        let html = sanitize_html(
            concat!(
                "<script>drop-script</script>",
                "<style>drop-style</style>",
                "<iframe>drop-iframe</iframe>",
                "<object>drop-object</object>",
                "<embed src=\"https://evil.example/x\">",
                "<template>drop-template</template>",
                "<textarea>drop-textarea</textarea>",
                "<svg><text>drop-svg</text></svg>",
                "<math><mi>drop-math</mi></math>",
                "<base href=\"https://evil.example/\">",
                "<meta http-equiv=\"refresh\" content=\"0\">",
                "<marquee>kept-unknown-text</marquee>",
                "<!-- drop-comment -->",
                "<p style=\"color:red\" onclick=\"alert(1)\">safe paragraph</p>",
                "<a href=\"javascript:alert(1)\" onmouseover=\"alert(1)\">unsafe link</a>",
                "<h2 id=\"evil\">author heading</h2>",
                "<h2 id=\"nonce-safe\">generated heading</h2>",
                "<code class=\"language-go\">ok</code>",
                "<code class=\"language-Rust\">bad uppercase</code>",
                "<code class=\"language-rust extra\">bad extra</code>",
            ),
            "index.md",
            targets,
            version_root,
            "nonce-".to_string(),
        );

        for absent in [
            "drop-script",
            "drop-style",
            "drop-iframe",
            "drop-object",
            "<embed",
            "drop-template",
            "drop-textarea",
            "drop-svg",
            "drop-math",
            "<base",
            "<meta",
            "drop-comment",
            "style=",
            "onclick=",
            "onmouseover=",
            "javascript:",
            "id=\"evil\"",
            "class=\"language-Rust\"",
            "class=\"language-rust extra\"",
        ] {
            assert!(!html.contains(absent), "unexpected {absent:?} in {html}");
        }
        assert!(html.contains("kept-unknown-text"));
        assert!(html.contains("safe paragraph"));
        assert!(html.contains("<a>unsafe link</a>"));
        assert!(html.contains("<h2>author heading</h2>"));
        assert!(html.contains("<h2 id=\"user-content-safe\">generated heading</h2>"));
        assert!(html.contains("<code class=\"language-go\">ok</code>"));
        assert!(html.contains("<code>bad uppercase</code>"));
        assert!(html.contains("<code>bad extra</code>"));
    }

    #[test]
    fn code_spans_and_blocks_escape_raw_angle_brackets() {
        let markdown = b"`List<String>`\n\n```text\nVec<String>\n```\n";
        let bundle = render_bundle(&bundle_with_files(&[(b"index.md", markdown)]));
        let html = page_html(&bundle, "");

        assert!(html.contains("<code>List&lt;String&gt;</code>"));
        assert!(html.contains("<code class=\"language-text\">Vec&lt;String&gt;\n</code>"));
    }

    #[test]
    fn recognized_fences_are_class_highlighted_and_unknown_fences_stay_plain() {
        let markdown = concat!(
            "```crn\nlet answer = \"safe\" # note\n```\n\n",
            "```json\n{\"answer\": 42}\n```\n\n",
            "```sh\necho '<safe>'\n```\n\n",
            "```unknown\na < b\n```\n",
        );
        let bundle = render_bundle(&bundle_with_files(&[(b"index.md", markdown.as_bytes())]));
        let html = page_html(&bundle, "");

        for language in ["crn", "json", "sh", "unknown"] {
            assert!(html.contains(&format!("<code class=\"language-{language}\">")));
        }
        assert!(html.contains("hl-keyword"));
        assert!(html.contains("hl-string"));
        assert!(html.contains("&lt;safe&gt;"));
        assert!(html.contains("<code class=\"language-unknown\">a &lt; b\n</code>"));
        assert!(!html.contains("style="));
    }

    #[test]
    fn crn_fence_html_matches_the_pre_shared_highlighter_output() {
        let markdown = concat!(
            "```crn\n",
            "provider aws {\n",
            "  source  = 'registry.carina-rs.dev/carina-rs/aws'\n",
            "  version = '0.7.2'\n",
            "}\n",
            "```\n",
        );
        let bundle = render_bundle(&bundle_with_files(&[(b"index.md", markdown.as_bytes())]));
        let html = page_html(&bundle, "");
        let start = html
            .find("<pre><code class=\"language-crn\">")
            .expect("crn code block");
        let end = html[start..]
            .find("</code></pre>")
            .map(|offset| start + offset + "</code></pre>".len())
            .expect("crn code block end");

        assert_eq!(
            &html[start..end],
            concat!(
                "<pre><code class=\"language-crn\">",
                "<span class=\"hl-source hl-crn\">",
                "<span class=\"hl-keyword hl-declaration hl-crn\">provider</span> ",
                "<span class=\"hl-variable hl-other hl-crn\">aws</span> {\n",
                "  <span class=\"hl-variable hl-other hl-crn\">source</span>  ",
                "<span class=\"hl-keyword hl-operator hl-assignment hl-crn\">=</span> ",
                "<span class=\"hl-string hl-quoted hl-single hl-crn\">",
                "<span class=\"hl-string hl-quoted hl-single hl-crn\">'</span>",
                "registry.carina-rs.dev/carina-rs/aws'</span>\n",
                "  <span class=\"hl-variable hl-other hl-crn\">version</span> ",
                "<span class=\"hl-keyword hl-operator hl-assignment hl-crn\">=</span> ",
                "<span class=\"hl-string hl-quoted hl-single hl-crn\">",
                "<span class=\"hl-string hl-quoted hl-single hl-crn\">'</span>",
                "0.7.2'</span>\n",
                "}\n",
                "</span></code></pre>"
            )
        );
    }

    #[test]
    fn sanitizer_keeps_only_highlight_classes_and_raw_markdown_cannot_smuggle_spans() {
        let sanitized = sanitize_html(
            concat!(
                "<pre><code class=\"language-crn\">",
                "<span class=\"hl-keyword hl-control hl-crn\">if</span>",
                "<span class=\"hl-keyword injected\">else</span>",
                "<span id=\"bad\" style=\"color:red\" class=\"hl-string\">x</span>",
                "</code></pre>"
            ),
            "index.md",
            Arc::new(HashMap::new()),
            "/providers/carina-rs/aws/1.2.3".into(),
            "nonce-".to_string(),
        );

        assert!(sanitized.contains("<span class=\"hl-keyword hl-control hl-crn\">if</span>"));
        assert!(sanitized.contains("<span>else</span>"));
        assert!(sanitized.contains("<span class=\"hl-string\">x</span>"));
        assert!(!sanitized.contains("injected"));
        assert!(!sanitized.contains("style="));
        assert!(!sanitized.contains("id=\"bad\""));

        let bundle = render_bundle(&bundle_with_files(&[(
            b"index.md",
            b"Before <span class=\"hl-keyword\">author</span> after\n",
        )]));
        let html = page_html(&bundle, "");
        assert!(html.contains("Before author after"));
        assert!(!html.contains("<span class=\"hl-keyword\">author</span>"));
    }

    #[test]
    fn rendered_headings_match_shared_slug_fixture() {
        #[derive(Deserialize)]
        struct HeadingSlugCase {
            name: String,
            headings: Vec<String>,
            ids: Vec<String>,
        }

        // The heading-slug-fixture CI job keeps this byte-identical to
        // carina-provider-aws/carina-docgen/tests/fixtures/heading-slugs.json.
        const FIXTURE: &str = include_str!("../tests/fixtures/heading-slugs.json");

        let cases: Vec<HeadingSlugCase> = serde_json::from_str(FIXTURE).unwrap();
        for case in cases {
            let markdown = case.headings.join("\n\n");
            let bundle = render_bundle(&bundle_with_files(&[(b"index.md", markdown.as_bytes())]));
            let html = page_html(&bundle, "");

            let actual_ids = html
                .match_indices("id=\"user-content-")
                .map(|(start, _)| {
                    let value = &html[start + "id=\"".len()..];
                    value[..value.find('"').unwrap()].to_string()
                })
                .collect::<Vec<_>>();
            let expected_ids = case
                .ids
                .iter()
                .map(|id| format!("user-content-{id}"))
                .collect::<Vec<_>>();

            assert_eq!(actual_ids, expected_ids, "fixture case {:?}", case.name);
        }
    }

    #[test]
    fn rendered_headings_have_unicode_slugs_duplicates_and_no_author_ids() {
        let markdown = concat!(
            "# Héllö, 世界! A_B-C\n\n",
            "## Héllö, 世界! A_B-C\n\n",
            "### A½Ⅳ٣ e\u{301}\n\n",
            "<h2 id=\"evil\">Raw heading</h2>\n",
        );

        let bundle = render_bundle(&bundle_with_files(&[(b"index.md", markdown.as_bytes())]));
        let html = page_html(&bundle, "");

        assert!(html.contains("id=\"user-content-héllö-世界-a_b-c\""));
        assert!(html.contains("id=\"user-content-héllö-世界-a_b-c-1\""));
        assert!(html.contains("id=\"user-content-a٣-e\""));
        assert!(!html.contains("Raw heading"));
        assert!(!html.contains("id=\"evil\""));
    }

    #[test]
    fn rendered_headings_never_reuse_a_generated_suffix() {
        let bundle = render_bundle(&bundle_with_files(&[(
            b"index.md",
            b"# A\n\n## A\n\n### A-1\n",
        )]));
        let html = page_html(&bundle, "");

        assert!(html.contains("id=\"user-content-a\""));
        assert_eq!(html.matches("id=\"user-content-a-1\"").count(), 1);
        assert!(html.contains("id=\"user-content-a-1-1\""));
    }

    #[test]
    fn rendered_headings_choose_the_first_unused_suffix_each_time() {
        let bundle = render_bundle(&bundle_with_files(&[(
            b"index.md",
            b"# a\n\n## a-1\n\n### a\n",
        )]));
        let html = page_html(&bundle, "");

        assert_eq!(html.matches("id=\"user-content-a\"").count(), 1);
        assert_eq!(html.matches("id=\"user-content-a-1\"").count(), 1);
        assert_eq!(html.matches("id=\"user-content-a-2\"").count(), 1);
    }

    #[test]
    fn identical_headings_render_with_linear_slug_allocation() {
        use std::time::{Duration, Instant};

        const RENDERED_COUNT: usize = 4_096;
        let markdown = "# a\n".repeat(RENDERED_COUNT);
        let bundle = render_bundle(&bundle_with_files(&[(b"index.md", markdown.as_bytes())]));
        let html = page_html(&bundle, "");

        assert_eq!(html.matches("id=\"user-content-a").count(), RENDERED_COUNT);
        assert!(html.contains(&format!("id=\"user-content-a-{}\"", RENDERED_COUNT - 1)));

        let maximum_heading_count = MAX_FILE_BYTES as usize / b"# a\n".len();
        let mut slugs = HeadingSlugs::default();
        let mut last = String::new();
        let started = Instant::now();
        for _ in 0..maximum_heading_count {
            last = slugs.unique("a".to_string());
        }
        assert!(started.elapsed() < Duration::from_secs(1));
        assert_eq!(last, format!("a-{}", maximum_heading_count - 1));
        assert_eq!(slugs.next_suffix.get("a"), Some(&maximum_heading_count));
    }

    #[test]
    fn rendered_links_follow_bundle_resolution_rules() {
        let index = br#"
# Index

[index climb](../index.md)
[final dot](resources/x.md/.)
"#;
        let resource = br#"# test.compute.X

[resource climb](../../index.md)
"#;
        let data_source = br#"# test.compute.Source

[fragment](#a)
[relative](../resources/x.md#a)
[query](../resources/x.md?/../x.md)
[missing](../resources/missing.md)
[encoded](../resources/%78.md#encoded)
[mail](mailto:user@example.com)
[http](https://example.com/path#unchanged)
"#;
        let bundle = render_bundle(&bundle_with_files(&[
            (b"index.md", index),
            (b"resources/x.md", resource),
            (b"data-sources/source.md", data_source),
        ]));
        let index_html = page_html(&bundle, "");
        let resource_html = page_html(&bundle, "/resources/x");
        let html = page_html(&bundle, "/data-sources/source");

        assert!(html.contains("href=\"#user-content-a\">fragment</a>"));
        assert!(html.contains(concat!(
            "href=\"/providers/carina-rs/aws/1.2.3/resources/x#user-content-a\"",
            ">relative</a>"
        )));
        assert!(html.contains(concat!(
            "href=\"/providers/carina-rs/aws/1.2.3/resources/x#user-content-encoded\"",
            ">encoded</a>"
        )));
        assert!(html.contains("href=\"mailto:user@example.com\">mail</a>"));
        assert!(html.contains("href=\"https://example.com/path#unchanged\">http</a>"));
        for removed in ["query", "missing"] {
            assert!(html.contains(&format!("<a>{removed}</a>")));
        }
        assert!(index_html.contains("<a>index climb</a>"));
        assert!(index_html.contains("<a>final dot</a>"));
        assert!(resource_html.contains("<a>resource climb</a>"));
    }

    #[test]
    fn sanitizer_drops_dangerous_schemes_and_protocol_relative_urls() {
        let html = sanitize_html(
            r#"
<a href="//evil.example/x">protocol relative</a>
<a href="jav&#x09;ascript:alert(1)">tab javascript</a>
<a href="javascript&colon;alert(1)">entity javascript</a>
<a href="data:text/html,%3Cscript%3Ealert(1)%3C/script%3E">data</a>
<a href="vbscript:alert(1)">vbscript</a>
<a href=" https://example.com">leading whitespace</a>
<a href="https:../x">https without authority</a>
<a href="http:x">http without authority</a>
<a href="https://example.com/a">https with authority</a>
<a href=" mailto:user@example.com">leading whitespace mailto</a>
"#,
            "index.md",
            Arc::new(HashMap::new()),
            "/providers/carina-rs/aws/1.2.3".into(),
            "nonce-".to_string(),
        );

        for text in [
            "protocol relative",
            "tab javascript",
            "entity javascript",
            "data",
            "vbscript",
            "leading whitespace",
            "https without authority",
            "http without authority",
            "leading whitespace mailto",
        ] {
            assert!(
                html.contains(&format!("<a>{text}</a>")),
                "unexpected HTML: {html}"
            );
        }
        assert!(html.contains("href=\"https://example.com/a\">https with authority</a>"));
    }

    #[test]
    fn rendered_code_keeps_only_valid_language_classes() {
        let markdown = "```rust+\nlet x = 1;\n```\n";

        let bundle = render_bundle(&bundle_with_files(&[(b"index.md", markdown.as_bytes())]));
        let html = page_html(&bundle, "");

        assert!(html.contains("<code class=\"language-rust+\">"));
    }

    #[test]
    fn renderer_enables_only_the_required_gfm_extensions() {
        let markdown = concat!(
            "| left | center |\n",
            "| :--- | :---: |\n",
            "| one | two |\n\n",
            "~~removed~~\n\n",
            "Bare https://example.com/path\n\n",
            "- [x] task syntax stays text\n\n",
            "footnote[^1]\n\n",
            "[^1]: definition\n",
        );

        let bundle = render_bundle(&bundle_with_files(&[(b"index.md", markdown.as_bytes())]));
        let html = page_html(&bundle, "");
        let main = html
            .split_once("<main class=\"docs-content\">")
            .unwrap()
            .1
            .split_once("</main>")
            .unwrap()
            .0;

        assert!(html.contains("<table>"));
        assert!(html.contains("<th align=\"left\">left</th>"));
        assert!(html.contains("<th align=\"center\">center</th>"));
        assert!(html.contains("<del>removed</del>"));
        assert!(html.contains("href=\"https://example.com/path\""));
        assert!(html.contains("[x] task syntax stays text"));
        assert!(html.contains("^1"));
        assert!(!html.contains("#user-content-fn"));
        assert!(!html.contains("↩"));
        assert!(!main.contains("type=\"checkbox\""));
        assert!(!html.contains("footnote-ref"));
    }

    #[test]
    fn index_fallback_has_the_full_sorted_navigation() {
        let bundle = render_bundle(&bundle_with_files(&[
            (b"resources/zeta.md", b"# example.compute.Zeta\n"),
            (b"index.md", b"# Index\n"),
            (b"data-sources/vpc.md", b"# example.compute.Vpc\n"),
            (b"resources/alpha.md", b"# example.compute.Alpha\n"),
            (b"data-sources/ami.md", b"# example.compute.Ami\n"),
        ]));
        let hrefs = [
            "/providers/carina-rs/aws/1.2.3/resources/alpha",
            "/providers/carina-rs/aws/1.2.3/resources/zeta",
            "/providers/carina-rs/aws/1.2.3/data-sources/ami",
            "/providers/carina-rs/aws/1.2.3/data-sources/vpc",
        ];

        let html = page_html(&bundle, "");
        let topbar = html.find("<header class=\"topbar\">").unwrap();
        let sidebar = html.find("<aside class=\"docs-sidebar\">").unwrap();
        let switcher = html.find("id=\"version-switcher\"").unwrap();
        let overview = html
            .find("href=\"/providers/carina-rs/aws/1.2.3\"")
            .unwrap();
        let main = html.find("<main").unwrap();

        assert!(topbar < sidebar && sidebar < main);
        assert!(switcher < overview, "switcher must be first in the sidebar");
        assert!(html.contains("href=\"https://carina-rs.dev/\""));
        assert!(html.contains(concat!(
            "<a href=\"https://carina-rs.dev/reference/dsl/syntax/\" ",
            "class=\"external\">Reference →</a>"
        )));
        assert!(html.contains(concat!(
            "<a href=\"/providers/index.html\" aria-current=\"page\">",
            "Providers</a>"
        )));
        assert!(html.contains(concat!(
            "<a href=\"https://github.com/carina-rs/carina\" ",
            "class=\"external\">GitHub →</a>"
        )));
        assert!(html.contains(concat!(
            "<input class=\"sidebar-toggle\" type=\"checkbox\" ",
            "id=\"sidebar-toggle\">"
        )));
        assert!(html.contains(concat!(
            "<label class=\"sidebar-toggle-label\" for=\"sidebar-toggle\">",
            "Provider navigation</label>"
        )));
        assert!(html.contains(concat!(
            "<nav class=\"sidebar-nav\" aria-label=\"Provider documentation\" ",
            "data-nav-url=\"/providers/carina-rs/aws/1.2.3/nav.json\" ",
            "data-current=\"/providers/carina-rs/aws/1.2.3\">"
        )));
        assert!(!html.contains("<details"));
        assert_eq!(html.matches("<script").count(), 1);
        assert!(!html.contains("<style"));
        assert!(!html.contains(" style="));

        for href in hrefs {
            assert!(html.contains(&format!("href=\"{href}\"")), "missing {href}");
        }
        assert!(hrefs
            .windows(2)
            .all(|pair| html.find(pair[0]).unwrap() < html.find(pair[1]).unwrap()));
        assert!(html.contains("Resources"));
        assert!(html.contains("Data Sources"));
        assert!(html.contains(concat!(
            "href=\"/providers/carina-rs/aws/1.2.3\" ",
            "aria-current=\"page\">Overview</a>"
        )));
    }

    #[test]
    fn resource_fallback_has_only_the_current_resource_and_an_index_link() {
        let bundle = render_bundle(&bundle_with_files(&[
            (b"index.md", b"# Index\n"),
            (b"resources/alpha.md", b"# example.compute.Alpha\n"),
            (b"resources/zeta.md", b"# example.compute.Zeta\n"),
            (b"data-sources/ami.md", b"# example.compute.Ami\n"),
        ]));
        let html = page_html(&bundle, "/resources/alpha");
        let fallback = sidebar_links(html);

        assert!(fallback.contains("Resources"));
        assert!(!fallback.contains("Data Sources"));
        assert!(fallback.contains(concat!(
            "href=\"/providers/carina-rs/aws/1.2.3/resources/alpha\" ",
            "aria-current=\"page\">example.compute.Alpha</a>"
        )));
        assert!(!fallback.contains("/resources/zeta"));
        assert!(!fallback.contains("/data-sources/ami"));
        assert!(fallback.contains("href=\"/providers/carina-rs/aws/1.2.3\">Overview</a>"));
        assert!(!fallback.contains("All pages"));
        assert_eq!(
            fallback
                .matches("href=\"/providers/carina-rs/aws/1.2.3\"")
                .count(),
            1
        );
    }

    #[test]
    fn data_source_fallback_has_only_the_current_data_source_and_an_index_link() {
        let bundle = render_bundle(&bundle_with_files(&[
            (b"index.md", b"# Index\n"),
            (b"resources/alpha.md", b"# example.compute.Alpha\n"),
            (b"data-sources/ami.md", b"# example.compute.Ami\n"),
            (b"data-sources/vpc.md", b"# example.compute.Vpc\n"),
        ]));
        let html = page_html(&bundle, "/data-sources/vpc");
        let fallback = sidebar_links(html);

        assert!(!fallback.contains("Resources"));
        assert!(fallback.contains("Data Sources"));
        assert!(fallback.contains(concat!(
            "href=\"/providers/carina-rs/aws/1.2.3/data-sources/vpc\" ",
            "aria-current=\"page\">example.compute.Vpc</a>"
        )));
        assert!(!fallback.contains("/data-sources/ami"));
        assert!(!fallback.contains("/resources/alpha"));
        assert!(fallback.contains("href=\"/providers/carina-rs/aws/1.2.3\">Overview</a>"));
        assert!(!fallback.contains("All pages"));
        assert_eq!(
            fallback
                .matches("href=\"/providers/carina-rs/aws/1.2.3\"")
                .count(),
            1
        );
    }

    #[test]
    fn navigation_document_is_compact_sorted_json_with_a_trailing_newline() {
        let bundle = render_bundle(&bundle_with_files(&[
            (b"resources/zeta.md", b"# example.compute.Zeta\n"),
            (b"index.md", b"# Index\n"),
            (b"data-sources/vpc.md", b"# example.compute.Vpc\n"),
            (b"resources/alpha.md", b"# example.compute.Alpha\n"),
            (b"data-sources/ami.md", b"# example.compute.Ami\n"),
        ]));

        assert_eq!(
            navigation_json(&bundle),
            concat!(
                "{\"overview\":\"/providers/carina-rs/aws/1.2.3\",",
                "\"resources\":[",
                "{\"slug\":\"alpha\",\"title\":\"example.compute.Alpha\",\"href\":\"/providers/carina-rs/aws/1.2.3/resources/alpha\"},",
                "{\"slug\":\"zeta\",\"title\":\"example.compute.Zeta\",\"href\":\"/providers/carina-rs/aws/1.2.3/resources/zeta\"}],",
                "\"data_sources\":[",
                "{\"slug\":\"ami\",\"title\":\"example.compute.Ami\",\"href\":\"/providers/carina-rs/aws/1.2.3/data-sources/ami\"},",
                "{\"slug\":\"vpc\",\"title\":\"example.compute.Vpc\",\"href\":\"/providers/carina-rs/aws/1.2.3/data-sources/vpc\"}]}\n"
            )
        );
    }

    #[test]
    fn fully_covered_version_builds_grouped_navigation_from_provider_metadata() {
        let archive = grouped_bundle();
        let validated = visit_docs_archive(&archive, |_, _| Ok(())).unwrap();
        let metadata = test_grouped_navigation_metadata();
        let navigation = derive_docs_navigation(validated.pages(), Some(&metadata)).navigation;

        assert_eq!(validated.pages().len(), 5);
        let DocsNavigation::Grouped(grouped) = navigation else {
            panic!("complete provider metadata must select grouped navigation");
        };
        let services = &grouped.services;
        assert_eq!(services.len(), 2);
        assert_eq!(services[0].id, "acm");
        assert_eq!(services[0].title, "ACM & <Certificates>");
        assert_eq!(
            services[0]
                .resources
                .iter()
                .map(NamedDocsPage::slug)
                .collect::<Vec<_>>(),
            ["acm_certificate", "acm_pca"]
        );
        assert!(services[0].data_sources.is_empty());
        assert_eq!(services[1].id, "ec2");
        assert_eq!(services[1].title, "EC2");
        assert_eq!(
            services[1].resources[0].path(),
            &NamedPagePath::new(DocsPageKind::Resource, "ec2_instance")
        );
        assert_eq!(
            services[1].data_sources[0].path(),
            &NamedPagePath::new(DocsPageKind::DataSource, "ec2_ami")
        );
        assert_eq!(
            grouped
                .service_for(&NamedPagePath::new(
                    DocsPageKind::Resource,
                    "acm_certificate",
                ))
                .map(|service| service.id.as_str()),
            Some("acm")
        );
        assert_eq!(
            grouped
                .service_for(&NamedPagePath::new(DocsPageKind::DataSource, "ec2_ami"))
                .map(|service| service.id.as_str()),
            Some("ec2")
        );
    }

    #[test]
    fn grouped_services_are_sorted_by_ascii_id_instead_of_title() {
        let archive = bundle_with_files(&[
            (b"index.md", b"# Index\n"),
            (b"resources/a.md", b"# test.compute.A\n"),
            (b"resources/z.md", b"# test.compute.Z\n"),
        ]);
        let validated = visit_docs_archive(&archive, |_, _| Ok(())).unwrap();
        let metadata = validate_provider_navigation_metadata(
            br#"{"services":[{"id":"z_service","title":"Alpha","resources":["z"],"data_sources":[]},{"id":"a_service","title":"Zulu","resources":["a"],"data_sources":[]}]}"#,
        )
        .unwrap();
        let navigation = derive_docs_navigation(validated.pages(), Some(&metadata)).navigation;
        let DocsNavigation::Grouped(grouped) = navigation else {
            panic!("complete provider metadata must select grouped navigation");
        };

        assert_eq!(
            grouped
                .services
                .iter()
                .map(|service| (service.id.as_str(), service.title.as_str()))
                .collect::<Vec<_>>(),
            [("a_service", "Zulu"), ("z_service", "Alpha")]
        );
    }

    #[test]
    fn metadata_with_an_unassigned_page_falls_back_to_flat_and_warns() {
        let archive = bundle_with_files(&[
            (b"index.md", b"# Index\n"),
            (
                b"resources/reference_resource.md",
                b"# reference.Resource\n",
            ),
            (b"resources/unassigned.md", b"# reference.Unassigned\n"),
            (
                b"data-sources/missing_data.md",
                b"# reference.MissingData\n",
            ),
        ]);
        let validated = visit_docs_archive(&archive, |_, _| Ok(())).unwrap();
        let version_root = DocsVersionRoot::new("carina-rs", "render-reference-grouped", "9.8.7");
        let logs = Arc::new(Mutex::new(Vec::new()));
        let writer = Arc::clone(&logs);
        let subscriber = tracing_subscriber::fmt()
            .without_time()
            .with_ansi(false)
            .with_writer(move || SharedLogBuffer(Arc::clone(&writer)))
            .finish();
        let subscriber_guard = tracing::subscriber::set_default(subscriber);

        let navigation = docs_navigation_for_version(validated.pages(), &version_root, true);
        drop(subscriber_guard);

        let DocsNavigation::Flat {
            resources,
            data_sources,
        } = navigation
        else {
            panic!("incomplete provider metadata must fall back to flat navigation");
        };
        assert_eq!(
            resources
                .iter()
                .map(NamedDocsPage::slug)
                .collect::<Vec<_>>(),
            ["reference_resource", "unassigned"]
        );
        assert_eq!(
            data_sources
                .iter()
                .map(NamedDocsPage::slug)
                .collect::<Vec<_>>(),
            ["missing_data"]
        );
        let logs = String::from_utf8(logs.lock().unwrap().clone()).unwrap();
        for expected in [
            "provider navigation metadata does not cover documentation version",
            "namespace=carina-rs",
            "name=render-reference-grouped",
            "version=9.8.7",
            "unassigned",
            "missing_data",
        ] {
            assert!(
                logs.contains(expected),
                "missing {expected:?} from {logs:?}"
            );
        }
    }

    #[test]
    fn duplicate_resource_type_names_in_one_service_section_render_flat_and_warn() {
        assert_duplicate_type_names_render_flat_and_warn(
            &[
                (b"index.md", b"# Index\n"),
                (b"resources/first.md", b"# example.first.Shared\n"),
                (b"resources/second.md", b"# example.second.Shared\n"),
            ],
            br#"{"services":[{"id":"shared","title":"Shared","resources":["first","second"],"data_sources":[]}]}"#,
            "shared.resources",
            &["example.first.Shared", "example.second.Shared"],
        );
    }

    #[test]
    fn duplicate_data_source_type_names_in_one_service_section_render_flat_and_warn() {
        assert_duplicate_type_names_render_flat_and_warn(
            &[
                (b"index.md", b"# Index\n"),
                (
                    b"data-sources/first.md",
                    b"# example.first.Shared\n",
                ),
                (
                    b"data-sources/second.md",
                    b"# example.second.Shared\n",
                ),
            ],
            br#"{"services":[{"id":"shared","title":"Shared","resources":[],"data_sources":["first","second"]}]}"#,
            "shared.data_sources",
            &["example.first.Shared", "example.second.Shared"],
        );
    }

    #[test]
    fn resource_and_data_source_may_share_a_grouped_type_name() {
        let archive = bundle_with_files(&[
            (b"index.md", b"# Index\n"),
            (b"resources/bucket.md", b"# aws.s3.Bucket\n"),
            (b"data-sources/bucket.md", b"# aws.s3.Bucket\n"),
        ]);
        let validated = visit_docs_archive(&archive, |_, _| Ok(())).unwrap();
        let metadata = validate_provider_navigation_metadata(
            br#"{"services":[{"id":"s3","title":"S3","resources":["bucket"],"data_sources":["bucket"]}]}"#,
        )
        .unwrap();

        let navigation = derive_docs_navigation(validated.pages(), Some(&metadata)).navigation;

        assert!(matches!(navigation, DocsNavigation::Grouped(_)));
    }

    #[test]
    fn provider_without_metadata_uses_flat_navigation() {
        let pages = [
            ValidatedDocsPage::Index,
            validated_page(
                docs_page_path(DocsPageKind::Resource, "zeta"),
                "# example.compute.Zeta\n",
            ),
            validated_page(
                docs_page_path(DocsPageKind::Resource, "alpha"),
                "# example.compute.Alpha\n",
            ),
        ];
        let version_root = DocsVersionRoot::new("example", "plain", "1.0.0");

        let navigation = docs_navigation_for_version(&pages, &version_root, true);

        let DocsNavigation::Flat { resources, .. } = navigation else {
            panic!("provider without metadata must use flat navigation");
        };
        assert_eq!(
            resources
                .iter()
                .map(NamedDocsPage::slug)
                .collect::<Vec<_>>(),
            ["alpha", "zeta"]
        );
    }

    #[test]
    fn invalid_runtime_metadata_falls_back_to_flat_without_panicking() {
        let pages = [
            ValidatedDocsPage::Index,
            validated_page(
                docs_page_path(DocsPageKind::Resource, "page"),
                "# example.compute.Page\n",
            ),
        ];
        let version_root = DocsVersionRoot::new("example", "invalid", "1.0.0");

        let navigation =
            docs_navigation_for_metadata_bytes(&pages, &version_root, Some(b"{"), false);

        assert!(matches!(navigation, DocsNavigation::Flat { .. }));
    }

    #[test]
    fn grouped_navigation_document_is_compact_sorted_json() {
        let bundle = render_grouped_bundle(&grouped_bundle());

        assert_eq!(
            navigation_json(&bundle),
            concat!(
                "{\"overview\":\"/providers/carina-rs/aws/1.2.3\",",
                "\"services\":[",
                "{\"id\":\"acm\",\"title\":\"ACM & <Certificates>\",\"resources\":[",
                "{\"slug\":\"acm_certificate\",\"title\":\"aws.acm.Certificate\",\"href\":\"/providers/carina-rs/aws/1.2.3/resources/acm_certificate\"},",
                "{\"slug\":\"acm_pca\",\"title\":\"aws.acm.Pca\",\"href\":\"/providers/carina-rs/aws/1.2.3/resources/acm_pca\"}],",
                "\"data_sources\":[]},",
                "{\"id\":\"ec2\",\"title\":\"EC2\",\"resources\":[",
                "{\"slug\":\"ec2_instance\",\"title\":\"aws.ec2.Instance\",\"href\":\"/providers/carina-rs/aws/1.2.3/resources/ec2_instance\"}],",
                "\"data_sources\":[",
                "{\"slug\":\"ec2_ami\",\"title\":\"aws.ec2.Ami\",\"href\":\"/providers/carina-rs/aws/1.2.3/data-sources/ec2_ami\"}]}]}\n"
            )
        );
    }

    #[test]
    fn grouped_index_fallback_renders_closed_services_with_every_page() {
        let bundle = render_grouped_bundle(&grouped_bundle());
        let fallback = sidebar_links(page_html(&bundle, ""));

        assert_eq!(
            fallback
                .matches("<details class=\"sidebar-service\">")
                .count(),
            2
        );
        assert!(!fallback.contains("<details class=\"sidebar-service\" open>"));
        assert!(fallback.contains(concat!(
            "<summary><h2 class=\"sidebar-service-title\">",
            "ACM &amp; &lt;Certificates&gt;</h2></summary>"
        )));
        assert!(
            fallback.contains("<summary><h2 class=\"sidebar-service-title\">EC2</h2></summary>")
        );
        assert!(fallback.find(">ACM").unwrap() < fallback.find(">EC2").unwrap());
        assert!(fallback.contains("<h3 class=\"sidebar-section-title\">Resources</h3>"));
        assert!(!fallback.contains("<h2 class=\"sidebar-section-title\">"));
        for suffix in [
            "/resources/acm_certificate",
            "/resources/acm_pca",
            "/resources/ec2_instance",
            "/data-sources/ec2_ami",
        ] {
            assert!(fallback.contains(suffix), "missing grouped link {suffix}");
        }
        for label in ["Certificate", "Pca", "Instance", "Ami"] {
            assert!(
                fallback.contains(&format!(">{label}</a>")),
                "missing grouped label {label:?}"
            );
        }
        assert!(!fallback.contains(">aws.acm.Pca</a>"));
    }

    #[test]
    fn grouped_resource_fallback_opens_only_the_current_pages_service() {
        let bundle = render_grouped_bundle(&grouped_bundle());
        let fallback = sidebar_links(page_html(&bundle, "/resources/acm_certificate"));

        assert_eq!(
            fallback
                .matches("<details class=\"sidebar-service\" open>")
                .count(),
            1
        );
        assert!(fallback.contains(concat!(
            "<summary><h2 class=\"sidebar-service-title\">",
            "ACM &amp; &lt;Certificates&gt;</h2></summary>"
        )));
        assert!(fallback.contains(concat!(
            "href=\"/providers/carina-rs/aws/1.2.3/resources/acm_certificate\" ",
            "aria-current=\"page\">Certificate</a>"
        )));
        assert!(!fallback.contains("/resources/acm_pca"));
        assert!(!fallback.contains("/resources/ec2_instance"));
        assert!(!fallback.contains("/data-sources/ec2_ami"));
        assert!(
            !fallback.contains("<summary><h2 class=\"sidebar-service-title\">EC2</h2></summary>")
        );
        assert!(!fallback.contains("Data Sources"));
    }

    #[test]
    fn grouped_sidebar_returns_an_error_when_the_current_page_has_no_service() {
        let archive = grouped_bundle();
        let validated = visit_docs_archive(&archive, |_, _| Ok(())).unwrap();
        let metadata = test_grouped_navigation_metadata();
        let navigation = derive_docs_navigation(validated.pages(), Some(&metadata)).navigation;
        let version_root = DocsVersionRoot::new("carina-rs", "aws", "1.2.3");
        let missing = validated_page(
            docs_page_path(DocsPageKind::Resource, "missing"),
            "# aws.test.Missing\n",
        );
        let error = render_sidebar(
            &version_root,
            &missing,
            &navigation,
            "providers/carina-rs/aws",
            "providers/carina-rs/aws/versions.json",
        )
        .unwrap_err();

        assert_eq!(
            error.detail(),
            "grouped navigation does not assign the current documentation page"
        );
    }

    #[test]
    fn services_json_in_a_bundle_is_refused_as_an_unexpected_file() {
        let archive = bundle_with_files(&[
            (b"index.md", b"# Index\n"),
            (b"services.json", br#"{"services":[]}"#),
        ]);

        let error = validate(&archive)
            .err()
            .expect("services.json must be refused");
        assert_eq!(
            error.detail(),
            "file is outside an allowed documentation location"
        );
    }

    #[test]
    fn provider_navigation_metadata_is_not_subject_to_the_markdown_table_cell_bound() {
        let title = "|".repeat(128);
        let whitespace = "\n".repeat(2_000);
        let document = format!(
            "{{{whitespace}\"services\":[{{\"id\":\"one\",\"title\":\"{title}\",\"resources\":[\"a\"],\"data_sources\":[]}}]}}"
        );
        assert!(table_cell_bound(&document).unwrap() > MAX_TABLE_CELL_BOUND);

        assert!(validate_provider_navigation_metadata(document.as_bytes()).is_ok());
    }

    #[test]
    fn navigation_document_json_escapes_inserted_coordinates() {
        let archive = bundle_with_files(&[(b"index.md", b"# Index\n")]);
        let validated = visit_docs_archive(&archive, |_, _| Ok(())).unwrap();
        let navigation = derive_docs_navigation(validated.pages(), None).navigation;
        // Exercise JSON escaping below the coordinate validation boundary.
        let version_root = DocsVersionRoot::new("ns\\\"", "name", "1.0.0\"");
        let page = render_navigation_document(&version_root, &navigation).unwrap();
        let json = std::str::from_utf8(&page.body).unwrap();
        let value: serde_json::Value = serde_json::from_str(json).unwrap();

        assert_eq!(value["overview"], "/providers/ns\\\"/name/1.0.0\"");
        assert!(json.contains("ns\\\\\\\""));
        assert!(json.contains("1.0.0\\\""));
        assert!(json.ends_with('\n'));
    }

    #[test]
    fn fingerprint_length_prefixes_make_key_and_body_boundaries_unambiguous() {
        assert_ne!(
            fingerprint_outputs([("a", HTML_CONTENT_TYPE, b"bc".as_slice())]),
            fingerprint_outputs([("ab", HTML_CONTENT_TYPE, b"c".as_slice())])
        );
    }

    #[test]
    fn fingerprint_includes_each_objects_content_type() {
        assert_ne!(
            fingerprint_outputs([("object", HTML_CONTENT_TYPE, b"same body".as_slice())]),
            fingerprint_outputs([("object", JSON_CONTENT_TYPE, b"same body".as_slice())])
        );
    }

    #[test]
    fn rendered_html_limit_rejects_the_whole_bundle() {
        assert_eq!(MAX_RENDERED_HTML_BYTES, 64 * 1024 * 1024);
        let source = [
            (b"index.md".as_slice(), b"# Index\n".as_slice()),
            (
                b"resources/x.md".as_slice(),
                b"# test.compute.X\n".as_slice(),
            ),
        ];
        let archive = bundle_with_files(&source);
        let validated = visit_docs_archive(&archive, |_, _| Ok(())).unwrap();
        let version_root = DocsVersionRoot::new("carina-rs", "aws", "1.2.3");
        let navigation = derive_docs_navigation(validated.pages(), None).navigation;
        let mut rendered =
            render_validated_archive(&archive, &validated, &version_root, &navigation, usize::MAX)
                .unwrap();
        append_navigation_document(&mut rendered, &navigation, &version_root, usize::MAX).unwrap();
        let page_sizes = rendered
            .iter()
            .map(|page| page.body.len())
            .collect::<Vec<_>>();
        let one_byte_below_total = page_sizes.iter().sum::<usize>() - 1;
        assert!(page_sizes.iter().all(|size| *size <= one_byte_below_total));

        let mut rendered = render_validated_archive(
            &archive,
            &validated,
            &version_root,
            &navigation,
            one_byte_below_total,
        )
        .unwrap();
        let error = append_navigation_document(
            &mut rendered,
            &navigation,
            &version_root,
            one_byte_below_total,
        )
        .unwrap_err();

        assert_eq!(
            error.detail(),
            "rendered documentation exceeds the 64 MiB total limit"
        );
    }

    #[test]
    fn two_thousand_navigation_pages_render_under_the_total_limit() {
        let two_thousand = synthetic_rendered_size(2_000, MAX_RENDERED_HTML_BYTES).unwrap();

        eprintln!("synthetic rendered size: 2000 navigation pages = {two_thousand} bytes");
        assert!(two_thousand < MAX_RENDERED_HTML_BYTES);
    }

    #[test]
    fn rendered_output_grows_linearly_with_navigation_pages() {
        let five_hundred = synthetic_rendered_size(500, usize::MAX).unwrap();
        let two_thousand = synthetic_rendered_size(2_000, usize::MAX).unwrap();

        eprintln!(
            "synthetic rendered sizes: 500 navigation pages = {five_hundred} bytes; 2000 navigation pages = {two_thousand} bytes"
        );
        let scaled_two_thousand = two_thousand * 500;
        let scaled_five_hundred = five_hundred * 2_000;
        assert!(
            scaled_two_thousand.abs_diff(scaled_five_hundred) <= scaled_five_hundred / 10,
            "bytes per page differ by more than 10%: 500 pages = {five_hundred}, 2000 pages = {two_thousand}"
        );
    }

    #[test]
    fn rendered_page_html_limit_stops_the_formatter_at_two_mibibytes() {
        let mut markdown = String::new();
        for _ in 0..1_500 {
            markdown.push_str(&"> ".repeat(60));
            markdown.push_str("x\n\noutside\n\n");
        }
        assert!(markdown.len() <= MAX_FILE_BYTES as usize);
        assert!(validate_table_cell_bound(&markdown).is_ok());

        let error = validate(&bundle_with_files(&[(b"index.md", markdown.as_bytes())]))
            .err()
            .expect("formatter output over 2 MiB must be rejected");

        assert_eq!(
            error.detail(),
            "rendered documentation page exceeds the 2 MiB limit"
        );
    }

    #[test]
    fn parse_tree_depth_64_passes_and_depth_65_fails() {
        let at_limit = format!("{}x\n", "> ".repeat(62));
        let too_deep = format!("{}x\n", "> ".repeat(63));

        assert!(validate(&bundle_with_files(&[(b"index.md", at_limit.as_bytes())])).is_ok());
        let error = validate(&bundle_with_files(&[(b"index.md", too_deep.as_bytes())]))
            .err()
            .expect("parse trees deeper than 64 levels must be rejected");

        assert_eq!(
            error.detail(),
            "documentation parse tree exceeds the 64-level depth limit"
        );
    }

    #[test]
    fn deeply_nested_emphasis_exceeds_the_parse_tree_depth_limit() {
        let mut markdown = String::new();
        for _ in 0..64 {
            markdown.push_str("*a ");
        }
        markdown.push('x');
        for _ in 0..64 {
            markdown.push_str(" a*");
        }
        markdown.push('\n');

        let error = validate(&bundle_with_files(&[(b"index.md", markdown.as_bytes())]))
            .err()
            .expect("deep inline emphasis must be rejected");

        assert_eq!(
            error.detail(),
            "documentation parse tree exceeds the 64-level depth limit"
        );
    }

    #[test]
    fn wide_table_bomb_is_rejected_before_comrak_runs() {
        let mut markdown = String::from("|");
        for _ in 0..512 {
            markdown.push_str("h|");
        }
        markdown.push('\n');
        markdown.push('|');
        for _ in 0..512 {
            markdown.push_str("-|");
        }
        markdown.push('\n');
        for _ in 0..512 {
            markdown.push_str("|x|\n");
        }

        assert!(markdown.len() < 8 * 1024);

        let error = validate(&bundle_with_files(&[(b"index.md", markdown.as_bytes())]))
            .err()
            .expect("wide table must exceed the table-cell bound");

        assert_eq!(
            error.detail(),
            "documentation file pipe-by-line product exceeds the 250,000 limit"
        );
    }

    #[test]
    fn cr_only_lines_count_toward_the_table_cell_bound() {
        let mut markdown = "|".repeat(1_000);
        markdown.push('\r');
        for _ in 0..249 {
            markdown.push_str("x\r");
        }
        assert_eq!(markdown.bytes().filter(|byte| *byte == b'\n').count(), 0);

        let error = validate(&bundle_with_files(&[(b"index.md", markdown.as_bytes())]))
            .err()
            .expect("CR-only lines must count toward the table-cell bound");

        assert_eq!(
            error.detail(),
            "documentation file pipe-by-line product exceeds the 250,000 limit"
        );
    }

    #[test]
    fn ordinary_table_stays_below_the_table_cell_bound() {
        let archive = bundle_with_files(&[(
            b"index.md",
            b"| name | value |\r\n| --- | --- |\r\n| one | two |\r\n",
        )]);

        assert!(validate(&archive).is_ok());
    }

    #[test]
    fn table_cell_bound_allows_exactly_250_000() {
        let mut markdown = "|".repeat(249_999);
        markdown.push_str("\r\n");

        assert_eq!(table_cell_bound(&markdown).unwrap(), 250_000);
        assert!(validate_table_cell_bound(&markdown).is_ok());

        markdown.insert(0, '|');
        assert_eq!(table_cell_bound(&markdown).unwrap(), 250_001);
        assert!(validate_table_cell_bound(&markdown).is_err());
    }

    #[test]
    fn table_cell_bound_counts_a_final_unterminated_line() {
        let mut markdown = "|".repeat(1_000);
        markdown.push_str("\r\n");
        for _ in 0..248 {
            markdown.push_str("x\r\n");
        }
        markdown.push('x');

        assert_eq!(table_cell_bound(&markdown).unwrap(), 250_250);
        assert!(validate_table_cell_bound(&markdown).is_err());
    }

    #[test]
    fn empty_markdown_has_no_lines_in_the_table_cell_bound() {
        assert_eq!(table_cell_bound("").unwrap(), 0);
        assert!(validate_table_cell_bound("").is_ok());
    }

    #[test]
    fn render_pass_rejects_a_path_inconsistent_with_validation() {
        let archive = bundle_with_files(&[(b"index.md", b"# Index\n")]);
        let validated_archive = validated_docs_archive(vec![validated_page(
            docs_page_path(DocsPageKind::Resource, "different"),
            "# test.compute.Different\n",
        )]);
        let version_root = DocsVersionRoot::new("carina-rs", "aws", "1.2.3");
        let navigation = derive_docs_navigation(validated_archive.pages(), None).navigation;

        let error = render_validated_archive(
            &archive,
            &validated_archive,
            &version_root,
            &navigation,
            MAX_RENDERED_HTML_BYTES,
        )
        .unwrap_err();

        assert_eq!(
            error.detail(),
            "documentation archive changed between validation and rendering"
        );
    }

    #[test]
    fn resource_page_title_and_label_identify_the_page_kind() {
        let bundle = render_bundle(&bundle_with_files(&[
            (b"index.md", b"# Provider\n"),
            (b"resources/s3_bucket.md", b"# aws.s3.Bucket\n"),
        ]));
        let html = page_html(&bundle, "/resources/s3_bucket");
        let main = html.split_once("<main class=\"docs-content\">").unwrap().1;

        assert!(html.contains("<title>aws.s3.Bucket (resource) · carina-rs/aws 1.2.3</title>"));
        assert!(main.starts_with(concat!(
            "\n<p class=\"docs-page-kind docs-page-kind-resource\">Resource</p>\n",
            "<h1"
        )));
    }

    #[test]
    fn data_source_page_title_and_label_identify_the_page_kind() {
        let bundle = render_bundle(&bundle_with_files(&[
            (b"index.md", b"# Provider\n"),
            (b"data-sources/s3_bucket.md", b"# aws.s3.Bucket\n"),
        ]));
        let html = page_html(&bundle, "/data-sources/s3_bucket");
        let main = html.split_once("<main class=\"docs-content\">").unwrap().1;

        assert!(html.contains("<title>aws.s3.Bucket (data source) · carina-rs/aws 1.2.3</title>"));
        assert!(main.starts_with(concat!(
            "\n<p class=\"docs-page-kind docs-page-kind-data-source\">Data source</p>\n",
            "<h1"
        )));
    }

    #[test]
    fn index_page_has_neither_a_kind_label_nor_a_parenthesized_kind() {
        let bundle = render_bundle(&bundle_with_files(&[(b"index.md", b"# Provider\n")]));
        let html = page_html(&bundle, "");
        let title = html
            .split_once("<title>")
            .unwrap()
            .1
            .split_once("</title>")
            .unwrap()
            .0;
        let main = html.split_once("<main class=\"docs-content\">").unwrap().1;

        assert_eq!(title, "carina-rs/aws 1.2.3");
        assert!(!main.contains("docs-page-kind"));
        assert!(main.starts_with("\n<h1"));
    }

    #[test]
    fn page_template_is_full_html5_and_escapes_inserted_values() {
        let version_root = DocsVersionRoot::new("ns", "name", "1.0.0");
        let current = ValidatedDocsPage::Index;
        let navigation = derive_docs_navigation(std::slice::from_ref(&current), None).navigation;
        let html = render_page_template(
            "<&\"' title",
            &version_root,
            &current,
            &navigation,
            "<p>sanitized body</p>\n",
        )
        .unwrap();

        assert!(html.starts_with("<!doctype html>\n<html>\n<head>\n"));
        assert!(html.contains("<meta charset=\"utf-8\">"));
        assert!(html.contains(concat!(
            "<meta name=\"viewport\" ",
            "content=\"width=device-width, initial-scale=1\">"
        )));
        assert!(html.contains("<title>&lt;&amp;&quot;&#39; title</title>"));
        let tokens = html
            .find("<link rel=\"stylesheet\" href=\"/providers/_assets/tokens.css\">")
            .unwrap();
        let stylesheet = html
            .find("<link rel=\"stylesheet\" href=\"/providers/_assets/docs.css\">")
            .unwrap();
        let highlighting = html
            .find("<link rel=\"stylesheet\" href=\"/providers/_assets/highlight.css\">")
            .unwrap();
        let script = html
            .find("<script src=\"/providers/_assets/docs.js\" defer></script>")
            .unwrap();
        assert!(tokens < stylesheet && stylesheet < highlighting && highlighting < script);
        assert!(html.contains(concat!(
            "<div class=\"version-switcher\"><select id=\"version-switcher\" hidden ",
            "data-versions-url=\"/providers/ns/name/versions.json\" ",
            "data-version-base=\"/providers/ns/name\" data-current=\"1.0.0\" ",
            "aria-label=\"Version\"></select></div>"
        )));
        assert!(!html.contains("version-switcher-go"));
        assert!(!html.contains(">Go</button>"));
        assert!(html.contains(concat!(
            "<nav class=\"sidebar-nav\" aria-label=\"Provider documentation\" ",
            "data-nav-url=\"/providers/ns/name/1.0.0/nav.json\" ",
            "data-current=\"/providers/ns/name/1.0.0\">"
        )));
        assert!(html.contains(concat!(
            "<a class=\"sidebar-link\" href=\"/providers/ns/name/1.0.0\" ",
            "aria-current=\"page\">Overview</a>"
        )));
        assert!(html.contains("<header class=\"topbar\">"));
        assert!(html.contains("<aside class=\"docs-sidebar\">"));
        assert!(html.contains("<input class=\"sidebar-toggle\" type=\"checkbox\""));
        assert!(!html.contains("<details"));
        assert!(html.contains("<link rel=\"stylesheet\" href=\"/providers/_assets/tokens.css\">"));
        assert!(html.contains("<link rel=\"stylesheet\" href=\"/providers/_assets/docs.css\">"));
        assert!(
            html.contains("<link rel=\"stylesheet\" href=\"/providers/_assets/highlight.css\">")
        );
        assert!(html.contains("<script src=\"/providers/_assets/docs.js\" defer></script>"));
        assert!(html.contains("<main class=\"docs-content\">\n<p>sanitized body</p>\n</main>"));
        assert!(!html.contains("<style"));
        assert!(!html.contains(" style="));
        assert!(html.ends_with("</body>\n</html>\n"));
    }

    #[test]
    fn page_template_escapes_switcher_coordinates() {
        let version_root = DocsVersionRoot::new("ns<&", "name\"'", "1.0.0\"<&");
        let page = validated_page(
            docs_page_path(DocsPageKind::Resource, "slug<&\"'"),
            "# example.compute.Resource\n",
        );
        let navigation = derive_docs_navigation(std::slice::from_ref(&page), None).navigation;
        let html = render_page_template("title", &version_root, &page, &navigation, "").unwrap();

        assert!(
            html.contains("href=\"/providers/ns&lt;&amp;/name&quot;&#39;/1.0.0&quot;&lt;&amp;\"")
        );
        assert!(html.contains(
            "data-versions-url=\"/providers/ns&lt;&amp;/name&quot;&#39;/versions.json\" "
        ));
        assert!(html.contains("data-version-base=\"/providers/ns&lt;&amp;/name&quot;&#39;\" "));
        assert!(html.contains("data-current=\"1.0.0&quot;&lt;&amp;\""));
        assert!(html.contains(concat!(
            "data-nav-url=\"/providers/ns&lt;&amp;/name&quot;&#39;/1.0.0&quot;&lt;&amp;/nav.json\" ",
            "data-current=\"/providers/ns&lt;&amp;/name&quot;&#39;/1.0.0&quot;&lt;&amp;",
            "/resources/slug&lt;&amp;&quot;&#39;\">"
        )));
        assert!(html.contains(concat!(
            "href=\"/providers/ns&lt;&amp;/name&quot;&#39;/1.0.0&quot;&lt;&amp;",
            "/resources/slug&lt;&amp;&quot;&#39;\" aria-current=\"page\">",
            "example.compute.Resource</a>"
        )));
    }

    #[test]
    fn rejects_trailing_compressed_data_and_a_second_gzip_member() {
        let mut trailing = valid_bundle();
        trailing.push(0);
        assert!(validate(&trailing).is_err());

        let mut two_members = valid_bundle();
        two_members.extend(valid_bundle());
        assert!(validate(&two_members).is_err());
    }

    #[test]
    fn rejects_non_posix_tar_and_bad_header_checksum() {
        let mut non_posix = TarBuilder::new();
        non_posix.entry_with_magic(b"index.md", b'0', b"ok", b"ustar ", b" \0");
        assert_invalid(non_posix.finish());

        let mut tar = valid_tar();
        tar[0] ^= 1;
        assert_invalid(gzip(&tar));

        let mut base_256 = TarBuilder::new();
        base_256.entry(b"index.md", b'0', b"");
        base_256.bytes[100] = 0x80;
        refresh_first_header_checksum(&mut base_256.bytes);
        assert_invalid(base_256.finish());

        let mut junk_after_terminator = TarBuilder::new();
        junk_after_terminator.entry(b"index.md", b'0', b"");
        junk_after_terminator.bytes[124..136].fill(0);
        junk_after_terminator.bytes[124] = b'0';
        junk_after_terminator.bytes[126] = b'1';
        refresh_first_header_checksum(&mut junk_after_terminator.bytes);
        assert_invalid(junk_after_terminator.finish());
    }

    #[test]
    fn rejects_every_non_file_directory_entry_type() {
        for &kind in b"123467LK" {
            let mut tar = TarBuilder::new();
            tar.entry(b"index.md", b'0', b"ok");
            tar.entry(b"resources/bad.md", kind, b"");
            assert_invalid(tar.finish());
        }
    }

    #[test]
    fn rejects_pax_global_headers_and_malformed_extended_headers() {
        let mut global = TarBuilder::new();
        global.entry(b"pax_global", b'g', &pax_record("comment", "bad"));
        global.entry(b"index.md", b'0', b"ok");
        assert_invalid(global.finish());

        let mut malformed = TarBuilder::new();
        malformed.entry(b"pax", b'x', b"13 path=bad\n");
        malformed.entry(b"index.md", b'0', b"ok");
        let error = validate(&gzip(&malformed.finish())).err().unwrap();
        assert_eq!(error.detail(), "pax record exceeds its header body");

        let mut dangling = TarBuilder::new();
        dangling.entry(b"index.md", b'0', b"ok");
        dangling.entry(b"pax", b'x', &pax_record("comment", "metadata"));
        assert_invalid(dangling.finish());
    }

    #[test]
    fn rejects_pax_extended_header_larger_than_sixty_four_kibibytes() {
        let mut tar = TarBuilder::new();
        tar.entry(b"pax", b'x', &pax_record("comment", &"x".repeat(64 * 1024)));
        tar.entry(b"index.md", b'0', b"ok");

        let error = validate(&gzip(&tar.finish())).err().unwrap();

        assert_eq!(
            error.detail(),
            "pax extended header exceeds the 64 KiB limit"
        );
    }

    #[test]
    fn pax_path_and_size_are_the_effective_entry_values() {
        let body = b"# test.compute.Resource\n";
        let mut tar = TarBuilder::new();
        tar.entry(b"index.md", b'0', b"# Index\n");
        let mut pax = pax_record("path", "./resources/effective.md");
        pax.extend(pax_record("size", &body.len().to_string()));
        tar.entry(b"pax", b'x', &pax);
        tar.entry_with_declared_size(b"ignored", b'0', body, 0);

        assert!(validate(&gzip(&tar.finish())).is_ok());
    }

    #[test]
    fn pax_metadata_does_not_count_toward_the_entry_limit() {
        let mut tar = TarBuilder::new();
        tar.entry(b"resources/", b'5', b"");
        tar.entry(b"data-sources/", b'5', b"");
        tar.entry(b"pax", b'x', &pax_record("path", "./index.md"));
        tar.entry(b"ignored", b'0', b"");
        for index in 0..4093 {
            tar.entry(
                format!("resources/r{index}.md").as_bytes(),
                b'0',
                b"# test.compute.Resource\n",
            );
        }

        // ここではrender出力sizeと切り分けてarchive構造だけをvisitし、
        // PAX metadataをentry数に含めない不変条件を検証する。
        let validated = visit_docs_archive(&gzip(&tar.finish()), |_, _| Ok(())).unwrap();
        assert_eq!(validated.pages().len(), 4094);
    }

    #[test]
    fn rejects_more_than_4096_effective_entries() {
        let mut tar = TarBuilder::new();
        tar.entry(b"resources/", b'5', b"");
        tar.entry(b"data-sources/", b'5', b"");
        tar.entry(b"index.md", b'0', b"");
        for index in 0..4094 {
            tar.entry(
                format!("resources/r{index}.md").as_bytes(),
                b'0',
                b"# test.compute.Resource\n",
            );
        }

        assert_invalid(tar.finish());
    }

    #[test]
    fn rejects_unnormalized_paths_and_duplicate_normalized_paths() {
        for path in [
            b"/index.md".as_slice(),
            b"././index.md",
            b"resources//bad.md",
            b"resources/./bad.md",
            b"resources/../bad.md",
        ] {
            let mut tar = TarBuilder::new();
            tar.entry(b"index.md", b'0', b"ok");
            tar.entry(path, b'0', b"bad");
            assert_invalid(tar.finish());
        }

        let mut duplicate = TarBuilder::new();
        duplicate.entry(b"index.md", b'0', b"one");
        duplicate.entry(b"./index.md", b'0', b"two");
        assert_invalid(duplicate.finish());
    }

    #[test]
    fn rejects_a_non_directory_whose_normalized_path_is_empty() {
        let mut tar = TarBuilder::new();
        tar.entry(b"./", b'0', b"");
        tar.entry(b"index.md", b'0', b"ok");

        assert_invalid(tar.finish());
    }

    #[test]
    fn rejects_a_directory_with_a_nonzero_declared_size() {
        let mut tar = TarBuilder::new();
        tar.entry(b"./", b'5', b"x");
        tar.entry(b"index.md", b'0', b"ok");

        assert_invalid(tar.finish());
    }

    #[test]
    fn rejects_unknown_directories_and_invalid_file_locations() {
        for (path, kind) in [
            (b"other/".as_slice(), b'5'),
            (b"nested/resources/".as_slice(), b'5'),
            (b"README.md".as_slice(), b'0'),
            (b"resources/nested/a.md".as_slice(), b'0'),
            (b"data-sources/a.txt".as_slice(), b'0'),
        ] {
            let mut tar = TarBuilder::new();
            tar.entry(b"index.md", b'0', b"ok");
            tar.entry(path, kind, b"");
            assert_invalid(tar.finish());
        }
    }

    #[test]
    fn rejects_invalid_or_overlong_slugs() {
        let overlong = format!("resources/{}.md", "a".repeat(129));
        for path in [
            "resources/_bad.md",
            "resources/Bad.md",
            "resources/a-b.md",
            "resources/a..md",
        ] {
            let mut tar = TarBuilder::new();
            tar.entry(b"index.md", b'0', b"ok");
            tar.entry(path.as_bytes(), b'0', b"");
            assert_invalid(tar.finish());
        }

        let mut tar = TarBuilder::new();
        tar.entry(b"index.md", b'0', b"ok");
        tar.entry(b"pax", b'x', &pax_record("path", &overlong));
        tar.entry(b"ignored", b'0', b"");
        assert_invalid(tar.finish());
    }

    #[test]
    fn provider_navigation_metadata_requires_utf8_and_an_exact_json_shape() {
        let invalid_documents = [
            (
                "non-UTF-8",
                "expected value at line 1 column 1",
                vec![0xff],
            ),
            (
                "malformed JSON",
                "EOF while parsing an object",
                b"{".to_vec(),
            ),
            (
                "top-level array",
                "expected struct ProviderNavigationDocument",
                b"[]".to_vec(),
            ),
            (
                "missing services",
                "missing field `services`",
                b"{}".to_vec(),
            ),
            (
                "unknown top-level field",
                "unknown field `extra`, expected `services`",
                br#"{"services":[{"id":"one","title":"One","resources":["a"],"data_sources":["c"]}],"extra":true}"#.to_vec(),
            ),
            (
                "duplicate top-level key",
                "duplicate field `services`",
                br#"{"services":[{"id":"one","title":"One","resources":["a"],"data_sources":["c"]}],"services":[{"id":"one","title":"One","resources":["a"],"data_sources":["c"]}]}"#.to_vec(),
            ),
            (
                "empty services",
                "services must be a non-empty array",
                br#"{"services":[]}"#.to_vec(),
            ),
            (
                "non-object service",
                "invalid type: string \"one\", expected struct ServiceDeclaration",
                br#"{"services":["one"]}"#.to_vec(),
            ),
            (
                "unknown service field",
                "unknown field `extra`",
                br#"{"services":[{"id":"one","title":"One","resources":["a"],"data_sources":["c"],"extra":true}]}"#.to_vec(),
            ),
            (
                "duplicate service field",
                "duplicate field `id`",
                br#"{"services":[{"id":"one","id":"two","title":"One","resources":["a"],"data_sources":["c"]}]}"#.to_vec(),
            ),
            (
                "missing resources",
                "missing field `resources`",
                br#"{"services":[{"id":"one","title":"One","data_sources":["c"]}]}"#.to_vec(),
            ),
            (
                "missing data_sources",
                "missing field `data_sources`",
                br#"{"services":[{"id":"one","title":"One","resources":["a"]}]}"#.to_vec(),
            ),
            (
                "resources is not an array",
                "invalid type: string \"a\", expected a sequence",
                br#"{"services":[{"id":"one","title":"One","resources":"a","data_sources":["c"]}]}"#.to_vec(),
            ),
            (
                "data_sources contains a non-string",
                "invalid type: integer `1`, expected a string",
                br#"{"services":[{"id":"one","title":"One","resources":["a"],"data_sources":[1]}]}"#.to_vec(),
            ),
        ];

        for (case, expected_error, document) in invalid_documents {
            assert_invalid_navigation_metadata(case, expected_error, &document);
        }
    }

    #[test]
    fn provider_navigation_metadata_validates_service_ids_and_titles() {
        for id in [
            "",
            "_one",
            "One",
            "one-two",
            "one.two",
            "é",
            &"a".repeat(129),
        ] {
            let document = format!(
                r#"{{"services":[{{"id":"{id}","title":"One","resources":["a"],"data_sources":["c"]}}]}}"#
            );
            assert_invalid_navigation_metadata(
                &format!("invalid id {id:?}"),
                "service id does not match the required form",
                document.as_bytes(),
            );
        }

        let overlong_title = "x".repeat(129);
        let invalid_titles = [
            ("empty title", String::new()),
            ("overlong title", overlong_title),
        ];
        for (case, encoded_title) in invalid_titles {
            let document = format!(
                r#"{{"services":[{{"id":"one","title":"{encoded_title}","resources":["a"],"data_sources":["c"]}}]}}"#
            );
            assert_invalid_navigation_metadata(
                case,
                "service title is invalid",
                document.as_bytes(),
            );
        }
    }

    #[test]
    fn provider_navigation_metadata_rejects_each_invalid_title_character_class() {
        for (case, character) in [
            ("C0 control U+0001", '\u{0001}'),
            ("DEL U+007F", '\u{007f}'),
            ("C1 control U+0085", '\u{0085}'),
            ("soft hyphen U+00AD", '\u{00ad}'),
            ("Arabic letter mark U+061C", '\u{061c}'),
            ("Mongolian vowel separator U+180E", '\u{180e}'),
            ("zero-width range U+200B..U+200F", '\u{200b}'),
            ("directional range U+2028..U+202E", '\u{2028}'),
            ("formatting range U+2060..U+206F", '\u{2060}'),
            ("byte-order mark U+FEFF", '\u{feff}'),
            ("interlinear annotation range U+FFF9..U+FFFB", '\u{fff9}'),
        ] {
            let document = serde_json::json!({
                "services": [{
                    "id": "one",
                    "title": format!("before{character}after"),
                    "resources": ["a"],
                    "data_sources": ["c"],
                }]
            })
            .to_string();

            assert_invalid_navigation_metadata(
                case,
                "service title is invalid",
                document.as_bytes(),
            );
        }
    }

    #[test]
    fn provider_navigation_metadata_accepts_exact_id_and_multibyte_title_boundaries() {
        let exact_id = "a".repeat(128);
        let exact_title = "é".repeat(128);
        let document = serde_json::json!({
            "services": [{
                "id": exact_id,
                "title": exact_title,
                "resources": ["a"],
                "data_sources": ["c"],
            }]
        })
        .to_string();

        assert!(validate_provider_navigation_metadata(document.as_bytes()).is_ok());

        let overlong_title = serde_json::json!({
            "services": [{
                "id": "one",
                "title": "é".repeat(129),
                "resources": ["a"],
                "data_sources": ["c"],
            }]
        })
        .to_string();
        assert_invalid_navigation_metadata(
            "129-character multibyte title",
            "service title is invalid",
            overlong_title.as_bytes(),
        );
    }

    #[test]
    fn provider_navigation_metadata_requires_unique_nonempty_services_and_slugs() {
        let invalid_documents = [
            (
                "duplicate ids",
                "contains a duplicate service id",
                br#"{"services":[{"id":"same","title":"First","resources":["a"],"data_sources":[]},{"id":"same","title":"Second","resources":[],"data_sources":["c"]}]}"#.as_slice(),
            ),
            (
                "duplicate titles",
                "contains a duplicate service title",
                br#"{"services":[{"id":"first","title":"Same","resources":["a"],"data_sources":[]},{"id":"second","title":"Same","resources":[],"data_sources":["c"]}]}"#.as_slice(),
            ),
            (
                "service with no pages",
                "contains a service with no pages",
                br#"{"services":[{"id":"empty","title":"Empty","resources":[],"data_sources":[]},{"id":"full","title":"Full","resources":["a"],"data_sources":["c"]}]}"#.as_slice(),
            ),
            (
                "invalid resource slug",
                "resource slug does not match the required form",
                br#"{"services":[{"id":"one","title":"One","resources":["bad-slug"],"data_sources":[]}]}"#.as_slice(),
            ),
            (
                "duplicate resource slug",
                "assigns a resource slug more than once",
                br#"{"services":[{"id":"one","title":"One","resources":["a","a"],"data_sources":[]}]}"#.as_slice(),
            ),
            (
                "duplicate data source slug across services",
                "assigns a data source slug more than once",
                br#"{"services":[{"id":"one","title":"One","resources":["a"],"data_sources":["c"]},{"id":"two","title":"Two","resources":["b"],"data_sources":["c"]}]}"#.as_slice(),
            ),
        ];

        for (case, expected_error, document) in invalid_documents {
            assert_invalid_navigation_metadata(case, expected_error, document);
        }

        let overlong_slug = "a".repeat(129);
        let document = format!(
            r#"{{"services":[{{"id":"one","title":"One","resources":[],"data_sources":["{overlong_slug}"]}}]}}"#
        );
        assert_invalid_navigation_metadata(
            "overlong data source slug",
            "data source slug does not match the required form",
            document.as_bytes(),
        );
    }

    #[test]
    fn provider_navigation_metadata_allows_union_slugs_absent_from_one_version() {
        let metadata = validate_provider_navigation_metadata(
            br#"{"services":[{"id":"one","title":"One","resources":["current","other_version"],"data_sources":["current"]}]}"#,
        )
        .unwrap();

        assert_eq!(metadata.services[0].resources, ["current", "other_version"]);
        assert_eq!(metadata.services[0].data_sources, ["current"]);
    }

    #[test]
    fn resource_and_data_source_sections_may_use_the_same_slug() {
        assert!(validate_provider_navigation_metadata(
            br#"{"services":[{"id":"one","title":"One","resources":["same"],"data_sources":["same"]}]}"#,
        )
        .is_ok());
    }

    #[test]
    fn every_embedded_provider_navigation_metadata_file_is_valid() {
        let mut coordinates = HashSet::new();
        for embedded in EMBEDDED_PROVIDER_NAVIGATION_METADATA {
            assert!(
                coordinates.insert((embedded.namespace, embedded.name)),
                "duplicate embedded metadata coordinates: {}/{}",
                embedded.namespace,
                embedded.name
            );
            validate_provider_navigation_metadata(embedded.bytes).unwrap_or_else(|error| {
                panic!(
                    "invalid embedded metadata for {}/{}: {error}",
                    embedded.namespace, embedded.name
                )
            });
        }
        assert!(coordinates.contains(&("carina-rs", "aws")));
        assert!(coordinates.contains(&("carina-rs", "awscc")));
    }

    #[test]
    fn requires_index_and_utf8_file_content() {
        let mut missing = TarBuilder::new();
        missing.entry(b"resources/a.md", b'0', b"# test.compute.A\n");
        assert_invalid(missing.finish());

        let mut non_utf8 = TarBuilder::new();
        non_utf8.entry(b"index.md", b'0', &[0xff]);
        assert_invalid(non_utf8.finish());
    }

    #[test]
    fn page_title_parsing_accepts_only_valid_first_line_dsl_names() {
        let exact_limit = format!("a.{}", "B".repeat(126));
        assert_eq!(exact_limit.len(), 128);
        let accepted = [
            ("CR-only line ending", "# a.B\rbody".to_string(), "a.B"),
            ("end of file", "# a.B".to_string(), "a.B"),
            (
                "exactly 128 bytes",
                format!("# {exact_limit}"),
                exact_limit.as_str(),
            ),
        ];
        for (case, markdown, expected) in accepted {
            let title = page_title::parse("resources/example.md", &markdown)
                .unwrap_or_else(|error| panic!("rejected {case}: {error}"));
            assert_eq!(title.as_str(), expected, "case: {case}");
        }

        let over_limit = format!("# a.{}", "B".repeat(127));
        let rejected = [
            (
                "missing H1 space",
                "#a.B".to_string(),
                "resources/example.md must begin with an ATX H1 title",
            ),
            (
                "empty title",
                "# ".to_string(),
                "resources/example.md title must be dot-separated ASCII identifier segments",
            ),
            (
                "empty middle segment",
                "# a..B".to_string(),
                "resources/example.md title must be dot-separated ASCII identifier segments",
            ),
            (
                "empty first segment",
                "# .a".to_string(),
                "resources/example.md title must be dot-separated ASCII identifier segments",
            ),
            (
                "empty last segment",
                "# a.".to_string(),
                "resources/example.md title must be dot-separated ASCII identifier segments",
            ),
            (
                "non-ASCII character",
                "# a.Bé".to_string(),
                "resources/example.md title must be dot-separated ASCII identifier segments",
            ),
            (
                "trailing space",
                "# a.B ".to_string(),
                "resources/example.md title must be dot-separated ASCII identifier segments",
            ),
            (
                "more than 128 bytes",
                over_limit,
                "resources/example.md title exceeds the 128-byte limit",
            ),
        ];
        for (case, markdown, expected_error) in rejected {
            let error = page_title::parse("resources/example.md", &markdown)
                .err()
                .unwrap_or_else(|| panic!("accepted {case}"));
            assert_eq!(error.detail(), expected_error, "case: {case}");
        }
    }

    #[test]
    fn bundle_rejects_a_data_source_title_after_a_blank_first_line() {
        let archive = bundle_with_files(&[
            (b"index.md", b"# Index\n"),
            (b"data-sources/example.md", b"\n# x.Y\n"),
        ]);

        let error = validate(&archive)
            .err()
            .expect("a title after a blank first line must be rejected");

        assert_eq!(
            error.detail(),
            "data-sources/example.md must begin with an ATX H1 title"
        );
    }

    #[test]
    fn page_title_accepts_crlf_and_stays_in_the_rendered_markdown() {
        let archive = bundle_with_files(&[
            (b"index.md", b"# Index\n"),
            (
                b"resources/example.md",
                b"# example.compute.Instance\r\n\r\nBody.\r\n",
            ),
        ]);

        let bundle = validate(&archive).unwrap();
        let html = page_html(&bundle, "/resources/example");

        assert!(html
            .contains("<title>example.compute.Instance (resource) · carina-rs/aws 1.2.3</title>"));
        assert!(html.contains(">example.compute.Instance</h1>"));
    }

    #[test]
    fn rejects_a_file_larger_than_256_kibibytes() {
        let mut tar = TarBuilder::new();
        tar.entry(b"index.md", b'0', &vec![b'x'; MAX_FILE + 1]);
        let error = validate(&gzip(&tar.finish()))
            .err()
            .expect("file over 256 KiB must be rejected");

        assert_eq!(error.detail(), "file exceeds the 256 KiB limit");
    }

    #[test]
    fn rejects_more_than_sixty_four_mib_of_files() {
        let mut tar = TarBuilder::new();
        tar.entry(b"index.md", b'0', &vec![b'x'; MAX_FILE]);
        let mut resource = b"# test.compute.Resource\n".to_vec();
        resource.resize(MAX_FILE, b'x');
        for index in 0..256 {
            tar.entry(format!("resources/r{index}.md").as_bytes(), b'0', &resource);
        }
        assert_invalid(tar.finish());
    }

    #[test]
    fn rejects_decompression_bomb_above_eighty_mib_without_accepting_trailing_zeros() {
        let mut tar = valid_tar();
        tar.resize(MAX_TAR_STREAM + 1, 0);
        assert!(validate(&gzip(&tar)).is_err());
    }

    #[test]
    fn permits_only_zero_bytes_after_the_end_marker() {
        let mut zeros = valid_tar();
        zeros.extend([0_u8; BLOCK]);
        assert!(validate(&gzip(&zeros)).is_ok());

        let mut nonzero = valid_tar();
        nonzero.extend([0_u8; 17]);
        nonzero.push(1);
        assert!(validate(&gzip(&nonzero)).is_err());
    }

    #[test]
    fn requires_two_zero_blocks_for_the_tar_end_marker() {
        let mut tar = TarBuilder::new();
        tar.entry(b"index.md", b'0', b"ok");
        let mut bytes = tar.bytes;
        bytes.extend([0_u8; BLOCK]);
        assert!(validate(&gzip(&bytes)).is_err());
    }

    fn assert_invalid(tar: Vec<u8>) {
        assert!(validate(&gzip(&tar)).is_err());
    }

    async fn assert_worker_response_error(bytes: Vec<u8>, expected: &str) {
        let error = read_worker_response(Cursor::new(bytes)).await.unwrap_err();
        assert_eq!(error.to_string(), expected);
    }

    fn encoded_worker_response(response: &WorkerResponse) -> Vec<u8> {
        let mut bytes = Vec::new();
        write_worker_response(&mut bytes, response).unwrap();
        bytes
    }

    fn raw_rendered_response(kind: u8, slug: &[u8], body_len: u64, body: &[u8]) -> Vec<u8> {
        let mut payload = Vec::new();
        payload.extend_from_slice(WORKER_RESPONSE_MAGIC);
        payload.push(WORKER_PROTOCOL_VERSION);
        payload.push(0);
        payload.extend_from_slice(&1_u32.to_le_bytes());
        payload.push(kind);
        payload.extend_from_slice(&(slug.len() as u32).to_le_bytes());
        payload.extend_from_slice(slug);
        payload.extend_from_slice(&body_len.to_le_bytes());
        payload.extend_from_slice(body);

        let mut response = (payload.len() as u64).to_le_bytes().to_vec();
        response.extend_from_slice(&payload);
        response
    }

    fn worker_page(path: DocsPagePath, body: &[u8]) -> WorkerPage {
        WorkerPage {
            path,
            body: body.to_vec(),
        }
    }

    fn docs_page_path(kind: DocsPageKind, slug: &str) -> DocsPagePath {
        DocsPagePath::Named(NamedPagePath::new(kind, slug))
    }

    fn validated_page(path: DocsPagePath, markdown: &str) -> ValidatedDocsPage {
        ValidatedDocsPage::parse(path, markdown).unwrap()
    }

    fn build_worker_bundle(
        archive: &[u8],
        expected_archive: &ValidatedDocsArchive,
        pages: Vec<WorkerPage>,
    ) -> Result<ValidatedDocsBundle, &'static str> {
        let coordinate = DocsCoordinate::new("carina-rs", "aws", "1.2.3").unwrap();
        build_validated_bundle_from_worker(
            fetched_docs_for_bytes(archive),
            &coordinate,
            expected_archive,
            pages,
        )
        .map(|bundle| {
            let (_, _, rendered) = bundle.into_parts();
            ValidatedDocsBundle {
                version_root: rendered.version_root,
                rendered_pages: rendered.rendered_pages,
            }
        })
    }

    fn render_bundle(archive: &[u8]) -> ValidatedDocsBundle {
        validate_and_render_docs_bundle(
            fetched_docs_for_bytes(archive),
            "carina-rs",
            "aws",
            "1.2.3",
        )
        .unwrap()
    }

    fn page_html<'a>(bundle: &'a ValidatedDocsBundle, suffix: &str) -> &'a str {
        let key = format!(
            "{}{suffix}",
            bundle.version_root.host_path().trim_start_matches('/')
        );
        let page = bundle
            .rendered_pages
            .iter()
            .find(|page| page.key() == key)
            .unwrap_or_else(|| panic!("missing rendered page {suffix:?}"));
        std::str::from_utf8(&page.body).unwrap()
    }

    fn navigation_json(bundle: &ValidatedDocsBundle) -> &str {
        let navigation = bundle
            .rendered_pages
            .iter()
            .find(|page| page.key().ends_with("/nav.json"))
            .expect("missing navigation document");
        std::str::from_utf8(&navigation.body).unwrap()
    }

    fn sidebar_links(html: &str) -> &str {
        html.split_once("<div class=\"sidebar-links\">")
            .expect("sidebar links container")
            .1
            .split_once("</div>\n</nav>")
            .expect("sidebar links closing tag")
            .0
    }

    fn grouped_bundle() -> Vec<u8> {
        bundle_with_files(&[
            (b"resources/ec2_instance.md", b"# aws.ec2.Instance\n"),
            (b"index.md", b"# Index\n"),
            (b"resources/acm_pca.md", b"# aws.acm.Pca\n"),
            (b"data-sources/ec2_ami.md", b"# aws.ec2.Ami\n"),
            (b"resources/acm_certificate.md", b"# aws.acm.Certificate\n"),
        ])
    }

    fn test_grouped_navigation_metadata() -> ProviderNavigationMetadata {
        validate_provider_navigation_metadata(TEST_GROUPED_NAVIGATION_METADATA.as_bytes()).unwrap()
    }

    fn render_grouped_bundle(archive: &[u8]) -> ValidatedDocsBundle {
        let version_root = DocsVersionRoot::new("carina-rs", "aws", "1.2.3");
        let validated = visit_docs_archive(archive, |_, _| Ok(())).unwrap();
        let metadata = test_grouped_navigation_metadata();
        let navigation = derive_docs_navigation(validated.pages(), Some(&metadata)).navigation;
        render_bundle_with_navigation(archive, &validated, version_root, &navigation)
    }

    fn render_bundle_with_navigation(
        archive: &[u8],
        validated: &ValidatedDocsArchive,
        version_root: DocsVersionRoot,
        navigation: &DocsNavigation,
    ) -> ValidatedDocsBundle {
        let mut rendered_pages = render_validated_archive(
            archive,
            validated,
            &version_root,
            navigation,
            MAX_RENDERED_HTML_BYTES,
        )
        .unwrap();
        append_navigation_document(
            &mut rendered_pages,
            navigation,
            &version_root,
            MAX_RENDERED_HTML_BYTES,
        )
        .unwrap();
        ValidatedDocsBundle {
            version_root,
            rendered_pages,
        }
    }

    fn assert_duplicate_type_names_render_flat_and_warn(
        files: &[(&[u8], &[u8])],
        metadata: &[u8],
        expected_section: &str,
        expected_titles: &[&str],
    ) {
        let archive = bundle_with_files(files);
        let validated = visit_docs_archive(&archive, |_, _| Ok(())).unwrap();
        let version_root = DocsVersionRoot::new("example", "duplicate-labels", "1.0.0");
        let logs = Arc::new(Mutex::new(Vec::new()));
        let writer = Arc::clone(&logs);
        let subscriber = tracing_subscriber::fmt()
            .without_time()
            .with_ansi(false)
            .with_writer(move || SharedLogBuffer(Arc::clone(&writer)))
            .finish();
        let subscriber_guard = tracing::subscriber::set_default(subscriber);

        let navigation = docs_navigation_for_metadata_bytes(
            validated.pages(),
            &version_root,
            Some(metadata),
            true,
        );
        drop(subscriber_guard);

        let bundle = render_bundle_with_navigation(&archive, &validated, version_root, &navigation);
        let document: serde_json::Value = serde_json::from_str(navigation_json(&bundle)).unwrap();
        assert!(
            document.get("resources").is_some(),
            "flat navigation must have top-level resources: {document}"
        );
        assert!(
            document.get("services").is_none(),
            "flat navigation must not have top-level services: {document}"
        );

        let fallback = sidebar_links(page_html(&bundle, ""));
        for title in expected_titles {
            assert!(
                fallback.contains(&format!(">{title}</a>")),
                "missing full title {title:?} from {fallback:?}"
            );
        }

        let logs = String::from_utf8(logs.lock().unwrap().clone()).unwrap();
        assert!(
            logs.contains(
                "provider navigation metadata does not cover documentation version; using flat navigation"
            ),
            "missing fallback warning from {logs:?}"
        );
        assert!(
            logs.contains(&format!(
                "duplicate_type_name_sections=[\"{expected_section}\"]"
            )),
            "missing duplicate section from {logs:?}"
        );
    }

    fn assert_invalid_navigation_metadata(case: &str, expected_error: &str, document: &[u8]) {
        let error = validate_provider_navigation_metadata(document)
            .err()
            .unwrap_or_else(|| panic!("invalid provider navigation metadata was accepted: {case}"));
        assert!(
            error.detail().contains(expected_error),
            "invalid provider navigation metadata case {case:?} returned {:?}, expected a message containing {expected_error:?}",
            error.detail()
        );
    }

    fn bundle_with_files(files: &[(&[u8], &[u8])]) -> Vec<u8> {
        let mut tar = TarBuilder::new();
        for (path, body) in files {
            tar.entry(path, b'0', body);
        }
        gzip(&tar.finish())
    }

    fn synthetic_rendered_size(
        navigation_page_count: usize,
        max_rendered_bytes: usize,
    ) -> Result<usize, DocsBundleError> {
        assert!(navigation_page_count > 0);
        let mut tar = TarBuilder::new();
        tar.entry(b"index.md", b'0', b"# Index\n");
        let resource_count = navigation_page_count / 2;
        for index in 0..resource_count {
            tar.entry(
                format!("resources/r{index:04}.md").as_bytes(),
                b'0',
                b"# test.compute.Resource\n",
            );
        }
        for index in resource_count..navigation_page_count {
            tar.entry(
                format!("data-sources/d{index:04}.md").as_bytes(),
                b'0',
                b"# test.compute.DataSource\n",
            );
        }
        let archive = gzip(&tar.finish());
        let validated = visit_docs_archive(&archive, |_, _| Ok(())).unwrap();
        let version_root = DocsVersionRoot::new("carina-rs", "aws", "1.2.3");
        let navigation = derive_docs_navigation(validated.pages(), None).navigation;
        let mut rendered_pages = render_validated_archive(
            &archive,
            &validated,
            &version_root,
            &navigation,
            max_rendered_bytes,
        )?;
        append_navigation_document(
            &mut rendered_pages,
            &navigation,
            &version_root,
            max_rendered_bytes,
        )?;
        Ok(rendered_pages.iter().map(|page| page.body.len()).sum())
    }

    fn validate(archive: &[u8]) -> Result<ValidatedDocsBundle, DocsBundleError> {
        validate_and_render_docs_bundle(
            fetched_docs_for_bytes(archive),
            "carina-rs",
            "aws",
            "1.2.3",
        )
    }

    fn valid_bundle() -> Vec<u8> {
        gzip(&valid_tar())
    }

    fn valid_tar() -> Vec<u8> {
        let mut tar = TarBuilder::new();
        tar.entry(b"./", b'5', b"");
        tar.entry(b"./resources/", b'5', b"");
        tar.entry(b"./data-sources/", b'5', b"");
        tar.entry(b"./index.md", b'0', b"# Provider\n");
        tar.entry(
            b"./PaxHeaders/resource",
            b'x',
            &pax_record("path", "./resources/s3_bucket.md"),
        );
        tar.entry(b"ignored", b'0', b"# aws.s3.Bucket\n");
        tar.entry(b"./data-sources/ami.md", b'0', b"# aws.ec2.Ami\n");
        tar.finish()
    }
}
