use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fmt::{self, Write as _};
use std::sync::OnceLock;

use comrak::adapters::SyntaxHighlighterAdapter;
use serde::Deserialize;
use syntect::html::{ClassStyle, ClassedHTMLGenerator};
use syntect::parsing::{SyntaxDefinition, SyntaxReference, SyntaxSet, SyntaxSetBuilder};
use syntect::util::LinesWithEndings;

pub(crate) const CRN_GRAMMAR: &str = include_str!("highlight/crn.tmLanguage.json");
pub(crate) const CARINA_DARK_THEME: &str = include_str!("highlight/carina-dark.theme.json");
pub(crate) const SYNTECT_CRATE_VERSION: &str = "5.3.0";
const CLASS_STYLE: ClassStyle = ClassStyle::SpacedPrefixed { prefix: "hl-" };
const BASH_SYNTAX_NAME: &str = "Bourne Again Shell (bash)";
const JSON_SYNTAX_NAME: &str = "JSON";

static HIGHLIGHTING: OnceLock<Result<Highlighting, HighlightError>> = OnceLock::new();
static THEME_CSS: OnceLock<Result<String, HighlightError>> = OnceLock::new();

#[derive(Clone, Debug)]
pub struct HighlightError {
    detail: String,
}

impl HighlightError {
    fn new(detail: impl Into<String>) -> Self {
        Self {
            detail: detail.into(),
        }
    }
}

impl fmt::Display for HighlightError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.detail)
    }
}

impl std::error::Error for HighlightError {}

pub struct HighlightedCode {
    language: CodeLanguage,
    html: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CodeLanguage {
    Crn,
}

pub(crate) struct Highlighting {
    crn_syntax_set: SyntaxSet,
    default_syntax_set: SyntaxSet,
}

impl CodeLanguage {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Crn => "crn",
        }
    }
}

impl HighlightedCode {
    pub fn new(language: CodeLanguage, code: &str) -> Self {
        let html = match highlighting() {
            Ok(highlighting) => highlight_html(highlighting, Some(language.as_str()), code),
            Err(error) => {
                warn_highlight_failure(language.as_str(), &error);
                escaped_code(code)
            }
        };
        Self { language, html }
    }

    pub fn language(&self) -> CodeLanguage {
        self.language
    }

    pub fn html(&self) -> &str {
        &self.html
    }
}

fn highlight_html(highlighting: &Highlighting, language: Option<&str>, code: &str) -> String {
    highlight_html_with(highlighting, language, code, |language, code| {
        highlighting.highlight_code(language, code)
    })
}

fn highlight_html_with<F>(
    highlighting: &Highlighting,
    language: Option<&str>,
    code: &str,
    highlight: F,
) -> String
where
    F: FnOnce(&str, &str) -> Result<String, HighlightError>,
{
    let Some(language) = language else {
        return escaped_code(code);
    };
    if highlighting.syntax_for_language(language).is_none() {
        return escaped_code(code);
    }
    match highlight(language, code) {
        Ok(highlighted) => highlighted,
        Err(error) => {
            warn_highlight_failure(language, &error);
            escaped_code(code)
        }
    }
}

pub(crate) fn highlighting() -> Result<&'static Highlighting, HighlightError> {
    match HIGHLIGHTING.get_or_init(build_highlighting) {
        Ok(highlighting) => Ok(highlighting),
        Err(error) => Err(error.clone()),
    }
}

pub fn theme_css() -> Result<&'static str, HighlightError> {
    // 親processのCSS生成はthemeだけを読む。fingerprint生成は別途SyntaxSetを構築する。
    match THEME_CSS.get_or_init(|| convert_theme_to_css(CARINA_DARK_THEME)) {
        Ok(css) => Ok(css.as_str()),
        Err(error) => Err(error.clone()),
    }
}

