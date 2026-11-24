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
    responses: Mutex<VecDeque<Result<ModelResponse, ModelError>>>,
    requests: Mutex<Vec<ModelRequest>>,
}

impl ScriptedModel {
    fn new(responses: impl IntoIterator<Item = ModelResponse>) -> Self {
        Self::with_results(responses.into_iter().map(Ok))
    }

    fn with_results(
        responses: impl IntoIterator<Item = Result<ModelResponse, ModelError>>,
    ) -> Self {
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
                .unwrap_or_else(|| Err(ModelError::new("script exhausted", false)))
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
    model: Arc<dyn ModelAdapter>,
    store: Arc<dyn EventStore>,
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

#[test]
fn abnormal_model_stops_never_complete_or_execute_tools() {
    for stop_reason in [
        StopReason::MaxTokens,
        StopReason::Other,
        StopReason::ToolUse,
        StopReason::EndTurn,
    ] {
        for with_tools in [false, true] {
            if (stop_reason == StopReason::ToolUse && with_tools)
                || (stop_reason == StopReason::EndTurn && !with_tools)
            {
                continue;
            }
            let calls = if with_tools {
                vec![ToolCall::new("echo", json!({ "message": "partial" }))]
            } else {
                vec![]
            };
            let model = Arc::new(ScriptedModel::new([ModelResponse {
                content: "unfinished".into(),
                tool_calls: calls.clone(),
                stop_reason,
                usage: TokenUsage::default(),
            }]));
            let store = Arc::new(MemoryEventStore::new());
            let executions = Arc::new(AtomicUsize::new(0));
            let runtime = runtime(
                model,
                store.clone(),
                CapabilityPolicy::deny_by_default().grant("echo", Access::Allow),
                executions.clone(),
            );
            let thread = runtime.start_thread().unwrap();
            assert!(block_on(runtime.run_turn(&thread, "finish")).is_err());
            assert_eq!(executions.load(Ordering::SeqCst), 0);
            let events = store.load_thread(&thread).unwrap();
            assert!(
                !events
                    .iter()
                    .any(|e| matches!(e.event, RuntimeEvent::TurnCompleted { .. }))
            );
            assert!(matches!(
                events.last().unwrap().event,
                RuntimeEvent::TurnFailed { .. }
            ));
            assert_eq!(
                events
                    .iter()
                    .filter(|e| matches!(e.event, RuntimeEvent::ToolReceiptRecorded { .. }))
                    .count(),
                calls.len()
            );
            assert!(
                runtime
                    .recover(&thread)
                    .unwrap()
                    .receipts_recorded
                    .is_empty()
            );
        }
    }
}

struct PendingModel;

#[test]
fn cancellation_during_model_call_prevents_successful_completion() {
    struct CancellingModel(agent_harness_core::CancellationToken);
    impl ModelAdapter for CancellingModel {
        fn complete<'a>(
            &'a self,
            _: ModelRequest,
        ) -> BoxFuture<'a, Result<ModelResponse, ModelError>> {
            Box::pin(async {
                self.0.cancel();
                Ok(ModelResponse::text("late response"))
            })
        }
    }
    let cancellation = agent_harness_core::CancellationToken::new();
    let runtime = AgentRuntime::new(
        Arc::new(CancellingModel(cancellation.clone())),
        Arc::new(MemoryEventStore::new()),
        Arc::new(LayeredContextCompiler::new()),
        ToolRegistry::new(),
        Arc::new(CapabilityPolicy::deny_by_default()),
    );
    let thread = runtime.start_thread().unwrap();
    assert!(matches!(
        block_on(runtime.run_turn_with_cancellation(&thread, "work", cancellation)),
        Err(RuntimeError::Cancelled(_))
    ));
    let events = runtime.events(&thread).unwrap();
    assert!(matches!(
        events.last().unwrap().event,
        RuntimeEvent::TurnCancelled
    ));
    assert!(
        !events
            .iter()
            .any(|e| matches!(e.event, RuntimeEvent::TurnCompleted { .. }))
    );
}

impl ModelAdapter for PendingModel {
    fn complete<'a>(&'a self, _: ModelRequest) -> BoxFuture<'a, Result<ModelResponse, ModelError>> {
        Box::pin(std::future::pending())
    }
}

