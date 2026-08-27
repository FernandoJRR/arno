//! Model runtime seam (SPEC §5.4, STACK.md §4).
//!
//! All model access goes through [`ModelProvider`] (AGENTS.md #4). Ollama is
//! only the first adapter impl — future providers drop in behind the trait
//! without touching the orchestrator.

pub mod ollama;

use async_trait::async_trait;
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

impl Role {
    pub fn as_str(&self) -> &'static str {
        match self {
            Role::System => "system",
            Role::User => "user",
            Role::Assistant => "assistant",
            Role::Tool => "tool",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ChatMessage {
    pub role: Role,
    pub content: String,
    /// Set on Tool messages to pair a result with the assistant tool call that
    /// produced it (context trimming must never split the pair — SPEC §8).
    pub tool_call_id: Option<String>,
}

impl ChatMessage {
    pub fn new(role: Role, content: impl Into<String>) -> Self {
        Self {
            role,
            content: content.into(),
            tool_call_id: None,
        }
    }

    pub fn tool_result(call_id: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: Role::Tool,
            content: content.into(),
            tool_call_id: Some(call_id.into()),
        }
    }
}

#[derive(Debug, Clone)]
pub struct CompletionRequest {
    /// Chronological transcript including the system message.
    pub messages: Vec<ChatMessage>,
    /// Merged tool schemas from the MCP registry (empty until M1).
    pub tools: Vec<Value>,
    /// Provider-neutral context budget (SPEC §8); Ollama maps it to num_ctx.
    pub context_tokens: u32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub args: Value,
}

#[derive(Debug, Clone, PartialEq)]
pub enum CompletionOutput {
    Final(String),
    ToolCalls(Vec<ToolCall>),
}

#[derive(Debug, thiserror::Error)]
pub enum ModelError {
    #[error("model transport error: {0}")]
    Transport(String),
    #[error("malformed model response: {0}")]
    Malformed(String),
    #[error("model call timed out")]
    Timeout,
}

#[async_trait]
pub trait ModelProvider: Send + Sync {
    async fn complete(&self, req: CompletionRequest) -> Result<CompletionOutput, ModelError>;
}
