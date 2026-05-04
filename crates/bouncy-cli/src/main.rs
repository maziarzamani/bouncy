//! `bouncy` — single-binary headless browser CLI.
//!
//! Three subcommand families share one binary:
//!   - `fetch` / `scrape` — static or batched HTML extraction. Static
//!     pages take the lol-html / hyper-rustls path directly; JavaScript
//!     only boots when `--eval` or `--selector` is given (lazy V8 init),
//!     so static workloads pay no V8 tax even though the binary ships V8.
//!   - `browse` — stateful multi-step browser session: open a URL,
//!     click / fill / submit / goto / read across pages with V8 +
//!     cookies preserved. Scriptable via `--do "step"` chains, or
//!     interactive as a REPL.
//!   - `serve` — Chrome DevTools Protocol server, drop-in for
//!     Playwright / puppeteer-core clients.
//!
//! Examples:
//!   bouncy fetch URL [--dump html|text|links]
//!   bouncy fetch URL --eval "document.title"
//!   bouncy fetch URL --selector '[data-ready=\"1\"]' --dump html
//!   bouncy fetch URL -X POST --body '...' -H 'Authorization: ...'
//!   bouncy scrape URL... [--concurrency N] [--format json|text]
//!   bouncy browse URL --do "fill input[name=q] hello" --do "submit form"
//!   bouncy browse URL                                    # interactive REPL

mod browse;
mod scrape;
mod select;

#[cfg(feature = "tui")]
mod scrape_tui;

use std::io::Write;
use std::sync::Arc;

use bouncy_extract::{extract_links, extract_text};
use bouncy_fetch::Fetcher;
use bouncy_js::Runtime;
use clap::{Parser, Subcommand, ValueEnum};
use url::Url;

