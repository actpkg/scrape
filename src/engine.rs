//! The Servo engine wrapper: engine singleton, one-page Browser.
//!
//! A trimmed copy of `components/servo`'s engine layer — keep engine-version
//! moves in sync with it. No sessions, no interaction surface: `evaluate`
//! exists to run this component's own in-page fixup/metadata scripts, not as
//! a tool.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::{Duration, Instant};

use act_sdk::prelude::*;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use servo::RenderingContext as _;

pub(crate) const DEFAULT_WIDTH: u32 = 1024;
pub(crate) const DEFAULT_HEIGHT: u32 = 768;
/// SPA hydration keeps writing the DOM after `load`; one second covers most
/// pages. Servo's 300 ms is tuned for static pages — deliberately not copied.
pub(crate) const DEFAULT_SETTLE_MS: u32 = 1000;
/// How long to wait for a load before giving up on it.
const LOAD_TIMEOUT: Duration = Duration::from_secs(30);
/// Gap between turns of the event loop.
///
/// Sleeping rather than yielding is load-bearing: socket readiness is only
/// delivered when the runtime parks, and a runtime whose queue is never empty
/// does not park — so a busy wait starves the very network IO a page needs.
const POLL_INTERVAL: Duration = Duration::from_millis(1);

/// The faces compiled into this component, as `(family, builtin: path, bytes)`.
///
/// DejaVu, under the Bitstream Vera licence — see `fonts/LICENSE`. They back the
/// CSS generic families; a page asking for anything else falls back to these.
/// Keep in sync with components/servo — same three faces.
const BUILTIN_FONTS: &[(&str, &str, &[u8])] = &[
    (
        "DejaVu Sans",
        "builtin:DejaVuSans",
        include_bytes!("../fonts/DejaVuSans.ttf"),
    ),
    (
        "DejaVu Serif",
        "builtin:DejaVuSerif",
        include_bytes!("../fonts/DejaVuSerif.ttf"),
    ),
    (
        "DejaVu Sans Mono",
        "builtin:DejaVuSansMono",
        include_bytes!("../fonts/DejaVuSansMono.ttf"),
    ),
];

/// Rewrite every relative `href`/`src` in the page to an absolute URL,
/// in-page, before the DOM is serialized.
///
/// Doing this in JavaScript costs no Rust dependency and gets the page's own
/// semantics (`document.baseURI` honours `<base>` tags). Per-element
/// `try/catch`: `data:` documents have no base to resolve against, and one
/// throwing attribute must not stop the rest.
const ABSOLUTIZE_SCRIPT: &str = r#"(() => {
    let n = 0;
    const abs = (el, attr) => {
        const v = el.getAttribute(attr);
        if (!v) return;
        try { el.setAttribute(attr, new URL(v, document.baseURI).href); n++; } catch (_) {}
    };
    for (const el of document.querySelectorAll('[href]')) abs(el, 'href');
    for (const el of document.querySelectorAll('[src]')) abs(el, 'src');
    return n;
})()"#;

/// Page metadata as one `JSON.stringify` string, so the string comes back
/// through the engine's JS-value renderer verbatim and parses with serde_json.
const META_SCRIPT: &str = r#"JSON.stringify({
    title: document.title || null,
    description: (document.querySelector('meta[name="description"], meta[property="og:description"]') || {}).content || null,
    lang: document.documentElement.lang || null,
    url: location.href
})"#;

// ---------------------------------------------------------------------------
// Everything from here through `render_js_value` is copied verbatim from
// components/servo/src/lib.rs except: the `Engine`/`Browser` struct fields,
// `claim_document`'s message (says "scrapes"), and the removal of
// `screenshot`, `click`, `scroll` and the session plumbing.
// ---------------------------------------------------------------------------
/// The engine behind every page.
///
/// `ServoBuilder::build` initialises process-global options and panics if it
/// runs twice, so there is exactly one engine however many pages are open. That
/// is also Servo's own model: one engine, many `WebView`s — the difference
/// between a browser and a tab.
pub(crate) struct Engine {
    servo: servo::Servo,
    rendering_context: Rc<servo::SoftwareRenderingContext>,
}

