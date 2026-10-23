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
    /// Set on an Assistant message that requested tool calls — what the model
    /// asked for (tool + args), so the *next* completion (even later in the
    /// same order's loop) can see its own prior request instead of only the
    /// orphaned result. Without this, a tool result travels with no matching
    /// request ever having been shown back to the model.
    pub tool_calls: Option<Vec<ToolCall>>,
}

impl ChatMessage {
    pub fn new(role: Role, content: impl Into<String>) -> Self {
        Self {
            role,
            content: content.into(),
            tool_call_id: None,
            tool_calls: None,
        }
    }

    pub fn tool_result(call_id: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: Role::Tool,
            content: content.into(),
            tool_call_id: Some(call_id.into()),
            tool_calls: None,
        }
    }

    /// The assistant's own turn requesting one or more tool calls — mirrors
    /// the native Ollama wire shape (empty `content`, a `tool_calls` array).
    pub fn assistant_tool_calls(calls: Vec<ToolCall>) -> Self {
        Self {
            role: Role::Assistant,
            content: String::new(),
            tool_call_id: None,
            tool_calls: Some(calls),
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
    /// JSON schema the reply must match (structured output), e.g. the
    /// router's group enum (SPEC §11 #23). `None` = free text / tool calls.
    pub format: Option<Value>,
}

/// What a readiness probe found (SPEC §11 #31).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ModelProbe {
    pub reachable: bool,
    /// Share of the loaded model on a GPU; None when unknown or not loaded.
    pub gpu_share: Option<f32>,
}

/// Prompt/latency figures for measurement (the replay tool); never content.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct CompletionStats {
    pub prompt_tokens: u64,
    pub output_tokens: u64,
    pub elapsed_ms: u64,
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

    /// Readiness probe (SPEC §11 #31), without a completion. Providers that
    /// cannot tell report reachable with an unknown placement.
    async fn probe(&self) -> ModelProbe {
        ModelProbe {
            reachable: true,
            gpu_share: None,
        }
    }

    /// Same as [`Self::complete`], plus stats when the provider reports them.
    async fn complete_with_stats(
        &self,
        req: CompletionRequest,
    ) -> Result<(CompletionOutput, Option<CompletionStats>), ModelError> {
        self.complete(req).await.map(|out| (out, None))
    }
}