#[derive(Parser, Debug)]
#[command(
    name = "bouncy",
    version,
    about = "Headless scraping + browsing CLI (bouncy)"
)]
struct Args {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand, Debug)]
// `Fetch` legitimately has lots of optional flags; one Cmd value lives
// per process so the size delta is harmless. Boxing every variant just
// to please the lint would obscure clap's derive output.
#[allow(clippy::large_enum_variant)]
enum Cmd {
    /// Fetch a single URL (boots V8 only when --eval / --selector asks for it).
    Fetch {
        url: String,
        #[arg(long, value_enum, default_value_t = DumpFormat::Html)]
        dump: DumpFormat,
        /// JS expression evaluated against the loaded DOM.
        #[arg(long, short)]
        eval: Option<String>,
        /// Wait for this CSS selector to match before dumping.
        /// (For element extraction, use `--select` instead.)
        #[arg(long)]
        selector: Option<String>,
        /// Extract the text content of every element matching this CSS
        /// selector and print one result per line. Skips the `--dump`
        /// path entirely. Use `--attr NAME` to extract an attribute
        /// value instead of text.
        ///
        /// Selector grammar: tag, `#id`, `.class`, `[attr]`,
        /// `[attr=value]` (no combinators or pseudo-classes yet).
        #[arg(long = "select")]
        select: Option<String>,
        /// Pair with `--select` to extract this attribute's value
        /// instead of text content. Elements without the attribute are
        /// silently skipped.
        #[arg(long = "attr", requires = "select")]
        attr: Option<String>,
        #[arg(long, default_value = "load")]
        wait_until: String,
        #[arg(long, default_value_t = 5)]
        wait: u64,
        #[arg(long)]
        user_agent: Option<String>,
        #[arg(long)]
        stealth: bool,
        #[arg(long, short)]
        quiet: bool,
        /// HTTP method (GET / POST / PUT / DELETE / ...).
        #[arg(long, short = 'X', default_value = "GET")]
        method: String,
        /// Request headers, repeatable. Format: `Name: Value`.
        #[arg(long = "header", short = 'H')]
        headers: Vec<String>,
        /// Inline request body. Mutually exclusive with `--body-file`.
        #[arg(long, conflicts_with = "body_file")]
        body: Option<String>,
        /// Read request body from a file.
        #[arg(long = "body-file")]
        body_file: Option<std::path::PathBuf>,
        /// Inline JSON body. Sets `Content-Type: application/json`
        /// automatically (unless one was already supplied via `-H`).
        #[arg(long, conflicts_with_all = ["body", "body_file"])]
        json: Option<String>,
        /// Basic auth, `user:pass`. Encodes a `Authorization: Basic …`
        /// header. Mutually exclusive with passing the same header by
        /// hand.
        #[arg(long)]
        auth: Option<String>,
        /// Write the response body to PATH instead of stdout.
        #[arg(long, short = 'o')]
        output: Option<std::path::PathBuf>,
        /// HTTP CONNECT proxy URL (e.g., `http://proxy.test:3128`).
        #[arg(long)]
        proxy: Option<String>,
        /// Per-request timeout in seconds. Wraps the whole fetch.
        #[arg(long, default_value_t = 30)]
        timeout: u64,
        /// Cookie jar file (JSON). Loaded before the fetch, written back
        /// after — so cookies persist across CLI invocations.
        #[arg(long = "cookie-jar")]
        cookie_jar: Option<std::path::PathBuf>,
        /// Block requests to known ad / analytics hosts (built-in list).
        #[arg(long = "block-trackers")]
        block_trackers: bool,
        /// Extra hosts to block (repeatable). Implies `--block-trackers`
        /// for matching purposes.
        #[arg(long = "block-host")]
        block_hosts: Vec<String>,
        /// Trust extra root CA(s) from PEM file(s). Repeatable.
        #[arg(long = "ca-file")]
        ca_files: Vec<std::path::PathBuf>,
        /// Maximum redirect hops to follow. 0 disables following.
        #[arg(long = "max-redirects", default_value_t = 10)]
        max_redirects: u32,
    },
    /// Scrape many URLs in parallel.
    Scrape {
        urls: Vec<String>,
        /// JS expression to evaluate per URL (boots V8 per row when set).
        #[arg(long, short)]
        eval: Option<String>,
        #[arg(long, default_value_t = 10)]
        concurrency: usize,
        #[arg(long, default_value = "json")]
        format: String,
        #[arg(long, default_value_t = 60)]
        timeout: u64,
        /// Cookie jar file (JSON). Loaded before the scrape, written
        /// back after — persists cookies across CLI invocations.
        #[arg(long = "cookie-jar")]
        cookie_jar: Option<std::path::PathBuf>,
        /// Block requests to known ad / analytics hosts (built-in list).
        #[arg(long = "block-trackers")]
        block_trackers: bool,
        /// Extra hosts to block (repeatable).
        #[arg(long = "block-host")]
        block_hosts: Vec<String>,
        /// Trust extra root CA(s) from PEM file(s). Repeatable.
        #[arg(long = "ca-file")]
        ca_files: Vec<std::path::PathBuf>,
        /// Maximum redirect hops to follow. 0 disables following.
        #[arg(long = "max-redirects", default_value_t = 10)]
        max_redirects: u32,
        /// Retry transient failures (network errors, 429, 5xx) up to N
        /// times per URL with exponential backoff. 0 disables retry.
        #[arg(long, default_value_t = 0)]
        retry: u32,
        /// Initial backoff in milliseconds. Each subsequent retry waits
        /// `delay * 2^attempt` (capped at 30 s).
        #[arg(long = "retry-delay-ms", default_value_t = 250)]
        retry_delay_ms: u64,
        /// Cap on simultaneous requests against any single host. Defaults
        /// to no per-host cap (the overall `--concurrency` is the only
        /// ceiling). Set this when scraping many URLs from one origin
        /// to avoid hammering the server and getting banned.
        #[arg(long = "per-host-concurrency")]
        per_host_concurrency: Option<usize>,
        /// User-Agent header sent with each request. Defaults to
        /// `bouncy/<version> (+repo URL)`. Override to identify yourself
        /// or to mimic a specific client.
        #[arg(long = "user-agent")]
        user_agent: Option<String>,
        /// Extract the text content of every element matching this CSS
        /// selector, per URL. Output rows gain a `selected: [...]` field
        /// (JSON) or a tab-separated extra column (text).
        ///
        /// Selector grammar: tag, `#id`, `.class`, `[attr]`,
        /// `[attr=value]` (no combinators or pseudo-classes yet).
        #[arg(long = "select")]
        select: Option<String>,
        /// Pair with `--select` to extract this attribute's value
        /// instead of text content. Elements without the attribute are
        /// silently skipped.
        #[arg(long = "attr", requires = "select")]
        attr: Option<String>,
        /// Live ratatui dashboard instead of the JSON / text summary.
        /// Off by default. Requires stdout to be a terminal.
        #[cfg(feature = "tui")]
        #[arg(long, default_value_t = false)]
        tui: bool,
    },
    /// Run a Chrome DevTools Protocol server (Playwright drop-in).
    Serve {
        #[arg(long, short, default_value_t = 9222)]
        port: u16,
        #[arg(long, default_value = "127.0.0.1")]
        host: String,
    },
    /// Open a stateful browse session — the same V8 + cookie jar
    /// persists across `click` / `fill` / `submit` / `goto` / `read`
    /// / `eval` steps. Two modes:
    ///   - **Scripted chain**: `bouncy browse <url> --do "click sel"
    ///     --do "fill sel value" --do "submit form"` — non-interactive,
    ///     scriptable, exits when the chain finishes.
    ///   - **REPL**: `bouncy browse <url>` (no `--do`) — drops into an
    ///     interactive prompt; one command per line; `exit` quits.
    Browse {
        /// Initial URL to open.
        url: String,
        /// Run a series of commands non-interactively. Repeatable.
        /// Each value is a single command string (e.g. `"click .btn"`,
        /// `"fill input[name=q] rust"`, `"submit form#search"`).
        ///
        /// Without `--do`, the session drops into an interactive REPL.
        #[arg(long = "do")]
        do_steps: Vec<String>,
        /// Emit the final snapshot (or each step's snapshot) as JSON
        /// instead of the human-friendly text format. Useful for piping
        /// into `jq` or downstream tools.
        #[arg(long, default_value_t = false)]
        json: bool,
        /// Override the outgoing User-Agent for this session.
        #[arg(long = "user-agent")]
        user_agent: Option<String>,
        /// Enable bouncy's stealth patches (canvas/audio/WebGPU/battery
        /// fingerprint randomization, hidden navigator.webdriver).
        #[arg(long, default_value_t = false)]
        stealth: bool,
    },
}

