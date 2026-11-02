//! The Servo engine wrapper: one engine, one `WebView`, many sequential
//! pages navigated inside that `WebView`.
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
/// Noto, under the SIL Open Font Licence — see `fonts/LICENSE-OFL`. Latin,
/// Cyrillic and Greek live in the four core faces; the script-specific faces
/// (Arabic, Devanagari, Hebrew, Thai) and the CJK families back pages in those
/// scripts, which the four core faces alone could not. A page asking for any
/// other family falls
/// back to these via the CSS generic families. Keep in sync with
/// components/servo — same three core faces plus script coverage.
const BUILTIN_FONTS: &[(&str, &str, &[u8])] = &[
    (
        "Noto Sans",
        "builtin:NotoSans",
        include_bytes!("../fonts/NotoSans-Regular.ttf"),
    ),
    (
        "Noto Serif",
        "builtin:NotoSerif",
        include_bytes!("../fonts/NotoSerif-Regular.ttf"),
    ),
    (
        "Noto Sans Mono",
        "builtin:NotoSansMono",
        include_bytes!("../fonts/NotoSansMono-Regular.ttf"),
    ),
    (
        "Noto Sans Arabic",
        "builtin:NotoSansArabic",
        include_bytes!("../fonts/NotoSansArabic-Regular.ttf"),
    ),
    (
        "Noto Sans Devanagari",
        "builtin:NotoSansDevanagari",
        include_bytes!("../fonts/NotoSansDevanagari-Regular.ttf"),
    ),
    (
        "Noto Sans Hebrew",
        "builtin:NotoSansHebrew",
        include_bytes!("../fonts/NotoSansHebrew-Regular.ttf"),
    ),
    (
        "Noto Sans Thai",
        "builtin:NotoSansThai",
        include_bytes!("../fonts/NotoSansThai-Regular.ttf"),
    ),
    (
        "Noto Sans JP",
        "builtin:NotoSansJP",
        include_bytes!("../fonts/NotoSansJP-Regular.otf"),
    ),
    (
        "Noto Sans KR",
        "builtin:NotoSansKR",
        include_bytes!("../fonts/NotoSansKR-Regular.otf"),
    ),
    (
        "Noto Sans SC",
        "builtin:NotoSansSC",
        include_bytes!("../fonts/NotoSansSC-Regular.otf"),
    ),
    (
        "Noto Sans TC",
        "builtin:NotoSansTC",
        include_bytes!("../fonts/NotoSansTC-Regular.otf"),
    ),
    (
        "Noto Sans HK",
        "builtin:NotoSansHK",
        include_bytes!("../fonts/NotoSansHK-Regular.otf"),
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
// the page lifecycle (pages navigate inside the engine's one persistent
// `WebView` — servo's `render` still leaks each page via `forget`), and the
// removal of `screenshot`, `click`, `scroll` and the session plumbing.
// ---------------------------------------------------------------------------
/// The engine behind every page.
///
/// `ServoBuilder::build` initialises process-global options and panics if it
/// runs twice, so there is exactly one engine however many pages are open.
/// Pages are navigations inside this engine's single `WebView` — the browser
/// is the tab here.
pub(crate) struct Engine {
    servo: servo::Servo,
    rendering_context: Rc<servo::SoftwareRenderingContext>,
    /// The engine's one `WebView`, created lazily on the first page and kept
    /// for the engine's whole life (see `Browser`).
    webview: RefCell<Option<servo::WebView>>,
    /// The delegate of `webview`, mirrored here so later pages can read the
    /// turn counter without reaching through the `WebView`.
    watch: RefCell<Rc<PageWatch>>,
}

thread_local! {
    static ENGINE: RefCell<Option<Rc<Engine>>> = const { RefCell::new(None) };
}

/// Watch the one `WebView` from the embedder side.
///
/// While a page loads it counts full event-loop turns (`pump` needs the count
/// to pace the settle window) and records the active document's URL
/// (`notify_url_changed` fires when the constellation activates a document).
/// The URL is what makes waiting for a load race-free: the load status alone
/// can flip to `Complete` for a *previous* document while a navigation is
/// still in flight, so a wait must observe the new URL before trusting the
/// status.
#[derive(Default)]
struct PageWatch {
    turns: std::cell::Cell<u64>,
    url: RefCell<Option<url::Url>>,
}

impl servo::WebViewDelegate for PageWatch {
    fn notify_url_changed(&self, _webview: servo::WebView, new_url: url::Url) {
        *self.url.borrow_mut() = Some(new_url);
    }

    fn notify_closed(&self, _webview: servo::WebView) {}
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
            webview: RefCell::new(None),
            watch: RefCell::new(Rc::new(PageWatch::default())),
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

/// A live page: the shared engine's one `WebView`, navigated to this call's
/// target.
///
/// The `WebView` itself is never dropped — its `Painter::drop` shuts the
/// render backend down synchronously, which on the coop thread layout parks
/// the calling fiber and deadlocks (the exact hang that forced the old
/// `forget()`). Pages instead come and go by *navigation*: `open` navigates
/// with `Replace`, so the constellation closes the previous page's pipeline
/// right away, and `close_page` navigates to `about:blank` the same way,
/// making the scraped page exit fully (`JS_DestroyContext` runs on the
/// page's own script thread) before the next call starts.
pub(crate) struct Browser {
    engine: Rc<Engine>,
    webview: servo::WebView,
    watch: Rc<PageWatch>,
}

impl Browser {
    pub(crate) async fn open(url: &str, width: u32, height: u32) -> Result<Browser, String> {
        let engine = engine(width, height)?;
        let parsed = servo::ServoUrl::parse(url).map_err(|error| format!("bad url: {error}"))?;
        let target = parsed.into_url();

        let browser = Browser::attach(engine)?;
        // A brand-new WebView starts with a pending `about:blank` load (the
        // builder's implicit initial URL). The constellation aborts any
        // navigation that arrives while a load for the same browsing context
        // is still pending, so the first `load_replace` must wait until the
        // initial document has activated — the watch learns that as the
        // first `notify_url_changed`. On every later call the URL is already
        // set and this returns immediately.
        let ready_deadline = Instant::now() + LOAD_TIMEOUT;
        while browser.watch.url.borrow().is_none() {
            if Instant::now() >= ready_deadline {
                log::warn!("webview's initial about:blank never activated");
                return Err("webview did not finish initializing".to_owned());
            }
            browser.engine.servo.spin_event_loop();
            tokio::time::sleep(POLL_INTERVAL).await;
        }
        browser.webview.load_replace(target.clone());
        browser.wait_for_load(Some(&target)).await;
        Ok(browser)
    }

    /// The engine's persistent `WebView`, creating it on first use.
    fn attach(engine: Rc<Engine>) -> Result<Browser, String> {
        let existing = engine.webview.borrow().clone();
        if let Some(webview) = existing {
            let watch = engine.watch.borrow().clone();
            return Ok(Browser {
                engine,
                webview,
                watch,
            });
        }
        let watch: Rc<PageWatch> = Rc::new(PageWatch::default());
        let webview = servo::WebViewBuilder::new(&engine.servo, engine.rendering_context.clone())
            .delegate(watch.clone())
            .build();
        *engine.webview.borrow_mut() = Some(webview.clone());
        *engine.watch.borrow_mut() = watch.clone();
        Ok(Browser {
            engine,
            webview,
            watch,
        })
    }

    /// Make this the page that paints.
    ///
    /// Pages share one surface, so whichever one is being worked on has to be
    /// raised first. Calls are strictly sequential, so this is enough.
    fn activate(&self) {
        self.webview.show();
    }

    /// Turn the event loop until the target page has finished loading.
    ///
    /// When `target` is given, the wait requires the constellation to have
    /// activated a document *at* that URL (`notify_url_changed`) before the
    /// `Complete` status counts. The status alone is not trustworthy: it
    /// belongs to whichever document loaded last, so right after a
    /// `load_replace` it is still `Complete` from the previous page while the
    /// new navigation has not even started — an early return here reads the
    /// DOM of the wrong (or dying) document.
    async fn wait_for_load(&self, target: Option<&url::Url>) {
        let deadline = Instant::now() + LOAD_TIMEOUT;
        loop {
            self.engine.servo.spin_event_loop();
            tokio::time::sleep(POLL_INTERVAL).await;
            let url_ok = match target {
                Some(target) => self.watch.url.borrow().as_ref() == Some(target),
                None => true,
            };
            if url_ok && self.webview.load_status() == servo::LoadStatus::Complete {
                return;
            }
            if Instant::now() >= deadline {
                log::warn!("load did not finish within {LOAD_TIMEOUT:?}");
                return;
            }
        }
    }

    /// Turn the event loop `turns` full turns, painting as we go.
    pub(crate) async fn pump(&self, turns: u32) {
        for _ in 0..turns {
            self.webview.paint();
            self.engine.servo.spin_event_loop();
            self.watch.turns.set(self.watch.turns.get() + 1);
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
