use std::future::Future;
use std::pin::Pin;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::context::CompiledContext;
use crate::id::{ThreadId, TurnId};
use crate::protocol::ToolCall;
use crate::tool::ToolDefinition;

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ModelRequest {
    pub thread_id: ThreadId,
    pub turn_id: TurnId,
    pub iteration: usize,
    pub context: CompiledContext,
    /// This list is already projected through tool policy. Denied tools are not
    /// sent to the model at all.
    pub tools: Vec<ToolDefinition>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    EndTurn,
    ToolUse,
    MaxTokens,
    Other,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct TokenUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ModelResponse {
    pub content: String,
    pub tool_calls: Vec<ToolCall>,
    pub stop_reason: StopReason,
    pub usage: TokenUsage,
}

impl ModelResponse {
    #[must_use]
    pub fn text(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            tool_calls: Vec::new(),
            stop_reason: StopReason::EndTurn,
            usage: TokenUsage::default(),
        }
    }

    #[must_use]
    pub fn tool_calls(tool_calls: Vec<ToolCall>) -> Self {
        Self {
            content: String::new(),
            tool_calls,
            stop_reason: StopReason::ToolUse,
            usage: TokenUsage::default(),
        }
    }
}

#[derive(Clone, Debug, Error, Eq, PartialEq)]
#[error("model request failed: {message}")]
pub struct ModelError {
    pub message: String,
    pub retryable: bool,
}

impl ModelError {
    #[must_use]
    pub fn new(message: impl Into<String>, retryable: bool) -> Self {
        Self {
            message: message.into(),
            retryable,
        }
    }
}

/// Provider-neutral model boundary. Adapters translate `ModelRequest` into a
/// provider protocol and translate streamed/provider output back into one
/// completed response.
pub trait ModelAdapter: Send + Sync {
    fn complete<'a>(
        &'a self,
        request: ModelRequest,
    ) -> BoxFuture<'a, Result<ModelResponse, ModelError>>;
}
