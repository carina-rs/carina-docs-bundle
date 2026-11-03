//! Test fixtures and fingerprint observations. These values confer no validation evidence.

use std::io::Write as _;
use std::sync::LazyLock;

use flate2::write::GzEncoder;
use flate2::Compression;

use super::{DocsBundleError, RenderContentTypes};

pub const RESOURCE_LIMIT_FAILURE_DETAIL: &str = super::RESOURCE_LIMIT_FAILURE_DETAIL;
pub const CRN_GRAMMAR: &str = super::CRN_GRAMMAR;
pub const CARINA_DARK_THEME: &str = super::CARINA_DARK_THEME;
pub const SYNTECT_CRATE_VERSION: &str = super::SYNTECT_CRATE_VERSION;

pub struct EmbeddedProviderNavigationMetadata {
    pub namespace: &'static str,
    pub name: &'static str,
    pub bytes: &'static [u8],
}

pub static EMBEDDED_PROVIDER_NAVIGATION_METADATA: LazyLock<
    Vec<EmbeddedProviderNavigationMetadata>,
> = LazyLock::new(|| {
    super::EMBEDDED_PROVIDER_NAVIGATION_METADATA
        .iter()
        .map(|metadata| EmbeddedProviderNavigationMetadata {
            namespace: metadata.namespace,
            name: metadata.name,
            bytes: metadata.bytes,
        })
        .collect()
});

/// A mutable observation for fingerprint tests, without a validated-object constructor.
#[derive(Debug, Clone)]
pub struct FingerprintInput {
    pub key: String,
    pub content_type: String,
    pub body: Vec<u8>,
}

impl FingerprintInput {
    pub fn key(&self) -> String {
        self.key.clone()
    }
}

impl FingerprintInput {
    fn from_private(input: super::FingerprintInput) -> Self {
        Self {
            key: input.key,
            content_type: input.content_type,
            body: input.body,
        }
    }
}

fn private_inputs(inputs: &[FingerprintInput]) -> Vec<super::FingerprintInput> {
    inputs
        .iter()
        .map(|input| super::FingerprintInput {
            key: input.key.clone(),
            content_type: input.content_type.clone(),
            body: input.body.clone(),
        })
        .collect()
}

pub fn render_reference_docs_bundle(
    content_types: RenderContentTypes,
) -> Result<Vec<FingerprintInput>, DocsBundleError> {
    super::render_reference_docs_bundle().map(|pages| {
        pages
            .into_iter()
            .map(|page| {
                let (key, body, kind) = page.into_parts();
                FingerprintInput {
                    key,
                    content_type: content_types.for_kind(kind).to_string(),
                    body,
                }
            })
            .collect()
    })
}

pub fn render_reference_fingerprint_inputs(
    index_outputs: impl IntoIterator<Item = (String, String, Vec<u8>)>,
    content_types: RenderContentTypes,
) -> Result<Vec<FingerprintInput>, DocsBundleError> {
    super::render_reference_fingerprint_inputs(index_outputs, content_types).map(|inputs| {
        inputs
            .into_iter()
            .map(FingerprintInput::from_private)
            .collect()
    })
}

pub fn fingerprint_with_highlighting_inputs(
    inputs: &[FingerprintInput],
    grammar: &[u8],
    theme: &[u8],
    identity: &[u8],
    json_content_type: &str,
) -> String {
    super::fingerprint_with_highlighting_inputs(
        &private_inputs(inputs),
        grammar,
        theme,
        identity,
        json_content_type,
    )
}

pub fn fingerprint_with_navigation_metadata_digest(
    inputs: &[FingerprintInput],
    grammar: &[u8],
    theme: &[u8],
    identity: &[u8],
    digest: &[u8],
    json_content_type: &str,
) -> String {
    super::fingerprint_with_navigation_metadata_digest(
        &private_inputs(inputs),
        grammar,
        theme,
        identity,
        digest,
        json_content_type,
    )
}

pub fn provider_navigation_metadata_digest<'a>(
    metadata: impl IntoIterator<Item = (&'a str, &'a str, &'a [u8])>,
) -> [u8; 32] {
    super::provider_navigation_metadata_digest(metadata)
}

pub fn fingerprint_outputs<K: AsRef<str>, C: AsRef<str>, B: AsRef<[u8]>>(
    outputs: impl IntoIterator<Item = (K, C, B)>,
) -> String {
    super::fingerprint_outputs(outputs)
}

