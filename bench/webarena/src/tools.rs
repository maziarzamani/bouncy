//! Anthropic-shaped tool definitions for the agent + a dispatcher
//! that turns a parsed tool call into a [`bouncy_browse::BrowseSession`]
//! method call.
//!
//! The schemas mirror the `bouncy_browse_*` MCP tool surface so the
//! model's mental model is the same whether it's running through MCP
//! or through this benchmark harness. Indices come from the latest
//! snapshot's `interactive` list — the model receives that list in
//! the user-turn payload, then references entries by `index`.

use anyhow::{anyhow, Result};
use bouncy_browse::{BrowseSession, ReadMode, Target};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// Anthropic-format tool schemas for the bouncy primitives the agent
/// is allowed to call. `done` is the terminal action — the model
/// signals completion by emitting it; the harness reads its `answer`
/// argument as the trajectory's output.
pub fn tool_schemas() -> Vec<Value> {
    vec![
        tool(
            "click",
            "Fire a synthetic click on an element. Pass either `selector` (CSS) or `index` (integer from the current snapshot's interactive list).",
            json!({
                "type": "object",
                "properties": {
                    "selector": {"type": "string"},
                    "index": {"type": "integer"}
                }
            }),
        ),
        tool(
            "fill",
            "Set a form field's value and dispatch input + change events. Pass either `selector` or `index`, plus the new `value`.",
            json!({
                "type": "object",
                "properties": {
                    "selector": {"type": "string"},
                    "index": {"type": "integer"},
                    "value": {"type": "string"}
                },
                "required": ["value"]
            }),
        ),
        tool(
            "submit",
            "Submit a form (or the form containing the matched submit button). Pass either `selector` or `index`.",
            json!({
                "type": "object",
                "properties": {
                    "selector": {"type": "string"},
                    "index": {"type": "integer"}
                }
            }),
        ),
        tool(
            "click_text",
            "Click the first link or button whose visible text matches. Trimmed + ASCII-case-insensitive; exact match preferred.",
            json!({
                "type": "object",
                "properties": {"text": {"type": "string"}},
                "required": ["text"]
            }),
        ),
        tool(
            "select_option",
            "Set a <select>'s value (matches against option.value first, then option text). Pass either `selector` or `index`.",
            json!({
                "type": "object",
                "properties": {
                    "selector": {"type": "string"},
                    "index": {"type": "integer"},
                    "value": {"type": "string"}
                },
                "required": ["value"]
            }),
        ),
        tool(
            "press_key",
            "Dispatch a keyboard event on an element. `key` is a single character or a named key (Enter, Tab, Escape, Backspace, ArrowUp/Down/Left/Right).",
            json!({
                "type": "object",
                "properties": {
                    "selector": {"type": "string"},
                    "index": {"type": "integer"},
                    "key": {"type": "string"}
                },
                "required": ["key"]
            }),
        ),
        tool(
            "goto",
            "Navigate the session to a fresh URL. Cookies and session state are preserved.",
            json!({
                "type": "object",
                "properties": {"url": {"type": "string"}},
                "required": ["url"]
            }),
        ),
        tool(
            "read",
            "Read text / HTML / attribute values from elements matching `selector` or `index`. `mode` is text (default) | html | attr:NAME.",
            json!({
                "type": "object",
                "properties": {
                    "selector": {"type": "string"},
                    "index": {"type": "integer"},
                    "mode": {"type": "string"}
                }
            }),
        ),
        tool(
            "wait_for",
            "Block until a CSS selector matches or text appears in the body. Pass exactly one of `selector` / `text`. Defaults to a 5s timeout.",
            json!({
                "type": "object",
                "properties": {
                    "selector": {"type": "string"},
                    "text": {"type": "string"},
                    "timeout_ms": {"type": "integer"}
                }
            }),
        ),
        tool(
            "back",
            "Re-navigate to the previously visited URL.",
            json!({"type": "object", "properties": {}}),
        ),
        tool(
            "done",
            "Signal task completion. `answer` is the final text answer (when the task asks a question) or a brief summary of what was accomplished. The agent loop terminates after this.",
            json!({
                "type": "object",
                "properties": {"answer": {"type": "string"}},
                "required": ["answer"]
            }),
        ),
    ]
}

fn tool(name: &str, description: &str, schema: Value) -> Value {
    json!({
        "name": name,
        "description": description,
        "input_schema": schema,
    })
}

/// Outcome of a single tool dispatch. `Done` tells the agent loop
/// to stop and return the answer to the judge.
#[derive(Debug, Clone)]
pub enum DispatchOutcome {
    /// The tool ran and the page state may have changed; the agent
    /// loop should snapshot before the next LLM turn.
    Continue { brief: String },
    /// The model called `done` — terminate the loop with this answer.
    Done { answer: String },
}

