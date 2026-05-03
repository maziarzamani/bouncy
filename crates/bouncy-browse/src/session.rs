//! [`BrowseSession`] — a held-open browser session that persists V8 state,
//! cookies, and current-page state across multiple primitive calls.
//!
//! ## Threading model
//!
//! `bouncy_js::Runtime` owns a `v8::OwnedIsolate` which is `!Send` — V8
//! isolates can't cross OS threads. So we can't simply put a `Runtime`
//! behind a `Mutex` and `tokio::task::spawn_blocking` in/out of it,
//! because each `spawn_blocking` call may land on a different blocking-pool
//! thread.
//!
//! Solution: an **actor** pattern. When you call `BrowseSession::open`,
//! we spawn one long-lived blocking task that owns the `Runtime` for the
//! lifetime of the session. The public async methods send `Command`
//! messages over an unbounded mpsc channel; the actor processes them
//! one at a time and replies on a `oneshot`. The actor uses the parent
//! Tokio runtime's `Handle` to `block_on` async fetches from inside the
//! sync loop — same trick `bouncy-mcp::glue::render_js_blocking` uses.
//!
//! When `BrowseSession` is dropped, the command channel closes, the
//! actor exits, and the V8 isolate gets cleaned up on its thread.

use std::sync::Arc;

use bouncy_dom::Document;
use bouncy_fetch::{CookieJar, Fetcher};
use bouncy_js::Runtime;
use thiserror::Error;
use tokio::sync::{mpsc, oneshot};

use crate::snapshot::{PageSnapshot, SnapshotOpts};

/// Maximum number of `location.href` redirects followed in one navigation.
/// Mirrors the cap in `crates/bouncy-cli/src/main.rs`'s `fetch_one`.
const MAX_NAV_HOPS: u32 = 10;