#[derive(Copy, Clone, Debug, ValueEnum)]
enum DumpFormat {
    Html,
    Text,
    Links,
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    match args.cmd {
        Cmd::Fetch {
            url,
            dump,
            eval,
            selector,
            select,
            attr,
            wait,
            user_agent,
            method,
            headers,
            body,
            body_file,
            json,
            auth,
            output,
            proxy,
            stealth,
            timeout,
            cookie_jar,
            block_trackers,
            block_hosts,
            ca_files,
            max_redirects,
            ..
        } => {
            fetch_one(
                &url,
                dump,
                eval.as_deref(),
                selector.as_deref(),
                select.as_deref(),
                attr.as_deref(),
                wait,
                user_agent.as_deref(),
                &method,
                &headers,
                body.as_deref(),
                body_file.as_deref(),
                json.as_deref(),
                auth.as_deref(),
                output.as_deref(),
                proxy.as_deref(),
                stealth,
                timeout,
                cookie_jar.as_deref(),
                block_trackers,
                &block_hosts,
                &ca_files,
                max_redirects,
            )
            .await
        }
        Cmd::Scrape {
            urls,
            eval,
            concurrency,
            format,
            cookie_jar,
            block_trackers,
            block_hosts,
            ca_files,
            max_redirects,
            retry,
            retry_delay_ms,
            per_host_concurrency,
            user_agent,
            select,
            attr,
            #[cfg(feature = "tui")]
            tui,
            ..
        } => {
            #[cfg(feature = "tui")]
            if tui {
                use std::io::IsTerminal;
                if !std::io::stdout().is_terminal() {
                    anyhow::bail!("--tui requires stdout to be a terminal; got pipe / redirect");
                }
                let total_urls = urls.len();
                let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
                let scrape_handle = tokio::spawn(scrape::scrape(
                    urls,
                    concurrency,
                    format,
                    eval,
                    cookie_jar,
                    block_trackers,
                    block_hosts,
                    ca_files,
                    max_redirects,
                    retry,
                    retry_delay_ms,
                    per_host_concurrency,
                    user_agent.clone(),
                    select.clone(),
                    attr.clone(),
                    Some(tx),
                ));
                let tui_result = scrape_tui::run_tui(rx, total_urls).await;
                scrape_handle.abort();
                let _ = scrape_handle.await;
                return tui_result;
            }
            scrape::scrape(
                urls,
                concurrency,
                format,
                eval,
                cookie_jar,
                block_trackers,
                block_hosts,
                ca_files,
                max_redirects,
                retry,
                retry_delay_ms,
                per_host_concurrency,
                user_agent,
                select,
                attr,
                None,
            )
            .await
        }
        Cmd::Serve { port, host } => serve(&host, port).await,
        Cmd::Browse {
            url,
            do_steps,
            json,
            user_agent,
            stealth,
        } => {
            browse::run(browse::Args {
                url,
                do_steps,
                json,
                user_agent,
                stealth,
            })
            .await
        }
    }
}

