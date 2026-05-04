//! The agent loop.
//!
//! Hands the model a task instruction + the current page snapshot,
//! reads the tool calls it wants to make, dispatches them against
//! the live [`bouncy_browse::BrowseSession`], and feeds the brief
//! tool-result back in. Loop ends on `done`, on `max_steps`, or on
//! a hard error.
//!
//! The trajectory captured here is what a future judge inspects.
//! Today's judge is a substring match on the model's `done` answer,
//! which is enough for a smoke test; richer rubrics plug in later.

use anyhow::{anyhow, Result};
use bouncy_browse::{BrowseOpts, BrowseSession, PageSnapshot};
use std::sync::Arc;
use std::time::Instant;

use crate::llm::{AssistantTurn, Block, LlmClient, Message, Role};
use crate::task::Task;
use crate::tools::{dispatch, tool_schemas, DispatchOutcome, ToolCall};

/// The system prompt handed to the model. Kept minimal so a
/// reader of the trajectory can understand exactly what the model
/// was told. WebArena-style tasks usually need very little
/// scaffolding because the instruction is self-contained.
pub const SYSTEM_PROMPT: &str = "You drive a headless browser through structured tools. Each user turn includes the current page snapshot — forms, links, buttons, an `interactive` list with stable indices. Reference elements by `index` whenever possible. Issue tool calls until you've completed the user's task, then call `done` with a concise final answer.";