#[derive(Debug, Error)]
pub enum BrowseError {
    #[error("fetch error: {0}")]
    Fetch(#[from] bouncy_fetch::Error),

    #[error("js runtime error: {0}")]
    Js(#[from] bouncy_js::Error),

    #[error("html parse error: {0}")]
    Dom(String),

    #[error("body is not valid UTF-8")]
    NotUtf8,

    #[error("session actor disappeared")]
    ActorGone,

    #[error("selector matched no elements: {0:?}")]
    NoMatch(String),

    #[error("invalid url: {0}")]
    Url(#[from] url::ParseError),

    #[error("io: {0}")]
    Io(String),
}

/// Configuration for a new browse session. `Default` is fine for most
/// callers; override `user_agent` and `stealth` to match the target site.
#[derive(Debug, Clone, Default)]
pub struct BrowseOpts {
    pub user_agent: Option<String>,
    pub stealth: bool,
    pub snapshot_opts: SnapshotOpts,
}

/// What [`BrowseSession::read`] should pull out of each matched element.
#[derive(Debug, Clone)]
pub enum ReadMode {
    /// `text_content()` — recursive visible text.
    Text,
    /// `outer_html()` — the element plus its tag.
    Html,
    /// Value of the named attribute. Elements without the attribute are
    /// silently skipped.
    Attr(String),
}

/// A held-open browser session. Cheap to clone the channel handle but
/// not the underlying actor — clone the surface for shared access.
pub struct BrowseSession {
    cmd_tx: mpsc::UnboundedSender<Command>,
}

impl BrowseSession {
    /// Open a new session at `url`. Returns the session and the initial
    /// page snapshot in one round trip.
    pub async fn open(url: &str, opts: BrowseOpts) -> Result<(Self, PageSnapshot), BrowseError> {
        let session = Self::spawn_actor(opts.clone());
        let snapshot = session
            .send(|reply| Command::Goto {
                url: url.to_string(),
                reply,
            })
            .await?;
        Ok((session, snapshot))
    }

    /// Navigate to a fresh URL inside the same session. Cookies and
    /// session state are preserved.
    pub async fn goto(&self, url: &str) -> Result<PageSnapshot, BrowseError> {
        self.send(|reply| Command::Goto {
            url: url.to_string(),
            reply,
        })
        .await
    }

    /// Fire a synthetic click on the matched element via the JS path
    /// (`document.querySelector(s).click()`). Drains any `location.href`
    /// navigations the click triggers, then returns the new snapshot.
    pub async fn click(&self, selector: &str) -> Result<PageSnapshot, BrowseError> {
        self.send(|reply| Command::Click {
            selector: selector.to_string(),
            reply,
        })
        .await
    }

    /// Set `value` on the matched form field and dispatch synthetic
    /// `input` and `change` events (so JS validators on the page see
    /// the change). Returns the new snapshot.
    pub async fn fill(&self, selector: &str, value: &str) -> Result<PageSnapshot, BrowseError> {
        self.send(|reply| Command::Fill {
            selector: selector.to_string(),
            value: value.to_string(),
            reply,
        })
        .await
    }

    /// Submit the form matched by `selector`. Three branches:
    ///   1. Form has an `action` attribute → build a real HTTP request
    ///      from the form's fields (urlencoded body for POST, query
    ///      string for GET) and load the response in this session.
    ///   2. Form has no `action` (JS-only handler) → dispatch a
    ///      `submit` event on the form; if the handler navigates via
    ///      `location.href = …`, that gets drained.
    ///   3. `selector` matches a submit `<button>` rather than a form →
    ///      climb to the enclosing `<form>` and apply the rules above.
    pub async fn submit(&self, selector: &str) -> Result<PageSnapshot, BrowseError> {
        self.send(|reply| Command::Submit {
            selector: selector.to_string(),
            reply,
        })
        .await
    }

    /// Read text / HTML / attribute values from every element matching
    /// `selector`. Pure read; doesn't mutate state and returns no snapshot.
    pub async fn read(&self, selector: &str, mode: ReadMode) -> Result<Vec<String>, BrowseError> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(Command::Read {
                selector: selector.to_string(),
                mode,
                reply: tx,
            })
            .map_err(|_| BrowseError::ActorGone)?;
        rx.await.map_err(|_| BrowseError::ActorGone)?
    }

    /// Escape hatch: evaluate arbitrary JS in the current page's V8
    /// context. Drains pending navigations after, then returns the new
    /// snapshot. Use sparingly; the higher-level primitives are safer.
    pub async fn eval(&self, expr: &str) -> Result<EvalResult, BrowseError> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(Command::Eval {
                expr: expr.to_string(),
                reply: tx,
            })
            .map_err(|_| BrowseError::ActorGone)?;
        rx.await.map_err(|_| BrowseError::ActorGone)?
    }

    /// Re-build a snapshot of the current page. Cheap (~1 ms for a typical
    /// page) — every state-changing primitive returns one already, so this
    /// is mostly useful when you want a fresh view without a state change.
    pub async fn snapshot(&self) -> Result<PageSnapshot, BrowseError> {
        self.send(|reply| Command::Snapshot { reply }).await
    }

    fn spawn_actor(opts: BrowseOpts) -> Self {
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel::<Command>();
        let outer_handle = tokio::runtime::Handle::current();
        // The actor lives on its own blocking-pool thread for the lifetime
        // of the session. When `cmd_tx` (held by `Self`) is dropped, the
        // recv loop exits and the V8 isolate is cleaned up on this thread.
        tokio::task::spawn_blocking(move || {
            actor_main(outer_handle, cmd_rx, opts);
        });
        Self { cmd_tx }
    }

    /// Helper for "send a snapshot-returning command and await its reply".
    async fn send<F>(&self, mk: F) -> Result<PageSnapshot, BrowseError>
    where
        F: FnOnce(oneshot::Sender<Result<PageSnapshot, BrowseError>>) -> Command,
    {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(mk(tx))
            .map_err(|_| BrowseError::ActorGone)?;
        rx.await.map_err(|_| BrowseError::ActorGone)?
    }
}

/// Eval result + a fresh snapshot of the page after the eval.
#[derive(Debug, Clone)]
pub struct EvalResult {
    pub result: String,
    pub snapshot: PageSnapshot,
}

enum Command {
    Goto {
        url: String,
        reply: oneshot::Sender<Result<PageSnapshot, BrowseError>>,
    },
    Click {
        selector: String,
        reply: oneshot::Sender<Result<PageSnapshot, BrowseError>>,
    },
    Fill {
        selector: String,
        value: String,
        reply: oneshot::Sender<Result<PageSnapshot, BrowseError>>,
    },
    Submit {
        selector: String,
        reply: oneshot::Sender<Result<PageSnapshot, BrowseError>>,
    },
    Read {
        selector: String,
        mode: ReadMode,
        reply: oneshot::Sender<Result<Vec<String>, BrowseError>>,
    },
    Eval {
        expr: String,
        reply: oneshot::Sender<Result<EvalResult, BrowseError>>,
    },
    Snapshot {
        reply: oneshot::Sender<Result<PageSnapshot, BrowseError>>,
    },
}

/// State held inside the actor for the lifetime of a session.
struct ActorState {
    rt: Runtime,
    fetcher: Arc<Fetcher>,
    current_url: String,
    snapshot_opts: SnapshotOpts,
    stealth: bool,
}

impl ActorState {
    fn new(outer_handle: &tokio::runtime::Handle, opts: &BrowseOpts) -> Result<Self, BrowseError> {
        // Always attach a cookie jar — session state across nav steps
        // is one of the main reasons callers reach for a session in the
        // first place. Login on page A, hit a protected page B, expect
        // the session cookie to replay.
        let mut builder = Fetcher::builder().cookie_jar(CookieJar::new());
        if let Some(ua) = &opts.user_agent {
            builder = builder.user_agent(ua.clone());
        }
        let fetcher = Arc::new(builder.build()?);
        let mut rt = Runtime::new(outer_handle.clone(), fetcher.clone());
        rt.set_stealth(opts.stealth);
        Ok(Self {
            rt,
            fetcher,
            current_url: String::new(),
            snapshot_opts: opts.snapshot_opts.clone(),
            stealth: opts.stealth,
        })
    }

