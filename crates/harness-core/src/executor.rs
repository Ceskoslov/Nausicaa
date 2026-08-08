use std::sync::Arc;

use crate::model::BoxFuture;
use crate::protocol::PreparedToolCall;
use crate::tool::{Tool, ToolError, ToolExecutionContext, ToolOutput};

/// Execution-plane seam. A container, VM, SSH, or cloud-sandbox backend can
/// implement this trait without changing the agent loop.
pub trait ToolExecutor: Send + Sync {
    fn execute<'a>(
        &'a self,
        tool: Arc<dyn Tool>,
        prepared: PreparedToolCall,
        context: ToolExecutionContext,
    ) -> BoxFuture<'a, Result<ToolOutput, ToolError>>;
}

/// Fail-closed default used until the embedding application chooses a real
/// execution boundary.
#[derive(Clone, Copy, Debug, Default)]
pub struct RejectingExecutor;

impl ToolExecutor for RejectingExecutor {
    fn execute<'a>(
        &'a self,
        _tool: Arc<dyn Tool>,
        _prepared: PreparedToolCall,
        _context: ToolExecutionContext,
    ) -> BoxFuture<'a, Result<ToolOutput, ToolError>> {
        Box::pin(async {
            Err(ToolError::ExecutorUnavailable(
                "no tool executor is configured".to_owned(),
            ))
        })
    }
}

/// Direct in-process execution. This is useful for tests and trusted embedded
/// tools; it is not an OS sandbox.
#[derive(Clone, Copy, Debug, Default)]
pub struct DirectExecutor;

impl ToolExecutor for DirectExecutor {
    fn execute<'a>(
        &'a self,
        tool: Arc<dyn Tool>,
        prepared: PreparedToolCall,
        context: ToolExecutionContext,
    ) -> BoxFuture<'a, Result<ToolOutput, ToolError>> {
        Box::pin(async move { tool.execute(prepared, context).await })
    }
}
