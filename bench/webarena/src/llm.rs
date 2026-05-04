//! LLM client abstraction.
//!
//! [`LlmClient`] is the only thing the agent loop depends on: given
//! the conversation so far + the tool schemas, return the next
//! assistant turn (text + zero or more tool calls). Two impls:
//!
//!   - [`AnthropicClient`] hits Anthropic's Messages API. Used for
//!     real benchmark runs.
//!   - [`ScriptedClient`] returns a pre-canned sequence of turns —
//!     used by integration tests so we exercise the loop end-to-end
//!     without burning API calls.

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::Mutex;

use crate::tools::ToolCall;

/// One turn in the conversation. Mirrors Anthropic's content-block
/// shape: a turn is either a text block, a model-issued tool call,
/// or a tool-result block we send back.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Block {
    Text {
        text: String,
    },
    ToolUse {
        id: String,
        call: ToolCall,
    },
    ToolResult {
        tool_use_id: String,
        content: String,
        is_error: bool,
    },
}

/// One full message — role + ordered blocks.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: Vec<Block>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    User,
    Assistant,
}

/// A model-emitted assistant turn. `text` is the optional plain-text
/// thinking-out-loud; `calls` is the ordered list of tool calls the
/// model wants the harness to dispatch (may be empty).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AssistantTurn {
    pub text: String,
    /// Each entry pairs the Anthropic `tool_use_id` with the call.
    /// The harness needs the id to attach the matching `tool_result`
    /// in the next user turn.
    pub calls: Vec<(String, ToolCall)>,
}

#[async_trait]
pub trait LlmClient: Send + Sync {
    async fn next_turn(
        &self,
        system: &str,
        messages: &[Message],
        tools: &[Value],
    ) -> Result<AssistantTurn>;
}

// ---- ScriptedClient ---------------------------------------------------------

/// Test-only client that hands back a pre-recorded sequence of
/// assistant turns. Useful for exercising the agent loop without
/// burning real API calls and to make smoke tests deterministic.
pub struct ScriptedClient {
    turns: Mutex<std::collections::VecDeque<AssistantTurn>>,
}

impl ScriptedClient {
    pub fn new(turns: Vec<AssistantTurn>) -> Self {
        Self {
            turns: Mutex::new(turns.into()),
        }
    }
}

#[async_trait]
impl LlmClient for ScriptedClient {
    async fn next_turn(
        &self,
        _system: &str,
        _messages: &[Message],
        _tools: &[Value],
    ) -> Result<AssistantTurn> {
        let mut g = self.turns.lock().expect("ScriptedClient mutex");
        g.pop_front()
            .ok_or_else(|| anyhow!("scripted client ran out of turns"))
    }
}

// ---- AnthropicClient --------------------------------------------------------

/// Real Anthropic Messages API client. Not used in tests; the
/// integration test uses [`ScriptedClient`] so the suite stays
/// hermetic.
///
/// Wire format reference:
///   <https://docs.anthropic.com/en/api/messages>
pub struct AnthropicClient {
    api_key: String,
    model: String,
    max_tokens: u32,
    endpoint: String,
}

impl AnthropicClient {
    /// Read the API key from the `ANTHROPIC_API_KEY` env var. Errors
    /// if the var is unset or empty so the harness fails loudly
    /// rather than sending unauthenticated requests.
    pub fn from_env(model: impl Into<String>) -> Result<Self> {
        let api_key =
            std::env::var("ANTHROPIC_API_KEY").map_err(|_| anyhow!("ANTHROPIC_API_KEY not set"))?;
        if api_key.is_empty() {
            return Err(anyhow!("ANTHROPIC_API_KEY is empty"));
        }
        Ok(Self {
            api_key,
            model: model.into(),
            max_tokens: 4096,
            endpoint: "https://api.anthropic.com/v1/messages".into(),
        })
    }
}

#[async_trait]
impl LlmClient for AnthropicClient {
    async fn next_turn(
        &self,
        system: &str,
        messages: &[Message],
        tools: &[Value],
    ) -> Result<AssistantTurn> {
        // Convert the harness's message shape to Anthropic's wire
        // format. The harness keeps ToolResult under the User role;
        // the API expects the same.
        let api_messages: Vec<Value> = messages.iter().map(message_to_api).collect();
        let body = serde_json::json!({
            "model": self.model,
            "max_tokens": self.max_tokens,
            "system": system,
            "tools": tools,
            "messages": api_messages,
        });

        let response = post_json(&self.endpoint, &self.api_key, &body).await?;
        parse_assistant_turn(&response)
    }
}

fn message_to_api(m: &Message) -> Value {
    let role = match m.role {
        Role::User => "user",
        Role::Assistant => "assistant",
    };
    let content: Vec<Value> = m.content.iter().map(block_to_api).collect();
    serde_json::json!({"role": role, "content": content})
}

fn block_to_api(b: &Block) -> Value {
    match b {
        Block::Text { text } => serde_json::json!({"type": "text", "text": text}),
        Block::ToolUse { id, call } => serde_json::json!({
            "type": "tool_use",
            "id": id,
            "name": call.name,
            "input": call.input,
        }),
        Block::ToolResult {
            tool_use_id,
            content,
            is_error,
        } => serde_json::json!({
            "type": "tool_result",
            "tool_use_id": tool_use_id,
            "content": content,
            "is_error": is_error,
        }),
    }
}