    /// Fetch a URL, load the body into the runtime, run inline scripts,
    /// drain `location.href` navs (up to `MAX_NAV_HOPS`).
    fn navigate_blocking(
        &mut self,
        url: &str,
        outer_handle: &tokio::runtime::Handle,
    ) -> Result<(), BrowseError> {
        let mut current = url.to_string();
        for _ in 0..=MAX_NAV_HOPS {
            let resp = outer_handle.block_on(self.fetcher.get(&current))?;
            let body = std::str::from_utf8(&resp.body).map_err(|_| BrowseError::NotUtf8)?;
            self.rt.load(body, &current)?;
            self.rt.run_inline_scripts()?;
            self.current_url = current.clone();
            if let Some(next) = self.rt.take_pending_nav() {
                current = next;
            } else {
                return Ok(());
            }
        }
        Err(BrowseError::Io(format!(
            "exceeded MAX_NAV_HOPS ({MAX_NAV_HOPS}) following location.href chain"
        )))
    }

    /// Drain pending `location.href` redirects after a JS-triggered nav
    /// (e.g. a click that called `location.href = …`).
    fn drain_navs(&mut self, outer_handle: &tokio::runtime::Handle) -> Result<(), BrowseError> {
        for _ in 0..=MAX_NAV_HOPS {
            match self.rt.take_pending_nav() {
                None => return Ok(()),
                Some(next) => {
                    let resp = outer_handle.block_on(self.fetcher.get(&next))?;
                    let body = std::str::from_utf8(&resp.body).map_err(|_| BrowseError::NotUtf8)?;
                    self.rt.load(body, &next)?;
                    self.rt.run_inline_scripts()?;
                    self.current_url = next;
                }
            }
        }
        Err(BrowseError::Io(format!(
            "exceeded MAX_NAV_HOPS ({MAX_NAV_HOPS}) draining post-action navigations"
        )))
    }

