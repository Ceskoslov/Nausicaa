//! Optional persistent task ledger with idempotency, leases, cancellation,
//! unknown-state recovery, and completion-delivery acknowledgement.

use std::collections::BTreeMap;
use std::fmt;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use agent_harness_core::ThreadId;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TaskId(String);

impl TaskId {
    #[must_use]
    pub fn new() -> Self {
        Self(format!("task_{}", ThreadId::new()))
    }

    #[must_use]
    pub fn from_string(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Default for TaskId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for TaskId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Pending,
    Running,
    Succeeded,
    Failed,
    Cancelled,
    Unknown,
}

impl TaskStatus {
    #[must_use]
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Succeeded | Self::Failed | Self::Cancelled | Self::Unknown
        )
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeliveryState {
    NotReady,
    Pending,
    Acknowledged,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct TaskLease {
    pub worker_id: String,
    pub expires_at_unix_ms: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct TaskRecord {
    pub id: TaskId,
    pub idempotency_key: String,
    pub payload: Value,
    pub status: TaskStatus,
    pub attempt: u32,
    pub lease: Option<TaskLease>,
    pub result: Option<Value>,
    pub error: Option<String>,
    pub delivery: DeliveryState,
    pub created_at_unix_ms: u64,
    pub updated_at_unix_ms: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Submission {
    pub task: TaskRecord,
    pub created: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum LedgerEvent {
    Submitted {
        task: TaskRecord,
    },
    Claimed {
        task_id: TaskId,
        lease: TaskLease,
    },
    Heartbeat {
        task_id: TaskId,
        expires_at_unix_ms: u64,
    },
    Succeeded {
        task_id: TaskId,
        result: Value,
    },
    Failed {
        task_id: TaskId,
        error: String,
    },
    Cancelled {
        task_id: TaskId,
        reason: String,
    },
    MarkedUnknown {
        task_id: TaskId,
        reason: String,
    },
    DeliveryAcknowledged {
        task_id: TaskId,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
struct LedgerEnvelope {
    sequence: u64,
    at_unix_ms: u64,
    event: LedgerEvent,
}

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum LedgerError {
    #[error("task ledger I/O error: {0}")]
    Io(String),
    #[error("task ledger serialization error: {0}")]
    Serialization(String),
    #[error("corrupt task ledger at line {line}: {message}")]
    Corrupt { line: usize, message: String },
    #[error("task ledger lock was poisoned")]
    Poisoned,
    #[error("task `{0}` does not exist")]
    UnknownTask(TaskId),
    #[error("invalid transition for task `{task_id}` from {status:?}: {operation}")]
    InvalidTransition {
        task_id: TaskId,
        status: TaskStatus,
        operation: String,
    },
}

struct LedgerState {
    file: File,
    next_sequence: u64,
    tasks: BTreeMap<TaskId, TaskRecord>,
    idempotency: BTreeMap<String, TaskId>,
}

pub struct JsonlTaskLedger {
    path: PathBuf,
    state: Mutex<LedgerState>,
}

impl JsonlTaskLedger {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, LedgerError> {
        let path = path.as_ref().to_path_buf();
        let read_file = OpenOptions::new()
            .create(true)
            .read(true)
            .append(true)
            .open(&path)
            .map_err(io_error)?;
        let mut tasks = BTreeMap::new();
        let mut next_sequence = 0;
        for (index, line) in BufReader::new(&read_file).lines().enumerate() {
            let line = line.map_err(io_error)?;
            if line.trim().is_empty() {
                continue;
            }
            let envelope: LedgerEnvelope =
                serde_json::from_str(&line).map_err(|error| LedgerError::Corrupt {
                    line: index + 1,
                    message: error.to_string(),
                })?;
            next_sequence = next_sequence.max(envelope.sequence + 1);
            apply_event(&mut tasks, &envelope.event, envelope.at_unix_ms)?;
        }
        let idempotency = tasks
            .values()
            .map(|task| (task.idempotency_key.clone(), task.id.clone()))
            .collect();
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(io_error)?;
        Ok(Self {
            path,
            state: Mutex::new(LedgerState {
                file,
                next_sequence,
                tasks,
                idempotency,
            }),
        })
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn submit(
        &self,
        idempotency_key: impl Into<String>,
        payload: Value,
    ) -> Result<Submission, LedgerError> {
        let idempotency_key = idempotency_key.into();
        let mut state = self.state.lock().map_err(|_| LedgerError::Poisoned)?;
        if let Some(task_id) = state.idempotency.get(&idempotency_key)
            && let Some(task) = state.tasks.get(task_id)
        {
            return Ok(Submission {
                task: task.clone(),
                created: false,
            });
        }
        let now = now_ms();
        let task = TaskRecord {
            id: TaskId::new(),
            idempotency_key,
            payload,
            status: TaskStatus::Pending,
            attempt: 0,
            lease: None,
            result: None,
            error: None,
            delivery: DeliveryState::NotReady,
            created_at_unix_ms: now,
            updated_at_unix_ms: now,
        };
        append_locked(&mut state, LedgerEvent::Submitted { task: task.clone() })?;
        Ok(Submission {
            task,
            created: true,
        })
    }

    pub fn claim_next(
        &self,
        worker_id: impl Into<String>,
        now_unix_ms: u64,
        lease_duration_ms: u64,
    ) -> Result<Option<TaskRecord>, LedgerError> {
        let worker_id = worker_id.into();
        let mut state = self.state.lock().map_err(|_| LedgerError::Poisoned)?;
        let Some(task_id) = state
            .tasks
            .values()
            .filter(|task| task.status == TaskStatus::Pending)
            .min_by_key(|task| (task.created_at_unix_ms, task.id.clone()))
            .map(|task| task.id.clone())
        else {
            return Ok(None);
        };
        let lease = TaskLease {
            worker_id,
            expires_at_unix_ms: now_unix_ms.saturating_add(lease_duration_ms.max(1)),
        };
        append_locked(
            &mut state,
            LedgerEvent::Claimed {
                task_id: task_id.clone(),
                lease,
            },
        )?;
        Ok(state.tasks.get(&task_id).cloned())
    }

    pub fn heartbeat(
        &self,
        task_id: &TaskId,
        worker_id: &str,
        expires_at_unix_ms: u64,
    ) -> Result<TaskRecord, LedgerError> {
        let mut state = self.state.lock().map_err(|_| LedgerError::Poisoned)?;
        let task = require_task(&state, task_id)?;
        if task.status != TaskStatus::Running
            || task.lease.as_ref().map(|lease| lease.worker_id.as_str()) != Some(worker_id)
        {
            return Err(invalid(task, "heartbeat"));
        }
        append_locked(
            &mut state,
            LedgerEvent::Heartbeat {
                task_id: task_id.clone(),
                expires_at_unix_ms,
            },
        )?;
        Ok(state.tasks[task_id].clone())
    }

    pub fn succeed(&self, task_id: &TaskId, result: Value) -> Result<TaskRecord, LedgerError> {
        self.finish(
            task_id,
            LedgerEvent::Succeeded {
                task_id: task_id.clone(),
                result,
            },
        )
    }

    pub fn fail(
        &self,
        task_id: &TaskId,
        error: impl Into<String>,
    ) -> Result<TaskRecord, LedgerError> {
        self.finish(
            task_id,
            LedgerEvent::Failed {
                task_id: task_id.clone(),
                error: error.into(),
            },
        )
    }

    fn finish(&self, task_id: &TaskId, event: LedgerEvent) -> Result<TaskRecord, LedgerError> {
        let mut state = self.state.lock().map_err(|_| LedgerError::Poisoned)?;
        let task = require_task(&state, task_id)?;
        if task.status != TaskStatus::Running {
            return Err(invalid(task, "finish"));
        }
        append_locked(&mut state, event)?;
        Ok(state.tasks[task_id].clone())
    }

    pub fn cancel(
        &self,
        task_id: &TaskId,
        reason: impl Into<String>,
    ) -> Result<TaskRecord, LedgerError> {
        let mut state = self.state.lock().map_err(|_| LedgerError::Poisoned)?;
        let task = require_task(&state, task_id)?;
        if task.status.is_terminal() {
            return Err(invalid(task, "cancel"));
        }
        append_locked(
            &mut state,
            LedgerEvent::Cancelled {
                task_id: task_id.clone(),
                reason: reason.into(),
            },
        )?;
        Ok(state.tasks[task_id].clone())
    }

    /// Marks expired running tasks unknown. It intentionally does not put them
    /// back into `pending`, because their side effects may already have happened.
    pub fn recover_expired(&self, now_unix_ms: u64) -> Result<Vec<TaskRecord>, LedgerError> {
        let mut state = self.state.lock().map_err(|_| LedgerError::Poisoned)?;
        let expired = state
            .tasks
            .values()
            .filter(|task| {
                task.status == TaskStatus::Running
                    && task
                        .lease
                        .as_ref()
                        .is_some_and(|lease| lease.expires_at_unix_ms <= now_unix_ms)
            })
            .map(|task| task.id.clone())
            .collect::<Vec<_>>();
        let mut recovered = Vec::new();
        for task_id in expired {
            append_locked(
                &mut state,
                LedgerEvent::MarkedUnknown {
                    task_id: task_id.clone(),
                    reason: "worker lease expired; completion cannot be determined".to_owned(),
                },
            )?;
            recovered.push(state.tasks[&task_id].clone());
        }
        Ok(recovered)
    }

    pub fn acknowledge_delivery(&self, task_id: &TaskId) -> Result<TaskRecord, LedgerError> {
        let mut state = self.state.lock().map_err(|_| LedgerError::Poisoned)?;
        let task = require_task(&state, task_id)?;
        if task.delivery != DeliveryState::Pending {
            return Err(invalid(task, "acknowledge_delivery"));
        }
        append_locked(
            &mut state,
            LedgerEvent::DeliveryAcknowledged {
                task_id: task_id.clone(),
            },
        )?;
        Ok(state.tasks[task_id].clone())
    }

    pub fn get(&self, task_id: &TaskId) -> Result<Option<TaskRecord>, LedgerError> {
        Ok(self
            .state
            .lock()
            .map_err(|_| LedgerError::Poisoned)?
            .tasks
            .get(task_id)
            .cloned())
    }

    pub fn list(&self) -> Result<Vec<TaskRecord>, LedgerError> {
        Ok(self
            .state
            .lock()
            .map_err(|_| LedgerError::Poisoned)?
            .tasks
            .values()
            .cloned()
            .collect())
    }
}

fn require_task<'a>(
    state: &'a LedgerState,
    task_id: &TaskId,
) -> Result<&'a TaskRecord, LedgerError> {
    state
        .tasks
        .get(task_id)
        .ok_or_else(|| LedgerError::UnknownTask(task_id.clone()))
}

fn invalid(task: &TaskRecord, operation: &str) -> LedgerError {
    LedgerError::InvalidTransition {
        task_id: task.id.clone(),
        status: task.status,
        operation: operation.to_owned(),
    }
}

fn append_locked(state: &mut LedgerState, event: LedgerEvent) -> Result<(), LedgerError> {
    let envelope = LedgerEnvelope {
        sequence: state.next_sequence,
        at_unix_ms: now_ms(),
        event: event.clone(),
    };
    let bytes = serde_json::to_vec(&envelope)
        .map_err(|error| LedgerError::Serialization(error.to_string()))?;
    state.file.write_all(&bytes).map_err(io_error)?;
    state.file.write_all(b"\n").map_err(io_error)?;
    state.file.flush().map_err(io_error)?;
    state.file.sync_data().map_err(io_error)?;
    apply_event(&mut state.tasks, &event, envelope.at_unix_ms)?;
    if let LedgerEvent::Submitted { task } = &event {
        state
            .idempotency
            .insert(task.idempotency_key.clone(), task.id.clone());
    }
    state.next_sequence += 1;
    Ok(())
}

fn apply_event(
    tasks: &mut BTreeMap<TaskId, TaskRecord>,
    event: &LedgerEvent,
    event_time: u64,
) -> Result<(), LedgerError> {
    match event {
        LedgerEvent::Submitted { task } => {
            tasks.insert(task.id.clone(), task.clone());
        }
        LedgerEvent::Claimed { task_id, lease } => {
            let task = tasks
                .get_mut(task_id)
                .ok_or_else(|| LedgerError::UnknownTask(task_id.clone()))?;
            task.status = TaskStatus::Running;
            task.attempt += 1;
            task.lease = Some(lease.clone());
            task.updated_at_unix_ms = event_time;
        }
        LedgerEvent::Heartbeat {
            task_id,
            expires_at_unix_ms,
        } => {
            let task = tasks
                .get_mut(task_id)
                .ok_or_else(|| LedgerError::UnknownTask(task_id.clone()))?;
            if let Some(lease) = &mut task.lease {
                lease.expires_at_unix_ms = *expires_at_unix_ms;
            }
            task.updated_at_unix_ms = event_time;
        }
        LedgerEvent::Succeeded { task_id, result } => {
            terminal(tasks, task_id, TaskStatus::Succeeded, None, event_time)?;
            tasks.get_mut(task_id).expect("checked").result = Some(result.clone());
        }
        LedgerEvent::Failed { task_id, error } => {
            terminal(
                tasks,
                task_id,
                TaskStatus::Failed,
                Some(error.clone()),
                event_time,
            )?;
        }
        LedgerEvent::Cancelled { task_id, reason } => {
            terminal(
                tasks,
                task_id,
                TaskStatus::Cancelled,
                Some(reason.clone()),
                event_time,
            )?;
        }
        LedgerEvent::MarkedUnknown { task_id, reason } => {
            terminal(
                tasks,
                task_id,
                TaskStatus::Unknown,
                Some(reason.clone()),
                event_time,
            )?;
        }
        LedgerEvent::DeliveryAcknowledged { task_id } => {
            let task = tasks
                .get_mut(task_id)
                .ok_or_else(|| LedgerError::UnknownTask(task_id.clone()))?;
            task.delivery = DeliveryState::Acknowledged;
            task.updated_at_unix_ms = event_time;
        }
    }
    Ok(())
}

fn terminal(
    tasks: &mut BTreeMap<TaskId, TaskRecord>,
    task_id: &TaskId,
    status: TaskStatus,
    error: Option<String>,
    event_time: u64,
) -> Result<(), LedgerError> {
    let task = tasks
        .get_mut(task_id)
        .ok_or_else(|| LedgerError::UnknownTask(task_id.clone()))?;
    task.status = status;
    task.lease = None;
    task.error = error;
    task.delivery = DeliveryState::Pending;
    task.updated_at_unix_ms = event_time;
    Ok(())
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

fn io_error(error: std::io::Error) -> LedgerError {
    LedgerError::Io(error.to_string())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use serde_json::json;

    use super::*;

    fn ledger() -> (PathBuf, JsonlTaskLedger) {
        let directory = std::env::temp_dir().join(ThreadId::new().as_str());
        fs::create_dir_all(&directory).unwrap();
        let ledger = JsonlTaskLedger::open(directory.join("tasks.jsonl")).unwrap();
        (directory, ledger)
    }

    #[test]
    fn submission_is_idempotent_and_completion_is_acknowledged() {
        let (directory, ledger) = ledger();
        let first = ledger.submit("same", json!({ "work": 1 })).unwrap();
        let second = ledger.submit("same", json!({ "work": 2 })).unwrap();
        assert!(first.created);
        assert!(!second.created);
        assert_eq!(first.task.id, second.task.id);

        let running = ledger.claim_next("worker", 100, 50).unwrap().unwrap();
        let done = ledger.succeed(&running.id, json!({ "ok": true })).unwrap();
        assert_eq!(done.delivery, DeliveryState::Pending);
        let acknowledged = ledger.acknowledge_delivery(&running.id).unwrap();
        assert_eq!(acknowledged.delivery, DeliveryState::Acknowledged);
        let task_id = acknowledged.id.clone();
        let updated_at = acknowledged.updated_at_unix_ms;
        drop(ledger);
        let reopened = JsonlTaskLedger::open(directory.join("tasks.jsonl")).unwrap();
        let replayed = reopened.get(&task_id).unwrap().unwrap();
        assert_eq!(replayed.delivery, DeliveryState::Acknowledged);
        assert_eq!(replayed.updated_at_unix_ms, updated_at);
        drop(reopened);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn expired_lease_becomes_unknown_instead_of_pending() {
        let (directory, ledger) = ledger();
        ledger.submit("once", json!({})).unwrap();
        ledger.claim_next("worker", 100, 10).unwrap().unwrap();

        let recovered = ledger.recover_expired(111).unwrap();

        assert_eq!(recovered.len(), 1);
        assert_eq!(recovered[0].status, TaskStatus::Unknown);
        assert!(ledger.claim_next("other", 112, 10).unwrap().is_none());
        drop(ledger);
        fs::remove_dir_all(directory).unwrap();
    }
}