fn build_highlighting() -> Result<Highlighting, HighlightError> {
    let crn = load_crn_syntax(CRN_GRAMMAR)?;
    let mut builder = SyntaxSetBuilder::new();
    builder.add(crn);
    let crn_syntax_set = builder.build();

    // Syntectのdefault dumpはlink済みの単一setで、BashとJSONだけを安全に
    // 非link化する公開APIがない。dumpは再linkせず読み、選択時に2syntaxだけを使う。
    let default_syntax_set = SyntaxSet::load_defaults_newlines();
    for (name, syntax_set) in [
        ("crn", &crn_syntax_set),
        (BASH_SYNTAX_NAME, &default_syntax_set),
        (JSON_SYNTAX_NAME, &default_syntax_set),
    ] {
        if syntax_set.find_syntax_by_name(name).is_none() {
            return Err(HighlightError::new(format!(
                "required syntax {name:?} is missing"
            )));
        }
    }

    Ok(Highlighting {
        crn_syntax_set,
        default_syntax_set,
    })
}

#[cfg(test)]
fn highlight_code(language: &str, code: &str) -> Result<Option<String>, HighlightError> {
    let highlighting = highlighting()?;
    if highlighting.syntax_for_language(language).is_none() {
        return Ok(None);
    }
    highlighting.highlight_code(language, code).map(Some)
}

impl Highlighting {
    fn syntax_for_language(&self, language: &str) -> Option<(&SyntaxReference, &SyntaxSet)> {
        let (syntax_name, syntax_set) = if ["crn", "carina"]
            .iter()
            .any(|alias| language.eq_ignore_ascii_case(alias))
        {
            ("crn", &self.crn_syntax_set)
        } else if ["sh", "shell", "bash"]
            .iter()
            .any(|alias| language.eq_ignore_ascii_case(alias))
        {
            (BASH_SYNTAX_NAME, &self.default_syntax_set)
        } else if language.eq_ignore_ascii_case("json") {
            (JSON_SYNTAX_NAME, &self.default_syntax_set)
        } else {
            return None;
        };
        syntax_set
            .find_syntax_by_name(syntax_name)
            .map(|syntax| (syntax, syntax_set))
    }

    fn highlight_code(&self, language: &str, code: &str) -> Result<String, HighlightError> {
        let (syntax, syntax_set) = self
            .syntax_for_language(language)
            .ok_or_else(|| HighlightError::new("unrecognized fence language"))?;
        let mut generator =
            ClassedHTMLGenerator::new_with_class_style(syntax, syntax_set, CLASS_STYLE);
        for line in LinesWithEndings::from(code) {
            generator
                .parse_html_for_line_which_includes_newline(line)
                .map_err(|error| {
                    HighlightError::new(format!(
                        "syntax highlighting failed for {language:?}: {error}"
                    ))
                })?;
        }
        Ok(generator.finalize())
    }
}

fn escaped_code(code: &str) -> String {
    let mut escaped = String::new();
    comrak::html::escape(&mut escaped, code)
        .expect("writing escaped code to a String is infallible");
    escaped
}

fn warn_highlight_failure(language: &str, error: &HighlightError) {
    tracing::warn!(
        language,
        error = %error,
        "syntax highlighting failed; rendering the code block as plain text"
    );
}

impl SyntaxHighlighterAdapter for Highlighting {
    fn write_highlighted(
        &self,
        output: &mut dyn fmt::Write,
        language: Option<&str>,
        code: &str,
    ) -> fmt::Result {
        output.write_str(&highlight_html(self, language, code))
    }

    fn write_pre_tag(
        &self,
        output: &mut dyn fmt::Write,
        attributes: HashMap<&'static str, Cow<'_, str>>,
    ) -> fmt::Result {
        comrak::html::write_opening_tag(output, "pre", attributes)
    }