    fn snapshot_now(&mut self) -> Result<PageSnapshot, BrowseError> {
        let html = self.rt.dump_html()?;
        let doc = Document::parse(&html).map_err(|e| BrowseError::Dom(e.to_string()))?;
        Ok(PageSnapshot::from_document(
            &doc,
            &self.current_url,
            self.snapshot_opts.clone(),
        ))
    }
}

fn actor_main(
    outer_handle: tokio::runtime::Handle,
    mut cmd_rx: mpsc::UnboundedReceiver<Command>,
    opts: BrowseOpts,
) {
    let mut state = match ActorState::new(&outer_handle, &opts) {
        Ok(s) => s,
        Err(e) => {
            // Drain commands and reply with the construction error so
            // callers don't hang.
            while let Some(cmd) = outer_handle.block_on(cmd_rx.recv()) {
                send_construction_failure(cmd, &e);
            }
            return;
        }
    };
    let _ = state.stealth; // currently informational; bridge already applies it

    while let Some(cmd) = outer_handle.block_on(cmd_rx.recv()) {
        match cmd {
            Command::Goto { url, reply } => {
                let result = state
                    .navigate_blocking(&url, &outer_handle)
                    .and_then(|_| state.snapshot_now());
                let _ = reply.send(result);
            }
            Command::Click { selector, reply } => {
                let result = run_click(&mut state, &outer_handle, &selector);
                let _ = reply.send(result);
            }
            Command::Fill {
                selector,
                value,
                reply,
            } => {
                let result = run_fill(&mut state, &outer_handle, &selector, &value);
                let _ = reply.send(result);
            }
            Command::Submit { selector, reply } => {
                let result = run_submit(&mut state, &outer_handle, &selector);
                let _ = reply.send(result);
            }
            Command::Read {
                selector,
                mode,
                reply,
            } => {
                let result = run_read(&mut state, &selector, mode);
                let _ = reply.send(result);
            }
            Command::Eval { expr, reply } => {
                let result = run_eval(&mut state, &outer_handle, &expr);
                let _ = reply.send(result);
            }
            Command::Snapshot { reply } => {
                let _ = reply.send(state.snapshot_now());
            }
        }
    }
}

fn send_construction_failure(cmd: Command, e: &BrowseError) {
    let msg = e.to_string();
    let err = || BrowseError::Io(msg.clone());
    match cmd {
        Command::Goto { reply, .. }
        | Command::Click { reply, .. }
        | Command::Fill { reply, .. }
        | Command::Submit { reply, .. }
        | Command::Snapshot { reply, .. } => {
            let _ = reply.send(Err(err()));
        }
        Command::Read { reply, .. } => {
            let _ = reply.send(Err(err()));
        }
        Command::Eval { reply, .. } => {
            let _ = reply.send(Err(err()));
        }
    }
}

// ---- primitive bodies (kept here so they share `ActorState` access) ----

fn run_click(
    state: &mut ActorState,
    handle: &tokio::runtime::Handle,
    selector: &str,
) -> Result<PageSnapshot, BrowseError> {
    let expr = format!(
        "(function(){{ var e = document.querySelector({sel}); if (!e) throw new Error('no match'); e.click(); return 'ok'; }})()",
        sel = js_string(selector),
    );
    state.rt.eval(&expr).map_err(|e| {
        // `no match` becomes a typed error; other JS errors fall through.
        if e.to_string().contains("no match") {
            BrowseError::NoMatch(selector.to_string())
        } else {
            BrowseError::Js(e)
        }
    })?;
    state.drain_navs(handle)?;
    state.snapshot_now()
}

fn run_fill(
    state: &mut ActorState,
    handle: &tokio::runtime::Handle,
    selector: &str,
    value: &str,
) -> Result<PageSnapshot, BrowseError> {
    let expr = format!(
        "(function(){{
            var e = document.querySelector({sel});
            if (!e) throw new Error('no match');
            e.value = {val};
            e.dispatchEvent(new Event('input', {{bubbles:true}}));
            e.dispatchEvent(new Event('change', {{bubbles:true}}));
            return 'ok';
        }})()",
        sel = js_string(selector),
        val = js_string(value),
    );
    state.rt.eval(&expr).map_err(|e| {
        if e.to_string().contains("no match") {
            BrowseError::NoMatch(selector.to_string())
        } else {
            BrowseError::Js(e)
        }
    })?;
    state.drain_navs(handle)?;
    state.snapshot_now()
}

