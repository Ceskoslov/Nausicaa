use serde::{Deserialize, Serialize};

use crate::approval::{ApprovalDecision, ApprovalRequest};
use crate::id::{CallId, EventId, ThreadId, TurnId};
use crate::model::{StopReason, TokenUsage};
use crate::protocol::{PreparedToolCall, ToolCall, ToolReceipt};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookPoint {
    BeforeModel,
    BeforeTool,
    AfterTool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RuntimeEvent {
    ThreadStarted,
    TurnStarted,
    UserMessage {
        content: String,
    },
    ContextCompiled {
        iteration: usize,
        prompt_segments: usize,
        message_count: usize,
        visible_tools: Vec<String>,
    },
    ModelRequestStarted {
        iteration: usize,
    },
    AssistantMessage {
        content: String,
        tool_calls: Vec<ToolCall>,
        stop_reason: StopReason,
        usage: TokenUsage,
    },
    ToolPrepared {
        prepared: PreparedToolCall,
    },
    ApprovalRequested {
        request: ApprovalRequest,
    },
    ApprovalResolved {
        call_id: CallId,
        decision: ApprovalDecision,
    },
    ToolExecutionStarted {
        prepared: PreparedToolCall,
    },
    ToolReceiptRecorded {
        receipt: ToolReceipt,
    },
    HookFailed {
        hook: String,
        point: HookPoint,
        reason: String,
    },
    TurnCompleted {
        content: String,
    },
    TurnFailed {
        error: String,
    },
    TurnCancelled,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct EventEnvelope {
    pub id: EventId,
    pub sequence: u64,
    pub at_unix_ms: u128,
    pub thread_id: ThreadId,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<TurnId>,
    pub event: RuntimeEvent,
}

/// Observer notifications happen only after the event store has durably
/// accepted the event. UIs therefore never see a receipt that recovery cannot
/// replay.
pub trait EventObserver: Send + Sync {
    fn on_event(&self, event: &EventEnvelope);
}
