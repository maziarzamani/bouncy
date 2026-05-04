<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="assets/logo-dark.png">
    <source media="(prefers-color-scheme: light)" srcset="assets/logo-light.png">
    <img src="assets/logo-dark.png" alt="bouncy" width="520">
  </picture>
</p>

<p align="center">
  <strong>Tiny Rust headless browser for scraping.</strong>
</p>

<p align="center">
  <a href="https://github.com/maziarzamani/bouncy/actions/workflows/ci.yml"><img src="https://github.com/maziarzamani/bouncy/actions/workflows/ci.yml/badge.svg" alt="CI"></a>
  <a href="https://crates.io/crates/bouncy-cli"><img src="https://img.shields.io/crates/v/bouncy-cli?logo=rust&label=crates.io" alt="crates.io"></a>
  <a href="https://www.rust-lang.org"><img src="https://img.shields.io/badge/rust-1.80%2B-orange?logo=rust&logoColor=white" alt="Rust 1.80+"></a>
  <a href="https://github.com/maziarzamani/bouncy/releases/latest"><img src="https://img.shields.io/github/v/release/maziarzamani/bouncy?logo=github&label=release" alt="Latest release"></a>
</p>

---

bouncy is a web scraper. Tiny, fast, ships as a single binary — no Node, no Chrome, no Python to install. Point it at a URL and get back the HTML, the visible text, or every link on the page. If the page only renders properly with JavaScript, bouncy will run the JavaScript too. Use it from the command line like curl, or drop it in as a Playwright backend.

## Features

- **One install, four modes** — `bouncy fetch` / `scrape` / `browse` (CLI; the last is a stateful multi-step browser), `bouncy-mcp` (MCP server for Claude Desktop, Claude Code, Cursor — including the new `bouncy_browse_*` tools that let LLMs drive autonomous browse flows), `bouncy serve` (CDP, drop-in for Playwright / Puppeteer). Both binaries in the same release tarball.
- **No runtime to install** — no Node, no Chrome, no Python.
- **Lazy V8** — boots only when the page actually needs JavaScript. Static pages stay 3–6 ms cold; JS pages 30–80 ms.
- **Lean** — 10–21 MB resident per page; ~40 MB binary with V8 or ~3.7 MB without.
- **Stealth, built in** — hides `navigator.webdriver`, randomizes canvas / audio / WebGPU / battery fingerprints per session.
- **Production touches** — JSON cookie jar, tracker blocklist (extensible), custom CAs, HTTP CONNECT proxy, HTTP/2 with connection pooling.
- **CSS-selector extraction** — `bouncy fetch <url> --select "h1"` returns the text of every match, one per line. Pair with `--attr href` for attribute values. Works on `scrape` too, where matches land in a `selected: [...]` field per row.
- **Per-host rate limiting** — `bouncy scrape <urls> --per-host-concurrency 2` caps any single origin to N in-flight requests. Avoids hammering one server when scraping a list that hits the same host repeatedly.
- **Configurable User-Agent** — `--user-agent` on `fetch` / `scrape`, with a sensible default of `bouncy/<version> (+repo URL)` so site operators can identify and reach you.
- **Live TUI dashboard** — `bouncy scrape <urls> --tui` swaps the JSON summary for a live ratatui UI: per-URL status grid, throughput, p50/p95 latency, status histogram. Off by default; opt-in flag.
- **Stateful browse sessions (library, in progress)** — `bouncy-browse` crate ships `BrowseSession` with `open` / `click` / `fill` / `submit` / `goto` / `read` / `eval` and structured page snapshots (forms, links, buttons, headings, meta). `submit` handles real HTTP form submission (POST + GET) and JS-only forms transparently. Foundation for an upcoming `bouncy browse` CLI subcommand and `bouncy_browse_*` MCP tools that let LLMs drive multi-step browsing flows autonomously.
- **Cross-platform binaries** — Linux x86_64, macOS Apple Silicon, Windows x86_64.

## See it

![bouncy scrape --tui dashboard](assets/tui-demo.gif)

