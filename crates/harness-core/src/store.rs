use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use thiserror::Error;

use crate::event::{EventEnvelope, RuntimeEvent};
use crate::id::{EventId, ThreadId, TurnId};

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum StoreError {
    #[error("event store I/O error: {0}")]
    Io(String),
    #[error("event serialization error: {0}")]
    Serialization(String),
    #[error("corrupt event at line {line}: {message}")]
    Corrupt { line: usize, message: String },
    #[error("event store lock was poisoned")]
    Poisoned,
}

pub trait EventStore: Send + Sync {
    /// Append must not return success until the event is durable according to
    /// the store's contract.
    fn append(
        &self,
        thread_id: ThreadId,
        turn_id: Option<TurnId>,
        event: RuntimeEvent,
    ) -> Result<EventEnvelope, StoreError>;

    fn load_thread(&self, thread_id: &ThreadId) -> Result<Vec<EventEnvelope>, StoreError>;
}

#[derive(Debug, Default)]
struct MemoryState {
    next_sequence: u64,
    events: Vec<EventEnvelope>,
}

#[derive(Debug, Default)]
pub struct MemoryEventStore {
    state: Mutex<MemoryState>,
}

impl MemoryEventStore {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn all_events(&self) -> Result<Vec<EventEnvelope>, StoreError> {
        self.state
            .lock()
            .map(|state| state.events.clone())
            .map_err(|_| StoreError::Poisoned)
    }
}

impl EventStore for MemoryEventStore {
    fn append(
        &self,
        thread_id: ThreadId,
        turn_id: Option<TurnId>,
        event: RuntimeEvent,
    ) -> Result<EventEnvelope, StoreError> {
        let mut state = self.state.lock().map_err(|_| StoreError::Poisoned)?;
        let envelope = make_envelope(state.next_sequence, thread_id, turn_id, event);
        state.next_sequence += 1;
        state.events.push(envelope.clone());
        Ok(envelope)
    }

    fn load_thread(&self, thread_id: &ThreadId) -> Result<Vec<EventEnvelope>, StoreError> {
        let state = self.state.lock().map_err(|_| StoreError::Poisoned)?;
        Ok(state
            .events
            .iter()
            .filter(|event| &event.thread_id == thread_id)
            .cloned()
            .collect())
    }
}

#[derive(Debug)]
struct JsonlState {
    file: File,
    next_sequence: u64,
    events: Vec<EventEnvelope>,
}

/// Append-only JSONL store. Each successful append is flushed and `sync_data`'d
/// before observers or the next model iteration can see it.
#[derive(Debug)]
pub struct JsonlEventStore {
    path: PathBuf,
    state: Mutex<JsonlState>,
}

impl JsonlEventStore {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, StoreError> {
        let path = path.as_ref().to_path_buf();
        let read_file = OpenOptions::new()
            .create(true)
            .read(true)
            .append(true)
            .open(&path)
            .map_err(io_error)?;

        let mut events = Vec::new();
        for (index, line) in BufReader::new(&read_file).lines().enumerate() {
            let line = line.map_err(io_error)?;
            if line.trim().is_empty() {
                continue;
            }
            let event = serde_json::from_str::<EventEnvelope>(&line).map_err(|error| {
                StoreError::Corrupt {
                    line: index + 1,
                    message: error.to_string(),
                }
            })?;
            events.push(event);
        }
        events.sort_by_key(|event| event.sequence);
        let next_sequence = events
            .iter()
            .map(|event| event.sequence)
            .max()
            .map_or(0, |sequence| sequence + 1);

        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(io_error)?;

        Ok(Self {
            path,
            state: Mutex::new(JsonlState {
                file,
                next_sequence,
                events,
            }),
        })
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl EventStore for JsonlEventStore {
    fn append(
        &self,
        thread_id: ThreadId,
        turn_id: Option<TurnId>,
        event: RuntimeEvent,
    ) -> Result<EventEnvelope, StoreError> {
        let mut state = self.state.lock().map_err(|_| StoreError::Poisoned)?;
        let envelope = make_envelope(state.next_sequence, thread_id, turn_id, event);
        let encoded = serde_json::to_vec(&envelope)
            .map_err(|error| StoreError::Serialization(error.to_string()))?;
        state.file.write_all(&encoded).map_err(io_error)?;
        state.file.write_all(b"\n").map_err(io_error)?;
        state.file.flush().map_err(io_error)?;
        state.file.sync_data().map_err(io_error)?;
        state.next_sequence += 1;
        state.events.push(envelope.clone());
        Ok(envelope)
    }

    fn load_thread(&self, thread_id: &ThreadId) -> Result<Vec<EventEnvelope>, StoreError> {
        let state = self.state.lock().map_err(|_| StoreError::Poisoned)?;
        Ok(state
            .events
            .iter()
            .filter(|event| &event.thread_id == thread_id)
            .cloned()
            .collect())
    }
}

fn make_envelope(
    sequence: u64,
    thread_id: ThreadId,
    turn_id: Option<TurnId>,
    event: RuntimeEvent,
) -> EventEnvelope {
    let at_unix_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    EventEnvelope {
        id: EventId::new(),
        sequence,
        at_unix_ms,
        thread_id,
        turn_id,
        event,
    }
}

fn io_error(error: std::io::Error) -> StoreError {
    StoreError::Io(error.to_string())
}
