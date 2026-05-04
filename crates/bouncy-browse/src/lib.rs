//! `bouncy-browse` — stateful browser primitives for bouncy.
//!
//! Surface:
//!   - [`BrowseSession`] holds a V8 runtime + cookie jar + current page across
//!     multiple `click` / `fill` / `goto` / `read` / `eval` calls, so flows like
//!     "open → click sign-up → fill form → submit → read welcome message"
//!     work end-to-end without restarting the engine.
//!   - [`PageSnapshot`] is the structured, LLM-friendly view of the current page:
//!     forms, links, buttons, inputs, headings, plus a truncated text summary.
//!     Returned from every state-changing primitive so callers don't have to ask
//!     "what does the page look like now?" after each action.
//!   - [`unique_selector`] generates stable CSS selectors for elements so a
//!     selector returned in one snapshot keeps targeting the same element on
//!     subsequent snapshots.
//!
//! No real layout, paint, or canvas — bouncy stays DOM-only by design. For the
//! ~80% of scraping flows that don't need pixel-accurate hit-testing, that's a
//! feature: ~30 ms cold start, ~20 MB resident, single binary, no Chromium.

pub mod session;
pub mod snapshot;

pub use session::{
    BrowseError, BrowseOpts, BrowseSession, ChainStep, ChainStepOutput, EvalResult, ReadMode,
    Target,
};
pub use snapshot::{
    unique_selector, ButtonSnapshot, FormSnapshot, HeadingSnapshot, InputSnapshot,
    InteractiveElement, LinkSnapshot, PageSnapshot, SelectOption, SnapshotOpts,
};