/// Implements the `submit` primitive — three branches per `BrowseSession::submit`'s
/// docs. The form-introspection happens in JS (so we don't have to climb /
/// extract in Rust); we then build a real HTTP request from the result.
fn run_submit(
    state: &mut ActorState,
    handle: &tokio::runtime::Handle,
    selector: &str,
) -> Result<PageSnapshot, BrowseError> {
    // Find the enclosing form, extract its action / method / fields.
    // Returns JSON: {action: string|null, method: string, fields: [[name,value], …]}
    // Throws if selector matches nothing OR if no enclosing <form> is found.
    let intro_expr = format!(
        r#"(function(){{
            var node = document.querySelector({sel});
            if (!node) throw new Error('no match');
            var form = node;
            while (form && form.tagName !== 'FORM') form = form.parentNode;
            if (!form) throw new Error('no enclosing form');
            var action = form.getAttribute('action');
            var method = (form.getAttribute('method') || 'GET').toUpperCase();
            var fields = [];
            // bouncy-dom's selector grammar is single-clause — no comma
            // unions yet — so we run three separate queries and merge in
            // document order via NodeId. Each list is already in document
            // order; we don't try to interleave them precisely (real
            // submission order would require tree-walking) but for almost
            // every form it's identical.
            function collect(tag) {{
                var list = form.querySelectorAll(tag);
                for (var i = 0; i < list.length; i++) {{
                    var e = list[i];
                    var name = e.getAttribute('name');
                    if (!name) continue;
                    var type = (e.getAttribute('type') || '').toLowerCase();
                    // Skip unchecked checkboxes/radios. bouncy-js doesn't
                    // yet polyfill the `.checked` IDL setter, so we read
                    // the attribute (HTML-defined initial state). Good
                    // enough for signup-style flows; revisit if a target
                    // site toggles checkboxes via JS.
                    if ((type === 'checkbox' || type === 'radio')
                        && e.getAttribute('checked') === null) continue;
                    // Skip disabled per HTML form-submission rules.
                    if (e.getAttribute('disabled') !== null) continue;
                    var value = e.value;
                    if (value === undefined || value === null) {{
                        value = e.getAttribute('value') || '';
                    }}
                    fields.push([name, String(value)]);
                }}
            }}
            collect('input');
            collect('textarea');
            collect('select');
            return JSON.stringify({{action: action, method: method, fields: fields}});
        }})()"#,
        sel = js_string(selector),
    );
    let intel = state.rt.eval(&intro_expr).map_err(|e| {
        let msg = e.to_string();
        if msg.contains("no match") {
            BrowseError::NoMatch(selector.to_string())
        } else if msg.contains("no enclosing form") {
            BrowseError::Io(format!(
                "selector {selector:?} is not a form and is not inside one"
            ))
        } else {
            BrowseError::Js(e)
        }
    })?;
    // Eval results come back as JSON-quoted strings; strip the outer
    // quoting, then parse the inner JSON.
    let parsed: FormIntel = serde_json::from_str(&unquote_eval_result(&intel)).map_err(|e| {
        BrowseError::Io(format!(
            "parsing form intel JSON failed: {e} (raw: {intel})"
        ))
    })?;

    match parsed.action.as_deref() {
        Some(action) if !action.is_empty() => {
            // Branch 1 + 2: real HTTP submission via the form's action.
            let base = url::Url::parse(&state.current_url)?;
            let target = base.join(action)?;
            match parsed.method.as_str() {
                "GET" => {
                    let mut url_with_query = target;
                    {
                        let mut q = url_with_query.query_pairs_mut();
                        for (name, value) in &parsed.fields {
                            q.append_pair(name, value);
                        }
                    }
                    state.navigate_blocking(url_with_query.as_str(), handle)?;
                }
                _ => {
                    // POST (and anything we don't specifically GET) — build
                    // a urlencoded body. Multipart / file uploads are out
                    // of scope for v1.
                    let body: String = url::form_urlencoded::Serializer::new(String::new())
                        .extend_pairs(parsed.fields.iter().map(|(n, v)| (n.as_str(), v.as_str())))
                        .finish();
                    let req = bouncy_fetch::FetchRequest::new(target.as_str())
                        .method("POST")
                        .header("Content-Type", "application/x-www-form-urlencoded")
                        .body_str(body);
                    let resp = handle.block_on(state.fetcher.request(req))?;
                    let html = std::str::from_utf8(&resp.body).map_err(|_| BrowseError::NotUtf8)?;
                    state.rt.load(html, target.as_str())?;
                    state.rt.run_inline_scripts()?;
                    state.current_url = target.to_string();
                    state.drain_navs(handle)?;
                }
            }
        }
        _ => {
            // Branch 3: no action attr — let the page's JS handle it. We
            // dispatch a `submit` event on the form (same way a real
            // browser would when the user hits Enter / clicks submit), then
            // drain any `location.href` redirects the handler triggers.
            let dispatch = format!(
                r#"(function(){{
                    var node = document.querySelector({sel});
                    var form = node;
                    while (form && form.tagName !== 'FORM') form = form.parentNode;
                    form.dispatchEvent(new Event('submit', {{bubbles:true, cancelable:true}}));
                    return 'ok';
                }})()"#,
                sel = js_string(selector),
            );
            state.rt.eval(&dispatch)?;
            state.drain_navs(handle)?;
        }
    }

    state.snapshot_now()
}

