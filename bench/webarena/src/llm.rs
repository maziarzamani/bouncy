//! LLM client abstraction.
//!
//! [`LlmClient`] is the only thing the agent loop depends on: given
//! the conversation so far + the tool schemas, return the next
//! assistant turn (text + zero or more tool calls). Three impls:
//!
//!   - [`AnthropicClient`] — direct Anthropic Messages API. Auth
//!     via `ANTHROPIC_API_KEY`.
//!   - [`BedrockClient`] (under the `bedrock` feature) — AWS
//!     Bedrock's Converse API for the same Claude models. Auth via
//!     the standard AWS credential chain (env vars, `~/.aws`, IAM
//!     role). Pick this when your billing / data-residency story
//!     wants Bedrock instead of Anthropic-direct.
//!   - [`ScriptedClient`] — returns a pre-canned sequence of turns;
//!     used by integration tests so the suite stays hermetic.

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

// ---- BedrockClient ----------------------------------------------------------

#[cfg(feature = "bedrock")]
mod bedrock {
    use super::{AssistantTurn, Block, LlmClient, Message, Role};
    use crate::tools::ToolCall;
    use anyhow::{anyhow, Result};
    use async_trait::async_trait;
    use aws_sdk_bedrockruntime::types::{
        ContentBlock, ConversationRole, Message as BrMessage, SystemContentBlock, Tool,
        ToolConfiguration, ToolInputSchema, ToolResultBlock, ToolResultContentBlock,
        ToolResultStatus, ToolSpecification, ToolUseBlock,
    };
    use aws_sdk_bedrockruntime::Client as BedrockApiClient;
    use aws_smithy_types::Document;
    use serde_json::Value;

    /// AWS Bedrock client for the Converse API. Uses the standard
    /// AWS credential chain — env vars (`AWS_ACCESS_KEY_ID` etc.),
    /// `~/.aws/credentials`, or IAM role on EC2/ECS/Lambda.
    ///
    /// `model_id` is a Bedrock model identifier — for Claude that's
    /// `anthropic.claude-sonnet-4-5-...-v1:0` style, NOT the bare
    /// Anthropic ID. Inference profile ARNs also work.
    pub struct BedrockClient {
        api: BedrockApiClient,
        model_id: String,
    }

