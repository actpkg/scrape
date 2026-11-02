# scrape

A web page — SPA included, scripts run — rendered by the Servo engine inside
a WebAssembly component and returned as GitHub-Flavored Markdown plus page
metadata. The self-hosted replacement for a hosted scrape endpoint: no
third-party service between you and the page.

## Try it

Needs [`act`](https://github.com/actcore/act-cli). Grab the component from the
[latest release](https://github.com/actpkg/scrape/releases/latest):

```bash
mkdir -p /tmp/scrape # the engine keeps client storage on disk

act call scrape.wasm scrape \
--grant '{"wasi:filesystem":{"mode":"allowlist","allow":[{"path":"/tmp/scrape/**","mode":"rw"}]}}' \
--allow wasi:sockets \
--args '{"url":"https://example.com/"}'
```

Two content parts come back: the page as `text/markdown`, and an
`application/json` part carrying `title`, `description`, `lang`, `url` and
`mode`. For an article rather than the whole page:

```bash
act call scrape.wasm scrape \
--grant '{"wasi:filesystem":{"mode":"allowlist","allow":[{"path":"/tmp/scrape/**","mode":"rw"}]}}' \
--allow wasi:sockets \
--args '{"url":"https://example.com/post/","mode":"readability"}'
```

## The tool

| tool | |
| --- | --- |
| `scrape` | renders one page — scripts included — returns its markdown and metadata |

`url` loads a page (`http:`, `https:` and `data:` all resolve); inline `html`
takes precedence when both are given. Viewport `width`/`height` default to
1024×768, `settle_ms` to 1000, `mode` to `full`.

## Modes

`full` (default) converts the whole page, minus script and style machinery.
`readability` extracts the main article first — Firefox Reader View's
algorithm — dropping navigation, ads and footers. Articles want
`readability`; apps, dashboards and docs with heavy sidebars want `full`.
When no article can be detected, `readability` falls back to the full page
rather than returning nothing.

## One scrape per instance

An instance scrapes exactly one document, then refuses further calls — for
the life of the instance, not once per call. Servo gives every document its
own script thread, every script thread creates a SpiderMonkey `JSContext`,
SpiderMonkey allows one per thread, and a wasip2 guest has exactly one
thread. There is no navigation tool and no crawl: one call, one page. **This
is not a crawler.** Start one instance per page — `act call` one-shot is the
intended shape.

## Settling

After the page's load event the component keeps driving it for `settle_ms`
(default 1000; servo's `render` gives a page 300) before capturing, so
ordinary hydration lands. SPAs that hydrate late need more: raise it to
3000–5000 when the markdown looks like a pre-hydration shell. There is no
network-idle heuristic — the page that gets captured is whatever existed when
settling ended.

## Capabilities

| capability | why |
| --- | --- |
| `wasi:filesystem` | the engine keeps client storage (localStorage and friends) on disk and will not start without a writable directory — scope it to a temp dir |
| `wasi:sockets` | the engine fetches the page and its subresources itself, over TCP, inside the sandbox; inline `html` needs no network. Implementing its own HTTP is also what leaves WebSockets working |

The page runs inside the sandbox, so what it can reach is the caller's
decision, not the page's.

## What it does not do

- **No crawl, no navigation.** One call, one page; see above.
- **Only the DejaVu fonts** are compiled in — no CJK, no Arabic, no colour
  emoji.
- **No WebGL, no GPU.** Rasterization is CPU-side; a heavy page is slow
  rather than impossible.
- **`srcset` and CSS `url()` references are not rewritten** — only `href` and
  `src` are resolved to absolute URLs against the page's base.
- **A page that fails to load is success-shaped.** The engine's own error
  document is what gets converted — read the markdown, not just the call
  status.
- **A heavy page outruns `settle_ms` silently.** Nothing cheap detects
  hydration completion; the capture is a fixed wait, not a signal. When in
  doubt, raise `settle_ms` and compare.
- **~3 GB peak RSS per instance.** Callers scraping very large pages should
  cap the component with `--max-memory`.

## Size

The packed artifact is **~113 MB**. Budget for it when pulling over OCI.

## Building it

```bash
just init # wkg wit fetch + wasi-sdk presence check
just build # cargo +1.95.0 wasm release build, then pack metadata in
just test # unit tests + e2e against `act run --mcp`, serial
```

Needs [wasi-sdk](https://github.com/WebAssembly/wasi-sdk/releases) at
`/opt/wasi-sdk` — the engine compiles a lot of C along the way: SpiderMonkey,
FreeType, aws-lc-rs, swgl.

Be warned about the first build: the engine is a git dependency, so cargo
fetches Servo's full history — about 1.7 GB — before compiling anything, and
then compiles a browser. Half an hour is normal. You do not need a Servo
checkout of your own, and the fetch is cached per machine.

Wasm release builds run `cargo +1.95.0` (see rust-toolchain.toml why:
1.100.0-beta.7's rust-lld mis-links mozjs_sys prebuilts).

If you only want to *use* the component, take the release artifact instead;
none of this applies.

## The forks

The engine is shared with the [servo component](../servo) — this component is
a different consumer of the same port, and the forks below are where the port
work lives. This repo's own code is the thin layer on top: the scrape tool,
HTML-to-markdown conversion, and the readability mode.

## The forks

| repo | what it carries |
| --- | --- |
| [GamePad64/servo](https://github.com/GamePad64/servo/tree/wasip2-port) | the port itself, ~2.3k lines |
| [GamePad64/webrender](https://github.com/GamePad64/webrender/tree/wasip2) | three threads and a rayon pool become tasks; FreeType glyph backend on wasm |
| [GamePad64/ipc-channel](https://github.com/GamePad64/ipc-channel/tree/wasip2) | the global router runs as a task |
| [GamePad64/surfman](https://github.com/GamePad64/surfman/tree/wasip2) | a wasi platform with a null device |
| [GamePad64/imsz](https://github.com/GamePad64/imsz/tree/wasip2) | one cfg line — the `Stdin` impl recursed on non-unix targets |
| [GamePad64/freetype-sys](https://github.com/GamePad64/freetype-sys/tree/wasip2) | PNG support made optional, off for wasm |

## Licence

MPL-2.0, matching Servo.
