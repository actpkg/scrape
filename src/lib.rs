//! `scrape` — Servo-rendered page → Markdown, as an ACT component.
//!
//! One tool: `scrape` loads a page — SPA included, scripts run — settles,
//! resolves relative URLs in-place, and returns the DOM as GitHub-Flavored
//! Markdown plus a JSON metadata part.
//!
//! # One document per instance
//!
//! Servo gives every document a script thread, every script thread creates a
//! SpiderMonkey `JSContext`, SpiderMonkey allows one per thread, and a wasip2
//! guest has one thread. A second `scrape` in the same instance is refused —
//! start another instance for another page. This is why the component has no
//! crawl surface: one call, one page, one instance.

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

#[cfg(target_arch = "wasm32")]
#[act_component]
mod component {
    use crate::engine::{
        Browser, DEFAULT_HEIGHT, DEFAULT_SETTLE_MS, DEFAULT_WIDTH, block_on, claim_document,
        target_url,
    };
    #[act_tool(
        description = "Render a web page — SPAs included, scripts run — and return GitHub-Flavored Markdown plus page metadata (title, description, lang, url). This component instance scrapes one document; start another instance for another page",
        streaming,
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
        ctx: &mut ActContext<()>,
    ) -> ActResult<()> {
        claim_document()?;
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
            // Servo's teardown blocks on the render backend — a task on this
            // same runtime — and hangs the component. Leaking the page is the
            // lesser failure, same call servo's `render` makes.
            browser.forget();
            Ok::<_, String>((dom, meta))
        })
        .map_err(|error| ActError::internal(format!("scrape failed: {error}")))?;

        let inpage: crate::convert::InPageMeta =
            serde_json::from_str(&meta_text).unwrap_or_default();
        let mut meta = inpage.into_meta(mode, inline_html);
        let markdown = crate::convert::to_markdown(&dom, &mut meta);

        // Two parts, each as what it is (servo `render`'s convention).
        ctx.send_content(
            markdown.into_bytes(),
            Some("text/markdown".to_owned()),
            Vec::new(),
        );
        ctx.send_content(
            crate::convert::meta_part_json(&meta),
            Some("application/json".to_owned()),
            Vec::new(),
        );
        Ok(())
    }
}
