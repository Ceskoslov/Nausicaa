use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use thiserror::Error;

use crate::approval::{ApprovalDecision, ApprovalProvider, ApprovalRequest, DenyAllApprovals};
use crate::context::{ContextCompiler, ContextError, ContextInput};
use crate::control::CancellationToken;
use crate::event::{EventEnvelope, EventObserver, HookPoint, RuntimeEvent};
use crate::executor::{RejectingExecutor, ToolExecutor};
use crate::hook::{HookContext, HookError, HookSet};
use crate::id::{CallId, ThreadId, TurnId};
use crate::model::{ModelAdapter, ModelError, ModelRequest};
use crate::policy::{CapabilityProjection, PolicyContext, ToolPolicy, project_capabilities};
use crate::protocol::{ToolCall, ToolReceipt, TranscriptMessage};
use crate::recovery::{RecoveryReport, recover_thread};
use crate::store::{EventStore, StoreError};
use crate::tool::{ToolExecutionContext, ToolRegistry};

#[derive(Clone, Debug)]
pub struct RuntimeConfig {
    pub max_model_iterations: usize,
    pub workspace: Option<PathBuf>,
    pub tool_metadata: BTreeMap<String, String>,
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            max_model_iterations: 32,
            workspace: None,
            tool_metadata: BTreeMap::new(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TurnOutcome {
    pub thread_id: ThreadId,
    pub turn_id: TurnId,
    pub content: String,
    pub model_iterations: usize,
}

#[derive(Debug, Error)]
pub enum RuntimeError {
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Context(#[from] ContextError),
    #[error(transparent)]
    Model(#[from] ModelError),
    #[error(transparent)]
    Hook(#[from] HookError),
    #[error("thread `{0}` does not exist")]
    UnknownThread(ThreadId),
    #[error("thread `{0}` already has an active turn")]
    ConcurrentTurn(ThreadId),
    #[error("turn `{turn_id}` already exists in thread `{thread_id}`")]
    DuplicateTurn {
        thread_id: ThreadId,
        turn_id: TurnId,
    },
    #[error("model returned an invalid response for turn `{turn_id}`: {reason}")]
    InvalidModelResponse { turn_id: TurnId, reason: String },
    #[error("turn `{0}` was cancelled")]
    Cancelled(TurnId),
    #[error("turn `{turn_id}` exceeded {maximum} model iterations")]
    MaxModelIterations { turn_id: TurnId, maximum: usize },
    #[error(
        "tool `{tool_name}` may have changed external state, but receipt `{call_id}` could not be persisted: {source}"
    )]
    ReceiptPersistenceUnknown {
        call_id: CallId,
        tool_name: String,
        source: StoreError,
    },
}

/// One event-driven cognitive core. The control plane owns thread/turn calls;
/// the execution plane is replaceable through `ToolExecutor`.
pub struct AgentRuntime {
    model: Arc<dyn ModelAdapter>,
    store: Arc<dyn EventStore>,
    compiler: Arc<dyn ContextCompiler>,
    tools: ToolRegistry,
    policy: Arc<dyn ToolPolicy>,
    policy_context: PolicyContext,
    parent_capabilities: Option<CapabilityProjection>,
    approvals: Arc<dyn ApprovalProvider>,
    executor: Arc<dyn ToolExecutor>,
    hooks: HookSet,
    observers: Vec<Arc<dyn EventObserver>>,
    config: RuntimeConfig,
    active_threads: Mutex<BTreeSet<ThreadId>>,
}

impl AgentRuntime {
    #[must_use]
    pub fn new(
        model: Arc<dyn ModelAdapter>,
        store: Arc<dyn EventStore>,
        compiler: Arc<dyn ContextCompiler>,
        tools: ToolRegistry,
        policy: Arc<dyn ToolPolicy>,
    ) -> Self {
        Self {
            model,
            store,
            compiler,
            tools,
            policy,
            policy_context: PolicyContext::default(),
            parent_capabilities: None,
            approvals: Arc::new(DenyAllApprovals),
            executor: Arc::new(RejectingExecutor),
            hooks: HookSet::default(),
            observers: Vec::new(),
            config: RuntimeConfig::default(),
            active_threads: Mutex::new(BTreeSet::new()),
        }
    }

    #[must_use]
    pub fn with_policy_context(mut self, context: PolicyContext) -> Self {
        self.policy_context = context;
        self
    }

    #[must_use]
    pub fn with_parent_capabilities(mut self, parent: CapabilityProjection) -> Self {
        self.parent_capabilities = Some(parent);
        self
    }

    #[must_use]
    pub fn with_approval_provider(mut self, provider: Arc<dyn ApprovalProvider>) -> Self {
        self.approvals = provider;
        self
    }

    #[must_use]
    pub fn with_executor(mut self, executor: Arc<dyn ToolExecutor>) -> Self {
        self.executor = executor;
        self
    }

    #[must_use]
    pub fn with_hooks(mut self, hooks: HookSet) -> Self {
        self.hooks = hooks;
        self
    }

    #[must_use]
    pub fn with_observer(mut self, observer: Arc<dyn EventObserver>) -> Self {
        self.observers.push(observer);
        self
    }

    #[must_use]
    pub fn with_config(mut self, config: RuntimeConfig) -> Self {
        self.config = config;
        self
    }

    #[must_use]
    pub fn capabilities(&self) -> CapabilityProjection {
        project_capabilities(
            self.tools.names(),
            self.policy.as_ref(),
            &self.policy_context,
            self.parent_capabilities.as_ref(),
        )
    }

    pub fn start_thread(&self) -> Result<ThreadId, RuntimeError> {
        let thread_id = ThreadId::new();
        self.append(thread_id.clone(), None, RuntimeEvent::ThreadStarted)?;
        Ok(thread_id)
    }

    pub fn events(&self, thread_id: &ThreadId) -> Result<Vec<EventEnvelope>, RuntimeError> {
        self.ensure_thread(thread_id)?;
        Ok(self.store.load_thread(thread_id)?)
    }

    /// Recovers an exclusively-owned thread and publishes recovered events to
    /// observers. No external action is automatically replayed.
    pub fn recover(&self, thread_id: &ThreadId) -> Result<RecoveryReport, RuntimeError> {
        self.ensure_thread(thread_id)?;
        let before = self
            .store
            .load_thread(thread_id)?
            .last()
            .map_or(0, |event| event.sequence.saturating_add(1));
        let report = recover_thread(self.store.as_ref(), thread_id)?;
        if !report.receipts_recorded.is_empty() || !report.turns_marked_failed.is_empty() {
            for event in self.store.load_thread(thread_id)? {
                if event.sequence >= before {
                    self.notify(&event);
                }
            }
        }
        Ok(report)
    }

    pub async fn run_turn(
        &self,
        thread_id: &ThreadId,
        input: impl Into<String>,
    ) -> Result<TurnOutcome, RuntimeError> {
        self.run_turn_with_cancellation(thread_id, input, CancellationToken::new())
            .await
    }

    pub async fn run_turn_with_cancellation(
        &self,
        thread_id: &ThreadId,
        input: impl Into<String>,
        cancellation: CancellationToken,
    ) -> Result<TurnOutcome, RuntimeError> {
        self.run_turn_with_id_and_cancellation(thread_id, TurnId::new(), input, cancellation)
            .await
    }

    /// Runs a turn with a control-plane supplied id, allowing an app server to
    /// return the id before the background model loop finishes.
    pub async fn run_turn_with_id_and_cancellation(
        &self,
        thread_id: &ThreadId,
        turn_id: TurnId,
        input: impl Into<String>,
        cancellation: CancellationToken,
    ) -> Result<TurnOutcome, RuntimeError> {
        self.ensure_thread(thread_id)?;
        let _active = self.acquire_thread(thread_id)?;
        self.ensure_turn_unused(thread_id, &turn_id)?;
        self.recover(thread_id)?;

        let input = input.into();
        self.append(
            thread_id.clone(),
            Some(turn_id.clone()),
            RuntimeEvent::TurnStarted,
        )?;
        self.append(
            thread_id.clone(),
            Some(turn_id.clone()),
            RuntimeEvent::UserMessage {
                content: input.clone(),
            },
        )?;

        let mut transcript = transcript_from_events(&self.store.load_thread(thread_id)?);
        let capabilities = self.capabilities();
        let visible_names: Vec<String> = capabilities.visible_names().map(str::to_owned).collect();
        let visible_tools = self
            .tools
            .definitions_for(visible_names.iter().map(String::as_str));
        let mut seen_call_ids = BTreeSet::<CallId>::new();

        for iteration in 0..self.config.max_model_iterations {
            if cancellation.is_cancelled() {
                return self.cancel_turn(thread_id, &turn_id, &[]);
            }

            let compiled = match self.compiler.compile(ContextInput {
                thread_id: thread_id.clone(),
                turn_id: turn_id.clone(),
                transcript: transcript.clone(),
            }) {
                Ok(compiled) => compiled,
                Err(error) => {
                    self.fail_turn(thread_id, &turn_id, error.to_string())?;
                    return Err(error.into());
                }
            };

            let hook_context = HookContext {
                thread_id: thread_id.clone(),
                turn_id: turn_id.clone(),
                iteration,
            };
            if let Err(error) = self.hooks.before_model(&hook_context, &compiled) {
                self.record_hook_failure(thread_id, &turn_id, HookPoint::BeforeModel, &error)?;
                self.fail_turn(thread_id, &turn_id, error.to_string())?;
                return Err(error.into());
            }

            self.append(
                thread_id.clone(),
                Some(turn_id.clone()),
                RuntimeEvent::ContextCompiled {
                    iteration,
                    prompt_segments: compiled.prompt.len(),
                    message_count: compiled.messages.len(),
                    visible_tools: visible_names.clone(),
                },
            )?;
            self.append(
                thread_id.clone(),
                Some(turn_id.clone()),
                RuntimeEvent::ModelRequestStarted { iteration },
            )?;

            let response = match self
                .model
                .complete(ModelRequest {
                    thread_id: thread_id.clone(),
                    turn_id: turn_id.clone(),
                    iteration,
                    context: compiled,
                    tools: visible_tools.clone(),
                })
                .await
            {
                Ok(response) => response,
                Err(error) => {
                    self.fail_turn(thread_id, &turn_id, error.to_string())?;
                    return Err(error.into());
                }
            };

            if let Err(reason) = validate_tool_calls(&response.tool_calls, &seen_call_ids) {
                self.fail_turn(thread_id, &turn_id, reason.clone())?;
                return Err(RuntimeError::InvalidModelResponse { turn_id, reason });
            }
            seen_call_ids.extend(response.tool_calls.iter().map(|call| call.id.clone()));

            self.append(
                thread_id.clone(),
                Some(turn_id.clone()),
                RuntimeEvent::AssistantMessage {
                    content: response.content.clone(),
                    tool_calls: response.tool_calls.clone(),
                    stop_reason: response.stop_reason,
                    usage: response.usage.clone(),
                },
            )?;
            transcript.push(TranscriptMessage::Assistant {
                content: response.content.clone(),
                tool_calls: response.tool_calls.clone(),
            });

            if response.tool_calls.is_empty() {
                self.append(
                    thread_id.clone(),
                    Some(turn_id.clone()),
                    RuntimeEvent::TurnCompleted {
                        content: response.content.clone(),
                    },
                )?;
                return Ok(TurnOutcome {
                    thread_id: thread_id.clone(),
                    turn_id,
                    content: response.content,
                    model_iterations: iteration + 1,
                });
            }

            for (index, call) in response.tool_calls.iter().enumerate() {
                if cancellation.is_cancelled() {
                    return self.cancel_turn(thread_id, &turn_id, &response.tool_calls[index..]);
                }
                let receipt = self
                    .handle_tool_call(
                        thread_id,
                        &turn_id,
                        iteration,
                        call,
                        &capabilities,
                        cancellation.clone(),
                    )
                    .await?;
                transcript.push(TranscriptMessage::Tool { receipt });
            }
        }

        self.fail_turn(
            thread_id,
            &turn_id,
            format!(
                "maximum model iterations ({}) exceeded",
                self.config.max_model_iterations
            ),
        )?;
        Err(RuntimeError::MaxModelIterations {
            turn_id,
            maximum: self.config.max_model_iterations,
        })
    }

    async fn handle_tool_call(
        &self,
        thread_id: &ThreadId,
        turn_id: &TurnId,
        iteration: usize,
        call: &ToolCall,
        capabilities: &CapabilityProjection,
        cancellation: CancellationToken,
    ) -> Result<ToolReceipt, RuntimeError> {
        use crate::policy::Access;

        let access = capabilities.access(&call.name);
        let Some(tool) = self.tools.get(&call.name) else {
            let receipt = ToolReceipt::denied(
                call.id.clone(),
                call.name.clone(),
                None,
                "tool is not registered",
            );
            self.record_receipt(thread_id, turn_id, receipt.clone(), false)?;
            return Ok(receipt);
        };
        if access == Access::Deny {
            let receipt = ToolReceipt::denied(
                call.id.clone(),
                call.name.clone(),
                None,
                "tool is denied by the effective capability projection",
            );
            self.record_receipt(thread_id, turn_id, receipt.clone(), false)?;
            return Ok(receipt);
        }

        let execution_context = ToolExecutionContext {
            thread_id: thread_id.clone(),
            turn_id: turn_id.clone(),
            workspace: self.config.workspace.clone(),
            cancellation,
            metadata: self.config.tool_metadata.clone(),
        };
        let prepared = match tool.prepare(call, &execution_context) {
            Ok(prepared) => prepared,
            Err(error) => {
                let receipt = ToolReceipt::malformed(call, error.to_string());
                self.record_receipt(thread_id, turn_id, receipt.clone(), false)?;
                return Ok(receipt);
            }
        };
        if prepared.call_id != call.id || prepared.action.tool_name != call.name {
            let receipt =
                ToolReceipt::malformed(call, "tool preparation changed the call id or tool name");
            self.record_receipt(thread_id, turn_id, receipt.clone(), false)?;
            return Ok(receipt);
        }
        self.append(
            thread_id.clone(),
            Some(turn_id.clone()),
            RuntimeEvent::ToolPrepared {
                prepared: prepared.clone(),
            },
        )?;

        let hook_context = HookContext {
            thread_id: thread_id.clone(),
            turn_id: turn_id.clone(),
            iteration,
        };
        if let Err(error) = self.hooks.before_tool(&hook_context, &prepared) {
            self.record_hook_failure(thread_id, turn_id, HookPoint::BeforeTool, &error)?;
            let receipt = ToolReceipt::denied(
                call.id.clone(),
                call.name.clone(),
                Some(prepared.action),
                error.to_string(),
            );
            self.record_receipt(thread_id, turn_id, receipt.clone(), false)?;
            return Ok(receipt);
        }

        if access == Access::Ask {
            let request = ApprovalRequest {
                thread_id: thread_id.clone(),
                turn_id: turn_id.clone(),
                call_id: call.id.clone(),
                action: prepared.action.clone(),
                reason: "effective tool policy requires approval".to_owned(),
            };
            self.append(
                thread_id.clone(),
                Some(turn_id.clone()),
                RuntimeEvent::ApprovalRequested {
                    request: request.clone(),
                },
            )?;
            let decision = self
                .approvals
                .request(request)
                .await
                .unwrap_or_else(|error| ApprovalDecision::Denied {
                    reason: error.to_string(),
                });
            self.append(
                thread_id.clone(),
                Some(turn_id.clone()),
                RuntimeEvent::ApprovalResolved {
                    call_id: call.id.clone(),
                    decision: decision.clone(),
                },
            )?;
            let denial = match decision {
                ApprovalDecision::Approved { action } if action == prepared.action => None,
                ApprovalDecision::Approved { .. } => Some(
                    "approval did not match the exact canonical action; execution was denied"
                        .to_owned(),
                ),
                ApprovalDecision::Denied { reason } => Some(reason),
            };
            if let Some(reason) = denial {
                let receipt = ToolReceipt::denied(
                    call.id.clone(),
                    call.name.clone(),
                    Some(prepared.action),
                    reason,
                );
                self.record_receipt(thread_id, turn_id, receipt.clone(), false)?;
                return Ok(receipt);
            }
        }

        self.append(
            thread_id.clone(),
            Some(turn_id.clone()),
            RuntimeEvent::ToolExecutionStarted {
                prepared: prepared.clone(),
            },
        )?;
        let receipt = match self
            .executor
            .execute(tool, prepared.clone(), execution_context)
            .await
        {
            Ok(output) => ToolReceipt::succeeded(prepared, output.value),
            Err(error) => ToolReceipt::failed(prepared, error.to_string()),
        };
        self.record_receipt(thread_id, turn_id, receipt.clone(), true)?;

        if let Err(error) = self.hooks.after_tool(&hook_context, &receipt) {
            self.record_hook_failure(thread_id, turn_id, HookPoint::AfterTool, &error)?;
            self.fail_turn(thread_id, turn_id, error.to_string())?;
            return Err(error.into());
        }
        Ok(receipt)
    }

    fn ensure_thread(&self, thread_id: &ThreadId) -> Result<(), RuntimeError> {
        let exists = self
            .store
            .load_thread(thread_id)?
            .iter()
            .any(|event| matches!(event.event, RuntimeEvent::ThreadStarted));
        if exists {
            Ok(())
        } else {
            Err(RuntimeError::UnknownThread(thread_id.clone()))
        }
    }

    fn ensure_turn_unused(
        &self,
        thread_id: &ThreadId,
        turn_id: &TurnId,
    ) -> Result<(), RuntimeError> {
        let duplicate = self
            .store
            .load_thread(thread_id)?
            .iter()
            .any(|event| event.turn_id.as_ref() == Some(turn_id));
        if duplicate {
            Err(RuntimeError::DuplicateTurn {
                thread_id: thread_id.clone(),
                turn_id: turn_id.clone(),
            })
        } else {
            Ok(())
        }
    }

    fn acquire_thread(&self, thread_id: &ThreadId) -> Result<ActiveThread<'_>, RuntimeError> {
        let mut active = self
            .active_threads
            .lock()
            .map_err(|_| StoreError::Poisoned)?;
        if !active.insert(thread_id.clone()) {
            return Err(RuntimeError::ConcurrentTurn(thread_id.clone()));
        }
        Ok(ActiveThread {
            active: &self.active_threads,
            thread_id: thread_id.clone(),
        })
    }

    fn cancel_turn<T>(
        &self,
        thread_id: &ThreadId,
        turn_id: &TurnId,
        unhandled_calls: &[ToolCall],
    ) -> Result<T, RuntimeError> {
        for call in unhandled_calls {
            let receipt = ToolReceipt::denied(
                call.id.clone(),
                call.name.clone(),
                None,
                "turn was cancelled before tool preparation",
            );
            self.record_receipt(thread_id, turn_id, receipt, false)?;
        }
        self.append(
            thread_id.clone(),
            Some(turn_id.clone()),
            RuntimeEvent::TurnCancelled,
        )?;
        Err(RuntimeError::Cancelled(turn_id.clone()))
    }

    fn fail_turn(
        &self,
        thread_id: &ThreadId,
        turn_id: &TurnId,
        error: String,
    ) -> Result<(), RuntimeError> {
        self.append(
            thread_id.clone(),
            Some(turn_id.clone()),
            RuntimeEvent::TurnFailed { error },
        )?;
        Ok(())
    }

    fn record_receipt(
        &self,
        thread_id: &ThreadId,
        turn_id: &TurnId,
        receipt: ToolReceipt,
        execution_crossed: bool,
    ) -> Result<(), RuntimeError> {
        let result = self.append(
            thread_id.clone(),
            Some(turn_id.clone()),
            RuntimeEvent::ToolReceiptRecorded {
                receipt: receipt.clone(),
            },
        );
        match result {
            Ok(_) => Ok(()),
            Err(RuntimeError::Store(source)) if execution_crossed => {
                Err(RuntimeError::ReceiptPersistenceUnknown {
                    call_id: receipt.call_id,
                    tool_name: receipt.tool_name,
                    source,
                })
            }
            Err(error) => Err(error),
        }
    }

    fn record_hook_failure(
        &self,
        thread_id: &ThreadId,
        turn_id: &TurnId,
        point: HookPoint,
        error: &HookError,
    ) -> Result<(), RuntimeError> {
        self.append(
            thread_id.clone(),
            Some(turn_id.clone()),
            RuntimeEvent::HookFailed {
                hook: error.hook.clone(),
                point,
                reason: error.reason.clone(),
            },
        )?;
        Ok(())
    }

    fn append(
        &self,
        thread_id: ThreadId,
        turn_id: Option<TurnId>,
        event: RuntimeEvent,
    ) -> Result<EventEnvelope, RuntimeError> {
        let envelope = self.store.append(thread_id, turn_id, event)?;
        self.notify(&envelope);
        Ok(envelope)
    }

    fn notify(&self, event: &EventEnvelope) {
        for observer in &self.observers {
            observer.on_event(event);
        }
    }
}

struct ActiveThread<'a> {
    active: &'a Mutex<BTreeSet<ThreadId>>,
    thread_id: ThreadId,
}

impl Drop for ActiveThread<'_> {
    fn drop(&mut self) {
        if let Ok(mut active) = self.active.lock() {
            active.remove(&self.thread_id);
        }
    }
}