#[test]
fn recovery_rejects_active_turn_and_recovers_after_future_is_dropped() {
    let store = Arc::new(MemoryEventStore::new());
    let runtime = AgentRuntime::new(
        Arc::new(PendingModel),
        store.clone(),
        Arc::new(LayeredContextCompiler::new()),
        ToolRegistry::new(),
        Arc::new(CapabilityPolicy::deny_by_default()),
    );
    let another_runtime = AgentRuntime::new(
        Arc::new(PendingModel),
        store,
        Arc::new(LayeredContextCompiler::new()),
        ToolRegistry::new(),
        Arc::new(CapabilityPolicy::deny_by_default()),
    );
    let thread = runtime.start_thread().unwrap();
    {
        let mut future = std::pin::pin!(runtime.run_turn(&thread, "work"));
        let mut context = Context::from_waker(Waker::noop());
        assert!(future.as_mut().poll(&mut context).is_pending());
        let before = runtime.events(&thread).unwrap();
        assert!(matches!(
            runtime.recover(&thread),
            Err(RuntimeError::ConcurrentTurn(_))
        ));
        assert!(matches!(
            another_runtime.recover(&thread),
            Err(RuntimeError::ConcurrentTurn(_))
        ));
        assert!(matches!(
            block_on(another_runtime.run_turn(&thread, "competing turn")),
            Err(RuntimeError::ConcurrentTurn(_))
        ));
        assert_eq!(runtime.events(&thread).unwrap(), before);
    }
    assert_eq!(
        runtime.recover(&thread).unwrap().turns_marked_failed.len(),
        1
    );
    assert!(
        runtime
            .recover(&thread)
            .unwrap()
            .turns_marked_failed
            .is_empty()
    );
}

struct RetryTestTimer(Option<agent_harness_core::CancellationToken>);

impl agent_harness_core::RetryTimer for RetryTestTimer {
    fn sleep(&self, _: std::time::Duration) -> BoxFuture<'_, ()> {
        Box::pin(async {
            if let Some(token) = &self.0 {
                token.cancel();
            }
        })
    }
}

#[test]
fn retry_reuses_the_request_and_preserves_the_previous_receipt() {
    use agent_harness_core::ModelRetryPolicy;
    use std::time::Duration;
    let call = ToolCall::new("echo", json!({"message": "hello"}));
    let model = Arc::new(ScriptedModel::with_results([
        Ok(ModelResponse::tool_calls(vec![call])),
        Err(ModelError::new("transient", true)),
        Ok(ModelResponse::text("done")),
    ]));
    let store = Arc::new(MemoryEventStore::new());
    let executions = Arc::new(AtomicUsize::new(0));
    let runtime = runtime(
        model.clone(),
        store.clone(),
        CapabilityPolicy::deny_by_default().grant("echo", Access::Allow),
        executions.clone(),
    )
    .with_model_retry(
        ModelRetryPolicy::new(1, Duration::from_millis(5), Duration::from_millis(10)).unwrap(),
        Arc::new(RetryTestTimer(None)),
    );
    let thread = runtime.start_thread().unwrap();
    let outcome = block_on(runtime.run_turn(&thread, "run once")).unwrap();
    assert_eq!(outcome.model_iterations, 2);
    assert_eq!(executions.load(Ordering::SeqCst), 1);
    let requests = model.requests();
    assert_eq!(requests.len(), 3);
    assert_eq!(requests[1], requests[2]);
    let events = store.load_thread(&thread).unwrap();
    let receipt = events
        .iter()
        .find(|event| matches!(event.event, RuntimeEvent::ToolReceiptRecorded { .. }))
        .unwrap();
    let retry = events
        .iter()
        .find(|event| {
            matches!(
                event.event,
                RuntimeEvent::ModelRequestRetried {
                    iteration: 1,
                    retry: 1
                }
            )
        })
        .unwrap();
    assert!(receipt.sequence < retry.sequence);
}

