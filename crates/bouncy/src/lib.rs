//! # bouncy — tiny Rust headless browser
//!
//! `bouncy` is the umbrella crate that re-exports the most commonly used
//! types from the `bouncy-*` workspace. If you're not sure which crate you
//! need, start here: `cargo add bouncy` and pick the modules you want via
//! Cargo features.
//!
//! Two distinct paths:
//!
//! - **Static scraping** — fetch a page and pull data out via streaming
//!   extractors. No V8, no DOM tree, ~30 ms cold start. Enabled by the
//!   default features (`fetch`, `extract`).
//! - **Stateful browsing** — open a session, click/fill/submit/read across
//!   multiple pages with V8 + cookies preserved. Enable with the
//!   `browse` feature.
//!
//! ## Cargo features
//!
//! | Feature   | Pulls in                       | What you get |
//! |-----------|--------------------------------|---|
//! | `fetch`   | [`bouncy-fetch`]               | HTTP client with stealth headers + cookie jar |
//! | `extract` | [`bouncy-extract`]             | Streaming title / text / link extractors |
//! | `browse`  | [`bouncy-browse`] + js + dom   | Stateful V8 browser session + structured page snapshots |
//! | `dom`     | [`bouncy-dom`]                 | The DOM tree the JS path runs against |
//! | `js`      | [`bouncy-js`]                  | Raw V8 runtime with bouncy's bootstrap polyfills |
//! | `full`    | all of the above               | One-stop shop |
//!
//! Default: `fetch` + `extract`. Browse is opt-in because V8 adds ~25 MB
//! to the dep tree.
//!
//! ## Quick example — static scrape
//!
//! ```no_run
//! # #[cfg(all(feature = "fetch", feature = "extract"))]
//! # async fn _example() -> Result<(), Box<dyn std::error::Error>> {
//! use bouncy::fetch::Fetcher;
//! use bouncy::extract::extract_title;
//!
//! let fetcher = Fetcher::new()?;
//! let resp = fetcher.get("https://example.com").await?;
//! let title = extract_title(&resp.body)?;
//! println!("title: {title:?}");
//! # Ok(()) }
//! ```
//!
//! ## Quick example — stateful browse (requires `browse` feature)
//!
//! ```no_run
//! # #[cfg(feature = "browse")]
//! # async fn _example() -> Result<(), Box<dyn std::error::Error>> {
//! use bouncy::browse::{BrowseOpts, BrowseSession};
//!
//! let (session, snapshot) =
//!     BrowseSession::open("https://example.com", BrowseOpts::default()).await?;
//! println!("opened {}, title {:?}", snapshot.url, snapshot.title);
//! # Ok(()) }
//! ```
//!
//! [`bouncy-fetch`]:   https://docs.rs/bouncy-fetch
//! [`bouncy-extract`]: https://docs.rs/bouncy-extract
//! [`bouncy-browse`]:  https://docs.rs/bouncy-browse
//! [`bouncy-dom`]:     https://docs.rs/bouncy-dom
//! [`bouncy-js`]:      https://docs.rs/bouncy-js

#![doc(html_root_url = "https://docs.rs/bouncy/0.1.8")]

#[cfg(feature = "fetch")]
pub use bouncy_fetch as fetch;

#[cfg(feature = "extract")]
pub use bouncy_extract as extract;

#[cfg(feature = "browse")]
pub use bouncy_browse as browse;

#[cfg(feature = "dom")]
pub use bouncy_dom as dom;

#[cfg(feature = "js")]
pub use bouncy_js as js;

/// Crate version, surfaced for diagnostics. Matches the `version` in
/// `Cargo.toml` and the workspace lockstep version.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