async fn serve(host: &str, port: u16) -> anyhow::Result<()> {
    let fetcher = Arc::new(Fetcher::new()?);
    let bind_addr = format!("{host}:{port}");
    let server = bouncy_cdp::Server::new(fetcher).bind(&bind_addr).await?;
    let local = server.local_addr();
    eprintln!(
        "bouncy serve listening on ws://{local}/devtools/browser/<id> (CDP — Playwright drop-in)"
    );
    server.serve().await?;
    Ok(())
}

/// Split a `--header` string into `(name, value)`. Forgiving about extra
/// whitespace around the colon.
fn parse_header(raw: &str) -> anyhow::Result<(&str, &str)> {
    let (name, value) = raw
        .split_once(':')
        .ok_or_else(|| anyhow::anyhow!("invalid --header {raw:?} (expected `Name: Value`)"))?;
    Ok((name.trim(), value.trim_start()))
}

/// Return a writer pointing at either `path` or stdout. Box-erased so
/// the rest of `fetch_one` doesn't care which variant it has.
fn open_output(path: Option<&std::path::Path>) -> anyhow::Result<Box<dyn Write>> {
    match path {
        Some(p) => {
            if let Some(parent) = p.parent() {
                if !parent.as_os_str().is_empty() {
                    std::fs::create_dir_all(parent)?;
                }
            }
            Ok(Box::new(std::fs::File::create(p)?))
        }
        None => Ok(Box::new(std::io::stdout())),
    }
}

/// Standard base64 (RFC 4648) — 16 lines, doesn't justify pulling a
/// crate in for one Authorization header. Output is plain ASCII.
fn base64_encode(input: &[u8]) -> String {
    const ALPHA: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    let mut chunks = input.chunks_exact(3);
    for c in chunks.by_ref() {
        let n = (u32::from(c[0]) << 16) | (u32::from(c[1]) << 8) | u32::from(c[2]);
        out.push(ALPHA[((n >> 18) & 0x3f) as usize] as char);
        out.push(ALPHA[((n >> 12) & 0x3f) as usize] as char);
        out.push(ALPHA[((n >> 6) & 0x3f) as usize] as char);
        out.push(ALPHA[(n & 0x3f) as usize] as char);
    }
    let rem = chunks.remainder();
    match rem.len() {
        1 => {
            let n = u32::from(rem[0]) << 16;
            out.push(ALPHA[((n >> 18) & 0x3f) as usize] as char);
            out.push(ALPHA[((n >> 12) & 0x3f) as usize] as char);
            out.push('=');
            out.push('=');
        }
        2 => {
            let n = (u32::from(rem[0]) << 16) | (u32::from(rem[1]) << 8);
            out.push(ALPHA[((n >> 18) & 0x3f) as usize] as char);
            out.push(ALPHA[((n >> 12) & 0x3f) as usize] as char);
            out.push(ALPHA[((n >> 6) & 0x3f) as usize] as char);
            out.push('=');
        }
        _ => {}
    }
    out
}