`bouncy scrape urls.txt --concurrency 50 --tui` — live status grid for every URL, throughput rate, p50/p95/max latency, response-code histogram. Updates at 10 Hz. Falls through to the classic JSON / text output when `--tui` isn't set, so scripts piping to `jq` keep working.

## Why bouncy

| | bouncy | Playwright |
|---|---|---|
| Cold start | 3–6 ms (static), ~30–80 ms (with V8) | 800–1500 ms |
| Memory per page | 10–21 MB | 200+ MB |
| Runs JavaScript | yes (lazy V8) | yes (real Chromium) |
| Real layout / paint / WebGL | no | yes |
| CDP server (Playwright drop-in) | yes | yes |
| Stealth mode | built-in (canvas / audio / WebGPU / battery randomization) | needs plugin |
| Runtime needed | none | Node + Chromium |

If you need a real browser (screenshots, true layout-dependent behaviour, full WebGL), use Playwright. bouncy is the right tool when the page renders correctly *enough* with a DOM + JS but no compositor — which is most scraping flows.

## Install

### From crates.io

```bash
cargo install bouncy-cli      # the `bouncy` CLI
cargo install bouncy-mcp      # the MCP server binary
```

Pulls in V8 prebuilts on first build (~30 s download, no from-source V8 compile).

### Prebuilt binary (no Rust toolchain needed)

