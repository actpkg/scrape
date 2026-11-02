//! The conversion pipeline: rendered DOM → GitHub-Flavored Markdown + metadata.
//!
//! Pure module — no engine, no wasm-only dependencies — so its unit tests run
//! on the host target (`cargo test --target x86_64-unknown-linux-gnu`).
//!
//! Two modes, one markdown flavor:
//! - `full` converts the whole document, skipping script/style machinery;
//! - `readability` first extracts the main article (Mozilla Readability port)
//!   and converts *the extracted HTML* with the same converter. Extraction
//!   finding nothing is not an error — it degrades to `full`.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// What to convert: the whole page or the extracted article.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(crate = "act_sdk::__private::serde", rename_all = "lowercase")]
#[schemars(crate = "act_sdk::__private::schemars")]
pub enum Mode {
    /// Convert the entire page.
    #[default]
    Full,
    /// Extract the main article first, dropping navigation, ads and footer.
    Readability,
}

/// Page metadata, collected in-page and returned alongside the markdown.
#[derive(Debug, Clone, Default)]
pub struct PageMeta {
    pub title: Option<String>,
    pub description: Option<String>,
    pub lang: Option<String>,
    /// The page's final URL. `None` for inline `html` input (the engine
    /// would report a giant `data:` URL, which is noise, not information).
    pub url: Option<String>,
    pub mode: Mode,
}

/// What the in-page metadata script returns (a `JSON.stringify` string).
///
/// Consumed by the engine layer (Task 3); unused until then.
#[derive(Debug, Default, Deserialize)]
#[serde(crate = "act_sdk::__private::serde", default)]
#[expect(dead_code, reason = "engine layer lands next and drives this")]
pub(crate) struct InPageMeta {
    title: Option<String>,
    description: Option<String>,
    lang: Option<String>,
    url: Option<String>,
}

impl InPageMeta {
    /// Merge into a `PageMeta`, choosing the url per input shape.
    #[expect(dead_code, reason = "engine layer lands next and drives this")]
    pub(crate) fn into_meta(self, mode: Mode, inline_html: bool) -> PageMeta {
        PageMeta {
            title: self.title,
            description: self.description,
            lang: self.lang,
            url: if inline_html { None } else { self.url },
            mode,
        }
    }
}

/// The `application/json` metadata content part.
///
/// Serialized through a field-ordered struct, not a `json!` map: serde_json's
/// `Value` map is a `BTreeMap` (unless `preserve_order` is enabled), so a map
/// would emit keys sorted alphabetically instead of in the documented order.
#[derive(Serialize)]
#[serde(crate = "act_sdk::__private::serde")]
struct MetaPartJson<'a> {
    title: &'a Option<String>,
    description: &'a Option<String>,
    lang: &'a Option<String>,
    url: &'a Option<String>,
    mode: Mode,
}

pub fn meta_part_json(meta: &PageMeta) -> Vec<u8> {
    serde_json::to_vec(&MetaPartJson {
        title: &meta.title,
        description: &meta.description,
        lang: &meta.lang,
        url: &meta.url,
        mode: meta.mode,
    })
    .expect("metadata part always serializes")
}

/// Convert a rendered DOM to markdown, per `meta.mode`.
///
/// Readability mode may sharpen `meta.title` from the extracted article —
/// that is why meta is `&mut`.
pub fn to_markdown(dom_html: &str, meta: &mut PageMeta) -> String {
    match meta.mode {
        Mode::Full => full_markdown(dom_html),
        Mode::Readability => readability_markdown(dom_html, meta),
    }
}

/// Whole-page conversion. The skip list is explicit: the DOM came from a live
/// page, and a converter that silently inlines script text would leak it into
/// the markdown.
fn full_markdown(html: &str) -> String {
    htmd::HtmlToMarkdown::builder()
        .skip_tags(vec!["script", "style", "noscript", "template"])
        .build()
        .convert(html)
        .unwrap_or_default()
}