#[test]
fn retry_exhaustion_and_backoff_cancellation_end_the_turn() {
    use agent_harness_core::{CancellationToken, ModelRetryPolicy};
    use std::time::Duration;
    for cancel in [false, true] {
        let cancellation = CancellationToken::new();
        let model = Arc::new(ScriptedModel::with_results([
            Err(ModelError::new("transient", true)),
            Err(ModelError::new("still transient", true)),
        ]));
        let store = Arc::new(MemoryEventStore::new());
        let runtime = runtime(
            model.clone(),
            store,
            CapabilityPolicy::deny_by_default(),
            Arc::new(AtomicUsize::new(0)),
        )
        .with_model_retry(
            ModelRetryPolicy::new(1, Duration::from_millis(20), Duration::from_millis(40)).unwrap(),
            Arc::new(RetryTestTimer(cancel.then(|| cancellation.clone()))),
        );
        let thread = runtime.start_thread().unwrap();
        let error =
            block_on(runtime.run_turn_with_cancellation(&thread, "test retries", cancellation))
                .unwrap_err();
        if cancel {
            assert!(matches!(error, RuntimeError::Cancelled(_)));
            assert_eq!(model.requests().len(), 1);
        } else {
            assert!(matches!(
                error,
                RuntimeError::ModelRetriesExhausted { retries: 1, .. }
            ));
            assert_eq!(model.requests().len(), 2);
        }
    }
}

struct ResumeJournal(std::path::PathBuf);

impl ResumeJournal {
    fn new() -> Self {
        let directory = std::env::temp_dir().join(format!("nausicaa-resume-{}", TurnId::new()));
        std::fs::create_dir(&directory).unwrap();
        Self(directory)
    }

    fn open(&self) -> agent_harness_core::JsonlEventStore {
        agent_harness_core::JsonlEventStore::open(self.0.join("events.jsonl")).unwrap()
    }
}

impl Drop for ResumeJournal {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn resume_reopens_model_request_without_duplicating_input() {
    use agent_harness_core::CancellationToken;

    let journal = ResumeJournal::new();
    let turn = TurnId::new();
    let thread = {
        let runtime = runtime(
            Arc::new(PendingModel),
            Arc::new(journal.open()),
            CapabilityPolicy::deny_by_default(),
            Arc::default(),
        );
        let thread = runtime.start_thread().unwrap();
        {
            let mut future = std::pin::pin!(runtime.run_resumable_turn(
                &thread,
                turn.clone(),
                "original",
                CancellationToken::new()
            ));
            assert!(
                future
                    .as_mut()
                    .poll(&mut Context::from_waker(Waker::noop()))
                    .is_pending()
            );
        }
        thread
    };

    let model = Arc::new(ScriptedModel::new([ModelResponse::text("done")]));
    let runtime = runtime(
        model.clone(),
        Arc::new(journal.open()),
        CapabilityPolicy::deny_by_default(),
        Arc::default(),
    );
    let outcome =
        block_on(runtime.resume_turn(&thread, turn.clone(), CancellationToken::new())).unwrap();
    assert_eq!(outcome.turn_id, turn);
    assert_eq!(outcome.model_iterations, 1);
    let requests = model.requests();
    assert_eq!(
        requests[0].context.messages,
        vec![TranscriptMessage::User {
            content: "original".into()
        }]
    );
    let events = runtime.events(&thread).unwrap();
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e.event, RuntimeEvent::ModelRequestStarted { .. }))
            .count(),
        1
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e.event, RuntimeEvent::TurnResumed { iteration: 0 }))
    );
    assert!(matches!(
        block_on(runtime.resume_turn(&thread, turn, CancellationToken::new())),
        Err(RuntimeError::TurnNotResumable { .. })
    ));
}

#[derive(Clone, Copy, Debug)]
enum CrashBoundary {
    Approval,
    Execution,
    BeforeReceipt,
    AfterReceipt,
}

struct PendingApproval;

impl ApprovalProvider for PendingApproval {
    fn request<'a>(
        &'a self,
        _: ApprovalRequest,
    ) -> BoxFuture<'a, Result<ApprovalDecision, ApprovalError>> {
        Box::pin(std::future::pending())
    }
}

struct PendingExecution;

impl agent_harness_core::ToolExecutor for PendingExecution {
    fn execute<'a>(
        &'a self,
        _: Arc<dyn Tool>,
        _: PreparedToolCall,
        _: ToolExecutionContext,
    ) -> BoxFuture<'a, Result<ToolOutput, ToolError>> {
        Box::pin(std::future::pending())
    }
}

