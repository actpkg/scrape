//! Drive the packed component through `act run --mcp` with a real MCP client.
//!
//! The harness observes exactly what an agent observes, over the same client
//! stack (`rmcp`) the host bridge itself is built on. Every test renders
//! inline `html` — no `wasi:sockets` grant, no network — except the env-gated
//! live smoke at the bottom.
//!
//! Env: WASM — path to the packed component (default: the component's
//!      release build output);
//!      ACT  — the act invocation (default `act`; `npx @actcore/act` also
//!             works — whitespace-split);
//!      SCRAPE_E2E_LIVE_URL — when set, the live smoke test scrapes it
//!             (requires network and a sockets grant); skipped otherwise.

use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use rmcp::{ServiceExt, model::CallToolRequestParams, transport::TokioChildProcess};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::Mutex as AsyncMutex;

/// `().serve(transport)` hands back the client-role service running over the
/// child process: role first, the unit client handler second.
type Client = rmcp::service::RunningService<rmcp::service::RoleClient, ()>;

/// Deliberately loose. `act run --mcp` instantiates the component before it
/// answers `initialize`, so "connect" includes that cost — for this engine
/// it is ~8s healthy; the bound exists so a stalled handshake fails with a
/// diagnostic instead of hanging.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(120);

/// The engine keeps client storage (localStorage and friends) under /tmp/servo
/// and refuses to start without a writable directory there — the path is the
/// engine's own, not this component's, so the grant names it verbatim.
const CLIENT_STORAGE: &str = "/tmp/servo";

/// An article big enough to win Readability's default extraction (its
/// `char_threshold` is 500 characters of body text).
const ARTICLE_BODY: &str = "Rust is a systems programming language that runs blazingly fast, prevents segfaults, and guarantees thread safety. It accomplishes memory safety without a garbage collector, and ownership moves all checks to compile time. The component model lets Rust compile to sandboxed wasm modules with capability-based imports. WebAssembly provides a portable binary format with near-native speed. Sandboxing untrusted input is the core promise here.";

fn wasm_path() -> PathBuf {
    PathBuf::from(std::env::var("WASM").unwrap_or_else(|_| {
        concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../target/wasm32-wasip2/release/component_scrape.wasm"
        )
        .into()
    }))
}

/// The ACT invocation, honouring the same override the component justfile
/// uses. Its default there is `npx @actcore/act` — two words — which cannot
/// be `argv[0]` for a non-shell spawn, so the value is whitespace-split into
/// program + leading args.
fn act_argv() -> Vec<String> {
    std::env::var("ACT")
        .unwrap_or_else(|_| "act".into())
        .split_whitespace()
        .map(str::to_string)
        .collect()
}

/// Spawn `act run <wasm> --mcp` with the grants the test needs.
///
/// Grants are NOT optional: the default policy mode is `ask` and a headless
/// run degrades it to deny. The base grant is filesystem-only — inline `html`
/// tests never touch the network. `with_sockets` adds the network grant for
/// the live smoke.
fn act_command(with_sockets: bool) -> tokio::process::Command {
    std::fs::create_dir_all(CLIENT_STORAGE).expect("create the engine's client-storage dir");

    let mut grant = json!({
        "wasi:filesystem": {
            "mode": "allowlist",
            "allow": [{"path": format!("{CLIENT_STORAGE}/**"), "mode": "rw"}],
        }
    });
    if with_sockets {
        grant["wasi:sockets"] = json!({"mode": "open"});
    }

    let argv = act_argv();
    let mut cmd = tokio::process::Command::new(&argv[0]);
    cmd.args(&argv[1..]);
    cmd.arg("run").arg(wasm_path()).arg("--mcp");
    cmd.args(["--grant", &grant.to_string()]);
    cmd
}

/// Spawn with stderr captured: the audit trail (refusals, per-call rollup)
/// writes there unconditionally — RUST_LOG never silences it.
fn spawn_with_captured_stderr(with_sockets: bool) -> (TokioChildProcess, Arc<AsyncMutex<String>>) {
    let (transport, stderr) = TokioChildProcess::builder(act_command(with_sockets))
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn act run --mcp with piped stderr");

    let captured = Arc::new(AsyncMutex::new(String::new()));
    let sink = captured.clone();
    let mut lines = BufReader::new(stderr.expect("stderr was piped")).lines();
    tokio::spawn(async move {
        while let Ok(Some(line)) = lines.next_line().await {
            sink.lock().await.push_str(&line);
            sink.lock().await.push('\n');
        }
    });

    (transport, captured)
}

