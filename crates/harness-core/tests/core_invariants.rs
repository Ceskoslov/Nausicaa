use std::collections::VecDeque;
use std::future::Future;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};

use agent_harness_core::{
    Access, AgentRuntime, ApprovalDecision, ApprovalError, ApprovalProvider, ApprovalRequest,
    BoxFuture, CanonicalAction, CapabilityPolicy, ContextCompiler, DirectExecutor, EffectKind,
    EventStore, LayeredContextCompiler, MemoryEventStore, ModelAdapter, ModelError, ModelRequest,
    ModelResponse, PolicyContext, PreparedToolCall, ReceiptStatus, RetrySafety, RuntimeError,
    RuntimeEvent, StopReason, ThreadId, TokenUsage, Tool, ToolCall, ToolDefinition, ToolError,
    ToolExecutionContext, ToolOutput, ToolReceipt, ToolRegistry, TranscriptMessage, TurnId,
    project_capabilities, recover_thread,
};
use serde_json::{Value, json};

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

struct ScriptedModel {
    responses: Mutex<VecDeque<ModelResponse>>,
    requests: Mutex<Vec<ModelRequest>>,
}

impl ScriptedModel {
    fn new(responses: impl IntoIterator<Item = ModelResponse>) -> Self {
        Self {
            responses: Mutex::new(responses.into_iter().collect()),
            requests: Mutex::new(Vec::new()),
        }
    }

    fn requests(&self) -> Vec<ModelRequest> {
        self.requests.lock().unwrap().clone()
    }
}

impl ModelAdapter for ScriptedModel {
    fn complete<'a>(
        &'a self,
        request: ModelRequest,
    ) -> BoxFuture<'a, Result<ModelResponse, ModelError>> {
        Box::pin(async move {
            self.requests.lock().unwrap().push(request);
            self.responses
                .lock()
                .unwrap()
                .pop_front()
                .ok_or_else(|| ModelError::new("script exhausted", false))
        })
    }
}

#[derive(Clone)]
struct EchoTool {
    executions: Arc<AtomicUsize>,
}

impl EchoTool {
    fn new(executions: Arc<AtomicUsize>) -> Self {
        Self { executions }
    }
}

impl Tool for EchoTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition::new(
            "echo",
            "Returns a normalized message",
            json!({
                "type": "object",
                "properties": { "message": { "type": "string" } },
                "required": ["message"],
                "additionalProperties": false
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
            .ok_or_else(|| ToolError::InvalidArguments("`message` must be a string".to_owned()))?;
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
        Box::pin(async move {
            self.executions.fetch_add(1, Ordering::SeqCst);
            Ok(ToolOutput::new(prepared.action.arguments))
        })
    }
}

struct ExactApprovals;

impl ApprovalProvider for ExactApprovals {
    fn request<'a>(
        &'a self,
        request: ApprovalRequest,
    ) -> BoxFuture<'a, Result<ApprovalDecision, ApprovalError>> {
        Box::pin(async move {
            Ok(ApprovalDecision::Approved {
                action: request.action,
            })
        })
    }
}

struct MismatchedApprovals;

impl ApprovalProvider for MismatchedApprovals {
    fn request<'a>(
        &'a self,
        request: ApprovalRequest,
    ) -> BoxFuture<'a, Result<ApprovalDecision, ApprovalError>> {
        Box::pin(async move {
            let mut different = request.action;
            different.scope = Some("a-different-scope".to_owned());
            Ok(ApprovalDecision::Approved { action: different })
        })
    }
}

fn runtime(
    model: Arc<ScriptedModel>,
    store: Arc<MemoryEventStore>,
    policy: CapabilityPolicy,
    executions: Arc<AtomicUsize>,
) -> AgentRuntime {
    let mut tools = ToolRegistry::new();
    tools.register(EchoTool::new(executions)).unwrap();
    AgentRuntime::new(
        model,
        store,
        Arc::new(LayeredContextCompiler::new()) as Arc<dyn ContextCompiler>,
        tools,
        Arc::new(policy),
    )
    .with_executor(Arc::new(DirectExecutor))
}

