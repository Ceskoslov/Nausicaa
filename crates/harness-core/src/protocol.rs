use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::id::CallId;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: CallId,
    pub name: String,
    pub arguments: Value,
}

impl ToolCall {
    #[must_use]
    pub fn new(name: impl Into<String>, arguments: Value) -> Self {
        Self {
            id: CallId::new(),
            name: name.into(),
            arguments,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EffectKind {
    ReadOnly,
    WorkspaceWrite,
    ExternalSideEffect,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RetrySafety {
    Safe,
    Idempotent,
    Unsafe,
}

/// The exact, normalized action that policy and approval bind to.
///
/// A tool must resolve paths, defaults, aliases, and execution scope before it
/// returns this value. Execution receives this same value, so it cannot silently
/// swap in the model's unnormalized arguments after approval.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct CanonicalAction {
    pub tool_name: String,
    pub arguments: Value,
    pub effect: EffectKind,
    pub retry_safety: RetrySafety,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
}

impl CanonicalAction {
    #[must_use]
    pub fn new(
        tool_name: impl Into<String>,
        arguments: Value,
        effect: EffectKind,
        retry_safety: RetrySafety,
    ) -> Self {
        Self {
            tool_name: tool_name.into(),
            arguments,
            effect,
            retry_safety,
            scope: None,
        }
    }

    #[must_use]
    pub fn in_scope(mut self, scope: impl Into<String>) -> Self {
        self.scope = Some(scope.into());
        self
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct PreparedToolCall {
    pub call_id: CallId,
    pub action: CanonicalAction,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReceiptStatus {
    Succeeded,
    Failed,
    Denied,
    Unknown,
}

/// Durable result of a tool request. Every model-emitted call gets one receipt,
/// including denied, malformed, interrupted, and unknown calls.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ToolReceipt {
    pub call_id: CallId,
    pub tool_name: String,
    pub status: ReceiptStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub action: Option<CanonicalAction>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl ToolReceipt {
    #[must_use]
    pub fn succeeded(prepared: PreparedToolCall, output: Value) -> Self {
        Self {
            call_id: prepared.call_id,
            tool_name: prepared.action.tool_name.clone(),
            status: ReceiptStatus::Succeeded,
            action: Some(prepared.action),
            output: Some(output),
            error: None,
        }
    }

    #[must_use]
    pub fn failed(prepared: PreparedToolCall, error: impl Into<String>) -> Self {
        Self {
            call_id: prepared.call_id,
            tool_name: prepared.action.tool_name.clone(),
            status: ReceiptStatus::Failed,
            action: Some(prepared.action),
            output: None,
            error: Some(error.into()),
        }
    }

    #[must_use]
    pub fn denied(
        call_id: CallId,
        tool_name: impl Into<String>,
        action: Option<CanonicalAction>,
        reason: impl Into<String>,
    ) -> Self {
        Self {
            call_id,
            tool_name: tool_name.into(),
            status: ReceiptStatus::Denied,
            action,
            output: None,
            error: Some(reason.into()),
        }
    }

    #[must_use]
    pub fn malformed(call: &ToolCall, error: impl Into<String>) -> Self {
        Self {
            call_id: call.id.clone(),
            tool_name: call.name.clone(),
            status: ReceiptStatus::Failed,
            action: None,
            output: None,
            error: Some(error.into()),
        }
    }

    #[must_use]
    pub fn unknown(prepared: PreparedToolCall, reason: impl Into<String>) -> Self {
        Self {
            call_id: prepared.call_id,
            tool_name: prepared.action.tool_name.clone(),
            status: ReceiptStatus::Unknown,
            action: Some(prepared.action),
            output: None,
            error: Some(reason.into()),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "snake_case")]
pub enum TranscriptMessage {
    User {
        content: String,
    },
    Assistant {
        content: String,
        tool_calls: Vec<ToolCall>,
    },
    Tool {
        receipt: ToolReceipt,
    },
}
