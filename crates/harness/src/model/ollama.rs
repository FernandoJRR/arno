//! First ModelProvider adapter impl: Ollama over the **native** `/api/chat`
//! API — chosen because the OpenAI-compatible `/v1` endpoint cannot set
//! `num_ctx` per request, lacks `tool_choice`, and cannot control model
//! thinking (`think`), all of which the spec's reliability policy depends on
//! (SPEC §8, STACK.md §4).
//!
//! Nothing outside this module may speak HTTP to a model provider.

use crate::model::{
    ChatMessage, CompletionOutput, CompletionRequest, ModelError, ModelProvider, Role, ToolCall,
};
use async_trait::async_trait;
use serde_json::{Value, json};
use std::time::Duration;

pub struct OllamaProvider {
    http: reqwest::Client,
    chat_url: String,
    model: String,
    think: ThinkMode,
}

/// Native-API `think` control for thinking-capable models (qwen3,
/// deepseek-r1, gpt-oss, ...). Booleans toggle the reasoning trace; levels
/// bound its length. Some models ignore booleans (gpt-oss requires a level).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThinkMode {
    Off,
    On,
    Low,
    Medium,
    High,
    Max,
}

impl std::str::FromStr for ThinkMode {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "off" => Ok(Self::Off),
            "on" => Ok(Self::On),
            "low" => Ok(Self::Low),
            "medium" => Ok(Self::Medium),
            "high" => Ok(Self::High),
            "max" => Ok(Self::Max),
            other => Err(format!(
                "invalid think mode {other:?} — expected off|on|low|medium|high|max"
            )),
        }
    }
}

impl ThinkMode {
    /// Wire form accepted by `/api/chat`: bool or level string.
    fn as_json(self) -> Value {
        match self {
            Self::Off => Value::Bool(false),
            Self::On => Value::Bool(true),
            Self::Low => json!("low"),
            Self::Medium => json!("medium"),
            Self::High => json!("high"),
            Self::Max => json!("max"),
        }
    }
}

impl OllamaProvider {
    pub fn new(
        base_url: reqwest::Url,
        model: impl Into<String>,
        timeout: Duration,
        think: ThinkMode,
    ) -> Self {
        let http = reqwest::Client::builder()
            .timeout(timeout)
            .build()
            .expect("reqwest client builds");
        let chat_url = format!("{}/api/chat", base_url.to_string().trim_end_matches('/'));
        Self {
            http,
            chat_url,
            model: model.into(),
            think,
        }
    }

    /// Body construction is pure so the request shape stays unit-testable.
    fn request_body(&self, req: &CompletionRequest) -> Value {
        let mut body = json!({
            "model": self.model,
            // SPEC §8: raise num_ctx first — the default silently truncates
            // and breaks tool use. This is why we use the native API.
            "options": {
                "num_ctx": req.context_tokens,
                "temperature": TOOL_SELECTION_TEMPERATURE,
            },
            // Explicit thinking control; `Off` forces determinism instead of
            // relying on per-model defaults.
            "think": self.think.as_json(),
            "stream": false,
        });
        body["messages"] = Value::Array(req.messages.iter().map(message_body).collect());
        if !req.tools.is_empty() {
            body["tools"] = Value::Array(req.tools.clone());
        }
        body
    }
}

/// Low temperature for tool selection (SPEC §5.4); not a timeout/retry knob.
const TOOL_SELECTION_TEMPERATURE: f32 = 0.2;

#[async_trait]
impl ModelProvider for OllamaProvider {
    async fn complete(&self, req: CompletionRequest) -> Result<CompletionOutput, ModelError> {
        let body = self.request_body(&req);

        let started = std::time::Instant::now();
        let resp = self
            .http
            .post(&self.chat_url)
            .json(&body)
            .send()
            .await
            .map_err(map_transport)?;
        tracing::debug!(
            elapsed_ms = started.elapsed().as_millis() as u64,
            "ollama generation done"
        );
        let status = resp.status();
        let payload: Value = resp
            .json()
            .await
            .map_err(|e| ModelError::Malformed(e.to_string()))?;
        if !status.is_success() {
            return Err(ModelError::Transport(format!(
                "ollama returned {status}: {}",
                payload
                    .get("error")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown error")
            )));
        }

        parse_output(&payload)
    }
}

fn message_body(m: &ChatMessage) -> Value {
    match m.role {
        // Native tool results carry their pairing id in `tool_call_id`.
        Role::Tool => json!({
            "role": "tool",
            "content": m.content,
            "tool_call_id": m.tool_call_id,
        }),
        // The assistant's own prior request — round-trips the same shape
        // `parse_output` reads a completion's tool calls from, so the model
        // sees exactly what it asked for on a later turn, not just the result.
        Role::Assistant if m.tool_calls.is_some() => json!({
            "role": "assistant",
            "content": m.content,
            "tool_calls": m.tool_calls.as_ref().unwrap().iter().map(|c| json!({
                "id": c.id,
                "function": { "name": c.name, "arguments": c.args },
            })).collect::<Vec<_>>(),
        }),
        role => json!({ "role": role.as_str(), "content": m.content }),
    }
}