struct FaultyResumeStore(agent_harness_core::JsonlEventStore, CrashBoundary);

impl EventStore for FaultyResumeStore {
    fn append(
        &self,
        thread_id: ThreadId,
        turn_id: Option<TurnId>,
        event: RuntimeEvent,
    ) -> Result<agent_harness_core::EventEnvelope, agent_harness_core::StoreError> {
        let fail = matches!(
            (self.1, &event),
            (
                CrashBoundary::BeforeReceipt,
                RuntimeEvent::ToolReceiptRecorded { .. }
            ) | (
                CrashBoundary::AfterReceipt,
                RuntimeEvent::ContextCompiled { iteration: 1, .. }
            )
        );
        if fail {
            return Err(agent_harness_core::StoreError::Io(
                "injected append failure".into(),
            ));
        }
        self.0.append(thread_id, turn_id, event)
    }

    fn load_thread(
        &self,
        thread_id: &ThreadId,
    ) -> Result<Vec<agent_harness_core::EventEnvelope>, agent_harness_core::StoreError> {
        self.0.load_thread(thread_id)
    }

    fn thread_guards(&self) -> Option<&agent_harness_core::store::ThreadGuards> {
        self.0.thread_guards()
    }
}

#[test]
fn restart_closes_incomplete_tool_boundaries_and_resumes_durable_receipts() {
    use agent_harness_core::CancellationToken;

    for boundary in [
        CrashBoundary::Approval,
        CrashBoundary::Execution,
        CrashBoundary::BeforeReceipt,
        CrashBoundary::AfterReceipt,
    ] {
        let journal = ResumeJournal::new();
        let turn = TurnId::new();
        let executions = Arc::new(AtomicUsize::new(0));
        let call = ToolCall::new("echo", json!({"message": "one effect"}));
        let thread = {
            let model = Arc::new(ScriptedModel::new([ModelResponse::tool_calls(vec![
                call.clone(),
            ])]));
            let store = Arc::new(FaultyResumeStore(journal.open(), boundary));
            let approval = matches!(boundary, CrashBoundary::Approval);
            let mut runtime = runtime(
                model,
                store,
                CapabilityPolicy::deny_by_default()
                    .grant("echo", if approval { Access::Ask } else { Access::Allow }),
                executions.clone(),
            );
            if approval {
                runtime = runtime.with_approval_provider(Arc::new(PendingApproval));
            } else if matches!(boundary, CrashBoundary::Execution) {
                runtime = runtime.with_executor(Arc::new(PendingExecution));
            }
            let thread = runtime.start_thread().unwrap();
            {
                let mut future = std::pin::pin!(runtime.run_resumable_turn(
                    &thread,
                    turn.clone(),
                    "echo once",
                    CancellationToken::new()
                ));
                let result = future
                    .as_mut()
                    .poll(&mut Context::from_waker(Waker::noop()));
                match boundary {
                    CrashBoundary::Approval | CrashBoundary::Execution => {
                        assert!(result.is_pending())
                    }
                    CrashBoundary::BeforeReceipt => assert!(matches!(
                        result,
                        Poll::Ready(Err(RuntimeError::ReceiptPersistenceUnknown { .. }))
                    )),
                    CrashBoundary::AfterReceipt => {
                        assert!(matches!(result, Poll::Ready(Err(RuntimeError::Store(_)))))
                    }
                }
            }
            thread
        };

        let model = Arc::new(ScriptedModel::new([ModelResponse::text("done")]));
        let runtime = runtime(
            model.clone(),
            Arc::new(journal.open()),
            CapabilityPolicy::deny_by_default(),
            executions.clone(),
        );
        let before = runtime.events(&thread).unwrap();
        let resumed =
            block_on(runtime.resume_turn(&thread, turn.clone(), CancellationToken::new()));
        if matches!(boundary, CrashBoundary::AfterReceipt) {
            assert_eq!(resumed.unwrap().model_iterations, 2);
            assert_eq!(model.requests()[0].iteration, 1);
            assert!(model.requests()[0].tools.is_empty());
            assert!(
                matches!(model.requests()[0].context.messages.last(), Some(TranscriptMessage::Tool { receipt }) if receipt.call_id == call.id && receipt.status == ReceiptStatus::Succeeded)
            );
            assert_eq!(executions.load(Ordering::SeqCst), 1);
        } else {
            assert!(
                matches!(resumed, Err(RuntimeError::TurnNotResumable { .. })),
                "{boundary:?}"
            );
            assert!(model.requests().is_empty());
            assert_eq!(runtime.events(&thread).unwrap(), before);
            let recovery = runtime.recover(&thread).unwrap();
            assert_eq!(recovery.turns_marked_failed, vec![turn]);
            assert_eq!(recovery.receipts_recorded.len(), 1);
            let status = if matches!(boundary, CrashBoundary::Approval) {
                ReceiptStatus::Failed
            } else {
                ReceiptStatus::Unknown
            };
            assert_eq!(recovery.receipts_recorded[0].status, status);
            assert_eq!(
                executions.load(Ordering::SeqCst),
                usize::from(matches!(boundary, CrashBoundary::BeforeReceipt))
            );
            assert!(
                runtime
                    .recover(&thread)
                    .unwrap()
                    .receipts_recorded
                    .is_empty()
            );
        }
    }
}

