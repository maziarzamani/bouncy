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

/// How a primitive addresses an element on the current page. Either a
/// CSS selector (the original surface) or an `index` from the current
/// snapshot's `interactive` list (the LLM-friendly surface inspired by
/// browser-use's clickable-element indexing).
///
/// Indices are stable inside a single snapshot only — every
/// state-changing primitive returns a fresh snapshot, and the LLM is
/// expected to reference indices from that latest snapshot.
#[derive(Debug, Clone)]
pub enum Target {
    Selector(String),
    Index(u32),
}

impl Target {
    pub fn selector(s: impl Into<String>) -> Self {
        Target::Selector(s.into())
    }

    pub fn index(i: u32) -> Self {
        Target::Index(i)
    }
}

/// One step in [`BrowseSession::chain`]. Mirrors the standalone
/// primitives so an LLM can batch a multi-action plan into a single
/// MCP / library round trip — saving N-1 message-pair latencies on
/// form-fill flows. Modeled on browser-use's `max_actions_per_step`
/// (default 4) but with no hard cap; the actor processes the list
/// in order and stops at the first error.
#[derive(Debug, Clone)]
pub enum ChainStep {
    Click(Target),
    Fill {
        target: Target,
        value: String,
    },
    Submit(Target),
    Goto(String),
    Read {
        target: Target,
        mode: ReadMode,
    },
    Eval(String),
    Snapshot,
    /// Fire keyboard events on the matched element (single keypress).
    PressKey {
        target: Target,
        key: String,
    },
    /// Set a `<select>`'s value to the option whose `value=` matches
    /// (falls back to matching the option's visible text).
    SelectOption {
        target: Target,
        value: String,
    },
    /// Click the first link/button whose visible text matches.
    /// Case-insensitive, trimmed, exact match preferred over substring.
    ClickText(String),
    /// Block until a selector resolves or `timeout_ms` elapses. Polls
    /// every ~50 ms. No-op success when the selector already matches.
    WaitFor {
        selector: String,
        timeout_ms: u64,
    },
    /// Block until visible body text contains `needle` or `timeout_ms`
    /// elapses. Same polling cadence as `WaitFor`.
    WaitForText {
        needle: String,
        timeout_ms: u64,
    },
    /// Pause the actor for `ms` milliseconds — the bouncy DOM is
    /// fully synchronous so this is mostly a courtesy / pacing knob.
    Wait {
        ms: u64,
    },
    /// Pop one entry off the back-history stack and re-navigate.
    Back,
    /// Pop one entry off the forward-history stack and re-navigate.
    Forward,
}

/// One result entry from [`BrowseSession::chain`]. The actor records
/// one of these per step — callers typically only need the last
/// snapshot, but per-step results help debug a chain that bailed
/// halfway.
#[derive(Debug, Clone)]
pub enum ChainStepOutput {
    Snapshot(PageSnapshot),
    Reads(Vec<String>),
    Eval {
        result: String,
        snapshot: PageSnapshot,
    },
}

/// A held-open browser session. Cheap to clone the channel handle but
/// not the underlying actor — clone the surface for shared access.
pub struct BrowseSession {
    cmd_tx: mpsc::UnboundedSender<Command>,
}