fn parse_output(payload: &Value) -> Result<CompletionOutput, ModelError> {
    let message = payload
        .get("message")
        .ok_or_else(|| ModelError::Malformed("missing message object".into()))?;
    let content = message
        .get("content")
        .and_then(Value::as_str)
        .ok_or_else(|| ModelError::Malformed("missing message.content".into()))?;

    if let Some(calls) = message.get("tool_calls").and_then(Value::as_array) {
        let parsed: Vec<ToolCall> = calls
            .iter()
            .enumerate()
            .map(|(i, c)| {
                let function = c.get("function").ok_or_else(|| {
                    ModelError::Malformed(format!("tool_calls[{i}] missing function"))
                })?;
                let name = function
                    .get("name")
                    .and_then(Value::as_str)
                    .ok_or_else(|| ModelError::Malformed(format!("tool_calls[{i}] missing name")))?
                    .to_owned();
                // Native API arguments are already an object (not a string).
                let args = function.get("arguments").cloned().unwrap_or(Value::Null);
                let id = c
                    .get("id")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                    .unwrap_or_else(|| format!("call_{i}"));
                Ok(ToolCall { id, name, args })
            })
            .collect::<Result<_, ModelError>>()?;
        if !parsed.is_empty() {
            return Ok(CompletionOutput::ToolCalls(parsed));
        }
    }
    Ok(CompletionOutput::Final(content.to_owned()))
}

fn map_transport(e: reqwest::Error) -> ModelError {
    if e.is_timeout() {
        ModelError::Timeout
    } else {
        ModelError::Transport(e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn think_mode_parses_documented_values_case_insensitively() {
        assert_eq!("off".parse(), Ok(ThinkMode::Off));
        assert_eq!("on".parse(), Ok(ThinkMode::On));
        assert_eq!("low".parse(), Ok(ThinkMode::Low));
        assert_eq!("Medium".parse(), Ok(ThinkMode::Medium));
        assert_eq!(" high ".parse(), Ok(ThinkMode::High));
        assert_eq!("MAX".parse(), Ok(ThinkMode::Max));
        assert!("bogus".parse::<ThinkMode>().is_err());
        assert!("true".parse::<ThinkMode>().is_err());
    }

    #[test]
    fn think_mode_serializes_to_native_wire_form() {
        assert_eq!(ThinkMode::Off.as_json(), Value::Bool(false));
        assert_eq!(ThinkMode::On.as_json(), Value::Bool(true));
        assert_eq!(ThinkMode::Low.as_json(), json!("low"));
        assert_eq!(ThinkMode::Medium.as_json(), json!("medium"));
        assert_eq!(ThinkMode::High.as_json(), json!("high"));
        assert_eq!(ThinkMode::Max.as_json(), json!("max"));
    }

    #[test]
    fn request_body_carries_think_flag_and_options() {
        let provider = OllamaProvider::new(
            "http://127.0.0.1:11434".parse().unwrap(),
            "qwen3:8b",
            Duration::from_secs(5),
            ThinkMode::Max,
        );
        let req = CompletionRequest {
            messages: vec![ChatMessage::new(Role::User, "hi")],
            tools: Vec::new(),
            context_tokens: 4096,
        };
        let body = provider.request_body(&req);
        assert_eq!(body["think"], json!("max"));
        assert_eq!(body["model"], "qwen3:8b");
        assert_eq!(body["options"]["num_ctx"], 4096);
        assert_eq!(body["stream"], false);
    }

    #[test]
    fn assistant_tool_calls_serialize_in_native_wire_shape() {
        let msg = ChatMessage::assistant_tool_calls(vec![ToolCall {
            id: "call_1".into(),
            name: "securo.list_transactions".into(),
            args: json!({"limit": 5}),
        }]);
        let body = message_body(&msg);
        assert_eq!(body["role"], "assistant");
        assert_eq!(body["content"], "");
        assert_eq!(body["tool_calls"][0]["id"], "call_1");
        assert_eq!(
            body["tool_calls"][0]["function"]["name"],
            "securo.list_transactions"
        );
        assert_eq!(body["tool_calls"][0]["function"]["arguments"]["limit"], 5);
    }

    #[test]
    fn plain_assistant_message_has_no_tool_calls_field() {
        let msg = ChatMessage::new(Role::Assistant, "hello");
        let body = message_body(&msg);
        assert_eq!(body["content"], "hello");
        assert!(body.get("tool_calls").is_none());
    }
}
