use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::context::CompiledContext;
use crate::control::CancellationToken;
use crate::id::{ThreadId, TurnId};
use crate::protocol::ToolCall;
use crate::tool::ToolDefinition;

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Diagnostic durations in milliseconds, not durable execution evidence.
/// DNS/TCP/TLS are phase durations. First-byte/first-text are elapsed from request
/// start; total uses a monotonic host clock. Missing measurements stay unknown.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct RequestTimings {
    pub dns_ms: Option<u64>,
    pub tcp_connect_ms: Option<u64>,
    pub tls_ms: Option<u64>,
    pub first_byte_ms: Option<u64>,
    pub first_text_ms: Option<u64>,
    pub total_ms: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ModelProgress {
    Started,
    /// Unvalidated draft text only. Never contains executable tool fragments.
    TextDelta {
        text: String,
    },
    /// A completed HTTP/model request is not a completed turn or accepted task.
    Finished {
        timings: RequestTimings,
        outcome: String,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ModelProgressEvent {
    pub thread_id: ThreadId,
    pub turn_id: TurnId,
    pub iteration: usize,
    pub progress: ModelProgress,
}

/// Best-effort, non-durable diagnostics. Implementations should return promptly;
/// progress cannot grant authority or substitute for durable runtime events.
pub trait ModelProgressObserver: Send + Sync {
    fn on_progress(&self, event: &ModelProgressEvent);
}

#[derive(Clone, Default)]
pub struct ModelControl {
    pub cancellation: CancellationToken,
    pub observer: Option<Arc<dyn ModelProgressObserver>>,
}

impl ModelControl {
    pub fn notify(&self, request: &ModelRequest, progress: ModelProgress) {
        if let Some(observer) = &self.observer {
            observer.on_progress(&ModelProgressEvent {
                thread_id: request.thread_id.clone(),
                turn_id: request.turn_id.clone(),
                iteration: request.iteration,
                progress,
            });
        }
    }
}

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

    /// Backward-compatible control seam. Legacy adapters keep their existing
    /// behavior; adapters must override this to interrupt an in-flight request.
    fn complete_controlled<'a>(
        &'a self,
        request: ModelRequest,
        _control: ModelControl,
    ) -> BoxFuture<'a, Result<ModelResponse, ModelError>> {
        self.complete(request)
    }
}