    fn write_code_tag(
        &self,
        output: &mut dyn fmt::Write,
        attributes: HashMap<&'static str, Cow<'_, str>>,
    ) -> fmt::Result {
        comrak::html::write_opening_tag(output, "code", attributes)
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TextMateGrammar {
    #[serde(rename = "$schema")]
    _schema: String,
    name: String,
    #[serde(rename = "scopeName")]
    scope_name: String,
    #[serde(rename = "displayName")]
    _display_name: String,
    #[serde(rename = "aliases")]
    _aliases: Vec<String>,
    #[serde(rename = "fileTypes")]
    file_types: Vec<String>,
    patterns: Vec<TextMatePattern>,
    repository: BTreeMap<String, TextMateRepositoryEntry>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TextMateRepositoryEntry {
    patterns: Vec<TextMatePattern>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TextMatePattern {
    include: Option<String>,
    name: Option<String>,
    #[serde(rename = "match")]
    match_pattern: Option<String>,
    begin: Option<String>,
    end: Option<String>,
    patterns: Option<Vec<TextMatePattern>>,
}

struct GeneratedContext {
    name: String,
    scope: String,
    end: String,
    patterns: String,
}

fn load_crn_syntax(json: &str) -> Result<SyntaxDefinition, HighlightError> {
    let grammar: TextMateGrammar = serde_json::from_str(json).map_err(|error| {
        HighlightError::new(format!(
            "vendored crn TextMate grammar is unsupported: {error}"
        ))
    })?;
    let repository_names = grammar
        .repository
        .keys()
        .map(String::as_str)
        .collect::<HashSet<_>>();
    if repository_names.contains("main")
        || repository_names.contains("prototype")
        || repository_names
            .iter()
            .any(|name| name.starts_with("__tm_begin_"))
    {
        return Err(HighlightError::new(
            "vendored crn TextMate grammar uses a reserved repository name",
        ));
    }

    let mut generated = Vec::new();
    let mut next_context_index = 0_usize;
    let main = convert_patterns(
        &grammar.patterns,
        &repository_names,
        &mut generated,
        &mut next_context_index,
    )?;
    let mut repositories = Vec::with_capacity(grammar.repository.len());
    for (name, entry) in &grammar.repository {
        if name.is_empty()
            || !name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
        {
            return Err(HighlightError::new(format!(
                "unsupported TextMate repository name {name:?}"
            )));
        }
        repositories.push((
            name,
            convert_patterns(
                &entry.patterns,
                &repository_names,
                &mut generated,
                &mut next_context_index,
            )?,
        ));
    }

    let mut yaml = String::from("%YAML 1.2\n---\n");
    writeln!(yaml, "name: {}", yaml_string(&grammar.name)).expect("String writes are infallible");
    write!(yaml, "file_extensions: [").expect("String writes are infallible");
    for (index, extension) in grammar.file_types.iter().enumerate() {
        if index > 0 {
            yaml.push_str(", ");
        }
        yaml.push_str(&yaml_string(extension));
    }
    writeln!(
        yaml,
        "]\nscope: {}\ncontexts:",
        yaml_string(&grammar.scope_name)
    )
    .expect("String writes are infallible");
    yaml.push_str("  main:\n");
    yaml.push_str(&main);
    for (name, patterns) in repositories {
        writeln!(yaml, "  {}:", yaml_string(name)).expect("String writes are infallible");
        if patterns.is_empty() {
            yaml.push_str("    []\n");
        } else {
            yaml.push_str(&patterns);
        }
    }
    for context in generated {
        writeln!(yaml, "  {}:", yaml_string(&context.name)).expect("String writes are infallible");
        writeln!(yaml, "    - meta_scope: {}", yaml_string(&context.scope))
            .expect("String writes are infallible");
        writeln!(yaml, "    - match: {}", yaml_string(&context.end))
            .expect("String writes are infallible");
        yaml.push_str("      pop: true\n");
        yaml.push_str(&context.patterns);
    }

    SyntaxDefinition::load_from_str(&yaml, true, Some("crn")).map_err(|error| {
        HighlightError::new(format!(
            "converted crn syntax definition cannot be loaded: {error}"
        ))
    })
}

fn convert_patterns(
    patterns: &[TextMatePattern],
    repository_names: &HashSet<&str>,
    generated: &mut Vec<GeneratedContext>,
    next_context_index: &mut usize,
) -> Result<String, HighlightError> {
    let mut yaml = String::new();
    for pattern in patterns {
        match (
            pattern.include.as_deref(),
            pattern.match_pattern.as_deref(),
            pattern.begin.as_deref(),
            pattern.end.as_deref(),
            pattern.name.as_deref(),
            pattern.patterns.as_deref(),
        ) {
            (Some(include), None, None, None, None, None) => {
                let Some(name) = include.strip_prefix('#') else {
                    return Err(HighlightError::new(format!(
                        "unsupported TextMate include {include:?}"
                    )));
                };
                if !repository_names.contains(name) {
                    return Err(HighlightError::new(format!(
                        "TextMate include {include:?} has no repository entry"
                    )));
                }
                writeln!(yaml, "    - include: {}", yaml_string(name))
                    .expect("String writes are infallible");
            }
            (None, Some(regex), None, None, Some(scope), None) => {
                writeln!(yaml, "    - match: {}", yaml_string(regex))
                    .expect("String writes are infallible");
                writeln!(yaml, "      scope: {}", yaml_string(scope))
                    .expect("String writes are infallible");
            }
            (None, None, Some(begin), Some(end), Some(scope), Some(children)) => {
                let context_name = format!("__tm_begin_{next_context_index}");
                *next_context_index = next_context_index
                    .checked_add(1)
                    .ok_or_else(|| HighlightError::new("TextMate context count overflow"))?;
                let child_patterns =
                    convert_patterns(children, repository_names, generated, next_context_index)?;
                generated.push(GeneratedContext {
                    name: context_name.clone(),
                    scope: scope.to_string(),
                    end: end.to_string(),
                    patterns: child_patterns,
                });
                writeln!(yaml, "    - match: {}", yaml_string(begin))
                    .expect("String writes are infallible");
                writeln!(yaml, "      scope: {}", yaml_string(scope))
                    .expect("String writes are infallible");
                writeln!(yaml, "      push: {}", yaml_string(&context_name))
                    .expect("String writes are infallible");
            }
            _ => {
                return Err(HighlightError::new(
                    "vendored crn TextMate grammar contains an unsupported pattern construct",
                ));
            }
        }
    }
    Ok(yaml)
}

fn yaml_string(value: &str) -> String {
    serde_json::to_string(value).expect("serializing a string is infallible")
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct VsCodeTheme {
    #[serde(rename = "name")]
    _name: String,
    #[serde(rename = "type")]
    _kind: String,
    colors: BTreeMap<String, String>,
    #[serde(rename = "tokenColors")]
    token_colors: Vec<TokenColor>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TokenColor {
    scope: ScopeList,
    settings: TokenSettings,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum ScopeList {
    One(String),
    Many(Vec<String>),
}

impl ScopeList {
    fn iter(&self) -> Box<dyn Iterator<Item = &str> + '_> {
        match self {
            Self::One(scope) => Box::new(std::iter::once(scope.as_str())),
            Self::Many(scopes) => Box::new(scopes.iter().map(String::as_str)),
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TokenSettings {
    foreground: Option<String>,
    #[serde(rename = "fontStyle")]
    font_style: Option<String>,
}

struct CssRule {
    selector: String,
    specificity: usize,
    declaration: String,
}

fn convert_theme_to_css(json: &str) -> Result<String, HighlightError> {
    let theme: VsCodeTheme = serde_json::from_str(json).map_err(|error| {
        HighlightError::new(format!(
            "vendored carina-dark theme is unsupported: {error}"
        ))
    })?;
    let foreground = theme
        .colors
        .get("editor.foreground")
        .ok_or_else(|| HighlightError::new("carina-dark theme lacks editor.foreground"))?;
    let background = theme
        .colors
        .get("editor.background")
        .ok_or_else(|| HighlightError::new("carina-dark theme lacks editor.background"))?;
    let foreground = normalized_color(foreground)?;
    let background = normalized_color(background)?;

    let mut rules = Vec::new();
    for token_color in theme.token_colors {
        let declaration = css_declaration(&token_color.settings)?;
        if declaration.is_empty() {
            return Err(HighlightError::new(
                "carina-dark token color has no supported settings",
            ));
        }
        for scope in token_color.scope.iter() {
            let (selector, specificity) = scope_selector(scope)?;
            rules.push(CssRule {
                selector,
                specificity,
                declaration: declaration.clone(),
            });
        }
    }
    rules.sort_by_key(|rule| rule.specificity);

    let mut css = format!(
        concat!(
            "/* Generated at startup from the vendored carina-dark VS Code theme. */\n",
            ".docs-content pre,\n",
            ".landing pre {{\n",
            "  color: {};\n",
            "  background-color: {};\n",
            "}}\n"
        ),
        foreground, background,
    );
    for rule in rules {
        writeln!(css, "{} {{", rule.selector).expect("String writes are infallible");
        css.push_str(&rule.declaration);
        css.push_str("}\n");
    }
    Ok(css)
}

fn normalized_color(color: &str) -> Result<String, HighlightError> {
    let Some(hex) = color.strip_prefix('#') else {
        return Err(HighlightError::new(format!(
            "unsupported theme color {color:?}"
        )));
    };
    if hex.len() != 6 || !hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(HighlightError::new(format!(
            "unsupported theme color {color:?}"
        )));
    }
    Ok(format!("#{}", hex.to_ascii_lowercase()))
}

fn css_declaration(settings: &TokenSettings) -> Result<String, HighlightError> {
    let mut declaration = String::new();
    if let Some(foreground) = &settings.foreground {
        writeln!(declaration, "  color: {};", normalized_color(foreground)?)
            .expect("String writes are infallible");
    }
    if let Some(font_style) = &settings.font_style {
        let styles = font_style.split_ascii_whitespace().collect::<BTreeSet<_>>();
        for style in &styles {
            if !matches!(*style, "bold" | "italic" | "underline" | "strikethrough") {
                return Err(HighlightError::new(format!(
                    "unsupported theme fontStyle {style:?}"
                )));
            }
        }
        if styles.contains("bold") {
            declaration.push_str("  font-weight: bold;\n");
        }
        if styles.contains("italic") {
            declaration.push_str("  font-style: italic;\n");
        }
        let mut decorations = Vec::new();
        if styles.contains("underline") {
            decorations.push("underline");
        }
        if styles.contains("strikethrough") {
            decorations.push("line-through");
        }
        if !decorations.is_empty() {
            writeln!(declaration, "  text-decoration: {};", decorations.join(" "))
                .expect("String writes are infallible");
        }
    }
    Ok(declaration)
}

fn scope_selector(scope: &str) -> Result<(String, usize), HighlightError> {
    let atoms = scope.split('.').collect::<Vec<_>>();
    if atoms.is_empty()
        || atoms.iter().any(|atom| {
            atom.is_empty()
                || !atom
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        })
    {
        return Err(HighlightError::new(format!(
            "unsupported TextMate theme scope {scope:?}"
        )));
    }
    let selector = atoms
        .iter()
        .map(|atom| format!(".hl-{atom}"))
        .collect::<String>();
    Ok((selector, atoms.len()))
}

#[cfg(test)]
mod tests {
    use super::{
        highlight_code, highlight_html, highlight_html_with, highlighting, load_crn_syntax,
        theme_css, HighlightError, CRN_GRAMMAR, SYNTECT_CRATE_VERSION,
    };

    #[test]
    fn vendored_crn_grammar_loads_and_highlights_its_token_kinds() {
        let highlighting = highlighting().unwrap();
        assert!(highlighting
            .crn_syntax_set
            .find_syntax_by_name("crn")
            .is_some());

        let html = highlight_code(
            "crn",
            "let name = \"carina\"\n# comment\nif count == 42 {}\n",
        )
        .unwrap()
        .expect("crn is a recognized fence language");

        for class in [
            "hl-keyword hl-declaration hl-crn",
            "hl-string hl-quoted hl-double hl-crn",
            "hl-comment hl-line hl-hash hl-crn",
            "hl-constant hl-numeric hl-crn",
        ] {
            assert!(html.contains(class), "missing {class:?} from {html}");
        }
    }

    #[test]
    fn json_and_shell_are_highlighted_while_unknown_languages_stay_plain() {
        let json = highlight_code("JSON", "{\"enabled\": true}\n")
            .unwrap()
            .expect("fence languages are case-insensitive");
        let shell = highlight_code("ShElL", "echo \"hello\"\n")
            .unwrap()
            .expect("fence languages are case-insensitive");

        assert!(
            json.contains("class=\"hl-"),
            "json was not highlighted: {json}"
        );
        assert!(
            shell.contains("class=\"hl-"),
            "shell was not highlighted: {shell}"
        );
        assert_eq!(highlight_code("unknown", "a < b\n").unwrap(), None);
        assert_eq!(highlight_code("console", "$ echo hello\n").unwrap(), None);
    }

    #[test]
    fn crn_is_built_without_relinking_the_default_syntax_dump() {
        let highlighting = highlighting().unwrap();
        let crn_syntax_names = highlighting
            .crn_syntax_set
            .syntaxes()
            .iter()
            .map(|syntax| syntax.name.as_str())
            .collect::<Vec<_>>();

        assert_eq!(crn_syntax_names, ["crn"]);
        assert!(highlighting
            .default_syntax_set
            .find_syntax_by_name("crn")
            .is_none());
    }

    #[test]
    fn crn_converter_rejects_the_reserved_prototype_repository() {
        let mut grammar = serde_json::from_str::<serde_json::Value>(CRN_GRAMMAR).unwrap();
        grammar["repository"]["prototype"] = serde_json::json!({ "patterns": [] });

        let error = load_crn_syntax(&serde_json::to_string(&grammar).unwrap()).unwrap_err();

        assert_eq!(
            error.to_string(),
            "vendored crn TextMate grammar uses a reserved repository name"
        );
    }

    #[test]
    fn highlighted_html_has_no_inline_style_and_escapes_code_text() {
        let html = highlight_code("bash", "echo '<unsafe>'\n")
            .unwrap()
            .expect("bash is recognized");

        assert!(!html.contains("style="));
        assert!(html.contains("&lt;unsafe&gt;"), "unescaped output: {html}");
        assert!(!html.contains("<unsafe>"));
    }

    #[test]
    fn shared_highlighter_escapes_unknown_and_missing_languages_as_plain_text() {
        let highlighting = highlighting().unwrap();
        for language in [Some("unknown"), None] {
            let html = highlight_html(highlighting, language, "a < b & c\n");

            assert_eq!(html, "a &lt; b &amp; c\n");
            assert!(!html.contains("class=\"hl-"));
        }
    }

    #[test]
    fn injected_highlighter_failure_falls_back_to_plain_escaped_text() {
        let highlighting = highlighting().unwrap();
        let html = highlight_html_with(highlighting, Some("json"), "a < b & c\n", |_, _| {
            Err(HighlightError::new("injected highlighter failure"))
        });

        assert_eq!(html, "a &lt; b &amp; c\n");
        assert!(!html.contains("class=\"hl-"));
    }

    #[test]
    fn syntect_fingerprint_identity_matches_cargo_lock() {
        let syntect_package = include_str!("../Cargo.lock")
            .split("[[package]]")
            .find(|package| package.lines().any(|line| line == "name = \"syntect\""))
            .expect("Cargo.lock must contain syntect");
        let locked_version = syntect_package
            .lines()
            .find_map(|line| line.strip_prefix("version = \"")?.strip_suffix('"'))
            .expect("the syntect package must have a version");

        assert_eq!(SYNTECT_CRATE_VERSION, locked_version);
    }

    #[test]
    fn vendored_theme_generates_specific_class_rules_and_base_colors() {
        let css = theme_css().unwrap();

        assert!(css.contains(concat!(
            ".docs-content pre,\n",
            ".landing pre {\n",
            "  color: #f1f5f9;\n",
            "  background-color: #1e293b;\n",
            "}"
        )));
        assert!(css.contains(".hl-keyword {\n  color: #fbbf24;\n}"));
        assert!(css.contains(".hl-string {\n  color: #86efac;\n}"));
        assert!(css.contains(concat!(
            ".hl-string.hl-quoted.hl-double.hl-crn {\n",
            "  color: #86efac;\n",
            "}"
        )));
        assert!(
            css.find(".hl-string {").unwrap()
                < css.find(".hl-string.hl-quoted.hl-double.hl-crn {").unwrap(),
            "less-specific scope rules must precede more-specific rules"
        );
    }
}
