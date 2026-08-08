use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context, Poll, Waker};

use agent_harness_core::{
    Access, AgentRuntime, BoxFuture, CanonicalAction, CapabilityPolicy, DirectExecutor, EffectKind,
    LayeredContextCompiler, MemoryEventStore, ModelAdapter, ModelError, ModelRequest,
    ModelResponse, PreparedToolCall, RetrySafety, Tool, ToolCall, ToolDefinition, ToolError,
    ToolExecutionContext, ToolOutput, ToolRegistry,
};
use serde_json::{Value, json};

struct DemoModel {
    calls: AtomicUsize,
}

impl ModelAdapter for DemoModel {
    fn complete<'a>(
        &'a self,
        request: ModelRequest,
    ) -> BoxFuture<'a, Result<ModelResponse, ModelError>> {
        Box::pin(async move {
            if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
                assert_eq!(request.tools.len(), 1);
                Ok(ModelResponse::tool_calls(vec![ToolCall::new(
                    "echo",
                    json!({ "message": "  hello from a durable receipt  " }),
                )]))
            } else {
                let receipt = request
                    .context
                    .messages
                    .last()
                    .expect("the tool receipt must be in the next request");
                Ok(ModelResponse::text(format!("finished after {receipt:?}")))
            }
        })
    }
}

struct Echo;

impl Tool for Echo {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition::new(
            "echo",
            "Echo a string",
            json!({
                "type": "object",
                "properties": { "message": { "type": "string" } },
                "required": ["message"]
            }),
        )
    }

    fn prepare(
        &self,
        call: &ToolCall,
        _context: &ToolExecutionContext,
    ) -> Result<PreparedToolCall, ToolError> {
        let message = call
            .arguments
            .get("message")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::InvalidArguments("message must be a string".to_owned()))?;
        Ok(PreparedToolCall {
            call_id: call.id.clone(),
            action: CanonicalAction::new(
                "echo",
                json!({ "message": message.trim() }),
                EffectKind::ReadOnly,
                RetrySafety::Safe,
            ),
        })
    }

    fn execute<'a>(
        &'a self,
        prepared: PreparedToolCall,
        _context: ToolExecutionContext,
    ) -> BoxFuture<'a, Result<ToolOutput, ToolError>> {
        Box::pin(async move { Ok(ToolOutput::new(prepared.action.arguments)) })
    }
}

fn block_on<F: Future>(future: F) -> F::Output {
    let mut future = std::pin::pin!(future);
    let mut context = Context::from_waker(Waker::noop());
    loop {
        match future.as_mut().poll(&mut context) {
            Poll::Ready(output) => return output,
            Poll::Pending => std::thread::yield_now(),
        }
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut tools = ToolRegistry::new();
    tools.register(Echo)?;

    let runtime = AgentRuntime::new(
        Arc::new(DemoModel {
            calls: AtomicUsize::new(0),
        }),
        Arc::new(MemoryEventStore::new()),
        Arc::new(LayeredContextCompiler::new()),
        tools,
        Arc::new(CapabilityPolicy::deny_by_default().grant("echo", Access::Allow)),
    )
    .with_executor(Arc::new(DirectExecutor));
    let thread = runtime.start_thread()?;
    let outcome = block_on(runtime.run_turn(&thread, "run the demo"))?;
    println!("{}", outcome.content);
    Ok(())
}
