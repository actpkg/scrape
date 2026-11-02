//! `scrape` — Servo-rendered page → Markdown, as an ACT component.
//!
//! One tool: `scrape` loads a page — SPA included, scripts run — settles,
//! resolves relative URLs in-place, and returns the DOM as GitHub-Flavored
//! Markdown plus a JSON metadata part.
//!
//! # One engine per instance, one `WebView`, many sequential pages
//!
//! The Servo engine — and its single `WebView` — start once per instance and
//! stay warm across calls; every call navigates that `WebView` to its page
//! (replacing the previous history entry, so the old page's pipeline closes
//! right away) and navigates it back to `about:blank` before returning. The
//! scraped page's script thread thus runs `JS_DestroyContext` on its own
//! thread and nothing accumulates. What this is *not* is concurrency: one
//! page at a time, pages strictly sequential. True multi-session (parallel
//! pages via act:sessions) is future work.

pub mod convert;

#[cfg(target_arch = "wasm32")]
mod engine;

// `#[act_component]` flattens `mod component`'s body to the crate root and
// drops its `use super::…` lines, so everything the tool body names must be
// visible from the root: the prelude and the types hoisted here (anydoc does
// the same). `crate::…` paths inside the body survive the rewrite.
#[cfg(target_arch = "wasm32")]
use crate::convert::Mode;
#[cfg(target_arch = "wasm32")]
use act_sdk::prelude::*;

/// Both parts of one scrape result, in wire order: the page as
/// GitHub-Flavored Markdown, then its metadata as a JSON part.
///
/// A dedicated local type (rather than `Vec<Content>`) lets the tool return a
/// plain immediate result: the SDK's `IntoToolResponse` has no `Vec<Content>`
/// impl on the released 0.14 line, and the tool must not be streaming (see the
/// note on the return value).
#[cfg(target_arch = "wasm32")]
struct ScrapeParts {
    markdown: Vec<u8>,
    meta_json: Vec<u8>,
}

#[cfg(target_arch = "wasm32")]
impl act_sdk::response::IntoToolResponse for ScrapeParts {
    fn into_tool_response(self, _default_language: &str) -> Vec<act_sdk::context::RawToolEvent> {
        vec![
            act_sdk::context::RawToolEvent::Content {
                data: self.markdown,
                mime_type: Some("text/markdown".to_owned()),
                metadata: Vec::new(),
            },
            act_sdk::context::RawToolEvent::Content {
                data: self.meta_json,
                mime_type: Some("application/json".to_owned()),
                metadata: Vec::new(),
            },
        ]
    }
}

#[cfg(target_arch = "wasm32")]
#[act_component]
mod component {
    use crate::engine::{
        Browser, DEFAULT_HEIGHT, DEFAULT_SETTLE_MS, DEFAULT_WIDTH, block_on,
        target_url,
    };
    #[act_tool(
        description = "Render a web page — SPAs included, scripts run — and return GitHub-Flavored Markdown plus page metadata (title, description, lang, url). One page at a time; pages run sequentially on one warm engine within this component instance",
        read_only
    )]
    fn scrape(
        /// HTML to scrape directly. Takes precedence over `url`.
        html: Option<String>,
        /// URL to load. `http:`, `https:` and `data:` all resolve; network
        /// access needs the `wasi:sockets` capability to be granted.
        url: Option<String>,
        /// Viewport width in CSS pixels. Defaults to 1024.
        width: Option<u32>,
        /// Viewport height in CSS pixels. Defaults to 768.
        height: Option<u32>,
        /// How long to keep driving the page after it finishes loading, in
        /// milliseconds. Defaults to 1000. Raise it for SPAs that hydrate late.
        settle_ms: Option<u32>,
        /// `"full"` converts the whole page (default); `"readability"` extracts
        /// the main article first, dropping navigation, ads and footers.
        mode: Option<Mode>,
    ) -> ActResult<ScrapeParts> {
        let width = width.unwrap_or(DEFAULT_WIDTH);
        let height = height.unwrap_or(DEFAULT_HEIGHT);
        let settle_ms = settle_ms.unwrap_or(DEFAULT_SETTLE_MS);
        let mode = mode.unwrap_or(Mode::Full);
        let inline_html = html.is_some();
        let target = target_url(html, url)?;

        // Order is the contract: settle lets hydration finish, fixup runs on
        // the *settled* DOM, then metadata and the DOM are read from the same
        // state the markdown is converted from.
        let (dom, meta_text) = block_on(async move {
            let browser = Browser::open(&target, width, height).await?;
            browser.pump(settle_ms).await;
            browser.absolutize_links().await;
            let meta = browser
                .page_meta()
                .await
                .unwrap_or_else(|_| "{}".to_owned());
            let dom = browser
                .evaluate("document.documentElement.outerHTML")
                .await
                .unwrap_or_default();
            // No explicit navigate-away here: the *next* call's `load_replace`
            // closes this page's pipeline the same way (the constellation's
            // Replace path closes the previous pipeline immediately), and
            // navigating to `about:blank` per call would spawn a fresh
            // about:blank pipeline whose creator lookup races the close —
            // the doomed extra script thread that deadlocked two-call runs.
            // Dropping the `WebView` itself is never an option either — its
            // painter teardown blocks on the render backend and deadlocks
            // the component (that hang is what the old `forget()` papered
            // over). `Servo::drop` is likewise never called.
            drop(browser);
            Ok::<_, String>((dom, meta))
        })
        .map_err(|error| ActError::internal(format!("scrape failed: {error}")))?;

        let inpage: crate::convert::InPageMeta =
            serde_json::from_str(&meta_text).unwrap_or_default();
        let mut meta = inpage.into_meta(mode, inline_html);
        let markdown = crate::convert::to_markdown(&dom, &mut meta);

        // Two parts, each as what it is (servo `render`'s convention).
        // Returned as an immediate result rather than streamed: the parts only
        // exist once the page is fully converted, so a stream adds nothing —
        // and streaming under the coop runtime wedges the export fiber after
        // the first call (the second call is then refused with `do_not_enter`,
        // an upstream wasmtime/wit-bindgen interaction under investigation).
        Ok(ScrapeParts {
            markdown: markdown.into_bytes(),
            meta_json: crate::convert::meta_part_json(&meta),
        })
    }
}