/// Article extraction, then the same converter. Degradation is deliberate:
/// a page with no detectable article still scrapes — as the full page.
fn readability_markdown(html: &str, meta: &mut PageMeta) -> String {
    let extractor = match readabilityrs::Readability::new(html, meta.url.as_deref(), None) {
        Ok(extractor) => extractor,
        Err(_) => return full_markdown(html),
    };
    let Some(article) = extractor.parse() else {
        return full_markdown(html);
    };
    if let Some(title) = article.title {
        meta.title = Some(title);
    }
    full_markdown(article.content.as_deref().unwrap_or(html))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Readability's default `char_threshold` is 500 characters of body text;
    /// fixtures meant to *win* extraction must clear it.
    const ARTICLE_BODY: &str = "Rust is a systems programming language that runs blazingly fast, prevents segfaults, and guarantees thread safety. It accomplishes memory safety without a garbage collector, and ownership moves all checks to compile time. The component model lets Rust compile to sandboxed wasm modules with capability-based imports. WebAssembly provides a portable binary format with near-native speed. Sandboxing untrusted input is the core promise here.";

    fn full_meta() -> PageMeta {
        PageMeta {
            mode: Mode::Full,
            ..Default::default()
        }
    }

    #[test]
    fn full_mode_converts_structure_and_links() {
        let html = concat!(
            "<html><head><style>body{color:red}</style></head><body>",
            "<h1>Heading</h1>",
            "<script>alert('no')</script>",
            "<p>See <a href=\"https://example.com/a\">the link</a>.</p>",
            "</body></html>"
        );
        let md = to_markdown(html, &mut full_meta());
        assert!(md.contains("# Heading"), "heading converted, got: {md}");
        assert!(
            md.contains("[the link](https://example.com/a)"),
            "link converted, got: {md}"
        );
        assert!(
            !md.contains("alert"),
            "script text must be skipped, got: {md}"
        );
        assert!(
            !md.contains("color:red"),
            "style text must be skipped, got: {md}"
        );
    }

    #[test]
    fn readability_mode_extracts_article_and_drops_furniture() {
        let html = format!(
            concat!(
                "<html><head><title>Article Title</title></head><body>",
                "<nav><a href=\"/1\">Menu One</a> <a href=\"/2\">Menu Two</a> <a href=\"/3\">Menu Three</a></nav>",
                "<article><h1>Real Heading</h1><p>{}</p><p>{}</p></article>",
                "<footer>Copyright Footer Slate</footer>",
                "</body></html>"
            ),
            ARTICLE_BODY, ARTICLE_BODY
        );
        let mut meta = PageMeta {
            mode: Mode::Readability,
            url: None,
            ..Default::default()
        };
        let md = to_markdown(&html, &mut meta);
        assert!(md.contains("Real Heading"), "article kept, got: {md}");
        assert!(md.contains("thread safety"), "article body kept, got: {md}");
        assert!(!md.contains("Menu One"), "navigation dropped, got: {md}");
        assert!(
            !md.contains("Copyright Footer Slate"),
            "footer dropped, got: {md}"
        );
        assert_eq!(
            meta.title.as_deref(),
            Some("Article Title"),
            "title from the page"
        );
    }

    #[test]
    fn readability_falls_back_to_full_when_no_article() {
        // A page with no extractable article: short, no dominant text block.
        let html = "<html><body><div>Just a stub line</div></body></html>";
        let mut meta = PageMeta {
            mode: Mode::Readability,
            ..Default::default()
        };
        let md = to_markdown(html, &mut meta);
        assert!(
            md.contains("Just a stub line"),
            "degrades to full conversion, got: {md}"
        );
    }

    #[test]
    fn empty_body_page_yields_empty_markdown_without_error() {
        let html = "<html><head></head><body></body></html>";
        let md = to_markdown(html, &mut full_meta());
        assert!(
            md.trim().is_empty(),
            "blank page scrapes to blank markdown, got: {md:?}"
        );
    }

    #[test]
    fn large_document_converts() {
        // ~1 MB of rendered DOM: conversion must complete, not panic.
        let para = format!("<p>{}</p>", ARTICLE_BODY);
        let html = format!("<html><body>{}</body></html>", para.repeat(2100));
        assert!(
            html.len() > 900_000,
            "fixture is a large document, got {}",
            html.len()
        );
        let md = to_markdown(&html, &mut full_meta());
        assert!(
            md.contains("thread safety"),
            "large conversion completes, got {} bytes",
            md.len()
        );
    }

    #[test]
    fn mode_wire_values_are_lowercase() {
        assert_eq!(serde_json::to_string(&Mode::Full).unwrap(), "\"full\"");
        assert_eq!(
            serde_json::to_string(&Mode::Readability).unwrap(),
            "\"readability\""
        );
    }

    #[test]
    fn meta_part_has_exact_shape() {
        let meta = PageMeta {
            title: Some("T".into()),
            description: None,
            lang: Some("en".into()),
            url: Some("https://example.com/".into()),
            mode: Mode::Readability,
        };
        let bytes = meta_part_json(&meta);
        // Byte-level order check: serde_json's `Value` map is a BTreeMap, so a
        // parse-then-iterate assertion would silently re-sort and see nothing.
        assert_eq!(
            std::str::from_utf8(&bytes).unwrap(),
            r#"{"title":"T","description":null,"lang":"en","url":"https://example.com/","mode":"readability"}"#
        );
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["mode"], "readability");
        assert_eq!(v["description"], serde_json::Value::Null);
    }

    #[test]
    fn inline_html_input_nulls_the_url() {
        let inpage: InPageMeta =
            serde_json::from_str(r#"{"title":"t","url":"data:text/html;base64,AAAA"}"#).unwrap();
        let meta = inpage.into_meta(Mode::Full, true);
        assert!(
            meta.url.is_none(),
            "data: url suppressed for inline html, got {:?}",
            meta.url
        );
        let inpage: InPageMeta =
            serde_json::from_str(r#"{"url":"https://example.com/x"}"#).unwrap();
        let meta = inpage.into_meta(Mode::Full, false);
        assert_eq!(meta.url.as_deref(), Some("https://example.com/x"));
    }
}