#[derive(serde::Deserialize)]
struct FormIntel {
    action: Option<String>,
    method: String,
    fields: Vec<(String, String)>,
}

/// `Runtime::eval` returns a `String` that, for string-returning JS, is
/// the JSON-quoted form (e.g. `"\"hello\""`). For our form-intel JSON
/// we need to peel that outer layer to get the inner JSON.
fn unquote_eval_result(s: &str) -> String {
    serde_json::from_str::<String>(s).unwrap_or_else(|_| s.to_string())
}

fn run_read(
    state: &mut ActorState,
    selector: &str,
    mode: ReadMode,
) -> Result<Vec<String>, BrowseError> {
    let html = state.rt.dump_html()?;
    let doc = Document::parse(&html).map_err(|e| BrowseError::Dom(e.to_string()))?;
    let nodes = doc.query_selector_all(selector);
    let out: Vec<String> = match mode {
        ReadMode::Text => nodes.into_iter().map(|n| doc.text_content(n)).collect(),
        ReadMode::Html => nodes.into_iter().map(|n| doc.outer_html(n)).collect(),
        ReadMode::Attr(name) => nodes
            .into_iter()
            .filter_map(|n| doc.get_attribute(n, &name))
            .collect(),
    };
    Ok(out)
}

fn run_eval(
    state: &mut ActorState,
    handle: &tokio::runtime::Handle,
    expr: &str,
) -> Result<EvalResult, BrowseError> {
    let result = state.rt.eval(expr)?;
    state.drain_navs(handle)?;
    let snapshot = state.snapshot_now()?;
    Ok(EvalResult { result, snapshot })
}

/// JSON-encode a string for safe interpolation into a JS expression.
/// Uses `serde_json` so all the escaping rules are handled correctly.
fn js_string(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_else(|_| "\"\"".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn js_string_escapes_special_chars() {
        assert_eq!(js_string("hello"), "\"hello\"");
        assert_eq!(js_string("a\"b"), "\"a\\\"b\"");
        assert_eq!(js_string("line1\nline2"), "\"line1\\nline2\"");
        assert_eq!(js_string("</script>"), "\"</script>\"");
    }

    #[test]
    fn browse_opts_default_is_sane() {
        let opts = BrowseOpts::default();
        assert!(opts.user_agent.is_none());
        assert!(!opts.stealth);
        assert_eq!(opts.snapshot_opts.max_text_summary_bytes, 2048);
    }

    #[test]
    fn read_mode_variants_are_distinct() {
        // Lean check that the enum is constructable in all variants.
        let _ = ReadMode::Text;
        let _ = ReadMode::Html;
        let _ = ReadMode::Attr("href".into());
    }
}