/// Outcome of one agent run.
#[derive(Debug, Clone)]
pub struct Trajectory {
    pub task_id: String,
    /// Final answer the model emitted via `done`, or `None` if the
    /// loop hit `max_steps` / errored without terminating.
    pub answer: Option<String>,
    pub steps: Vec<TrajectoryStep>,
    pub elapsed: std::time::Duration,
    /// Set when the loop terminated without `done` (max_steps reached
    /// or an unrecoverable error). The string is the human-readable
    /// reason — used for debugging the trajectory, not the judge.
    pub stop_reason: StopReason,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StopReason {
    Done,
    MaxSteps,
    Error(String),
}

#[derive(Debug, Clone)]
pub struct TrajectoryStep {
    pub assistant_text: String,
    pub call: ToolCall,
    pub tool_use_id: String,
    /// Brief summary the dispatcher emitted (or the error message
    /// when the dispatch failed). Surfaced back to the model in the
    /// next user turn.
    pub result: String,
    pub is_error: bool,
}

/// Run a single task. Spawns a fresh [`BrowseSession`] so each task
/// is independent — no cookie / V8 leakage across tasks.
pub async fn run_task(
    task: &Task,
    llm: Arc<dyn LlmClient>,
    opts: BrowseOpts,
) -> Result<Trajectory> {
    let started = Instant::now();
    let (session, snap) = BrowseSession::open(&task.start_url, opts).await?;
    let tools = tool_schemas();

    let mut messages: Vec<Message> = Vec::new();
    messages.push(Message {
        role: Role::User,
        content: vec![Block::Text {
            text: format_user_turn(&task.instruction, &snap),
        }],
    });

    let mut steps: Vec<TrajectoryStep> = Vec::new();
    let mut answer: Option<String> = None;
    let mut stop_reason = StopReason::MaxSteps;

    for _step in 0..task.max_steps {
        let turn: AssistantTurn = llm.next_turn(SYSTEM_PROMPT, &messages, &tools).await?;
        // Append the assistant's full turn (text + tool_use blocks)
        // to the conversation so the next user turn can attach
        // tool_results to the right ids.
        let mut assistant_blocks: Vec<Block> = Vec::new();
        if !turn.text.is_empty() {
            assistant_blocks.push(Block::Text {
                text: turn.text.clone(),
            });
        }
        for (id, call) in &turn.calls {
            assistant_blocks.push(Block::ToolUse {
                id: id.clone(),
                call: call.clone(),
            });
        }
        if !assistant_blocks.is_empty() {
            messages.push(Message {
                role: Role::Assistant,
                content: assistant_blocks,
            });
        }
        if turn.calls.is_empty() {
            // No tool call — the model is stuck or commenting. Stop
            // rather than spin: the trajectory shows the last
            // assistant text for the human to inspect.
            stop_reason = StopReason::Error("model returned no tool call".into());
            break;
        }

        let mut user_blocks: Vec<Block> = Vec::new();
        let mut terminate_with: Option<String> = None;
        for (id, call) in turn.calls {
            let result = dispatch(&session, &call.name, &call.input).await;
            match result {
                Ok(DispatchOutcome::Done { answer: a }) => {
                    let summary = format!("done — answer captured ({} chars)", a.chars().count());
                    user_blocks.push(Block::ToolResult {
                        tool_use_id: id.clone(),
                        content: summary.clone(),
                        is_error: false,
                    });
                    steps.push(TrajectoryStep {
                        assistant_text: turn.text.clone(),
                        call,
                        tool_use_id: id,
                        result: summary,
                        is_error: false,
                    });
                    terminate_with = Some(a);
                    break;
                }
                Ok(DispatchOutcome::Continue { brief }) => {
                    user_blocks.push(Block::ToolResult {
                        tool_use_id: id.clone(),
                        content: brief.clone(),
                        is_error: false,
                    });
                    steps.push(TrajectoryStep {
                        assistant_text: turn.text.clone(),
                        call,
                        tool_use_id: id,
                        result: brief,
                        is_error: false,
                    });
                }
                Err(e) => {
                    let msg = e.to_string();
                    user_blocks.push(Block::ToolResult {
                        tool_use_id: id.clone(),
                        content: msg.clone(),
                        is_error: true,
                    });
                    steps.push(TrajectoryStep {
                        assistant_text: turn.text.clone(),
                        call,
                        tool_use_id: id,
                        result: msg,
                        is_error: true,
                    });
                }
            }
        }
        if let Some(a) = terminate_with {
            answer = Some(a);
            stop_reason = StopReason::Done;
            break;
        }

        // Refresh the page snapshot for the next user turn so the
        // model sees the post-action state. Cheap (~1 ms).
        let snap = session
            .snapshot()
            .await
            .map_err(|e| anyhow!("snapshot after step: {e}"))?;
        user_blocks.push(Block::Text {
            text: format_followup_turn(&snap),
        });
        messages.push(Message {
            role: Role::User,
            content: user_blocks,
        });
    }

    Ok(Trajectory {
        task_id: task.id.clone(),
        answer,
        steps,
        elapsed: started.elapsed(),
        stop_reason,
    })
}

fn format_user_turn(instruction: &str, snap: &PageSnapshot) -> String {
    format!(
        "Task: {instruction}\n\n--- current page ---\n{}",
        snapshot_for_prompt(snap)
    )
}

fn format_followup_turn(snap: &PageSnapshot) -> String {
    format!(
        "--- current page after the last action ---\n{}",
        snapshot_for_prompt(snap)
    )
}

/// Compact textual rendering of a [`PageSnapshot`] for the prompt.
/// Keeps the indexed `interactive` list front-and-center because
/// that's how the model is meant to address elements.
fn snapshot_for_prompt(snap: &PageSnapshot) -> String {
    let mut out = String::new();
    out.push_str(&format!("URL: {}\n", snap.url));
    if !snap.title.is_empty() {
        out.push_str(&format!("Title: {}\n", snap.title));
    }
    out.push_str("\nInteractive elements:\n");
    if snap.interactive.is_empty() {
        out.push_str("  (none — page has no forms, fields, links, or buttons)\n");
    }
    for e in &snap.interactive {
        out.push_str(&format!(
            "  [{}] {} — {} ({})\n",
            e.index, e.kind, e.label, e.selector
        ));
    }
    if !snap.text_summary.is_empty() {
        out.push_str("\nText summary:\n");
        out.push_str(&snap.text_summary);
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use bouncy_browse::{
        ButtonSnapshot, FormSnapshot, HeadingSnapshot, InteractiveElement, LinkSnapshot,
        PageSnapshot,
    };
    use std::collections::BTreeMap;

    fn empty_snapshot() -> PageSnapshot {
        PageSnapshot {
            url: "https://x.test/".into(),
            title: "Demo".into(),
            forms: vec![],
            links: vec![],
            buttons: vec![],
            inputs: vec![],
            headings: vec![],
            interactive: vec![],
            text_summary: "hello".into(),
            meta: BTreeMap::new(),
        }
    }

    #[test]
    fn snapshot_for_prompt_renders_url_title_and_interactive() {
        let snap = PageSnapshot {
            interactive: vec![
                InteractiveElement {
                    index: 0,
                    kind: "link".into(),
                    selector: "a".into(),
                    label: "About".into(),
                },
                InteractiveElement {
                    index: 1,
                    kind: "button".into(),
                    selector: "#go".into(),
                    label: "Go".into(),
                },
            ],
            ..empty_snapshot()
        };
        let out = snapshot_for_prompt(&snap);
        assert!(out.contains("URL: https://x.test/"));
        assert!(out.contains("Title: Demo"));
        assert!(out.contains("[0] link — About (a)"));
        assert!(out.contains("[1] button — Go (#go)"));
    }

    #[test]
    fn snapshot_for_prompt_handles_empty_interactive() {
        let snap = empty_snapshot();
        let out = snapshot_for_prompt(&snap);
        assert!(out.contains("(none"));
    }

    #[test]
    fn format_user_turn_includes_task_and_snapshot() {
        let snap = empty_snapshot();
        let out = format_user_turn("click sign up", &snap);
        assert!(out.contains("Task: click sign up"));
        assert!(out.contains("URL: https://x.test/"));
    }

    // Suppress dead-code warnings for the snapshot helper structs
    // imported just for type-completeness.
    #[allow(dead_code)]
    fn _unused_imports(_: FormSnapshot, _: LinkSnapshot, _: ButtonSnapshot, _: HeadingSnapshot) {}
}