fn parse_assistant_turn(response: &Value) -> Result<AssistantTurn> {
    let blocks = response
        .get("content")
        .and_then(|v| v.as_array())
        .ok_or_else(|| anyhow!("response missing content array: {response}"))?;
    let mut text = String::new();
    let mut calls = Vec::new();
    for b in blocks {
        match b.get("type").and_then(|v| v.as_str()) {
            Some("text") => {
                if let Some(t) = b.get("text").and_then(|v| v.as_str()) {
                    if !text.is_empty() {
                        text.push('\n');
                    }
                    text.push_str(t);
                }
            }
            Some("tool_use") => {
                let id = b
                    .get("id")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| anyhow!("tool_use missing id"))?
                    .to_string();
                let name = b
                    .get("name")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| anyhow!("tool_use missing name"))?
                    .to_string();
                let input = b.get("input").cloned().unwrap_or(Value::Null);
                calls.push((id, ToolCall { name, input }));
            }
            _ => {}
        }
    }
    Ok(AssistantTurn { text, calls })
}

/// Hyper + rustls POST. We avoid `reqwest` to keep the dependency
/// graph tight — the workspace already pulls in hyper-rustls for
/// scraping, so reusing it is free.
async fn post_json(url: &str, api_key: &str, body: &Value) -> Result<Value> {
    use http_body_util::{BodyExt, Full};
    use hyper_util::{client::legacy::Client, rt::TokioExecutor};

    let connector = hyper_rustls::HttpsConnectorBuilder::new()
        .with_native_roots()?
        .https_or_http()
        .enable_http1()
        .enable_http2()
        .build();
    let client: Client<_, Full<bytes::Bytes>> =
        Client::builder(TokioExecutor::new()).build(connector);

    let body_bytes = serde_json::to_vec(body)?;
    let req = hyper::Request::builder()
        .method("POST")
        .uri(url)
        .header("content-type", "application/json")
        .header("x-api-key", api_key)
        .header("anthropic-version", "2023-06-01")
        .body(Full::new(bytes::Bytes::from(body_bytes)))?;
    let resp = client.request(req).await?;
    let status = resp.status();
    let body = resp.into_body().collect().await?.to_bytes();
    if !status.is_success() {
        return Err(anyhow!(
            "anthropic api {}: {}",
            status,
            String::from_utf8_lossy(&body)
        ));
    }
    Ok(serde_json::from_slice(&body)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn scripted_client_pops_in_order() {
        let s = ScriptedClient::new(vec![
            AssistantTurn {
                text: "first".into(),
                calls: vec![],
            },
            AssistantTurn {
                text: "second".into(),
                calls: vec![],
            },
        ]);
        assert_eq!(s.next_turn("", &[], &[]).await.unwrap().text, "first");
        assert_eq!(s.next_turn("", &[], &[]).await.unwrap().text, "second");
        assert!(s.next_turn("", &[], &[]).await.is_err());
    }

    #[test]
    fn parse_assistant_turn_extracts_text_and_tool_calls() {
        let response = json!({
            "content": [
                {"type": "text", "text": "I'll click."},
                {"type": "tool_use", "id": "call_1", "name": "click", "input": {"index": 3}},
            ]
        });
        let t = parse_assistant_turn(&response).unwrap();
        assert_eq!(t.text, "I'll click.");
        assert_eq!(t.calls.len(), 1);
        assert_eq!(t.calls[0].0, "call_1");
        assert_eq!(t.calls[0].1.name, "click");
    }

    #[test]
    fn parse_assistant_turn_handles_no_tool_calls() {
        let response = json!({"content": [{"type": "text", "text": "just text"}]});
        let t = parse_assistant_turn(&response).unwrap();
        assert_eq!(t.text, "just text");
        assert!(t.calls.is_empty());
    }

    #[test]
    fn block_to_api_serializes_tool_use() {
        let b = Block::ToolUse {
            id: "x".into(),
            call: ToolCall {
                name: "fill".into(),
                input: json!({"value": "y"}),
            },
        };
        let v = block_to_api(&b);
        assert_eq!(v["type"], "tool_use");
        assert_eq!(v["id"], "x");
        assert_eq!(v["name"], "fill");
        assert_eq!(v["input"]["value"], "y");
    }

    #[test]
    fn block_to_api_serializes_tool_result_with_error_flag() {
        let b = Block::ToolResult {
            tool_use_id: "x".into(),
            content: "oops".into(),
            is_error: true,
        };
        let v = block_to_api(&b);
        assert_eq!(v["type"], "tool_result");
        assert_eq!(v["is_error"], true);
    }

    #[test]
    fn anthropic_client_from_env_errors_when_unset() {
        let prev = std::env::var("ANTHROPIC_API_KEY").ok();
        // SAFETY: tests run in the same process; we restore at the end.
        unsafe {
            std::env::remove_var("ANTHROPIC_API_KEY");
        }
        let r = AnthropicClient::from_env("claude-x");
        assert!(r.is_err());
        if let Some(v) = prev {
            unsafe {
                std::env::set_var("ANTHROPIC_API_KEY", v);
            }
        }
    }
}