#[allow(clippy::too_many_arguments)]
async fn fetch_one(
    url: &str,
    dump: DumpFormat,
    eval: Option<&str>,
    selector: Option<&str>,
    select: Option<&str>,
    select_attr: Option<&str>,
    wait_secs: u64,
    user_agent: Option<&str>,
    method: &str,
    headers: &[String],
    body: Option<&str>,
    body_file: Option<&std::path::Path>,
    json: Option<&str>,
    auth: Option<&str>,
    output: Option<&std::path::Path>,
    proxy: Option<&str>,
    stealth: bool,
    timeout_secs: u64,
    cookie_jar_path: Option<&std::path::Path>,
    block_trackers: bool,
    block_hosts: &[String],
    ca_files: &[std::path::PathBuf],
    max_redirects: u32,
) -> anyhow::Result<()> {
    let jar = load_cookie_jar(cookie_jar_path)?;
    let fetcher = Arc::new({
        let mut b = bouncy_fetch::Fetcher::builder().max_redirects(max_redirects);
        if let Some(p) = proxy {
            b = b.proxy(p.to_string());
        }
        if timeout_secs > 0 {
            b = b.request_timeout(std::time::Duration::from_secs(timeout_secs));
        }
        if let Some(j) = jar.clone() {
            b = b.cookie_jar(j);
        }
        if let Some(bl) = build_blocklist(block_trackers, block_hosts) {
            b = b.tracker_blocklist(bl);
        }
        for path in ca_files {
            b = b.ca_file(path);
        }
        if let Some(ua) = user_agent {
            b = b.user_agent(ua);
        }
        b.build()?
    });
    let _ = Fetcher::new; // keep the import live without warning suppression

    // Build the request — method, headers, body — and fetch.
    // --json sets a JSON body and adds Content-Type if the user didn't.
    // --body / --body-file / --json are mutually exclusive (clap enforces).
    let body_bytes: Option<bytes::Bytes> = match (body, body_file, json) {
        (Some(s), _, _) => Some(bytes::Bytes::copy_from_slice(s.as_bytes())),
        (None, Some(path), _) => Some(bytes::Bytes::from(std::fs::read(path)?)),
        (None, None, Some(s)) => Some(bytes::Bytes::copy_from_slice(s.as_bytes())),
        (None, None, None) => None,
    };
    let mut req = bouncy_fetch::FetchRequest::new(url).method(method);
    let mut saw_content_type = false;
    let mut saw_authorization = false;
    for h in headers {
        let (name, value) = parse_header(h)?;
        if name.eq_ignore_ascii_case("content-type") {
            saw_content_type = true;
        }
        if name.eq_ignore_ascii_case("authorization") {
            saw_authorization = true;
        }
        req = req.header(name, value);
    }
    if json.is_some() && !saw_content_type {
        req = req.header("Content-Type", "application/json");
    }
    if let Some(creds) = auth {
        if saw_authorization {
            anyhow::bail!("--auth conflicts with an explicit Authorization header — pick one");
        }
        req = req.header(
            "Authorization",
            format!("Basic {}", base64_encode(creds.as_bytes())),
        );
    }
    if let Some(b) = body_bytes {
        req = req.body_bytes(b);
    }
    let resp = fetcher.request(req).await?;

    // Lazy-V8: boot only if the user wants JS execution. Static workloads
    // never touch V8 even though it's linked into the binary.
    let needs_js = eval.is_some() || selector.is_some() || stealth;
    let html_body = if needs_js {
        let mut rt = Runtime::new(tokio::runtime::Handle::current(), fetcher.clone());
        rt.set_stealth(stealth);
        let html_str = std::str::from_utf8(&resp.body)?;
        rt.load(html_str, url)?;
        rt.run_inline_scripts()?;
        // Follow up to MAX_NAV_HOPS `location.href = '...'` redirects. Cap
        // is here so a runaway script can't loop forever; mid-script
        // suspension isn't supported (RECIPE.md "What's not in v1").
        const MAX_NAV_HOPS: u32 = 10;
        let mut hops = 0u32;
        while let Some(next_url) = rt.take_pending_nav() {
            if hops >= MAX_NAV_HOPS {
                break;
            }
            hops += 1;
            let next_resp = fetcher
                .request(bouncy_fetch::FetchRequest::new(&next_url))
                .await?;
            let next_html = std::str::from_utf8(&next_resp.body)?;
            rt.load(next_html, &next_url)?;
            rt.run_inline_scripts()?;
        }
        if let Some(sel) = selector {
            let timeout_ms = wait_secs.saturating_mul(1000);
            let _ = rt.wait_for_selector(sel, timeout_ms).await?;
        }
        if let Some(expr) = eval {
            // --eval short-circuits: print just the expression's result.
            let v = rt.eval(expr)?;
            let mut out = open_output(output)?;
            writeln!(out, "{v}")?;
            return Ok(());
        }
        rt.dump_html()?.into_bytes()
    } else {
        resp.body.to_vec()
    };

    let mut out = open_output(output)?;
    if let Some(sel) = select {
        // CSS-selector extraction short-circuits the --dump path: one
        // result per line. `--attr NAME` switches from text to attribute
        // value extraction.
        let html_str = std::str::from_utf8(&html_body)?;
        let lines = match select_attr {
            Some(attr_name) => select::select_attr(html_str, sel, attr_name)?,
            None => select::select_text(html_str, sel)?,
        };
        for line in lines {
            writeln!(out, "{}", line)?;
        }
    } else {
        match dump {
            DumpFormat::Html => out.write_all(&html_body)?,
            DumpFormat::Text => {
                let t = extract_text(&html_body)?;
                out.write_all(t.as_bytes())?;
            }
            DumpFormat::Links => {
                let base = Url::parse(url)?;
                for l in extract_links(&html_body, &base)? {
                    writeln!(out, "{}\t{}", l.url, l.text)?;
                }
            }
        }
    }
    drop(out);

    // Persist the cookie jar back to disk if --cookie-jar was given.
    if let Some(j) = jar {
        save_cookie_jar(cookie_jar_path, &j)?;
    }
    Ok(())
}