#[test]
fn receipt_is_durable_before_the_next_model_request() {
    let call = ToolCall::new("echo", json!({ "message": "  hello  " }));
    let model = Arc::new(ScriptedModel::new([
        ModelResponse::tool_calls(vec![call]),
        ModelResponse::text("done"),
    ]));
    let store = Arc::new(MemoryEventStore::new());
    let executions = Arc::new(AtomicUsize::new(0));
    let runtime = runtime(
        model.clone(),
        store.clone(),
        CapabilityPolicy::deny_by_default().grant("echo", Access::Allow),
        executions.clone(),
    );
    let thread = runtime.start_thread().unwrap();

    let outcome = block_on(runtime.run_turn(&thread, "echo it")).unwrap();

    assert_eq!(outcome.content, "done");
    assert_eq!(executions.load(Ordering::SeqCst), 1);
    let requests = model.requests();
    assert_eq!(requests.len(), 2);
    assert!(matches!(
        requests[1].context.messages.last(),
        Some(TranscriptMessage::Tool {
            receipt: ToolReceipt {
                status: ReceiptStatus::Succeeded,
                ..
            }
        })
    ));

    let events = store.load_thread(&thread).unwrap();
    let started = events
        .iter()
        .find(|event| matches!(event.event, RuntimeEvent::ToolExecutionStarted { .. }))
        .unwrap()
        .sequence;
    let receipt = events
        .iter()
        .find(|event| matches!(event.event, RuntimeEvent::ToolReceiptRecorded { .. }))
        .unwrap()
        .sequence;
    let second_model_request = events
        .iter()
        .find(|event| {
            matches!(
                event.event,
                RuntimeEvent::ModelRequestStarted { iteration: 1 }
            )
        })
        .unwrap()
        .sequence;
    assert!(started < receipt && receipt < second_model_request);
}

#[test]
fn denied_tools_are_not_projected_to_the_model() {
    let model = Arc::new(ScriptedModel::new([ModelResponse::text("done")]));
    let store = Arc::new(MemoryEventStore::new());
    let executions = Arc::new(AtomicUsize::new(0));
    let runtime = runtime(
        model.clone(),
        store,
        CapabilityPolicy::deny_by_default(),
        executions,
    );
    let thread = runtime.start_thread().unwrap();

    block_on(runtime.run_turn(&thread, "hello")).unwrap();

    assert!(model.requests()[0].tools.is_empty());
}

#[test]
fn child_capabilities_cannot_expand_the_parent() {
    let names = ["echo", "write"];
    let parent_policy = CapabilityPolicy::deny_by_default()
        .grant("echo", Access::Ask)
        .grant("write", Access::Deny);
    let child_policy = CapabilityPolicy::deny_by_default()
        .grant("echo", Access::Allow)
        .grant("write", Access::Allow);
    let context = PolicyContext::default();
    let parent = project_capabilities(names, &parent_policy, &context, None);
    let child = project_capabilities(names, &child_policy, &context, Some(&parent));

    assert_eq!(child.access("echo"), Access::Ask);
    assert_eq!(child.access("write"), Access::Deny);
}

#[test]
fn duplicate_call_ids_fail_before_any_tool_execution() {
    let call = ToolCall::new("echo", json!({ "message": "hello" }));
    let model = Arc::new(ScriptedModel::new([ModelResponse::tool_calls(vec![
        call.clone(),
        call,
    ])]));
    let store = Arc::new(MemoryEventStore::new());
    let executions = Arc::new(AtomicUsize::new(0));
    let runtime = runtime(
        model,
        store.clone(),
        CapabilityPolicy::deny_by_default().grant("echo", Access::Allow),
        executions.clone(),
    );
    let thread = runtime.start_thread().unwrap();

    let error = block_on(runtime.run_turn(&thread, "echo")).unwrap_err();

    assert!(matches!(error, RuntimeError::InvalidModelResponse { .. }));
    assert_eq!(executions.load(Ordering::SeqCst), 0);
    assert!(
        !store
            .load_thread(&thread)
            .unwrap()
            .iter()
            .any(|event| matches!(event.event, RuntimeEvent::AssistantMessage { .. }))
    );
}