/// A connected MCP client plus everything its child wrote to stderr.
async fn connect_with(with_sockets: bool) -> (Client, Arc<AsyncMutex<String>>) {
    let (transport, captured) = spawn_with_captured_stderr(with_sockets);
    match tokio::time::timeout(CONNECT_TIMEOUT, async { ().serve(transport).await }).await {
        Ok(Ok(client)) => (client, captured),
        Ok(Err(error)) => panic!(
            "rmcp handshake with act run --mcp failed: {error}\n--- act stderr ---\n{}",
            captured.lock().await
        ),
        Err(_) => panic!(
            "MCP client did not connect within {}s; act's stderr so far:\n{}",
            CONNECT_TIMEOUT.as_secs(),
            captured.lock().await
        ),
    }
}

async fn connect() -> (Client, Arc<AsyncMutex<String>>) {
    connect_with(false).await
}

/// The manifest probe — and the fast-fail for a missing/stale wasm: an
/// unpacked wasm declares no ceiling, so every grant is refused as "outside
/// ceiling" and the failures point anywhere but at the missing metadata.
#[test]
fn manifest_reports_name_version_and_capabilities() {
    let output = {
        let argv = act_argv();
        let mut cmd = std::process::Command::new(&argv[0]);
        cmd.args(&argv[1..]);
        cmd.args(["inspect", "component-manifest"])
            .arg(wasm_path())
            .output()
            .expect("run act inspect component-manifest")
    };
    assert!(
        output.status.success(),
        "inspect failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let manifest: Value = serde_json::from_slice(&output.stdout).expect("manifest is JSON");
    assert_eq!(
        manifest["std"]["name"], "scrape",
        "packed manifest must carry the component name"
    );
    assert!(
        manifest["std"]["version"].is_string(),
        "packed manifest must carry a version, got: {}",
        manifest["std"]["version"]
    );
    let capabilities = &manifest["std"]["capabilities"];
    let caps = capabilities
        .as_object()
        .expect("capabilities are an object");
    assert!(
        caps.contains_key("wasi:sockets"),
        "scrape fetches pages itself over TCP — wasi:sockets must be declared, got: {capabilities}"
    );
    assert!(
        caps.contains_key("wasi:filesystem"),
        "the engine refuses to start without writable client storage — \
         wasi:filesystem must be declared, got: {capabilities}"
    );
}

#[tokio::test]
async fn lists_exactly_the_scrape_tool() {
    let (client, _captured) = connect().await;
    let tools = client.list_all_tools().await.expect("list_all_tools");
    let names: Vec<String> = tools.iter().map(|t| t.name.to_string()).collect();
    assert!(
        names.iter().any(|n| n == "scrape"),
        "scrape must be among the tools, got: {names:?}"
    );
    // Tool-provider only — no session-provider export, so the host must not
    // synthesise session tools either.
    for absent in ["open_session", "close_session"] {
        assert!(
            !names.iter().any(|n| n == absent),
            "{absent} must not be exposed (no session-provider), got: {names:?}"
        );
    }
    client.cancel().await.ok();
}

/// Call the scrape tool, return the parsed result.
async fn call_scrape(client: &Client, args: Value) -> rmcp::model::CallToolResult {
    client
        .call_tool(
            CallToolRequestParams::new("scrape")
                .with_arguments(args.as_object().expect("args object").clone()),
        )
        .await
        .expect("call_tool scrape")
}

/// Pull the markdown text and the metadata object out of a two-part result.
fn parts(result: &rmcp::model::CallToolResult) -> (String, Value) {
    assert_ne!(result.is_error, Some(true), "scrape failed: {result:?}");
    assert_eq!(
        result.content.len(),
        2,
        "scrape returns exactly markdown and metadata, got: {:?}",
        result.content
    );
    let md = match &result.content[0] {
        rmcp::model::ContentBlock::Text(text) => text,
        other => panic!("expected the first block to be text, got: {other:?}"),
    };
    let md_meta = md
        .meta
        .as_ref()
        .expect("the markdown block must carry _meta");
    assert_eq!(
        md_meta
            .0
            .get("dev.actcore/mime-type")
            .and_then(|v| v.as_str()),
        Some("text/markdown"),
        "the first part is markdown; text blocks surface their type only through meta"
    );
    let meta = match &result.content[1] {
        rmcp::model::ContentBlock::Text(text) => text,
        other => panic!("expected the second block to be text, got: {other:?}"),
    };
    let meta_meta = meta
        .meta
        .as_ref()
        .expect("the metadata block must carry _meta");
    assert_eq!(
        meta_meta
            .0
            .get("dev.actcore/mime-type")
            .and_then(|v| v.as_str()),
        Some("application/json"),
        "the second part is JSON"
    );
    let parsed: Value = serde_json::from_str(&meta.text).expect("metadata part parses as JSON");
    (md.text.clone(), parsed)
}

#[tokio::test]
async fn scrapes_a_page_built_by_script() {
    let (client, _captured) = connect().await;

    // A page whose content is built by script, scraped end to end: markdown
    // carrying the script's text proves the engine ran the script, not just
    // parsed the markup.
    let args = json!({
        "html": (
            "<html><head><title>Fixture</title></head><body><div id=out></div>\
             <script>document.getElementById('out').textContent='rendered by script'</script>\
             <p><a href=\"/relative/path\">rel</a></p></body></html>"
        ),
        "width": 400,
        "height": 200,
    });
    let (md, meta) = parts(&call_scrape(&client, args).await);
    assert!(
        md.contains("rendered by script"),
        "the script's text in the markdown, got: {md}"
    );
    assert!(
        !md.contains("getElementById"),
        "script text must not leak into the markdown, got: {md}"
    );
    assert_eq!(meta["mode"], "full");
    assert_eq!(meta["title"], "Fixture");
    assert!(
        meta["url"].is_null(),
        "inline html input reports no url, got: {}",
        meta["url"]
    );

    client.cancel().await.ok();
}

#[tokio::test]
async fn readability_mode_drops_the_furniture() {
    let (client, _captured) = connect().await;
    let args = json!({
        "html": format!(
            concat!(
                "<html><head><title>Article Title</title></head><body>",
                "<nav><a href=\"/1\">Menu One</a> <a href=\"/2\">Menu Two</a> <a href=\"/3\">Menu Three</a></nav>",
                "<article><h1>Real Heading</h1><p>{}</p><p>{}</p></article>",
                "<footer>Copyright Footer Slate</footer>",
                "</body></html>"
            ),
            ARTICLE_BODY, ARTICLE_BODY
        ),
        "mode": "readability",
    });
    let (md, meta) = parts(&call_scrape(&client, args).await);
    assert_eq!(meta["mode"], "readability");
    assert!(md.contains("Real Heading"), "article kept, got: {md}");
    assert!(!md.contains("Menu One"), "navigation dropped, got: {md}");
    assert!(
        !md.contains("Copyright Footer Slate"),
        "footer dropped, got: {md}"
    );

    client.cancel().await.ok();
}

#[tokio::test]
async fn second_scrape_in_one_instance_is_refused() {
    let (client, _captured) = connect().await;
    let first = json!({"html": "<html><body><p>first</p></body></html>"});
    let second = json!({"html": "<html><body><p>second</p></body></html>"});

    let ok = call_scrape(&client, first).await;
    assert_ne!(
        ok.is_error,
        Some(true),
        "the first scrape must succeed: {ok:?}"
    );

    let refused = call_scrape(&client, second).await;
    assert_eq!(
        refused.is_error,
        Some(true),
        "the second scrape must be refused, got: {refused:?}"
    );
    let text = refused
        .content
        .first()
        .map(|c| match c {
            rmcp::model::ContentBlock::Text(t) => t.text.clone(),
            other => format!("{other:?}"),
        })
        .unwrap_or_default();
    assert!(
        text.contains("one document"),
        "the refusal explains the one-document limit, got: {text}"
    );

    // The instance survives to explain itself: the server still answers.
    let tools = client
        .list_all_tools()
        .await
        .expect("server still alive after refusal");
    assert!(!tools.is_empty());

    client.cancel().await.ok();
}

/// Live smoke — only when SCRAPE_E2E_LIVE_URL is set. Needs network and the
/// sockets grant; CI stays offline-safe without the env var.
#[tokio::test]
async fn live_smoke() {
    let Some(url) = std::env::var("SCRAPE_E2E_LIVE_URL").ok() else {
        eprintln!("SCRAPE_E2E_LIVE_URL not set — skipping the live smoke");
        return;
    };
    let (client, _captured) = connect_with(true).await;
    let (md, meta) = parts(&call_scrape(&client, json!({ "url": url })).await);
    assert!(
        !md.trim().is_empty(),
        "live page scrapes to non-empty markdown"
    );
    assert_eq!(meta["mode"], "full");
    let reported = meta["url"].as_str().unwrap_or_default();
    let host = url
        .split("//")
        .nth(1)
        .and_then(|rest| rest.split('/').next())
        .unwrap_or_default();
    assert!(
        reported.contains(host),
        "metadata reports the page's own url, got: {reported}"
    );

    client.cancel().await.ok();
}