Grab the latest tarball / zip from [Releases](https://github.com/maziarzamani/bouncy/releases). Each tag publishes:

- `bouncy-vX.Y.Z-x86_64-unknown-linux-gnu.tar.gz`
- `bouncy-vX.Y.Z-aarch64-apple-darwin.tar.gz` (Apple Silicon)
- `bouncy-vX.Y.Z-x86_64-pc-windows-msvc.zip`

Drop the binary on your `PATH` and run `bouncy --help`.

#### macOS: first run

The release binaries aren't codesigned (no Apple Developer certificate), so Gatekeeper will block the first launch with *"cannot be opened because Apple cannot check it for malicious software"*. Strip the quarantine attribute once and you're done:

```bash
xattr -d com.apple.quarantine ./bouncy ./bouncy-mcp
```

Or, in System Settings → Privacy & Security, click **Open Anyway** after the first failed launch.

### Build from source

Rust 1.80+ ([rustup.rs](https://rustup.rs)), stable channel.

```bash
git clone https://github.com/maziarzamani/bouncy
cd bouncy
cargo build --release -p bouncy-cli      # the `bouncy` CLI
cargo build --release -p bouncy-mcp      # the MCP server
```

The default build pulls a prebuilt V8 binary on first run (~30 s, no from-source V8 compile).

## Use as a library

Every internal crate is published, so you can grab just the bits you need from another Rust project:

```toml
[dependencies]
bouncy-fetch   = "0.1"   # HTTP client (hyper + rustls, no reqwest overhead)
bouncy-extract = "0.1"   # streaming HTML title / text / link extractor
bouncy-js      = "0.1"   # embedded V8 + DOM bridge
bouncy-cdp     = "0.1"   # Chrome DevTools Protocol server
bouncy-dom     = "0.1"   # spec-compliant HTML5 DOM tree
bouncy-browse  = "0.1"   # stateful browser session: open / click / fill / read with structured snapshots
```

Tiny example — fetch a page and pull its title:

```rust
use bouncy_fetch::Fetcher;
use bouncy_extract::extract_title;

let fetcher = Fetcher::new()?;
let resp = fetcher.get("https://example.com").await?;
let title = extract_title(&resp.body)?;
println!("{:?}", title);   // Some("Example Domain")
```

Or drive a stateful browse session — cookies, V8, DOM all persist across steps:

```rust
use bouncy_browse::{BrowseSession, BrowseOpts, ReadMode};

let (session, snap) =
    BrowseSession::open("https://help.com", BrowseOpts::default()).await?;
println!("{}", snap.title);                       // e.g. "Help Center"

// Snapshots list every form / link / button / input on the page with
// a stable selector — pick one and act on it. The same session handles
// the whole flow; cookies replay automatically across navigations.
session.click("a[href='/signup']").await?;        // navigate to signup
session.fill("input[name=name]", "Maziar").await?;
session.fill("input[name=email]", "me@x.test").await?;
session.submit("form#signup").await?;             // submit the form
let h1 = session.read("h1", ReadMode::Text).await?;
println!("{:?}", h1);                             // ["Welcome, Maziar!"]
```

`submit` handles three cases without you having to think about it:
the form has an `action` attribute (real HTTP `POST` / `GET` from the
field values), the form is JS-only (a `submit` event fires and the
page's handler runs), or the selector hits a submit `<button>` rather
than the `<form>` itself (it climbs to the enclosing form).

## Quick Start

### Fetch a page

```bash
# Static HTML — never touches V8.
bouncy fetch https://example.com --dump html
bouncy fetch https://example.com --dump links
bouncy fetch https://example.com --dump text
```

### Extract with a CSS selector

```bash
# Text content of every match, one per line.
bouncy fetch https://example.com --select "h1"
# → Example Domain

# Attribute value of every match.
bouncy fetch https://example.com --select "a" --attr href

# Selector grammar today: tag, #id, .class, [attr], [attr=value]
# (no combinators or pseudo-classes yet).
```

### Run JavaScript

```bash
# Boots V8 only because --eval / --selector is set.
bouncy fetch https://news.example.com --selector '.post' --dump html
bouncy fetch https://example.com --eval "document.title"
bouncy fetch https://store.test/p/123 --eval "document.querySelector('[itemprop=price]').textContent"
```

### POST, headers, body, proxy

```bash
bouncy fetch https://api.example.com/x \
  -X POST \
  -H 'Authorization: Bearer …' \
  -H 'Content-Type: application/json' \
  --body '{"hello":"world"}'

# Through an HTTP CONNECT proxy.
bouncy fetch https://api.example.com/x --proxy http://proxy.test:3128

# PUT a file.
bouncy fetch https://api.example.com/upload \
  -X PUT --body-file ./payload.json -H 'Content-Type: application/json'
```

### Stealth

Hides `navigator.webdriver`, swaps the UA for a recent Chrome string, masks polyfill methods so `.toString()` returns the canonical `[native code]` shape, and randomises canvas / audio / WebGPU / battery / WebGL renderer / `document.fonts` per session (stable within a session, varies across them).

```bash
bouncy fetch https://bot-detector.test --stealth --eval "navigator.webdriver"
# → undefined
```

### Cookie jar

`--cookie-jar` reads a JSON file before the request (if it exists) and writes it back after. `Set-Cookie` from one invocation replays on the next.

```bash
# Log in once, capture cookies.
bouncy fetch https://app.test/login -X POST --body 'u=me&p=pw' --cookie-jar ./jar.json

# Reuse them on a follow-up request.
bouncy fetch https://app.test/profile --cookie-jar ./jar.json --dump text
```

### Block trackers

`--block-trackers` drops requests to a small built-in list of ad / analytics hosts (Google Analytics, GTM, DoubleClick, Facebook pixel, Mixpanel, Segment, Hotjar, Amplitude, FullStory, ScoreCard). Add your own with `--block-host` (repeatable, suffix-matched).

```bash
bouncy fetch https://news.example.com --block-trackers --dump html
bouncy fetch https://news.example.com --block-host ads.example.net --block-host metrics.example.net
```

### Scrape in parallel

```bash
bouncy scrape url1 url2 url3 \
  --concurrency 25 \
  --eval "document.querySelector('h1').textContent" \
  --format json

# Per-host throttle: at most 2 in-flight against any single origin
# even with --concurrency 25, so you don't hammer one server.
bouncy scrape urls.txt --concurrency 25 --per-host-concurrency 2

# Selector extraction per row — adds a `selected: [...]` field to each
# JSON row. No V8 boot required.
bouncy scrape urls.txt --select "h1" --format json | jq '.results[].selected'

# Identify yourself with a custom UA. Default identifies as bouncy.
bouncy scrape urls.txt --user-agent "my-bot/1.0 (+contact@example.com)"
```

#### Live dashboard (`--tui`)

For a long parallel job, swap the JSON / text summary for a live ratatui dashboard — per-URL status grid (queued / in-flight / 200 / retry / failed), throughput gauge, p50 / p95 / max latency, status code histogram. Off by default; explicit opt-in:

```bash
bouncy scrape urls.txt --concurrency 50 --tui
```

`q` (or `Esc`) quits, `↑↓` / `jk` scrolls the URL list, `PgUp` / `PgDn` pages. Requires stdout to be a terminal — piping or redirecting with `--tui` set is rejected with an error so scripts never end up with TUI escape codes in their output. Built behind the default-on `tui` Cargo feature; `--no-default-features` builds skip the ratatui + crossterm dep tree entirely.

### Browse — interactive or scripted multi-step flows

When a single fetch isn't enough — log in, click through, fill a form, submit, read the result — `bouncy browse` opens a stateful session that keeps V8 + cookies alive across steps. Two modes:

```bash
# Scripted chain — non-interactive, scriptable, pipe-friendly.
bouncy browse https://help.com \
  --do "fill input[name=name] Maziar" \
  --do "fill input[name=email] me@x.test" \
  --do "submit form#signup" \
  --do "read h1"

# REPL — drop into an interactive prompt; one command per line.
bouncy browse https://help.com
> click a[href='/signup']
   ↳ snapshot @ https://help.com/signup — title="Sign up", 1 forms, 0 links, 1 buttons, 2 inputs, 1 headings
> fill input[name=email] me@x.test
> submit form
> exit

# JSON output — pipe the final snapshot into jq.
bouncy browse https://example.com --json --do "read h1" | jq .
```

Same primitives the `bouncy_browse_*` MCP tools expose, just driven from a shell instead of an LLM. See the CLI Reference below for the full command grammar.

### MCP server

`bouncy-mcp` is a separate binary (shipped in the same release tarball) that exposes bouncy as a Model Context Protocol server, so LLM clients like Claude Desktop and Claude Code can call bouncy as typed tools instead of shelling out.

| Tool | Path | What it does |
|---|---|---|
| `fetch` | HTTP | Raw fetch with optional method / headers / body / basic auth / cookies / proxy / **`user_agent`** / **`select`** + **`select_attr`** for CSS-selector extraction |
| `extract_title` | static | `<title>` text from an HTML string |
| `extract_text` | static | Visible body text from an HTML string |
| `extract_links` | static | All `<a href>` links resolved against a base URL |
| `js_eval` | V8 | Fetch a URL, boot V8, run a JS expression, return the result |
| `scrape` | auto | Single URL: auto JS-vs-static branch, optional eval / selector wait, configurable retries, plus `user_agent` and `select` / `select_attr` for static extraction |
| `scrape_many` | auto | URL list, scraped sequentially. Accepts `user_agent`, `select`, `select_attr`, and `per_host_concurrency` (latter is advisory on the MCP today since runs are serialized) |
| `bouncy_browse_open` | session | Open a stateful browse session at a URL. Returns `session_id` + initial page snapshot (forms / links / buttons / inputs / headings / meta / text_summary). Sessions auto-expire after 15 min idle, capped at 20 per server. |
| `bouncy_browse_click` | session | Fire a synthetic click on the matched element; drains any `location.href` redirects. Returns the new snapshot. |
| `bouncy_browse_fill` | session | Set a form field's value and dispatch synthetic `input` + `change` events. Returns the new snapshot. |
| `bouncy_browse_submit` | session | Submit the form (or the form containing the matched submit button). Real HTTP POST/GET for `<form action>`; synthetic `submit` event for JS-only forms. Returns the new snapshot. |
| `bouncy_browse_goto` | session | Navigate to a fresh URL inside the same session. Cookies persist. Returns the new snapshot. |
| `bouncy_browse_read` | session | Read text / HTML / attribute values from every element matching `selector`. `mode` is `"text"` / `"html"` / `"attr:NAME"`. Pure read; no snapshot. |
| `bouncy_browse_eval` | session | Escape hatch: arbitrary JS in the session's V8 context. Returns the result + new snapshot. |
| `bouncy_browse_close` | session | Close a session and free its V8 isolate. Idempotent. |

The **`bouncy_browse_*`** tools turn bouncy into a stateful browser Claude (or any MCP client) can drive autonomously: open a page, read the snapshot, click links, fill forms, submit them, extract data — all in one held-open session that persists cookies + JS state across calls. No Chromium dependency. Single 40 MB binary.

**Claude Desktop** — add to `~/Library/Application Support/Claude/claude_desktop_config.json` (macOS) or the equivalent on your platform:

```json
{
  "mcpServers": {
    "bouncy": { "command": "/usr/local/bin/bouncy-mcp" }
  }
}
```

**Claude Code:**

```bash
claude mcp add bouncy bouncy-mcp
```

V8 startup is lazy — sessions that only call `fetch` / `extract_*` never boot V8. The first JS-using call (`js_eval`, or `scrape` with `eval` / `selector`) takes 2–3 s; subsequent JS calls reuse the warm isolate.

#### Debugging tool calls

To poke at the MCP server interactively without going through Claude (great for verifying tools, seeing schemas, sanity-checking responses), use the official inspector:

```bash
npx @modelcontextprotocol/inspector bouncy-mcp
```

Opens a web UI where you can list every tool, fill in arguments, fire calls, and see the raw JSON-RPC traffic.

### CDP server (Playwright)

```bash
bouncy serve --port 9222
# → ws://127.0.0.1:9222/devtools/browser/<id>
```

Speaks Chrome DevTools Protocol for `Runtime.evaluate`, `Page.navigate`, `DOM.querySelector`, `DOM.getOuterHTML`, `Network.setExtraHTTPHeaders`, `Browser.getVersion`, plus the no-op handshake methods `puppeteer-core` fires on connect. `Input.dispatchMouseEvent` is acknowledged so click flows don't bail, but actual hit-testing requires layout — use `page.evaluate("document.querySelector(...).click()")` instead, which goes through our real synthetic-event path.

## Benchmarks

20 runs/cell with [hyperfine](https://github.com/sharkdp/hyperfine), identical local fixture server, Linux x86_64. Chrome via Playwright (`chromium.launch()` per run — same cold-start cost bouncy pays).

| Page                  | bouncy | Chrome (Playwright) | Speedup |
|-----------------------|------:|--------------------:|--------:|
| Static HTML           | 10 ms |              535 ms |     54× |
| JS + XHR + fetch      | 14 ms |              534 ms |     37× |
| Dynamic scripts       | 14 ms |              531 ms |     38× |
| 100-URL parallel      | 56 ms |             5753 ms |    103× |

Peak RSS: bouncy ~24 MB vs Chrome ~118 MB.

## CLI Reference

### `bouncy fetch <URL>`

Fetch and (optionally) render a single page.

| Flag | Default | Description |
|---|---|---|
| `--dump` | `html` | Output: `html`, `text`, or `links` |
| `--select` | — | CSS selector for static text extraction (one match per line). Bypasses `--dump`. |
| `--attr` | — | Pair with `--select` to extract attribute values instead of text |
| `--eval` | — | JavaScript expression to evaluate (boots V8) |
| `--selector` | — | Wait for this CSS selector before dumping (boots V8). For static extraction use `--select` instead. |
| `--wait` | `5` | Selector wait timeout in seconds |
| `-X`, `--method` | `GET` | HTTP method |
| `-H`, `--header` | — | Repeatable. Format: `Name: Value` |
| `--body` | — | Inline request body |
| `--body-file` | — | Read request body from file |
| `--json` | — | Inline JSON body. Sets `Content-Type: application/json` if you didn't |
| `--auth` | — | Basic auth, `user:pass`. Sets `Authorization: Basic …` |
| `-o`, `--output` | stdout | Write the response body to PATH instead of stdout |
| `--proxy` | — | HTTP CONNECT proxy URL |
| `--timeout` | `30` | Per-request timeout in seconds (whole fetch) |
| `--cookie-jar` | — | JSON cookie jar; loaded before, saved after — persists across runs |
| `--block-trackers` | off | Drop requests to a built-in list of ad / analytics hosts |
| `--block-host` | — | Repeatable. Extra hosts to block (suffix-matched) |
| `--ca-file` | — | Repeatable. Trust extra root CA(s) from PEM file(s) |
| `--max-redirects` | `10` | Hops to follow on 3xx. 0 disables following. |
| `--stealth` | off | Hide `navigator.webdriver`, mask polyfills, Chrome UA |
| `--user-agent` | — | UA override |
| `--quiet` | off | Suppress banner |

### `bouncy scrape <URL...>`

Scrape multiple URLs in parallel.

| Flag | Default | Description |
|---|---|---|
| `--concurrency` | `10` | Parallel workers |
| `--per-host-concurrency` | — | Cap on simultaneous requests against any single host. Default: no per-host cap. |
| `--eval` | — | JS expression per page (boots V8 per row when set) |
| `--select` | — | CSS selector for static text/attribute extraction per row. Result lands in a `selected: [...]` field. |
| `--attr` | — | Pair with `--select` to extract attribute values instead of text. |
| `--user-agent` | — | UA override. Default: `bouncy/<version> (+repo URL)` |
| `--format` | `json` | Output: `json` or `text` |
| `--timeout` | `60` | Per-URL timeout in seconds |
| `--cookie-jar` | — | JSON cookie jar; loaded before, saved after — persists across runs |
| `--block-trackers` | off | Drop requests to a built-in list of ad / analytics hosts |
| `--block-host` | — | Repeatable. Extra hosts to block (suffix-matched) |
| `--ca-file` | — | Repeatable. Trust extra root CA(s) from PEM file(s) |
| `--max-redirects` | `10` | Hops to follow on 3xx. 0 disables following. |
| `--retry` | `0` | Retry transient failures (network errors, 429, 5xx) up to N times per URL |
| `--retry-delay-ms` | `250` | Initial backoff. Each retry waits `delay × 2^attempt`, capped at 30 s |
| `--tui` | off | Live ratatui dashboard instead of the JSON / text summary. Requires stdout to be a terminal. |

### `bouncy serve`

Run a Chrome DevTools Protocol server.

| Flag | Default | Description |
|---|---|---|
| `-p`, `--port` | `9222` | WebSocket port |
| `--host` | `127.0.0.1` | Bind address |

### `bouncy browse <URL>`

Open a stateful browse session — same V8 + cookie jar persists across `click` / `fill` / `submit` / `goto` / `read` / `eval` steps. Two modes:

- **Scripted chain** (non-interactive, scriptable):

  ```bash
  bouncy browse https://help.com \
    --do "fill input[name=name] Maziar" \
    --do "fill input[name=email] me@x.test" \
    --do "submit form#signup" \
    --do "read h1"
  ```

- **REPL** (no `--do`): drops into an interactive prompt; one command per line; `exit` quits. Pipes work too — `bouncy browse <url> < script.txt`.

Command grammar (same in both modes):

```text
click <selector>                fire synthetic click on matched element
fill  <selector> <value>        set input value (fires input + change events)
submit <selector>               submit form (or form containing the matched button)
goto  <url>                     navigate this session to a new URL
read  <selector> [mode]         mode: text (default) | html | attr:NAME
eval  <js>                      evaluate JS in the page's V8 context
snapshot                        re-print the current page snapshot
help                            show help
exit                            quit (REPL only)
```

| Flag | Default | Description |
|---|---|---|
| `--do` | — | Repeatable. Each value is a single command string. Without `--do`, drops into a REPL on stdin. |
| `--json` | off | Emit final snapshot (chain) or per-step output (REPL) as JSON instead of text — pipe into `jq`. |
| `--user-agent` | — | UA override. Defaults to `bouncy/<version> (+repo URL)`. |
| `--stealth` | off | Enable canvas / audio / WebGPU / battery fingerprint randomization. |

## License

MIT — see [LICENSE](./LICENSE).
