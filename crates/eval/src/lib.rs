//! Paired evaluation of identical isolated fixtures with core-only completion and
//! optional task acceptance. Scripted runs measure harness behavior, not model IQ.
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::task::{Context, Poll, Wake, Waker};
use std::time::Instant;

use agent_harness_core::*;
use agent_harness_executor_process::WriteFileTool;
use agent_harness_task::*;
use serde::{Deserialize, Serialize};
use serde_json::json;

pub type EvalResult<T> = Result<T, Box<dyn std::error::Error>>;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Fixture {
    pub version: u32,
    pub id: String,
    pub split: String,
    pub objective: String,
    pub initial: String,
    pub expected: String,
    pub proposals: Vec<String>,
    pub max_attempts: u32,
    pub deny_write: bool,
    pub interrupt_execution: bool,
}

/// Complete pinned fixture bytes are also retained with every result.
pub fn fixtures() -> EvalResult<Vec<Fixture>> {
    [
        include_str!("../fixtures/development/repair.json"),
        include_str!("../fixtures/development/exhausted.json"),
        include_str!("../fixtures/development/denied.json"),
        include_str!("../fixtures/held-out/identity.json"),
        include_str!("../fixtures/held-out/unknown.json"),
    ]
    .iter()
    .map(|s| Ok(serde_json::from_str(s)?))
    .collect()
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    CoreOnly,
    Task,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RunReport {
    pub fixture: Fixture,
    pub mode: Mode,
    /// Snapshot model identifier or the built-in script version.
    pub model: String,
    pub harness_version: String,
    pub max_model_iterations_per_turn: usize,
    pub artifact_directory: PathBuf,
    pub reported_success: bool,
    pub verified_success: bool,
    pub false_completion: bool,
    pub attempts: u32,
    pub model_iterations: usize,
    pub elapsed_ms: u128,
    pub tokens: TokenUsage,
    /// True only for live adapter usage; scripted usage is deliberately zero.
    pub live_model: bool,
    pub interventions: u32,
    pub denied_receipts: usize,
    pub unknown_receipts: usize,
    pub task_status: Option<TaskStatus>,
    pub turn_errors: Vec<String>,
}

struct ScriptedModel {
    fixture: Fixture,
    calls: AtomicUsize,
}
impl ModelAdapter for ScriptedModel {
    fn complete<'a>(
        &'a self,
        _request: ModelRequest,
    ) -> BoxFuture<'a, Result<ModelResponse, ModelError>> {
        Box::pin(async move {
            let call = self.calls.fetch_add(1, Ordering::SeqCst);
            if call % 2 == 1 {
                return Ok(ModelResponse::text("The requested change is complete."));
            }
            let content = self
                .fixture
                .proposals
                .get(call / 2)
                .or_else(|| self.fixture.proposals.last())
                .ok_or_else(|| ModelError::new("empty fixture script", false))?;
            Ok(ModelResponse::tool_calls(vec![ToolCall::new(
                "write_file",
                json!({"path": "lib.rs", "content": content}),
            )]))
        })
    }
}

struct InterruptedExecutor {
    entered: Arc<AtomicBool>,
}
impl ToolExecutor for InterruptedExecutor {
    fn execute<'a>(
        &'a self,
        _tool: Arc<dyn Tool>,
        _prepared: PreparedToolCall,
        _context: ToolExecutionContext,
    ) -> BoxFuture<'a, Result<ToolOutput, ToolError>> {
        self.entered.store(true, Ordering::SeqCst);
        Box::pin(std::future::pending())
    }
}

struct ThreadWake(std::thread::Thread);
impl Wake for ThreadWake {
    fn wake(self: Arc<Self>) {
        self.0.unpark();
    }
    fn wake_by_ref(self: &Arc<Self>) {
        self.0.unpark();
    }
}
fn block_on<F: Future>(future: F) -> F::Output {
    let waker = Waker::from(Arc::new(ThreadWake(std::thread::current())));
    let mut context = Context::from_waker(&waker);
    let mut future = std::pin::pin!(future);
    loop {
        match future.as_mut().poll(&mut context) {
            Poll::Ready(value) => return value,
            Poll::Pending => std::thread::park(),
        }
    }
}