thread_local! {
    static ENGINE: RefCell<Option<Rc<Engine>>> = const { RefCell::new(None) };
    /// Whether this instance has already had a document.
    ///
    /// Not "has one now": the limit is for the life of the instance, because a
    /// closed page's script thread is leaked rather than torn down.
    static DOCUMENT_USED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Claim this instance's one document, or explain why there is not another.
///
/// Every document Servo opens gets a script thread, and every script thread
/// creates a SpiderMonkey `JSContext`. SpiderMonkey allows one per thread and a
/// wasip2 guest has exactly one thread, so the second document dies inside
/// `JS_NewContext`. That covers a second session, and equally a navigation away
/// from the first page: both build a new pipeline. Refusing with an explanation
/// beats taking the component down.
pub(crate) fn claim_document() -> Result<(), ActError> {
    if DOCUMENT_USED.with(|used| used.replace(true)) {
        return Err(ActError::internal(
            "this component instance scrapes one document and has already used it: \
             start another instance for another page. Servo gives each document a \
             script thread, and SpiderMonkey allows one JavaScript context per \
             thread — a wasip2 guest has one thread.",
        ));
    }
    Ok(())
}

/// The engine, started on first use and sized to fit `width` x `height`.
pub(crate) fn engine(width: u32, height: u32) -> Result<Rc<Engine>, String> {
    ENGINE.with(|slot| {
        let mut slot = slot.borrow_mut();
        if let Some(engine) = slot.as_ref() {
            // One surface serves every page, so it has to be at least as large
            // as the largest of them.
            let current = engine.rendering_context.size();
            if current.width < width || current.height < height {
                engine.rendering_context.resize(servo::PhysicalSize::new(
                    current.width.max(width),
                    current.height.max(height),
                ));
            }
            return Ok(engine.clone());
        }

        // `inventory` collects nothing under wasip2, so register the resource
        // reader explicitly rather than relying on life-before-main.
        servo::resources::set_resource_reader(&servo_default_resources::DefaultResourceReader);
        // There is no system font database in a guest, so the faces come from
        // here. Which faces to carry is this component's choice, which is why the
        // engine takes them rather than embedding them: see `fonts/`.
        servo::set_builtin_fonts(BUILTIN_FONTS);

        let servo = servo::ServoBuilder::default().build();
        // CPU rasterisation: swgl gives WebRender a real GL implementation
        // writing into ordinary memory. There is no GPU inside a component.
        let rendering_context = Rc::new(
            servo::SoftwareRenderingContext::new(servo::PhysicalSize::new(width, height))
                .map_err(|error| format!("software rendering context: {error:?}"))?,
        );
        let engine = Rc::new(Engine {
            servo,
            rendering_context,
        });
        *slot = Some(engine.clone());
        Ok(engine)
    })
}

thread_local! {
    /// The one runtime every Servo actor lives on, created on first use and
    /// re-entered by each tool call.
    static RUNTIME: (tokio::runtime::Runtime, tokio::task::LocalSet) = (
        tokio::runtime::Builder::new_current_thread()
            // IO as well as time: the network reaches the guest through
            // `tokio::net`, which needs the IO driver.
            .enable_all()
            .build()
            .expect("could not build the component runtime"),
        tokio::task::LocalSet::new(),
    );
}

/// Run `future` on the component runtime.
pub(crate) fn block_on<F: std::future::Future>(future: F) -> F::Output {
    RUNTIME.with(|(runtime, local)| local.block_on(runtime, future))
}

/// Turn the caller's `html` / `url` pair into something Servo can load.
pub(crate) fn target_url(html: Option<String>, url: Option<String>) -> Result<String, ActError> {
    match (html, url) {
        // base64 rather than percent-encoding: it is the conventional spelling
        // for a `data:` URL carrying a document, and markup percent-encodes
        // badly — almost every character is reserved, so the URL comes out
        // around three times the size of the page instead of a third larger.
        (Some(html), _) => Ok(format!(
            "data:text/html;charset=utf-8;base64,{}",
            BASE64.encode(html.as_bytes())
        )),
        (None, Some(url)) => Ok(url),
        (None, None) => Err(ActError::invalid_args("provide either `html` or `url`")),
    }
}

/// A live page: one `WebView` on the shared engine.
pub(crate) struct Browser {
    engine: Rc<Engine>,
    webview: servo::WebView,
}

impl Browser {
    pub(crate) async fn open(url: &str, width: u32, height: u32) -> Result<Browser, String> {
        let engine = engine(width, height)?;
        let parsed = servo::ServoUrl::parse(url).map_err(|error| format!("bad url: {error}"))?;
        let webview = servo::WebViewBuilder::new(&engine.servo, engine.rendering_context.clone())
            .url(parsed.into_url())
            .build();

        let browser = Browser { engine, webview };
        browser.activate();
        browser.wait_for_load().await;
        Ok(browser)
    }

    /// Make this the page that paints.
    ///
    /// Pages share one surface, so whichever one is being worked on has to be
    /// raised first. Calls are strictly sequential, so this is enough.
    fn activate(&self) {
        self.webview.show();
    }

    /// Turn the event loop until the page reports it has finished loading.
    async fn wait_for_load(&self) {
        let deadline = Instant::now() + LOAD_TIMEOUT;
        loop {
            self.engine.servo.spin_event_loop();
            tokio::time::sleep(POLL_INTERVAL).await;
            if self.webview.load_status() == servo::LoadStatus::Complete {
                return;
            }
            if Instant::now() >= deadline {
                log::warn!("load did not finish within {LOAD_TIMEOUT:?}");
                return;
            }
        }
    }

    /// Turn the event loop `turns` times, painting as we go.
    pub(crate) async fn pump(&self, turns: u32) {
        for _ in 0..turns {
            self.webview.paint();
            self.engine.servo.spin_event_loop();
            tokio::time::sleep(POLL_INTERVAL).await;
        }
    }
    /// Evaluate `script` in the page, through the same path an embedder uses.
    pub(crate) async fn evaluate(&self, script: &str) -> Result<String, String> {
        self.activate();
        let slot: Rc<RefCell<Option<Result<String, String>>>> = Default::default();
        let sink = slot.clone();
        self.webview.evaluate_javascript(script, move |result| {
            *sink.borrow_mut() = Some(match result {
                Ok(value) => Ok(render_js_value(&value)),
                Err(error) => Err(format!("{error:?}")),
            });
        });

        let deadline = Instant::now() + LOAD_TIMEOUT;
        loop {
            self.engine.servo.spin_event_loop();
            tokio::time::sleep(POLL_INTERVAL).await;
            if let Some(result) = slot.borrow_mut().take() {
                return result;
            }
            if Instant::now() >= deadline {
                return Err("evaluation did not return in time".to_owned());
            }
        }
    }
    /// Give up the page without running Servo's teardown.
    ///
    /// Dropping it properly would be better — the script thread's SpiderMonkey
    /// `JSContext` is what limits an instance to one page — but the teardown
    /// path blocks on the render backend, a task on this same runtime, and that
    /// hangs the component outright. Leaking is the lesser failure.
    pub(crate) fn forget(self) {
        std::mem::forget(self);
    }
}
/// Render a JavaScript value as text a caller can read or parse.
fn render_js_value(value: &servo::JSValue) -> String {
    match value {
        servo::JSValue::Undefined => "undefined".to_owned(),
        servo::JSValue::Null => "null".to_owned(),
        servo::JSValue::Boolean(value) => value.to_string(),
        servo::JSValue::Number(value) => value.to_string(),
        servo::JSValue::String(value) => value.clone(),
        servo::JSValue::Element(id)
        | servo::JSValue::ShadowRoot(id)
        | servo::JSValue::Frame(id)
        | servo::JSValue::Window(id) => id.clone(),
        servo::JSValue::Array(values) => {
            let rendered: Vec<_> = values.iter().map(render_js_value).collect();
            format!("[{}]", rendered.join(", "))
        }
        servo::JSValue::Object(fields) => {
            let rendered: Vec<_> = fields
                .iter()
                .map(|(key, value)| format!("{key}: {}", render_js_value(value)))
                .collect();
            format!("{{{}}}", rendered.join(", "))
        }
    }
}
impl Browser {
    /// Resolve relative `href`/`src` against the page's own base URI.
    pub(crate) async fn absolutize_links(&self) {
        let _ = self.evaluate(ABSOLUTIZE_SCRIPT).await;
    }

    /// The page's metadata as a JSON string (from `META_SCRIPT`).
    pub(crate) async fn page_meta(&self) -> Result<String, String> {
        self.evaluate(META_SCRIPT).await
    }
}