fn transcript_from_events(events: &[EventEnvelope]) -> Vec<TranscriptMessage> {
    events
        .iter()
        .filter_map(|envelope| match &envelope.event {
            RuntimeEvent::UserMessage { content } => Some(TranscriptMessage::User {
                content: content.clone(),
            }),
            RuntimeEvent::AssistantMessage {
                content,
                tool_calls,
                ..
            } => Some(TranscriptMessage::Assistant {
                content: content.clone(),
                tool_calls: tool_calls.clone(),
            }),
            RuntimeEvent::ToolReceiptRecorded { receipt } => Some(TranscriptMessage::Tool {
                receipt: receipt.clone(),
            }),
            _ => None,
        })
        .collect()
}

fn validate_tool_calls(
    calls: &[ToolCall],
    previously_seen: &BTreeSet<CallId>,
) -> Result<(), String> {
    let mut current = BTreeSet::new();
    for call in calls {
        if call.id.as_str().trim().is_empty() {
            return Err("model returned a tool call with an empty id".to_owned());
        }
        if call.name.trim().is_empty() {
            return Err(format!("tool call `{}` has an empty tool name", call.id));
        }
        if previously_seen.contains(&call.id) || !current.insert(call.id.clone()) {
            return Err(format!(
                "tool call id `{}` was reused within the same turn",
                call.id
            ));
        }
    }
    Ok(())
}