/// Parse + execute one tool call against the live session. Returns
/// either `Continue` (with a short summary the loop can feed back
/// to the model) or `Done` (terminal). Errors propagate up so the
/// loop can decide whether to surface them to the model or stop.
pub async fn dispatch(
    session: &BrowseSession,
    name: &str,
    input: &Value,
) -> Result<DispatchOutcome> {
    match name {
        "click" => {
            let target = parse_target(input)?;
            session.click_target(target).await?;
            Ok(DispatchOutcome::Continue {
                brief: "click ok".into(),
            })
        }
        "fill" => {
            let target = parse_target(input)?;
            let value = string_field(input, "value")?;
            session.fill_target(target, &value).await?;
            Ok(DispatchOutcome::Continue {
                brief: format!("fill ok ({} chars)", value.chars().count()),
            })
        }
        "submit" => {
            let target = parse_target(input)?;
            session.submit_target(target).await?;
            Ok(DispatchOutcome::Continue {
                brief: "submit ok".into(),
            })
        }
        "click_text" => {
            let text = string_field(input, "text")?;
            session.click_text(&text).await?;
            Ok(DispatchOutcome::Continue {
                brief: format!("click_text ok ({text:?})"),
            })
        }
        "select_option" => {
            let target = parse_target(input)?;
            let value = string_field(input, "value")?;
            session.select_option(target, &value).await?;
            Ok(DispatchOutcome::Continue {
                brief: "select_option ok".into(),
            })
        }
        "press_key" => {
            let target = parse_target(input)?;
            let key = string_field(input, "key")?;
            session.press_key(target, &key).await?;
            Ok(DispatchOutcome::Continue {
                brief: format!("press_key ok ({key})"),
            })
        }
        "goto" => {
            let url = string_field(input, "url")?;
            session.goto(&url).await?;
            Ok(DispatchOutcome::Continue {
                brief: format!("goto {url}"),
            })
        }
        "read" => {
            let target = parse_target(input)?;
            let mode = match input.get("mode").and_then(|v| v.as_str()) {
                None | Some("") | Some("text") => ReadMode::Text,
                Some("html") => ReadMode::Html,
                Some(s) if s.starts_with("attr:") => ReadMode::Attr(s[5..].to_string()),
                Some(other) => return Err(anyhow!("unknown read mode {other:?}")),
            };
            let matches = session.read_target(target, mode).await?;
            // The brief is the read output itself — the agent uses
            // this to reason about what's on the page without an
            // extra round trip.
            let joined = matches.join("\n");
            let truncated = truncate_for_brief(&joined);
            Ok(DispatchOutcome::Continue {
                brief: format!("read returned {} match(es): {truncated}", matches.len()),
            })
        }
        "wait_for" => {
            let timeout_ms = input
                .get("timeout_ms")
                .and_then(|v| v.as_u64())
                .unwrap_or(5_000);
            let sel = input.get("selector").and_then(|v| v.as_str());
            let text = input.get("text").and_then(|v| v.as_str());
            match (sel, text) {
                (Some(s), None) => {
                    session.wait_for(s, timeout_ms).await?;
                }
                (None, Some(t)) => {
                    session.wait_for_text(t, timeout_ms).await?;
                }
                _ => return Err(anyhow!("wait_for requires exactly one of selector / text")),
            }
            Ok(DispatchOutcome::Continue {
                brief: "wait_for ok".into(),
            })
        }
        "back" => {
            session.back().await?;
            Ok(DispatchOutcome::Continue {
                brief: "back ok".into(),
            })
        }
        "done" => {
            let answer = string_field(input, "answer")?;
            Ok(DispatchOutcome::Done { answer })
        }
        other => Err(anyhow!("unknown tool {other:?}")),
    }
}

fn parse_target(input: &Value) -> Result<Target> {
    if let Some(idx) = input.get("index").and_then(|v| v.as_u64()) {
        return Ok(Target::Index(idx as u32));
    }
    if let Some(sel) = input.get("selector").and_then(|v| v.as_str()) {
        if !sel.is_empty() {
            return Ok(Target::selector(sel));
        }
    }
    Err(anyhow!("missing selector or index in tool args"))
}

fn string_field(input: &Value, key: &str) -> Result<String> {
    input
        .get(key)
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| anyhow!("missing string field {key:?}"))
}

const BRIEF_CAP: usize = 400;

fn truncate_for_brief(s: &str) -> String {
    if s.len() <= BRIEF_CAP {
        return s.to_string();
    }
    let mut end = BRIEF_CAP;
    while !s.is_char_boundary(end) && end > 0 {
        end -= 1;
    }
    format!("{} […]", &s[..end])
}

/// Compact, JSON-serialised description of a tool call the model
/// emitted. Used in tests and trace logs. Mirrors the shape
/// Anthropic's API delivers (`name` + `input` object).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub name: String,
    pub input: Value,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_schemas_include_done_and_indexed_targets() {
        let schemas = tool_schemas();
        let names: Vec<&str> = schemas
            .iter()
            .map(|s| s["name"].as_str().unwrap())
            .collect();
        assert!(names.contains(&"click"));
        assert!(names.contains(&"fill"));
        assert!(names.contains(&"done"));
        assert!(names.contains(&"click_text"));
        assert!(names.contains(&"select_option"));
        assert!(names.contains(&"press_key"));
        assert!(names.contains(&"wait_for"));
    }

    #[test]
    fn parse_target_prefers_index_over_selector() {
        let input = json!({"index": 5, "selector": "#x"});
        let t = parse_target(&input).unwrap();
        assert!(matches!(t, Target::Index(5)));
    }

    #[test]
    fn parse_target_falls_back_to_selector() {
        let input = json!({"selector": "h1"});
        let t = parse_target(&input).unwrap();
        match t {
            Target::Selector(s) => assert_eq!(s, "h1"),
            _ => panic!("expected Selector"),
        }
    }

    #[test]
    fn parse_target_errors_when_neither_present() {
        assert!(parse_target(&json!({})).is_err());
        assert!(parse_target(&json!({"selector": ""})).is_err());
    }

    #[test]
    fn string_field_errors_when_missing() {
        assert!(string_field(&json!({}), "x").is_err());
        assert!(string_field(&json!({"x": "ok"}), "x").is_ok());
    }

    #[test]
    fn truncate_for_brief_caps_long_strings() {
        let long = "a".repeat(BRIEF_CAP * 2);
        let out = truncate_for_brief(&long);
        assert!(out.ends_with("[…]"));
        assert!(out.len() <= BRIEF_CAP + 6);
    }

    #[test]
    fn truncate_for_brief_passes_short_through() {
        assert_eq!(truncate_for_brief("hello"), "hello");
    }
}