    /// Format an SDK error with its full source chain.
    ///
    /// `aws-sdk` errors usually wrap a more useful inner error
    /// (`SdkError::DispatchFailure(ConnectorError(io error: …))`),
    /// but `Display` only prints the top — so you get cryptic
    /// "dispatch failure" messages with no signal about *what*
    /// failed. Walking `source()` and concatenating gets you the
    /// full chain in one line: TLS handshake errors, DNS lookup
    /// failures, IO timeouts, all readable.
    fn describe_sdk_error<E: std::error::Error + 'static>(e: &E) -> String {
        let mut out = e.to_string();
        let mut cur: Option<&dyn std::error::Error> = e.source();
        while let Some(s) = cur {
            out.push_str(" → ");
            out.push_str(&s.to_string());
            cur = s.source();
        }
        out
    }

    impl BedrockClient {
        /// Build from the default AWS config. Honours `AWS_REGION`
        /// / `AWS_DEFAULT_REGION` env vars; pass `region` to
        /// override.
        pub async fn from_env(model_id: impl Into<String>, region: Option<String>) -> Result<Self> {
            let model_id = model_id.into();
            let region_label = region.clone().unwrap_or_else(|| "<env-default>".into());
            // The credential chain can do real I/O (env, ~/.aws,
            // IMDS, SSO, etc.) — print so a stalled lookup is
            // diagnosable instead of silent.
            eprintln!("  bedrock: loading AWS config (region={region_label}) …");
            let mut loader = aws_config::defaults(aws_config::BehaviorVersion::latest());
            if let Some(r) = region {
                loader = loader.region(aws_config::Region::new(r));
            }
            let cfg = loader.load().await;
            eprintln!(
                "  bedrock: config loaded (region={}, model={model_id})",
                cfg.region().map(|r| r.as_ref()).unwrap_or("<unknown>")
            );
            let api = BedrockApiClient::new(&cfg);
            Ok(Self { api, model_id })
        }
    }

    #[async_trait]
    impl LlmClient for BedrockClient {
        async fn next_turn(
            &self,
            system: &str,
            messages: &[Message],
            tools: &[Value],
        ) -> Result<AssistantTurn> {
            let br_messages: Vec<BrMessage> = messages
                .iter()
                .map(message_to_bedrock)
                .collect::<Result<Vec<_>>>()?;

            let mut tool_specs: Vec<Tool> = Vec::with_capacity(tools.len());
            for t in tools {
                tool_specs.push(tool_to_bedrock(t)?);
            }
            let tool_cfg = ToolConfiguration::builder()
                .set_tools(Some(tool_specs))
                .build()
                .map_err(|e| anyhow!("build tool config: {e}"))?;

            let resp = self
                .api
                .converse()
                .model_id(&self.model_id)
                .system(SystemContentBlock::Text(system.to_string()))
                .set_messages(Some(br_messages))
                .tool_config(tool_cfg)
                .send()
                .await
                .map_err(|e| anyhow!("bedrock converse: {}", describe_sdk_error(&e)))?;

            let output = resp
                .output()
                .ok_or_else(|| anyhow!("bedrock response missing output"))?;
            let msg = output
                .as_message()
                .map_err(|_| anyhow!("bedrock output was not a message"))?;
            assistant_from_bedrock(msg)
        }
    }

    fn message_to_bedrock(m: &Message) -> Result<BrMessage> {
        let role = match m.role {
            Role::User => ConversationRole::User,
            Role::Assistant => ConversationRole::Assistant,
        };
        let content: Vec<ContentBlock> = m
            .content
            .iter()
            .map(block_to_bedrock)
            .collect::<Result<Vec<_>>>()?;
        BrMessage::builder()
            .role(role)
            .set_content(Some(content))
            .build()
            .map_err(|e| anyhow!("build bedrock message: {e}"))
    }

    fn block_to_bedrock(b: &Block) -> Result<ContentBlock> {
        Ok(match b {
            Block::Text { text } => ContentBlock::Text(text.clone()),
            Block::ToolUse { id, call } => {
                let input = json_to_document(&call.input);
                let block = ToolUseBlock::builder()
                    .tool_use_id(id)
                    .name(&call.name)
                    .input(input)
                    .build()
                    .map_err(|e| anyhow!("build tool_use: {e}"))?;
                ContentBlock::ToolUse(block)
            }
            Block::ToolResult {
                tool_use_id,
                content,
                is_error,
            } => {
                let body = ToolResultContentBlock::Text(content.clone());
                let status = if *is_error {
                    ToolResultStatus::Error
                } else {
                    ToolResultStatus::Success
                };
                let block = ToolResultBlock::builder()
                    .tool_use_id(tool_use_id)
                    .content(body)
                    .status(status)
                    .build()
                    .map_err(|e| anyhow!("build tool_result: {e}"))?;
                ContentBlock::ToolResult(block)
            }
        })
    }

    fn tool_to_bedrock(tool: &Value) -> Result<Tool> {
        let name = tool["name"]
            .as_str()
            .ok_or_else(|| anyhow!("tool missing name"))?;
        let description = tool["description"].as_str().unwrap_or("").to_string();
        let schema_json = tool
            .get("input_schema")
            .ok_or_else(|| anyhow!("tool {name} missing input_schema"))?;
        let schema_doc = json_to_document(schema_json);
        let spec = ToolSpecification::builder()
            .name(name)
            .description(description)
            .input_schema(ToolInputSchema::Json(schema_doc))
            .build()
            .map_err(|e| anyhow!("build tool spec: {e}"))?;
        Ok(Tool::ToolSpec(spec))
    }

    fn assistant_from_bedrock(msg: &BrMessage) -> Result<AssistantTurn> {
        let mut text = String::new();
        let mut calls: Vec<(String, ToolCall)> = Vec::new();
        for block in msg.content() {
            match block {
                ContentBlock::Text(t) => {
                    if !text.is_empty() {
                        text.push('\n');
                    }
                    text.push_str(t);
                }
                ContentBlock::ToolUse(tu) => {
                    let id = tu.tool_use_id().to_string();
                    let name = tu.name().to_string();
                    let input = document_to_json(tu.input());
                    calls.push((id, ToolCall { name, input }));
                }
                _ => {
                    // Other block types (image, document, reasoning…)
                    // aren't part of bouncy's tool surface, so we
                    // silently ignore them.
                }
            }
        }
        Ok(AssistantTurn { text, calls })
    }

    /// `serde_json::Value` ↔ AWS smithy `Document` conversion. The
    /// Converse API uses Document for tool input and tool result
    /// payloads; bouncy's harness uses serde_json everywhere else.
    fn json_to_document(v: &Value) -> Document {
        match v {
            Value::Null => Document::Null,
            Value::Bool(b) => Document::Bool(*b),
            Value::Number(n) => {
                if let Some(i) = n.as_i64() {
                    Document::Number(aws_smithy_types::Number::NegInt(i))
                } else if let Some(u) = n.as_u64() {
                    Document::Number(aws_smithy_types::Number::PosInt(u))
                } else if let Some(f) = n.as_f64() {
                    Document::Number(aws_smithy_types::Number::Float(f))
                } else {
                    Document::Null
                }
            }
            Value::String(s) => Document::String(s.clone()),
            Value::Array(a) => Document::Array(a.iter().map(json_to_document).collect()),
            Value::Object(o) => Document::Object(
                o.iter()
                    .map(|(k, v)| (k.clone(), json_to_document(v)))
                    .collect(),
            ),
        }
    }

    fn document_to_json(d: &Document) -> Value {
        match d {
            Document::Null => Value::Null,
            Document::Bool(b) => Value::Bool(*b),
            Document::Number(n) => match n {
                aws_smithy_types::Number::PosInt(u) => Value::Number((*u).into()),
                aws_smithy_types::Number::NegInt(i) => Value::Number((*i).into()),
                aws_smithy_types::Number::Float(f) => serde_json::Number::from_f64(*f)
                    .map(Value::Number)
                    .unwrap_or(Value::Null),
            },
            Document::String(s) => Value::String(s.clone()),
            Document::Array(a) => Value::Array(a.iter().map(document_to_json).collect()),
            Document::Object(o) => Value::Object(
                o.iter()
                    .map(|(k, v)| (k.clone(), document_to_json(v)))
                    .collect(),
            ),
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use serde_json::json;

        #[test]
        fn json_document_round_trip_preserves_structure() {
            let v = json!({
                "name": "click",
                "input": {"index": 5, "selector": "#x", "flags": [true, false]}
            });
            let doc = json_to_document(&v);
            let back = document_to_json(&doc);
            assert_eq!(v, back);
        }

        #[test]
        fn tool_to_bedrock_extracts_name_description_and_schema() {
            let v = json!({
                "name": "click",
                "description": "click an element",
                "input_schema": {"type": "object", "properties": {"selector": {"type": "string"}}}
            });
            let t = tool_to_bedrock(&v).unwrap();
            match t {
                Tool::ToolSpec(spec) => {
                    assert_eq!(spec.name(), "click");
                    assert_eq!(spec.description().unwrap_or(""), "click an element");
                }
                _ => panic!("expected ToolSpec"),
            }
        }
    }
}

#[cfg(feature = "bedrock")]
pub use bedrock::BedrockClient;
