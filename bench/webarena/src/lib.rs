//! `bouncy-bench-webarena` — agent-loop harness for running bouncy +
//! Claude against WebArena-shaped tasks.
//!
//! Architecture:
//!
//!   - [`task::Task`] — one WebArena-shaped task: starting URL, plain-text
//!     instruction, optional success criteria for the judge.
//!   - [`agent::run_task`] — opens a [`bouncy_browse::BrowseSession`],
//!     loops: snapshot → ask the LLM what to do next → execute the
//!     returned tool call → snapshot. Stops on `done` (model
//!     terminated) / `max_steps` / hard error.
//!   - [`llm::LlmClient`] — abstraction over "given a conversation,
//!     return the next assistant turn". Real impl talks to Anthropic
//!     Messages; tests use [`llm::ScriptedClient`] to drive the loop
//!     without burning API calls.
//!   - [`tools::TOOL_SCHEMAS`] — Anthropic-tool-use JSON for every
//!     primitive the model can invoke. Mirrors the
//!     `bouncy_browse_*` MCP tool surface.
//!   - [`judge::Judge`] — scores a finished trajectory against a
//!     task's success criteria. Today's implementation is a string
//!     match on the model's `done` answer; pluggable so a real
//!     WebArena run can swap in WebArena's official judge.
//!
//! This crate is **not published** (publish=false). It lives in the
//! repo as the foundation for a leaderboard submission to
//! `leaderboard.steel.dev`. The runtime cost (LLM API + dockerized
//! WebArena fixtures) is the user's, not crates.io's.

pub mod agent;
pub mod fixture;
pub mod judge;
pub mod llm;
pub mod task;
pub mod tools;
