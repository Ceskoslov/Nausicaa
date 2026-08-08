//! Event-driven core primitives for a secure agent harness.
//!
//! The crate intentionally separates model intent, policy, approval, execution,
//! and durable receipts. It does not treat prompts or memory as security
//! boundaries.

pub mod approval;
pub mod context;
pub mod control;
pub mod event;
pub mod executor;
pub mod hook;
pub mod id;
pub mod model;
pub mod policy;
pub mod protocol;
pub mod recovery;
pub mod runtime;
pub mod store;
pub mod tool;

pub use approval::{
    ApprovalDecision, ApprovalError, ApprovalProvider, ApprovalRequest, DenyAllApprovals,
};
pub use context::{
    CompiledContext, ContextCompiler, ContextError, ContextInput, DirectoryRuleLoader,
    LayeredContextCompiler, PromptLayer, PromptSegment,
};
pub use control::CancellationToken;
pub use event::{EventEnvelope, EventObserver, HookPoint, RuntimeEvent};
pub use executor::{DirectExecutor, RejectingExecutor, ToolExecutor};
pub use hook::{Hook, HookContext, HookError, HookSet};
pub use id::{CallId, EventId, ThreadId, TurnId};
pub use model::{
    BoxFuture, ModelAdapter, ModelError, ModelRequest, ModelResponse, StopReason, TokenUsage,
};
pub use policy::{
    Access, CapabilityPolicy, CapabilityProjection, PolicyContext, ToolPolicy, project_capabilities,
};
pub use protocol::{
    CanonicalAction, EffectKind, PreparedToolCall, ReceiptStatus, RetrySafety, ToolCall,
    ToolReceipt, TranscriptMessage,
};
pub use recovery::{RecoveryReport, recover_thread};
pub use runtime::{AgentRuntime, RuntimeConfig, RuntimeError, TurnOutcome};
pub use store::{EventStore, JsonlEventStore, MemoryEventStore, StoreError};
pub use tool::{
    RegistryError, Tool, ToolDefinition, ToolError, ToolExecutionContext, ToolOutput, ToolRegistry,
};
