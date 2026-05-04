use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct Cookie {
    pub name: String,
    pub value: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct BasicAuth {
    pub user: String,
    pub pass: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct FetchInput {
    pub url: String,
    pub method: Option<String>,
    pub headers: Option<HashMap<String, String>>,
    pub body: Option<String>,
    pub timeout_ms: Option<u64>,
    pub basic_auth: Option<BasicAuth>,
    pub cookies: Option<Vec<Cookie>>,
    pub max_body_bytes: Option<u64>,
    /// Override the outgoing User-Agent. Defaults to
    /// `bouncy/<version> (+repo URL)`.
    pub user_agent: Option<String>,
    /// CSS selector to extract from the response body. When set, the
    /// returned `selected` field carries one entry per match. Selector
    /// grammar: tag, `#id`, `.class`, `[attr]`, `[attr=value]`.
    pub select: Option<String>,
    /// Pair with `select` to extract the named attribute's value
    /// instead of text content.
    pub select_attr: Option<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct FetchOutput {
    pub status: u16,
    pub headers: HashMap<String, String>,
    pub body_text: Option<String>,
    pub body_base64: Option<String>,
    pub truncated: bool,
    pub final_url: String,
    /// Present when the input carried a `select`. One entry per match.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub selected: Option<Vec<String>>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ExtractTitleInput {
    pub html: String,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct ExtractTitleOutput {
    pub title: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ExtractTextInput {
    pub html: String,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct ExtractTextOutput {
    pub text: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ExtractLinksInput {
    pub html: String,
    pub base_url: String,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct LinkOut {
    pub url: String,
    pub text: String,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct ExtractLinksOutput {
    pub links: Vec<LinkOut>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct JsEvalInput {
    pub url: String,
    pub eval: String,
    pub wait_for: Option<String>,
    pub timeout_ms: Option<u64>,
    pub cookies: Option<Vec<Cookie>>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct JsEvalOutput {
    pub result: String,
    pub html: String,
    pub final_url: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ScrapeInput {
    pub url: String,
    pub eval: Option<String>,
    /// JS-wait selector — block until this CSS selector matches before
    /// dumping. (For static text/attribute extraction, use `select`.)
    pub selector: Option<String>,
    pub headers: Option<HashMap<String, String>>,
    pub timeout_ms: Option<u64>,
    pub max_retries: Option<u32>,
    pub cookies: Option<Vec<Cookie>>,
    /// Override the outgoing User-Agent.
    pub user_agent: Option<String>,
    /// CSS selector for static text/attribute extraction (no V8). When
    /// set, the response gains a `selected` field with one entry per
    /// match.
    pub select: Option<String>,
    /// Pair with `select` to extract attribute values instead of text.
    pub select_attr: Option<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct ScrapeOutput {
    pub url: String,
    pub status: u16,
    pub html: String,
    pub eval_result: Option<String>,
    pub took_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub selected: Option<Vec<String>>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ScrapeManyInput {
    pub urls: Vec<String>,
    pub eval: Option<String>,
    pub selector: Option<String>,
    pub max_concurrency: Option<u32>,
    pub timeout_ms: Option<u64>,
    /// Cap on simultaneous requests against any single host.
    pub per_host_concurrency: Option<u32>,
    /// Override the outgoing User-Agent.
    pub user_agent: Option<String>,
    /// CSS selector for static text/attribute extraction per URL.
    pub select: Option<String>,
    /// Pair with `select` to extract attribute values instead of text.
    pub select_attr: Option<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct ScrapeManyResult {
    pub url: String,
    pub ok: bool,
    pub status: Option<u16>,
    pub html: Option<String>,
    pub eval_result: Option<String>,
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub selected: Option<Vec<String>>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct ScrapeManyOutput {
    pub results: Vec<ScrapeManyResult>,
}

// =================================================================
//  bouncy_browse_* — stateful browser primitives.
//
// Each `open` call creates a session and returns a `session_id`. All
// subsequent tool calls take that id and either drive the same V8 +
// cookie-jar + DOM state forward (click / fill / submit / goto / eval)
// or read from it (read). Sessions auto-expire after 15 minutes idle
// and are capped at 20 per server. Explicit `close` removes one early.
// =================================================================

/// `bouncy_browse_open`
#[derive(Debug, Deserialize, JsonSchema)]
pub struct BrowseOpenInput {
    pub url: String,
    /// Override outgoing User-Agent. Default: `bouncy/<version> (+repo)`.
    pub user_agent: Option<String>,
    /// Enable bouncy's stealth patches (canvas/audio/WebGPU/battery
    /// fingerprint randomization, hidden navigator.webdriver).
    pub stealth: Option<bool>,
    /// Placeholder → real value map. When `bouncy_browse_fill` (or
    /// `chain`) sees a `value` that exactly equals one of these keys,
    /// the placeholder is swapped for the real value before reaching
    /// the page. The LLM driving the session never sees the real
    /// secret — it only ever knows the placeholder name. Mirrors
    /// browser-use's `sensitive_data` parameter.
    #[serde(default)]
    pub secrets: Option<HashMap<String, String>>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct BrowseOpenOutput {
    pub session_id: String,
    pub snapshot: bouncy_browse::PageSnapshot,
}

/// `bouncy_browse_click`
#[derive(Debug, Deserialize, JsonSchema)]
pub struct BrowseClickInput {
    pub session_id: String,
    /// CSS selector — bouncy-dom grammar today is single-clause:
    /// tag, `#id`, `.class`, `[attr]`, `[attr=value]`. Required unless
    /// `index` is given.
    pub selector: Option<String>,
    /// Integer index from the current snapshot's `interactive` list.
    /// Either `selector` or `index` is required.
    pub index: Option<u32>,
}

/// `bouncy_browse_fill`
#[derive(Debug, Deserialize, JsonSchema)]
pub struct BrowseFillInput {
    pub session_id: String,
    pub selector: Option<String>,
    pub index: Option<u32>,
    pub value: String,
}

/// `bouncy_browse_submit`
#[derive(Debug, Deserialize, JsonSchema)]
pub struct BrowseSubmitInput {
    pub session_id: String,
    /// Selector for the form OR a submit button inside it. The primitive
    /// climbs to the enclosing `<form>` automatically.
    pub selector: Option<String>,
    pub index: Option<u32>,
}

/// `bouncy_browse_goto`
#[derive(Debug, Deserialize, JsonSchema)]
pub struct BrowseGotoInput {
    pub session_id: String,
    pub url: String,
}

/// `bouncy_browse_read`
#[derive(Debug, Deserialize, JsonSchema)]
pub struct BrowseReadInput {
    pub session_id: String,
    pub selector: Option<String>,
    pub index: Option<u32>,
    /// `"text"` (default), `"html"`, or `"attr:NAME"` for attribute values.
    pub mode: Option<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct BrowseReadOutput {
    pub matches: Vec<String>,
}

/// `bouncy_browse_eval` — escape hatch for cases the higher-level
/// primitives don't cover.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct BrowseEvalInput {
    pub session_id: String,
    /// JS expression. Returned value is coerced to a string.
    pub expr: String,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct BrowseEvalOutput {
    pub result: String,
    pub snapshot: bouncy_browse::PageSnapshot,
}

/// `bouncy_browse_close`
#[derive(Debug, Deserialize, JsonSchema)]
pub struct BrowseCloseInput {
    pub session_id: String,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct BrowseCloseOutput {
    /// `true` if a session was removed; `false` if the id was unknown
    /// (already expired or never existed).
    pub closed: bool,
}

/// Output for every state-changing browse tool: returns the page
/// snapshot that resulted from the action so the caller (LLM) doesn't
/// have to ask for it separately.
#[derive(Debug, Serialize, JsonSchema)]
pub struct BrowseSnapshotOutput {
    pub snapshot: bouncy_browse::PageSnapshot,
}

// =================================================================
//  bouncy_browse_* — primitives adopted from browser-use.
//
//  Indexed targets: every snapshot includes an `interactive` list
//  whose entries each have an integer `index`. Tools in this section
//  accept either a CSS `selector` or an `index`. If both are given,
//  `index` wins. If neither, the call errors.
// =================================================================

/// Either a CSS selector or an integer index from the current
/// snapshot's `interactive` list. Accepts either; rejects neither.
#[derive(Debug, Deserialize, JsonSchema, Default)]
pub struct TargetInput {
    pub selector: Option<String>,
    pub index: Option<u32>,
}

/// `bouncy_browse_click_text`
#[derive(Debug, Deserialize, JsonSchema)]
pub struct BrowseClickTextInput {
    pub session_id: String,
    /// Visible text to match against `<a>` and `<button>` content.
    /// Trimmed + ASCII-case-insensitive; exact match preferred over
    /// substring.
    pub text: String,
}

/// `bouncy_browse_select_option`
#[derive(Debug, Deserialize, JsonSchema)]
pub struct BrowseSelectOptionInput {
    pub session_id: String,
    #[serde(flatten)]
    pub target: TargetInput,
    /// Matches against `option.value=` first, then against the
    /// option's visible text.
    pub value: String,
}

/// `bouncy_browse_press_key`
#[derive(Debug, Deserialize, JsonSchema)]
pub struct BrowsePressKeyInput {
    pub session_id: String,
    #[serde(flatten)]
    pub target: TargetInput,
    /// Single character or named key: Enter, Tab, Escape,
    /// Backspace, Delete, ArrowUp/Down/Left/Right.
    pub key: String,
}

/// `bouncy_browse_wait_for`
#[derive(Debug, Deserialize, JsonSchema)]
pub struct BrowseWaitForInput {
    pub session_id: String,
    /// CSS selector to wait for. One of `selector` / `text` is required.
    pub selector: Option<String>,
    /// Visible body text substring to wait for. Trimmed +
    /// case-insensitive.
    pub text: Option<String>,
    /// Polling timeout. Defaults to 5000 ms when omitted.
    pub timeout_ms: Option<u64>,
}

/// `bouncy_browse_wait`
#[derive(Debug, Deserialize, JsonSchema)]
pub struct BrowseWaitInput {
    pub session_id: String,
    /// Sleep duration. Required.
    pub ms: u64,
}

/// `bouncy_browse_back` / `bouncy_browse_forward`
#[derive(Debug, Deserialize, JsonSchema)]
pub struct BrowseHistoryInput {
    pub session_id: String,
}

/// One step inside `bouncy_browse_chain`. Exactly one variant
/// per step — the JSON tag is the action name.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum BrowseChainStepInput {
    Click {
        #[serde(flatten)]
        target: TargetInput,
    },
    Fill {
        #[serde(flatten)]
        target: TargetInput,
        value: String,
    },
    Submit {
        #[serde(flatten)]
        target: TargetInput,
    },
    Goto {
        url: String,
    },
    Read {
        #[serde(flatten)]
        target: TargetInput,
        /// `"text"` (default) | `"html"` | `"attr:NAME"`.
        mode: Option<String>,
    },
    Eval {
        expr: String,
    },
    Snapshot,
    PressKey {
        #[serde(flatten)]
        target: TargetInput,
        key: String,
    },
    SelectOption {
        #[serde(flatten)]
        target: TargetInput,
        value: String,
    },
    ClickText {
        text: String,
    },
    WaitFor {
        selector: String,
        timeout_ms: Option<u64>,
    },
    WaitForText {
        text: String,
        timeout_ms: Option<u64>,
    },
    Wait {
        ms: u64,
    },
    Back,
    Forward,
}

/// `bouncy_browse_chain`
#[derive(Debug, Deserialize, JsonSchema)]
pub struct BrowseChainInput {
    pub session_id: String,
    /// Steps to run in order. The actor stops at the first error and
    /// returns whatever steps completed.
    pub steps: Vec<BrowseChainStepInput>,
}

/// One result entry from `bouncy_browse_chain`. Mirrors the
/// per-step union of outcomes — Snapshot for state changes, Reads
/// for `read`, EvalResult for `eval`.
#[derive(Debug, Serialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum BrowseChainStepOutput {
    Snapshot {
        snapshot: bouncy_browse::PageSnapshot,
    },
    Reads {
        matches: Vec<String>,
    },
    EvalResult {
        result: String,
        snapshot: bouncy_browse::PageSnapshot,
    },
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct BrowseChainOutput {
    pub steps: Vec<BrowseChainStepOutput>,
    /// Convenience: the snapshot at the end of the chain (or `None`
    /// if the last step returned reads only). Lets the LLM consume
    /// the final state without scanning `steps`.
    pub snapshot: Option<bouncy_browse::PageSnapshot>,
}
