# bouncy

Tiny Rust headless browser. Scrape pages or drive multi-step browse sessions
(click / fill / submit / read) — from a CLI, a library, or autonomously from
Claude (MCP).

This is the **umbrella facade crate**. It re-exports the most commonly used
types from the `bouncy-*` workspace so you don't have to pick the right
sub-crate up front:

```toml
[dependencies]
bouncy = "0.1"
```

By default you get `bouncy::fetch` (HTTP client) and `bouncy::extract`
(streaming title/text/link extractors) — the static scraping path, no V8.

For the stateful browser surface (sessions, click, fill, submit, read with
structured page snapshots), enable the `browse` feature:

```toml
[dependencies]
bouncy = { version = "0.1", features = ["browse"] }
```

## Surface

| Module           | Feature   | What's in it |
|------------------|-----------|---|
| `bouncy::fetch`   | `fetch`   | `Fetcher`, `FetchRequest`, `Response`, `CookieJar` |
| `bouncy::extract` | `extract` | `extract_title`, `extract_text`, `extract_links`, `Link` |
| `bouncy::browse`  | `browse`  | `BrowseSession`, `BrowseOpts`, `PageSnapshot`, `FormSnapshot`, `LinkSnapshot`, `ButtonSnapshot`, `InputSnapshot`, `HeadingSnapshot`, `ReadMode`, `EvalResult`, `BrowseError` |
| `bouncy::dom`     | `dom`     | `Document`, `NodeId` (lower-level DOM tree) |
| `bouncy::js`      | `js`      | `Runtime` (raw V8 with bouncy's bootstrap) |

`full` enables everything.

## CLI and MCP server

The `bouncy` library has two companion binaries that ship from the same
workspace:

- **`bouncy` CLI** — `cargo install bouncy-cli`. `bouncy scrape <url>`
  for static fetch, `bouncy browse <url>` for an interactive REPL or
  scripted multi-step flow.
- **`bouncy-mcp`** — `cargo install bouncy-mcp`. A Model Context Protocol
  server that exposes `bouncy_browse_*` tools so Claude Desktop / Cursor /
  any MCP client can drive a browse session autonomously.

See the [project README](https://github.com/maziarzamani/bouncy) for the
full story, comparison vs Chromium-based stacks, and a feature matrix.

## License

MIT.
