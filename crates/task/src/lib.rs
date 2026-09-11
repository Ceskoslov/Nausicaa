//! Optional task acceptance, independent of model turn completion and authority.
//!
//! The trusted host fixes criteria, starts an attempt before generation, and
//! supplies artifact snapshots and complete receipts after a turn. This layer
//! never executes tools or retries actions. Each task owns a separate versioned
//! journal; callers must ensure exclusive ownership across instances/processes.

use std::collections::BTreeSet;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::sync::Arc;

use agent_harness_core::{ReceiptStatus, ToolReceipt};
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// An inspectable, retained UTF-8 snapshot. IDs are logical names, not paths to
/// read automatically. Hosts must capture artifacts from their trusted workspace.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ArtifactRef {
    pub id: String,
    pub content: String,
}

/// A deliberately narrow deterministic oracle: exact UTF-8 artifact contents.
/// Every criterion is required; missing artifacts fail closed.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Criterion {
    pub id: String,
    pub artifact_id: String,
    pub expected: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Budget {
    /// Generation attempts, each followed by verification even on the last try.
    pub max_attempts: u32,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct TaskSpec {
    pub objective: String,
    pub criteria: Vec<Criterion>,
    pub budget: Budget,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct CheckEvidence {
    pub criterion_id: String,
    pub artifact_id: String,
    pub passed: bool,
    pub feedback: String,
}

/// Stateless checker; model prose cannot bypass it.
#[derive(Clone, Copy, Debug, Default)]
pub struct CompletionGate;

impl CompletionGate {
    pub fn check(spec: &TaskSpec, artifacts: &[ArtifactRef]) -> Vec<CheckEvidence> {
        spec.criteria.iter().map(|criterion| {
            let matches: Vec<_> = artifacts.iter().filter(|a| a.id == criterion.artifact_id).collect();
            let passed = matches.len() == 1 && matches[0].content == criterion.expected;
            CheckEvidence {
                criterion_id: criterion.id.clone(),
                artifact_id: criterion.artifact_id.clone(),
                passed,
                feedback: if passed { "exact content verified".to_owned() } else {
                    format!("Repair artifact {:?} to satisfy criterion {:?}: expected exact content {:?}; found {} snapshot(s)", criterion.artifact_id, criterion.id, criterion.expected, matches.len())
                },
            }
        }).collect()
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Ready,
    Running,
    NeedsRepair,
    Succeeded,
    BudgetExhausted,
    Blocked,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct AttemptEvidence {
    pub attempt: u32,
    /// Host-supplied core turn ID for joining the two independent journals.
    pub turn_id: String,
    pub artifacts: Vec<ArtifactRef>,
    pub checks: Vec<CheckEvidence>,
    pub receipts: Vec<ToolReceipt>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct TaskState {
    pub spec: TaskSpec,
    pub status: TaskStatus,
    pub attempts: u32,
    pub evidence: Vec<AttemptEvidence>,
    pub remaining_work: Vec<String>,
    active_turn: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
enum TaskEvent {
    Created {
        spec: TaskSpec,
    },
    AttemptStarted {
        turn_id: String,
    },
    AttemptChecked {
        artifacts: Vec<ArtifactRef>,
        receipts: Vec<ToolReceipt>,
    },
    Blocked {
        reason: String,
    },
}

#[derive(Serialize, Deserialize)]
struct Record {
    version: u32,
    sequence: usize,
    payload: TaskEvent,
}

#[derive(Debug, Error)]
pub enum TaskError {
    #[error("task journal I/O: {0}")]
    Io(#[from] std::io::Error),
    #[error("task journal JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("invalid task transition or journal: {0}")]
    Invalid(String),
}

fn invalid(message: &str) -> TaskError {
    TaskError::Invalid(message.to_owned())
}

/// Pins the immutable task contract after an inner compiler has compacted chat
/// history. Scope one instance to one task; use the outermost compiler position
/// so another compactor cannot remove its segment. This is advisory context only.
pub struct TaskContextCompiler {
    inner: Arc<dyn agent_harness_core::ContextCompiler>,
    contract: agent_harness_core::PromptSegment,
}

impl TaskContextCompiler {
    pub fn new(
        inner: Arc<dyn agent_harness_core::ContextCompiler>,
        spec: &TaskSpec,
    ) -> Result<Self, TaskError> {
        initial_state(spec.clone())?;
        Ok(Self {
            inner,
            contract: agent_harness_core::PromptSegment::new(
                agent_harness_core::PromptLayer::Stable,
                "task-contract-v1",
                format!(
                    "Host task contract (context, not tool authority):\n{}",
                    serde_json::to_string(spec)?
                ),
            ),
        })
    }
}

impl agent_harness_core::ContextCompiler for TaskContextCompiler {
    fn compile(
        &self,
        input: agent_harness_core::ContextInput,
    ) -> Result<agent_harness_core::CompiledContext, agent_harness_core::ContextError> {
        let mut context = self.inner.compile(input)?;
        context.prompt.insert(0, self.contract.clone());
        Ok(context)
    }
}

/// One task per append-only JSONL file. Successful changes are synced before
/// publication. An I/O failure poisons writes; reopen and inspect before recovery.
/// Incomplete tails are rejected, never silently repaired or re-executed.
#[derive(Debug)]
pub struct TaskJournal {
    file: File,
    state: TaskState,
    sequence: usize,
    write_failed: bool,
}

impl TaskJournal {
    pub fn create(path: impl AsRef<Path>, spec: TaskSpec) -> Result<Self, TaskError> {
        let state = initial_state(spec.clone())?;
        let file = OpenOptions::new()
            .create_new(true)
            .append(true)
            .open(path)?;
        let mut journal = Self {
            file,
            state,
            sequence: 0,
            write_failed: false,
        };
        journal.append(TaskEvent::Created { spec })?;
        Ok(journal)
    }

    pub fn open(path: impl AsRef<Path>) -> Result<Self, TaskError> {
        let bytes = std::fs::read(&path)?;
        if !bytes.ends_with(b"\n") {
            return Err(invalid(
                "empty or unterminated journal; inspect before recovery",
            ));
        }
        let mut state = None;
        let mut sequence = 0;
        for line in bytes.split_inclusive(|b| *b == b'\n') {
            let record: Record = serde_json::from_slice(line)?;
            if record.version != 1 || record.sequence != sequence {
                return Err(invalid("unsupported version or out-of-order sequence"));
            }
            match (&mut state, record.payload) {
                (None, TaskEvent::Created { spec }) => state = Some(initial_state(spec)?),
                (Some(current), event) => apply(current, &event)?,
                _ => return Err(invalid("journal must begin with task creation")),
            }
            sequence += 1;
        }
        Ok(Self {
            file: OpenOptions::new().append(true).open(path)?,
            state: state.ok_or_else(|| invalid("missing task creation"))?,
            sequence,
            write_failed: false,
        })
    }

    pub fn state(&self) -> &TaskState {
        &self.state
    }

    /// Persist before calling the runtime. A replayed running attempt requires
    /// explicit host reconciliation, never an automatic generation retry.
    pub fn start_attempt(&mut self, turn_id: impl Into<String>) -> Result<(), TaskError> {
        self.transition(TaskEvent::AttemptStarted {
            turn_id: turn_id.into(),
        })
    }

    /// Supply snapshots and all receipts from the associated core turn. Unknown
    /// outcomes block the task even if artifact checks pass.
    pub fn verify(
        &mut self,
        artifacts: Vec<ArtifactRef>,
        receipts: Vec<ToolReceipt>,
    ) -> Result<(), TaskError> {
        self.transition(TaskEvent::AttemptChecked {
            artifacts,
            receipts,
        })
    }

    /// Explicitly stop for host intervention (including interrupted attempts).
    pub fn block(&mut self, reason: impl Into<String>) -> Result<(), TaskError> {
        self.transition(TaskEvent::Blocked {
            reason: reason.into(),
        })
    }

    fn transition(&mut self, event: TaskEvent) -> Result<(), TaskError> {
        let mut next = self.state.clone();
        apply(&mut next, &event)?;
        self.append(event)?;
        self.state = next;
        Ok(())
    }

    fn append(&mut self, payload: TaskEvent) -> Result<(), TaskError> {
        if self.write_failed {
            return Err(invalid("previous write failed; reopen journal"));
        }
        let mut bytes = serde_json::to_vec(&Record {
            version: 1,
            sequence: self.sequence,
            payload,
        })?;
        bytes.push(b'\n');
        self.write_failed = true;
        self.file.write_all(&bytes)?;
        self.file.sync_data()?;
        self.write_failed = false;
        self.sequence += 1;
        Ok(())
    }
}

fn initial_state(spec: TaskSpec) -> Result<TaskState, TaskError> {
    let mut ids = BTreeSet::new();
    if spec.objective.trim().is_empty() || spec.criteria.is_empty() || spec.budget.max_attempts == 0
    {
        return Err(invalid(
            "objective, required criteria and positive attempt budget are mandatory",
        ));
    }
    for criterion in &spec.criteria {
        if criterion.id.trim().is_empty()
            || criterion.artifact_id.trim().is_empty()
            || !ids.insert(&criterion.id)
        {
            return Err(invalid(
                "criterion IDs must be nonempty and unique; artifact IDs must be nonempty",
            ));
        }
    }
    Ok(TaskState {
        remaining_work: spec.criteria.iter().map(|c| c.id.clone()).collect(),
        spec,
        status: TaskStatus::Ready,
        attempts: 0,
        evidence: Vec::new(),
        active_turn: None,
    })
}

fn apply(state: &mut TaskState, event: &TaskEvent) -> Result<(), TaskError> {
    match event {
        TaskEvent::Created { .. } => return Err(invalid("task specification is immutable")),
        TaskEvent::AttemptStarted { turn_id } => {
            if !matches!(state.status, TaskStatus::Ready | TaskStatus::NeedsRepair)
                || state.attempts >= state.spec.budget.max_attempts
                || turn_id.trim().is_empty()
                || state.evidence.iter().any(|e| e.turn_id == *turn_id)
            {
                return Err(invalid(
                    "attempt requires a ready task, fresh turn ID and remaining budget",
                ));
            }
            state.attempts += 1;
            state.active_turn = Some(turn_id.clone());
            state.status = TaskStatus::Running;
        }
        TaskEvent::AttemptChecked {
            artifacts,
            receipts,
        } => {
            if state.status != TaskStatus::Running {
                return Err(invalid("verification requires a running attempt"));
            }
            let checks = CompletionGate::check(&state.spec, artifacts);
            state.remaining_work = checks
                .iter()
                .filter(|c| !c.passed)
                .map(|c| c.feedback.clone())
                .collect();
            if receipts.iter().any(|r| r.status == ReceiptStatus::Unknown) {
                state.remaining_work.push(
                    "Reconcile unknown tool outcomes with the host; do not replay actions"
                        .to_owned(),
                );
                state.status = TaskStatus::Blocked;
            } else if checks.iter().all(|c| c.passed) {
                state.status = TaskStatus::Succeeded;
            } else if state.attempts == state.spec.budget.max_attempts {
                state.status = TaskStatus::BudgetExhausted;
            } else {
                state.status = TaskStatus::NeedsRepair;
            }
            state.evidence.push(AttemptEvidence {
                attempt: state.attempts,
                turn_id: state
                    .active_turn
                    .take()
                    .ok_or_else(|| invalid("missing active turn"))?,
                artifacts: artifacts.clone(),
                checks,
                receipts: receipts.clone(),
            });
        }
        TaskEvent::Blocked { reason } => {
            if !matches!(
                state.status,
                TaskStatus::Ready | TaskStatus::Running | TaskStatus::NeedsRepair
            ) || reason.trim().is_empty()
            {
                return Err(invalid("blocking requires a nonterminal task and a reason"));
            }
            state.status = TaskStatus::Blocked;
            state.remaining_work.push(reason.clone());
        }
    }
    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn append_failure_preserves_state_and_poisons_writes() {
        let mut journal = TaskJournal {
            file: OpenOptions::new().write(true).open("/dev/full").unwrap(),
            state: initial_state(TaskSpec {
                objective: "test".into(),
                criteria: vec![Criterion {
                    id: "check".into(),
                    artifact_id: "file".into(),
                    expected: "ok".into(),
                }],
                budget: Budget { max_attempts: 1 },
            })
            .unwrap(),
            sequence: 1,
            write_failed: false,
        };
        let before = journal.state().clone();
        assert!(matches!(
            journal.start_attempt("turn"),
            Err(TaskError::Io(_))
        ));
        assert_eq!(journal.state(), &before);
        assert!(journal.write_failed);
        assert!(matches!(
            journal.start_attempt("turn"),
            Err(TaskError::Invalid(_))
        ));
    }
}
