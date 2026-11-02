---
name: scrape
description: Render a web page — SPAs included, scripts run — with the Servo engine and get GitHub-Flavored Markdown plus page metadata, inside a sandbox with no browser installed.
metadata:
  act: {}
---

# scrape

One tool, `scrape`: give it `url` (or inline `html`) and it renders the page in
the Servo engine — scripts included — then returns two content parts:

1. `text/markdown` — the page as GitHub-Flavored Markdown. Relative `href`/`src`
   are already resolved to absolute URLs against the page's own base.
2. `application/json` — `{ "title", "description", "lang", "url", "mode" }`.

## Modes

- `"full"` (default) — the whole page, minus script/style machinery.
- `"readability"` — extracts the main article first (Firefox Reader View's
  algorithm), dropping navigation, ads and footers. Use it for articles; use
  `full` for apps, dashboards, docs with heavy sidebars.

## One scrape per instance

An instance scrapes exactly one document, then refuses further calls — the
browser's JavaScript engine allows one context per thread and a wasm guest has
one thread. Run one instance per page (`act call` one-shot is the intended
shape). There is no crawl tool and no navigation: one call, one page.

## Settling

`settle_ms` (default 1000) is how long the page is kept running after its load
event before capture. SPAs that hydrate late need more — raise it (3000–5000)
if the markdown looks like a pre-hydration shell. There is no network-idle
detection; the page that gets captured is whatever existed when settling ended.

## Capabilities

- **`wasi:filesystem`** is required even for offline pages: the engine keeps
  client storage on disk and will not start without a writable directory. Scope
  the grant to a temporary directory.
- **`wasi:sockets`** is required for any `url` input — the engine fetches the
  page and its subresources itself, inside the sandbox. Inline `html` needs no
  network. Granting sockets lets the page reach whatever it points at, so
  scope the grant if the page is not trusted.

## What it does not do

No crawl — one page per call, start another instance per page. No WebGL/GPU —
CPU rasterization, heavy pages are slow rather than impossible. Only the
DejaVu fonts compiled in (no CJK, Arabic or emoji text). Relative `srcset` and
CSS `url()` references are not rewritten (only `href`/`src` are). A page that
fails to load returns the engine's own error document converted — check the
markdown content, not just the call status.