#[test]
fn resume_rejects_duplicate_call_id_after_durable_tool_batch() {
    use agent_harness_core::CancellationToken;
    let journal = ResumeJournal::new();
    let turn = TurnId::new();
    let call = ToolCall::new("echo", json!({"message": "one effect"}));
    let executions = Arc::new(AtomicUsize::new(0));
    let policy = CapabilityPolicy::deny_by_default().grant("echo", Access::Allow);
    let thread = {
        let first = runtime(
            Arc::new(ScriptedModel::new([ModelResponse::tool_calls(vec![
                call.clone(),
            ])])),
            Arc::new(FaultyResumeStore(
                journal.open(),
                CrashBoundary::AfterReceipt,
            )),
            policy.clone(),
            executions.clone(),
        );
        let thread = first.start_thread().unwrap();
        assert!(
            block_on(first.run_resumable_turn(
                &thread,
                turn.clone(),
                "echo once",
                CancellationToken::new()
            ))
            .is_err()
        );
        thread
    };
    let second = runtime(
        Arc::new(ScriptedModel::new([ModelResponse::tool_calls(vec![call])])),
        Arc::new(journal.open()),
        policy,
        executions.clone(),
    );
    assert!(matches!(
        block_on(second.resume_turn(&thread, turn, CancellationToken::new())),
        Err(RuntimeError::InvalidModelResponse { .. })
    ));
    assert_eq!(executions.load(Ordering::SeqCst), 1);
}

#[test]
fn generation_deadline_during_backoff_preserves_verification_time() {
    use agent_harness_core::{ModelRetryPolicy, RetryTimer, TaskBudget, TaskBudgetError};
    use std::time::{Duration, Instant};

    struct TestClock(Arc<Mutex<Instant>>);
    impl RetryTimer for TestClock {
        fn sleep(&self, duration: Duration) -> BoxFuture<'_, ()> {
            Box::pin(async move { *self.0.lock().unwrap() += duration })
        }
    }

    let now = Arc::new(Mutex::new(Instant::now()));
    let budget =
        TaskBudget::with_clock(Duration::from_millis(30), Duration::from_millis(20), 100, {
            let now = now.clone();
            move || *now.lock().unwrap()
        })
        .unwrap();
    let model = Arc::new(ScriptedModel::with_results([Err(ModelError::new(
        "transient",
        true,
    ))]));
    let runtime = runtime(
        model.clone(),
        Arc::new(MemoryEventStore::new()),
        CapabilityPolicy::deny_by_default(),
        Arc::default(),
    )
    .with_task_budget(budget.clone())
    .with_model_retry(
        ModelRetryPolicy::new(3, Duration::from_millis(30), Duration::from_millis(30)).unwrap(),
        Arc::new(TestClock(now)),
    );
    let thread = runtime.start_thread().unwrap();
    assert!(matches!(
        block_on(runtime.run_turn(&thread, "bounded work")),
        Err(RuntimeError::TaskBudget(TaskBudgetError::GenerationTime))
    ));
    assert_eq!(model.requests().len(), 1);
    assert_eq!(budget.verification_remaining(), Duration::from_millis(20));
}