impl std::fmt::Debug for BrowseSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Don't expose internal channel handles; just enough to know
        // whether the actor is still reachable.
        f.debug_struct("BrowseSession")
            .field("actor_alive", &!self.cmd_tx.is_closed())
            .finish()
    }
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

    /// Click whichever of `selector` / `index` resolves first. The
    /// index variant looks up the element in a fresh snapshot of the
    /// current page; the selector variant goes straight to JS.
    pub async fn click_target(&self, target: Target) -> Result<PageSnapshot, BrowseError> {
        self.send(|reply| Command::ClickTarget { target, reply })
            .await
    }

    /// `fill` against either a selector or a snapshot index.
    pub async fn fill_target(
        &self,
        target: Target,
        value: &str,
    ) -> Result<PageSnapshot, BrowseError> {
        self.send(|reply| Command::FillTarget {
            target,
            value: value.to_string(),
            reply,
        })
        .await
    }

    /// `submit` against either a selector or a snapshot index.
    pub async fn submit_target(&self, target: Target) -> Result<PageSnapshot, BrowseError> {
        self.send(|reply| Command::SubmitTarget { target, reply })
            .await
    }

    /// `read` against either a selector or a snapshot index.
    pub async fn read_target(
        &self,
        target: Target,
        mode: ReadMode,
    ) -> Result<Vec<String>, BrowseError> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(Command::ReadTarget {
                target,
                mode,
                reply: tx,
            })
            .map_err(|_| BrowseError::ActorGone)?;
        rx.await.map_err(|_| BrowseError::ActorGone)?
    }

    /// Click the first link/button whose visible text matches. The
    /// match is trimmed, case-insensitive, and prefers exact equality
    /// over substring match.
    pub async fn click_text(&self, text: &str) -> Result<PageSnapshot, BrowseError> {
        self.send(|reply| Command::ClickText {
            text: text.to_string(),
            reply,
        })
        .await
    }

    /// Set a `<select>`'s value to the option whose `value=` matches
    /// (or, falling back, whose visible text matches). Dispatches
    /// `change` so listeners on the page react.
    pub async fn select_option(
        &self,
        target: Target,
        value: &str,
    ) -> Result<PageSnapshot, BrowseError> {
        self.send(|reply| Command::SelectOption {
            target,
            value: value.to_string(),
            reply,
        })
        .await
    }

    /// Fire `keydown` + `keyup` (and `keypress` for printable keys) on
    /// the matched element. `key` accepts either a single character
    /// or one of the named keys (`Enter`, `Tab`, `Escape`, `ArrowUp`,
    /// `ArrowDown`, `ArrowLeft`, `ArrowRight`, `Backspace`).
    pub async fn press_key(&self, target: Target, key: &str) -> Result<PageSnapshot, BrowseError> {
        self.send(|reply| Command::PressKey {
            target,
            key: key.to_string(),
            reply,
        })
        .await
    }

    /// Block until `selector` matches at least one element on the
    /// current page, or until `timeout_ms` elapses. Returns the latest
    /// snapshot in either case (`Err(NoMatch)` on timeout). Polls
    /// every ~50 ms — bouncy's DOM is synchronous so the polling
    /// only ever observes script-driven mutations.
    pub async fn wait_for(
        &self,
        selector: &str,
        timeout_ms: u64,
    ) -> Result<PageSnapshot, BrowseError> {
        self.send(|reply| Command::WaitFor {
            selector: selector.to_string(),
            timeout_ms,
            reply,
        })
        .await
    }

    /// Like [`Self::wait_for`] but matches against the page's visible
    /// body text rather than a CSS selector.
    pub async fn wait_for_text(
        &self,
        needle: &str,
        timeout_ms: u64,
    ) -> Result<PageSnapshot, BrowseError> {
        self.send(|reply| Command::WaitForText {
            needle: needle.to_string(),
            timeout_ms,
            reply,
        })
        .await
    }

    /// Sleep for `ms` milliseconds. Useful as a pacing knob between
    /// requests when you don't want the chain hammering a server.
    pub async fn wait_ms(&self, ms: u64) -> Result<PageSnapshot, BrowseError> {
        self.send(|reply| Command::WaitMs { ms, reply }).await
    }

    /// Re-navigate to the previously-visited URL. Errors with
    /// `BrowseError::Io("history empty")` when there's nothing to go
    /// back to. Implemented via a per-session URL stack — bouncy's
    /// V8 doesn't model real `history.back()` semantics.
    pub async fn back(&self) -> Result<PageSnapshot, BrowseError> {
        self.send(|reply| Command::Back { reply }).await
    }

    /// Inverse of [`Self::back`]. Errors when nothing has been
    /// `back`ed yet on this session.
    pub async fn forward(&self) -> Result<PageSnapshot, BrowseError> {
        self.send(|reply| Command::Forward { reply }).await
    }

    /// Run a list of [`ChainStep`]s in order, returning per-step
    /// outputs. Stops at the first error so callers can see how far
    /// the chain got. Inspired by browser-use's
    /// `max_actions_per_step` — lets an LLM batch a planned sequence
    /// (fill 3 fields, submit, read result) into one round trip.
    pub async fn chain(&self, steps: Vec<ChainStep>) -> Result<Vec<ChainStepOutput>, BrowseError> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(Command::Chain { steps, reply: tx })
            .map_err(|_| BrowseError::ActorGone)?;
        rx.await.map_err(|_| BrowseError::ActorGone)?
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
    ClickTarget {
        target: Target,
        reply: oneshot::Sender<Result<PageSnapshot, BrowseError>>,
    },
    FillTarget {
        target: Target,
        value: String,
        reply: oneshot::Sender<Result<PageSnapshot, BrowseError>>,
    },
    SubmitTarget {
        target: Target,
        reply: oneshot::Sender<Result<PageSnapshot, BrowseError>>,
    },
    ReadTarget {
        target: Target,
        mode: ReadMode,
        reply: oneshot::Sender<Result<Vec<String>, BrowseError>>,
    },
    ClickText {
        text: String,
        reply: oneshot::Sender<Result<PageSnapshot, BrowseError>>,
    },
    SelectOption {
        target: Target,
        value: String,
        reply: oneshot::Sender<Result<PageSnapshot, BrowseError>>,
    },
    PressKey {
        target: Target,
        key: String,
        reply: oneshot::Sender<Result<PageSnapshot, BrowseError>>,
    },
    WaitFor {
        selector: String,
        timeout_ms: u64,
        reply: oneshot::Sender<Result<PageSnapshot, BrowseError>>,
    },
    WaitForText {
        needle: String,
        timeout_ms: u64,
        reply: oneshot::Sender<Result<PageSnapshot, BrowseError>>,
    },
    WaitMs {
        ms: u64,
        reply: oneshot::Sender<Result<PageSnapshot, BrowseError>>,
    },
    Back {
        reply: oneshot::Sender<Result<PageSnapshot, BrowseError>>,
    },
    Forward {
        reply: oneshot::Sender<Result<PageSnapshot, BrowseError>>,
    },
    Chain {
        steps: Vec<ChainStep>,
        reply: oneshot::Sender<Result<Vec<ChainStepOutput>, BrowseError>>,
    },
}