#[test]
fn exact_approval_allows_execution() {
    let call = ToolCall::new("echo", json!({ "message": "hello" }));
    let model = Arc::new(ScriptedModel::new([
        ModelResponse::tool_calls(vec![call]),
        ModelResponse::text("done"),
    ]));
    let store = Arc::new(MemoryEventStore::new());
    let executions = Arc::new(AtomicUsize::new(0));
    let runtime = runtime(
        model,
        store,
        CapabilityPolicy::deny_by_default().grant("echo", Access::Ask),
        executions.clone(),
    )
    .with_approval_provider(Arc::new(ExactApprovals));
    let thread = runtime.start_thread().unwrap();

    block_on(runtime.run_turn(&thread, "echo")).unwrap();

    assert_eq!(executions.load(Ordering::SeqCst), 1);
}

#[test]
fn mismatched_approval_never_crosses_the_executor_boundary() {
    let call = ToolCall::new("echo", json!({ "message": "hello" }));
    let model = Arc::new(ScriptedModel::new([
        ModelResponse::tool_calls(vec![call]),
        ModelResponse::text("handled denial"),
    ]));
    let store = Arc::new(MemoryEventStore::new());
    let executions = Arc::new(AtomicUsize::new(0));
    let runtime = runtime(
        model.clone(),
        store.clone(),
        CapabilityPolicy::deny_by_default().grant("echo", Access::Ask),
        executions.clone(),
    )
    .with_approval_provider(Arc::new(MismatchedApprovals));
    let thread = runtime.start_thread().unwrap();

    block_on(runtime.run_turn(&thread, "echo")).unwrap();

    assert_eq!(executions.load(Ordering::SeqCst), 0);
    assert!(
        !store
            .load_thread(&thread)
            .unwrap()
            .iter()
            .any(|event| matches!(event.event, RuntimeEvent::ToolExecutionStarted { .. }))
    );
    assert!(matches!(
        model.requests()[1].context.messages.last(),
        Some(TranscriptMessage::Tool {
            receipt: ToolReceipt {
                status: ReceiptStatus::Denied,
                ..
            }
        })
    ));
}

#[test]
fn interrupted_execution_becomes_unknown_and_is_not_replayed() {
    let store = MemoryEventStore::new();
    let thread = ThreadId::new();
    let turn = TurnId::new();
    let call = ToolCall::new("send_email", json!({ "to": "person@example.test" }));
    let prepared = PreparedToolCall {
        call_id: call.id.clone(),
        action: CanonicalAction::new(
            "send_email",
            call.arguments.clone(),
            EffectKind::ExternalSideEffect,
            RetrySafety::Unsafe,
        ),
    };
    store
        .append(thread.clone(), None, RuntimeEvent::ThreadStarted)
        .unwrap();
    store
        .append(
            thread.clone(),
            Some(turn.clone()),
            RuntimeEvent::TurnStarted,
        )
        .unwrap();
    store
        .append(
            thread.clone(),
            Some(turn.clone()),
            RuntimeEvent::AssistantMessage {
                content: String::new(),
                tool_calls: vec![call],
                stop_reason: StopReason::ToolUse,
                usage: TokenUsage::default(),
            },
        )
        .unwrap();
    store
        .append(
            thread.clone(),
            Some(turn.clone()),
            RuntimeEvent::ToolPrepared {
                prepared: prepared.clone(),
            },
        )
        .unwrap();
    store
        .append(
            thread.clone(),
            Some(turn.clone()),
            RuntimeEvent::ToolExecutionStarted { prepared },
        )
        .unwrap();

    let report = recover_thread(&store, &thread).unwrap();

    assert_eq!(report.receipts_recorded.len(), 1);
    assert_eq!(report.receipts_recorded[0].status, ReceiptStatus::Unknown);
    assert_eq!(report.turns_marked_failed, vec![turn]);
    assert!(
        recover_thread(&store, &thread)
            .unwrap()
            .receipts_recorded
            .is_empty()
    );
}