pub fn run_scripted(fixture: &Fixture, mode: Mode, directory: &Path) -> EvalResult<RunReport> {
    run(
        fixture,
        mode,
        directory,
        Arc::new(ScriptedModel {
            fixture: fixture.clone(),
            calls: AtomicUsize::new(0),
        }),
        "scripted-v1",
        false,
    )
}

/// The caller owns model pinning and runtime limits. The CLI records full live
/// configuration separately. Fault-injection fixtures are scripted-only.
pub fn run(
    fixture: &Fixture,
    mode: Mode,
    directory: &Path,
    model: Arc<dyn ModelAdapter>,
    model_id: &str,
    live_model: bool,
) -> EvalResult<RunReport> {
    if fixture.version != 1 || fixture.proposals.is_empty() || fixture.max_attempts == 0 {
        return Err("unsupported or invalid fixture".into());
    }
    if live_model && fixture.interrupt_execution {
        return Err("interruption injection requires the synchronous scripted model".into());
    }
    // Never reuse a prior fixture workspace or overwrite prior evidence.
    std::fs::create_dir(directory)?;
    let workspace = directory.join("workspace");
    std::fs::create_dir(&workspace)?;
    std::fs::write(workspace.join("lib.rs"), &fixture.initial)?;
    std::fs::write(
        directory.join("fixture.json"),
        serde_json::to_vec_pretty(fixture)?,
    )?;
    let spec = TaskSpec {
        objective: fixture.objective.clone(),
        criteria: vec![Criterion {
            id: "source-content-v1".into(),
            artifact_id: "lib.rs".into(),
            expected: fixture.expected.clone(),
        }],
        budget: Budget {
            max_attempts: fixture.max_attempts,
        },
    };
    let mut task = match mode {
        Mode::CoreOnly => None,
        Mode::Task => Some(TaskJournal::create(
            directory.join("task.jsonl"),
            spec.clone(),
        )?),
    };
    let store = Arc::new(JsonlEventStore::open(directory.join("events.jsonl"))?);
    let mut registry = ToolRegistry::new();
    registry.register(WriteFileTool::new(&workspace)?)?;
    let entered = Arc::new(AtomicBool::new(false));
    let executor: Arc<dyn ToolExecutor> = if fixture.interrupt_execution {
        Arc::new(InterruptedExecutor {
            entered: entered.clone(),
        })
    } else {
        Arc::new(DirectExecutor)
    };
    let runtime = AgentRuntime::new(
        model,
        store.clone(),
        Arc::new(LayeredContextCompiler::new()),
        registry,
        Arc::new(CapabilityPolicy::deny_by_default().grant(
            "write_file",
            if fixture.deny_write {
                Access::Deny
            } else {
                Access::Allow
            },
        )),
    )
    .with_executor(executor)
    .with_config(RuntimeConfig {
        max_model_iterations: 4,
        workspace: Some(workspace.clone()),
        ..RuntimeConfig::default()
    });
    let thread = runtime.start_thread()?;
    let started = Instant::now();
    let mut attempts = 0;
    let mut normal_completion = false;
    let mut verified_success;
    let mut turn_errors = Vec::new();
    let mut input = format!(
        "{}\nEdit lib.rs. Required exact source contents:\n{}",
        fixture.objective, fixture.expected
    );
    loop {
        let turn = TurnId::new();
        if let Some(task) = &mut task {
            task.start_attempt(turn.to_string())?;
        }
        attempts += 1;
        let future = runtime.run_turn_with_id_and_cancellation(
            &thread,
            turn.clone(),
            input.clone(),
            CancellationToken::new(),
        );
        if fixture.interrupt_execution {
            {
                let mut future = Box::pin(future);
                if !matches!(
                    future
                        .as_mut()
                        .poll(&mut Context::from_waker(Waker::noop())),
                    Poll::Pending
                ) || !entered.load(Ordering::SeqCst)
                {
                    return Err("fault fixture failed to reach execution boundary".into());
                }
            }
            runtime.recover(&thread)?;
            turn_errors.push(
                "injected interruption at execution boundary; recovered without replay".into(),
            );
        } else {
            match block_on(future) {
                Ok(_) => normal_completion = true,
                Err(error) => {
                    normal_completion = false;
                    turn_errors.push(error.to_string());
                    // Close any accepted calls before collecting evidence. If
                    // persistence is poisoned, propagate the error and leave
                    // the task running for explicit host reconciliation.
                    runtime.recover(&thread)?;
                }
            }
        }
        let receipts: Vec<_> = store
            .load_thread(&thread)?
            .into_iter()
            .filter(|e| e.turn_id.as_ref() == Some(&turn))
            .filter_map(|e| match e.event {
                RuntimeEvent::ToolReceiptRecorded { receipt } => Some(receipt),
                _ => None,
            })
            .collect();
        let artifacts = vec![ArtifactRef {
            id: "lib.rs".into(),
            content: std::fs::read_to_string(workspace.join("lib.rs"))?,
        }];
        let checks = CompletionGate::check(&spec, &artifacts);
        verified_success = checks.iter().all(|c| c.passed)
            && !receipts.iter().any(|r| r.status == ReceiptStatus::Unknown);
        std::fs::write(
            directory.join(format!("evidence-{attempts}.json")),
            serde_json::to_vec_pretty(
                &json!({"turn_id": turn, "artifacts": artifacts, "checks": checks, "receipts": receipts}),
            )?,
        )?;
        let Some(task) = &mut task else {
            break;
        };
        task.verify(artifacts, receipts)?;
        // Runtime errors are not a blanket permission to retry unknown effects.
        if task.state().status != TaskStatus::NeedsRepair {
            break;
        }
        if !normal_completion {
            task.block("Core turn failed; host inspection required before further generation")?;
            break;
        }
        input = format!(
            "{}\nVerification feedback:\n{}",
            fixture.objective,
            task.state().remaining_work.join("\n")
        );
    }
    let events = store.load_thread(&thread)?;
    let mut tokens = TokenUsage::default();
    let mut iterations = 0;
    let mut denied = 0;
    let mut unknown = 0;
    for envelope in &events {
        match &envelope.event {
            RuntimeEvent::ModelRequestStarted { .. } => iterations += 1,
            RuntimeEvent::AssistantMessage { usage, .. } => {
                tokens.input_tokens += usage.input_tokens;
                tokens.output_tokens += usage.output_tokens;
            }
            RuntimeEvent::ToolReceiptRecorded { receipt } => match receipt.status {
                ReceiptStatus::Denied => denied += 1,
                ReceiptStatus::Unknown => unknown += 1,
                _ => {}
            },
            _ => {}
        }
    }
    let task_status = task.as_ref().map(|t| t.state().status.clone());
    let reported_success = task_status
        .as_ref()
        .map_or(normal_completion, |s| *s == TaskStatus::Succeeded);
    let report = RunReport {
        fixture: fixture.clone(),
        mode,
        model: model_id.into(),
        harness_version: env!("CARGO_PKG_VERSION").into(),
        max_model_iterations_per_turn: 4,
        artifact_directory: directory.to_path_buf(),
        reported_success,
        verified_success,
        false_completion: reported_success && !verified_success,
        attempts,
        model_iterations: iterations,
        elapsed_ms: started.elapsed().as_millis(),
        tokens,
        live_model,
        interventions: u32::from(unknown > 0 || task_status == Some(TaskStatus::Blocked)),
        denied_receipts: denied,
        unknown_receipts: unknown,
        task_status,
        turn_errors,
    };
    std::fs::write(
        directory.join("report.json"),
        serde_json::to_vec_pretty(&report)?,
    )?;
    Ok(report)
}