/// State held inside the actor for the lifetime of a session.
struct ActorState {
    rt: Runtime,
    fetcher: Arc<Fetcher>,
    current_url: String,
    snapshot_opts: SnapshotOpts,
    stealth: bool,
    /// Back-history stack — URLs visited before `current_url`. Pushed
    /// every time `navigate_blocking` lands on a fresh URL; popped by
    /// `Back`.
    back_stack: Vec<String>,
    /// Forward-history stack — URLs popped off `back_stack`. Cleared
    /// on any forward navigation that isn't itself a `Forward` (mirror
    /// of how real browsers behave).
    forward_stack: Vec<String>,
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
            back_stack: Vec::new(),
            forward_stack: Vec::new(),
        })
    }

    /// Fetch a URL, load the body into the runtime, run inline scripts,
    /// drain `location.href` navs (up to `MAX_NAV_HOPS`).
    fn navigate_blocking(
        &mut self,
        url: &str,
        outer_handle: &tokio::runtime::Handle,
    ) -> Result<(), BrowseError> {
        self.navigate_blocking_inner(url, outer_handle, /* tracked = */ true)
    }

    /// Inner helper that lets `Back`/`Forward` opt out of pushing onto
    /// the history stack — those commands manipulate the stacks
    /// themselves rather than appending to them.
    fn navigate_blocking_inner(
        &mut self,
        url: &str,
        outer_handle: &tokio::runtime::Handle,
        tracked: bool,
    ) -> Result<(), BrowseError> {
        let mut current = url.to_string();
        let prev = self.current_url.clone();
        for _ in 0..=MAX_NAV_HOPS {
            let resp = outer_handle.block_on(self.fetcher.get(&current))?;
            let body = std::str::from_utf8(&resp.body).map_err(|_| BrowseError::NotUtf8)?;
            self.rt.load(body, &current)?;
            self.rt.run_inline_scripts()?;
            self.current_url = current.clone();
            if let Some(next) = self.rt.take_pending_nav() {
                current = next;
            } else {
                if tracked && !prev.is_empty() && prev != self.current_url {
                    self.back_stack.push(prev);
                    // Any new (non-back/forward) navigation kills the
                    // forward stack — same way Chrome / Firefox do it.
                    self.forward_stack.clear();
                }
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

    /// Resolve a [`Target`] to a CSS selector against the current
    /// page. Selector targets pass through; index targets are
    /// resolved by snapshotting and looking up the index in the flat
    /// `interactive` list. Errors with `NoMatch` if the index doesn't
    /// exist in the current snapshot.
    fn resolve_target(&mut self, target: &Target) -> Result<String, BrowseError> {
        match target {
            Target::Selector(s) => Ok(s.clone()),
            Target::Index(i) => {
                let snap = self.snapshot_now()?;
                snap.selector_for_index(*i)
                    .map(|s| s.to_string())
                    .ok_or_else(|| BrowseError::NoMatch(format!("@{i}")))
            }
        }
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
            Command::ClickTarget { target, reply } => {
                let result = state
                    .resolve_target(&target)
                    .and_then(|sel| run_click(&mut state, &outer_handle, &sel));
                let _ = reply.send(result);
            }
            Command::FillTarget {
                target,
                value,
                reply,
            } => {
                let result = state
                    .resolve_target(&target)
                    .and_then(|sel| run_fill(&mut state, &outer_handle, &sel, &value));
                let _ = reply.send(result);
            }
            Command::SubmitTarget { target, reply } => {
                let result = state
                    .resolve_target(&target)
                    .and_then(|sel| run_submit(&mut state, &outer_handle, &sel));
                let _ = reply.send(result);
            }
            Command::ReadTarget {
                target,
                mode,
                reply,
            } => {
                let result = state
                    .resolve_target(&target)
                    .and_then(|sel| run_read(&mut state, &sel, mode));
                let _ = reply.send(result);
            }
            Command::ClickText { text, reply } => {
                let result = run_click_text(&mut state, &outer_handle, &text);
                let _ = reply.send(result);
            }
            Command::SelectOption {
                target,
                value,
                reply,
            } => {
                let result = state
                    .resolve_target(&target)
                    .and_then(|sel| run_select_option(&mut state, &outer_handle, &sel, &value));
                let _ = reply.send(result);
            }
            Command::PressKey { target, key, reply } => {
                let result = state
                    .resolve_target(&target)
                    .and_then(|sel| run_press_key(&mut state, &outer_handle, &sel, &key));
                let _ = reply.send(result);
            }
            Command::WaitFor {
                selector,
                timeout_ms,
                reply,
            } => {
                let result = run_wait_for(&mut state, &outer_handle, &selector, timeout_ms);
                let _ = reply.send(result);
            }
            Command::WaitForText {
                needle,
                timeout_ms,
                reply,
            } => {
                let result = run_wait_for_text(&mut state, &outer_handle, &needle, timeout_ms);
                let _ = reply.send(result);
            }
            Command::WaitMs { ms, reply } => {
                let result = run_wait_ms(&mut state, &outer_handle, ms);
                let _ = reply.send(result);
            }
            Command::Back { reply } => {
                let result = run_back(&mut state, &outer_handle);
                let _ = reply.send(result);
            }
            Command::Forward { reply } => {
                let result = run_forward(&mut state, &outer_handle);
                let _ = reply.send(result);
            }
            Command::Chain { steps, reply } => {
                let result = run_chain(&mut state, &outer_handle, steps);
                let _ = reply.send(result);
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
        | Command::Snapshot { reply, .. }
        | Command::ClickTarget { reply, .. }
        | Command::FillTarget { reply, .. }
        | Command::SubmitTarget { reply, .. }
        | Command::ClickText { reply, .. }
        | Command::SelectOption { reply, .. }
        | Command::PressKey { reply, .. }
        | Command::WaitFor { reply, .. }
        | Command::WaitForText { reply, .. }
        | Command::WaitMs { reply, .. }
        | Command::Back { reply, .. }
        | Command::Forward { reply, .. } => {
            let _ = reply.send(Err(err()));
        }
        Command::Read { reply, .. } | Command::ReadTarget { reply, .. } => {
            let _ = reply.send(Err(err()));
        }
        Command::Eval { reply, .. } => {
            let _ = reply.send(Err(err()));
        }
        Command::Chain { reply, .. } => {
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

// ---- new primitives ---------------------------------------------------------

/// Click the first link/button whose visible text matches. The match
/// is trimmed + ASCII-case-insensitive; exact equality wins, otherwise
/// the first substring hit wins. Searches `<a>` and `<button>` only —
/// arbitrary clickable divs would need a selector.
fn run_click_text(
    state: &mut ActorState,
    handle: &tokio::runtime::Handle,
    text: &str,
) -> Result<PageSnapshot, BrowseError> {
    let needle = text.trim();
    let html = state.rt.dump_html()?;
    let doc = Document::parse(&html).map_err(|e| BrowseError::Dom(e.to_string()))?;
    let mut exact: Option<String> = None;
    let mut substring: Option<String> = None;
    for tag in ["a", "button"] {
        for nid in doc.query_selector_all(tag) {
            let body = doc.text_content(nid);
            let body_t = body.trim();
            if body_t.eq_ignore_ascii_case(needle) {
                exact = Some(crate::snapshot::unique_selector(&doc, nid));
                break;
            }
            if substring.is_none() && contains_ignore_ascii_case(body_t, needle) {
                substring = Some(crate::snapshot::unique_selector(&doc, nid));
            }
        }
        if exact.is_some() {
            break;
        }
    }
    let selector = exact
        .or(substring)
        .ok_or_else(|| BrowseError::NoMatch(format!("text: {needle:?}")))?;
    run_click(state, handle, &selector)
}

fn contains_ignore_ascii_case(haystack: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return true;
    }
    let h = haystack.to_ascii_lowercase();
    let n = needle.to_ascii_lowercase();
    h.contains(&n)
}

/// Set a `<select>`'s value via JS, dispatching `input` + `change`.
/// `value` matches against `option.value` first, then against
/// `option.text` so the LLM can use whichever it has.
fn run_select_option(
    state: &mut ActorState,
    handle: &tokio::runtime::Handle,
    selector: &str,
    value: &str,
) -> Result<PageSnapshot, BrowseError> {
    let expr = format!(
        r#"(function(){{
            var e = document.querySelector({sel});
            if (!e) throw new Error('no match');
            if (e.tagName !== 'SELECT') throw new Error('not a select');
            var want = {val};
            var matched = -1;
            var opts = e.querySelectorAll('option');
            for (var i = 0; i < opts.length; i++) {{
                var v = opts[i].getAttribute('value');
                if (v === null) v = (opts[i].textContent || '').trim();
                if (v === want) {{ matched = i; break; }}
            }}
            if (matched < 0) {{
                for (var j = 0; j < opts.length; j++) {{
                    if ((opts[j].textContent || '').trim() === want) {{ matched = j; break; }}
                }}
            }}
            if (matched < 0) throw new Error('no option matches');
            e.value = opts[matched].getAttribute('value') !== null
                      ? opts[matched].getAttribute('value')
                      : (opts[matched].textContent || '').trim();
            e.dispatchEvent(new Event('input', {{bubbles:true}}));
            e.dispatchEvent(new Event('change', {{bubbles:true}}));
            return 'ok';
        }})()"#,
        sel = js_string(selector),
        val = js_string(value),
    );
    state.rt.eval(&expr).map_err(|e| {
        let msg = e.to_string();
        if msg.contains("no match") {
            BrowseError::NoMatch(selector.to_string())
        } else if msg.contains("not a select") {
            BrowseError::Io(format!("element {selector:?} is not a <select>"))
        } else if msg.contains("no option matches") {
            BrowseError::NoMatch(format!("option {value:?} in {selector:?}"))
        } else {
            BrowseError::Js(e)
        }
    })?;
    state.drain_navs(handle)?;
    state.snapshot_now()
}

/// Dispatch keyboard events on the matched element. Synthesizes
/// `keydown` + (optionally `keypress`) + `keyup`. Single-character
/// keys count as printable; named keys (Enter, Tab, Escape,
/// ArrowUp/Down/Left/Right, Backspace) are passed through with
/// reasonable `keyCode` values for the legacy listeners that still
/// check them.
fn run_press_key(
    state: &mut ActorState,
    handle: &tokio::runtime::Handle,
    selector: &str,
    key: &str,
) -> Result<PageSnapshot, BrowseError> {
    let key_code = key_code_for(key);
    let printable = key.chars().count() == 1;
    let expr = format!(
        r#"(function(){{
            var e = document.querySelector({sel});
            if (!e) throw new Error('no match');
            var key = {key_str};
            var code = {code};
            var printable = {printable};
            function fire(type) {{
                var ev = new KeyboardEvent(type, {{
                    key: key, code: key, keyCode: code, which: code,
                    bubbles: true, cancelable: true,
                }});
                e.dispatchEvent(ev);
            }}
            fire('keydown');
            if (printable) fire('keypress');
            fire('keyup');
            return 'ok';
        }})()"#,
        sel = js_string(selector),
        key_str = js_string(key),
        code = key_code,
        printable = if printable { "true" } else { "false" },
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

/// Approximate `keyCode` mapping for the named keys the LLM is most
/// likely to send. Legacy keyup/keydown listeners on real sites still
/// branch on `keyCode`/`which`, so emitting plausible values matters
/// even though `key` is the modern surface.
fn key_code_for(key: &str) -> u32 {
    match key {
        "Enter" => 13,
        "Tab" => 9,
        "Escape" | "Esc" => 27,
        "Backspace" => 8,
        "Delete" => 46,
        "ArrowUp" => 38,
        "ArrowDown" => 40,
        "ArrowLeft" => 37,
        "ArrowRight" => 39,
        " " | "Space" => 32,
        s if s.chars().count() == 1 => {
            let c = s.chars().next().unwrap();
            (c as u32).to_ascii_uppercase_u32()
        }
        _ => 0,
    }
}

trait UpperU32 {
    fn to_ascii_uppercase_u32(self) -> u32;
}
impl UpperU32 for u32 {
    fn to_ascii_uppercase_u32(self) -> u32 {
        if (b'a' as u32..=b'z' as u32).contains(&self) {
            self - 32
        } else {
            self
        }
    }
}

const WAIT_POLL_INTERVAL_MS: u64 = 50;

/// Block until `selector` resolves. Polls every
/// `WAIT_POLL_INTERVAL_MS` milliseconds; returns the latest snapshot
/// on success, `BrowseError::NoMatch` on timeout.
fn run_wait_for(
    state: &mut ActorState,
    handle: &tokio::runtime::Handle,
    selector: &str,
    timeout_ms: u64,
) -> Result<PageSnapshot, BrowseError> {
    let start = std::time::Instant::now();
    loop {
        let html = state.rt.dump_html()?;
        let doc = Document::parse(&html).map_err(|e| BrowseError::Dom(e.to_string()))?;
        if !doc.query_selector_all(selector).is_empty() {
            return state.snapshot_now();
        }
        if start.elapsed().as_millis() as u64 >= timeout_ms {
            return Err(BrowseError::NoMatch(format!(
                "{selector:?} (waited {timeout_ms}ms)"
            )));
        }
        handle.block_on(tokio::time::sleep(std::time::Duration::from_millis(
            WAIT_POLL_INTERVAL_MS,
        )));
    }
}

fn run_wait_for_text(
    state: &mut ActorState,
    handle: &tokio::runtime::Handle,
    needle: &str,
    timeout_ms: u64,
) -> Result<PageSnapshot, BrowseError> {
    let start = std::time::Instant::now();
    loop {
        let html = state.rt.dump_html()?;
        let doc = Document::parse(&html).map_err(|e| BrowseError::Dom(e.to_string()))?;
        let body = doc.body_text();
        if contains_ignore_ascii_case(&body, needle) {
            return state.snapshot_now();
        }
        if start.elapsed().as_millis() as u64 >= timeout_ms {
            return Err(BrowseError::NoMatch(format!(
                "text {needle:?} (waited {timeout_ms}ms)"
            )));
        }
        handle.block_on(tokio::time::sleep(std::time::Duration::from_millis(
            WAIT_POLL_INTERVAL_MS,
        )));
    }
}

fn run_wait_ms(
    state: &mut ActorState,
    handle: &tokio::runtime::Handle,
    ms: u64,
) -> Result<PageSnapshot, BrowseError> {
    handle.block_on(tokio::time::sleep(std::time::Duration::from_millis(ms)));
    state.snapshot_now()
}

fn run_back(
    state: &mut ActorState,
    handle: &tokio::runtime::Handle,
) -> Result<PageSnapshot, BrowseError> {
    let prev = state
        .back_stack
        .pop()
        .ok_or_else(|| BrowseError::Io("history empty".into()))?;
    let current = state.current_url.clone();
    state.navigate_blocking_inner(&prev, handle, /* tracked = */ false)?;
    if !current.is_empty() {
        state.forward_stack.push(current);
    }
    state.snapshot_now()
}

fn run_forward(
    state: &mut ActorState,
    handle: &tokio::runtime::Handle,
) -> Result<PageSnapshot, BrowseError> {
    let next = state
        .forward_stack
        .pop()
        .ok_or_else(|| BrowseError::Io("forward stack empty".into()))?;
    let current = state.current_url.clone();
    state.navigate_blocking_inner(&next, handle, /* tracked = */ false)?;
    if !current.is_empty() {
        state.back_stack.push(current);
    }
    state.snapshot_now()
}

/// Run a chain of [`ChainStep`]s in order. Stops at the first error
/// so the caller can see how far the chain got from the partial
/// result vector. Each step's output is captured.
fn run_chain(
    state: &mut ActorState,
    handle: &tokio::runtime::Handle,
    steps: Vec<ChainStep>,
) -> Result<Vec<ChainStepOutput>, BrowseError> {
    let mut out = Vec::with_capacity(steps.len());
    for step in steps {
        match step {
            ChainStep::Click(target) => {
                let sel = state.resolve_target(&target)?;
                let snap = run_click(state, handle, &sel)?;
                out.push(ChainStepOutput::Snapshot(snap));
            }
            ChainStep::Fill { target, value } => {
                let sel = state.resolve_target(&target)?;
                let snap = run_fill(state, handle, &sel, &value)?;
                out.push(ChainStepOutput::Snapshot(snap));
            }
            ChainStep::Submit(target) => {
                let sel = state.resolve_target(&target)?;
                let snap = run_submit(state, handle, &sel)?;
                out.push(ChainStepOutput::Snapshot(snap));
            }
            ChainStep::Goto(url) => {
                state.navigate_blocking(&url, handle)?;
                out.push(ChainStepOutput::Snapshot(state.snapshot_now()?));
            }
            ChainStep::Read { target, mode } => {
                let sel = state.resolve_target(&target)?;
                let reads = run_read(state, &sel, mode)?;
                out.push(ChainStepOutput::Reads(reads));
            }
            ChainStep::Eval(expr) => {
                let r = run_eval(state, handle, &expr)?;
                out.push(ChainStepOutput::Eval {
                    result: r.result,
                    snapshot: r.snapshot,
                });
            }
            ChainStep::Snapshot => {
                out.push(ChainStepOutput::Snapshot(state.snapshot_now()?));
            }
            ChainStep::PressKey { target, key } => {
                let sel = state.resolve_target(&target)?;
                let snap = run_press_key(state, handle, &sel, &key)?;
                out.push(ChainStepOutput::Snapshot(snap));
            }
            ChainStep::SelectOption { target, value } => {
                let sel = state.resolve_target(&target)?;
                let snap = run_select_option(state, handle, &sel, &value)?;
                out.push(ChainStepOutput::Snapshot(snap));
            }
            ChainStep::ClickText(text) => {
                let snap = run_click_text(state, handle, &text)?;
                out.push(ChainStepOutput::Snapshot(snap));
            }
            ChainStep::WaitFor {
                selector,
                timeout_ms,
            } => {
                let snap = run_wait_for(state, handle, &selector, timeout_ms)?;
                out.push(ChainStepOutput::Snapshot(snap));
            }
            ChainStep::WaitForText { needle, timeout_ms } => {
                let snap = run_wait_for_text(state, handle, &needle, timeout_ms)?;
                out.push(ChainStepOutput::Snapshot(snap));
            }
            ChainStep::Wait { ms } => {
                let snap = run_wait_ms(state, handle, ms)?;
                out.push(ChainStepOutput::Snapshot(snap));
            }
            ChainStep::Back => {
                let snap = run_back(state, handle)?;
                out.push(ChainStepOutput::Snapshot(snap));
            }
            ChainStep::Forward => {
                let snap = run_forward(state, handle)?;
                out.push(ChainStepOutput::Snapshot(snap));
            }
        }
    }
    Ok(out)
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