/// Read a JSON cookie jar from `path` if the file exists and is non-empty,
/// otherwise return None (the caller proceeds without persisted cookies).
pub(crate) fn load_cookie_jar(
    path: Option<&std::path::Path>,
) -> anyhow::Result<Option<bouncy_fetch::CookieJar>> {
    let Some(p) = path else {
        return Ok(None);
    };
    if !p.exists() {
        return Ok(Some(bouncy_fetch::CookieJar::new()));
    }
    let bytes = std::fs::read(p)?;
    if bytes.is_empty() {
        return Ok(Some(bouncy_fetch::CookieJar::new()));
    }
    let s = std::str::from_utf8(&bytes)?;
    Ok(Some(bouncy_fetch::CookieJar::from_json(s)?))
}

/// Combine `--block-trackers` (built-in list) and `--block-host` (extra
/// hosts) into a single TrackerBlocklist, or None if neither was given.
pub(crate) fn build_blocklist(
    block_trackers: bool,
    extra_hosts: &[String],
) -> Option<bouncy_fetch::TrackerBlocklist> {
    if !block_trackers && extra_hosts.is_empty() {
        return None;
    }
    let mut hosts: Vec<String> = if block_trackers {
        bouncy_fetch::TrackerBlocklist::default_set()
            .hosts_iter()
            .collect()
    } else {
        Vec::new()
    };
    hosts.extend(extra_hosts.iter().cloned());
    Some(bouncy_fetch::TrackerBlocklist::from_hosts(hosts))
}

pub(crate) fn save_cookie_jar(
    path: Option<&std::path::Path>,
    jar: &bouncy_fetch::CookieJar,
) -> anyhow::Result<()> {
    if let Some(p) = path {
        if let Some(parent) = p.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }
        std::fs::write(p, jar.to_json())?;
    }
    Ok(())
}