pub fn valid_docs_bundle_bytes() -> Vec<u8> {
    docs_bundle_bytes(b"# Provider\n")
}

pub fn docs_bundle_bytes(index: &[u8]) -> Vec<u8> {
    let mut tar = TestTarBuilder::new();
    tar.entry(b"./resources/", b'5', b"");
    tar.entry(b"./data-sources/", b'5', b"");
    tar.entry(b"./index.md", b'0', index);
    tar.entry(b"./resources/s3_bucket.md", b'0', b"# aws.s3.Bucket\n");
    gzip_test_bytes(&tar.finish())
}

const TEST_TAR_BLOCK_BYTES: usize = 512;

#[derive(Default)]
pub struct TestTarBuilder {
    pub bytes: Vec<u8>,
}

impl TestTarBuilder {
    pub fn new() -> Self {
        Self { bytes: Vec::new() }
    }

    pub fn entry(&mut self, path: &[u8], kind: u8, body: &[u8]) {
        self.entry_with_declared_size(path, kind, body, body.len() as u64);
    }

    pub fn entry_with_declared_size(
        &mut self,
        path: &[u8],
        kind: u8,
        body: &[u8],
        declared_size: u64,
    ) {
        self.entry_full(path, kind, body, declared_size, b"ustar\0", b"00");
    }

    pub fn entry_with_magic(
        &mut self,
        path: &[u8],
        kind: u8,
        body: &[u8],
        magic: &[u8; 6],
        version: &[u8; 2],
    ) {
        self.entry_full(path, kind, body, body.len() as u64, magic, version);
    }

    fn entry_full(
        &mut self,
        path: &[u8],
        kind: u8,
        body: &[u8],
        declared_size: u64,
        magic: &[u8; 6],
        version: &[u8; 2],
    ) {
        assert!(path.len() <= 100);
        let mut header = [0_u8; TEST_TAR_BLOCK_BYTES];
        header[..path.len()].copy_from_slice(path);
        write_test_tar_octal(&mut header[100..108], 0o644);
        write_test_tar_octal(&mut header[108..116], 0);
        write_test_tar_octal(&mut header[116..124], 0);
        write_test_tar_octal(&mut header[124..136], declared_size);
        write_test_tar_octal(&mut header[136..148], 0);
        header[148..156].fill(b' ');
        header[156] = kind;
        header[257..263].copy_from_slice(magic);
        header[263..265].copy_from_slice(version);
        let checksum = header.iter().map(|byte| u64::from(*byte)).sum();
        write_test_tar_checksum(&mut header[148..156], checksum);
        self.bytes.extend(header);
        self.bytes.extend(body);
        let padding =
            (TEST_TAR_BLOCK_BYTES - body.len() % TEST_TAR_BLOCK_BYTES) % TEST_TAR_BLOCK_BYTES;
        self.bytes.resize(self.bytes.len() + padding, 0);
    }

    pub fn finish(mut self) -> Vec<u8> {
        self.bytes.extend([0_u8; TEST_TAR_BLOCK_BYTES * 2]);
        self.bytes
    }
}

pub fn gzip_test_bytes(bytes: &[u8]) -> Vec<u8> {
    let mut encoder = GzEncoder::new(Vec::new(), Compression::fast());
    encoder.write_all(bytes).unwrap();
    encoder.finish().unwrap()
}

pub fn test_pax_record(key: &str, value: &str) -> Vec<u8> {
    let suffix = format!(" {key}={value}\n");
    let mut length = suffix.len() + 1;
    loop {
        let next = suffix.len() + length.to_string().len();
        if next == length {
            return format!("{length}{suffix}").into_bytes();
        }
        length = next;
    }
}

pub fn refresh_test_tar_first_header_checksum(tar: &mut [u8]) {
    tar[148..156].fill(b' ');
    let checksum = tar[..TEST_TAR_BLOCK_BYTES]
        .iter()
        .map(|byte| u64::from(*byte))
        .sum();
    write_test_tar_checksum(&mut tar[148..156], checksum);
}

fn write_test_tar_octal(field: &mut [u8], value: u64) {
    field.fill(0);
    let encoded = format!("{:0width$o}", value, width = field.len() - 1);
    field[..encoded.len()].copy_from_slice(encoded.as_bytes());
}

fn write_test_tar_checksum(field: &mut [u8], value: u64) {
    let encoded = format!("{value:06o}");
    field[..6].copy_from_slice(encoded.as_bytes());
    field[6] = 0;
    field[7] = b' ';
}
