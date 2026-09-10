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
    write_failed: bool,
}

/// Append-only JSONL store. Each successful append is flushed and `sync_data`'d
/// before observers or the next model iteration can see it.
#[derive(Debug)]
pub struct JsonlEventStore {
    path: PathBuf,
    recovered_tail: Option<PathBuf>,
    state: Mutex<JsonlState>,
}

impl JsonlEventStore {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, StoreError> {
        let path = path.as_ref().to_path_buf();
        let mut file = OpenOptions::new()
            .create(true)
            .read(true)
            .append(true)
            .open(&path)
            .map_err(io_error)?;

        let mut events = Vec::new();
        let mut reader = BufReader::new(&file);
        let mut line = Vec::new();
        let mut line_number = 0;
        let mut valid_bytes = 0_u64;
        let mut torn_tail = None;
        let mut needs_newline = false;
        loop {
            line.clear();
            let count = reader.read_until(b'\n', &mut line).map_err(io_error)?;
            if count == 0 {
                break;
            }
            line_number += 1;
            let terminated = line.ends_with(b"\n");
            if !line.iter().all(u8::is_ascii_whitespace) {
                match serde_json::from_slice::<EventEnvelope>(&line) {
                    Ok(event) => events.push(event),
                    Err(error) if !terminated && error.is_eof() => {
                        torn_tail = Some(line.clone());
                        break;
                    }
                    Err(error) => {
                        return Err(StoreError::Corrupt {
                            line: line_number,
                            message: error.to_string(),
                        });
                    }
                }
            }
            valid_bytes += count as u64;
            needs_newline = !terminated;
        }
        drop(reader);

        // Only an unterminated, syntactically incomplete final record is
        // repairable. Preserve it durably before modifying the original log.
        let recovered_tail = if let Some(bytes) = torn_tail {
            let mut backup_name = path.as_os_str().to_os_string();
            backup_name.push(format!(".torn-{}", EventId::new()));
            let backup_path = PathBuf::from(backup_name);
            let mut options = OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let mut backup = options.open(&backup_path).map_err(io_error)?;
            backup.write_all(&bytes).map_err(io_error)?;
            backup.sync_all().map_err(io_error)?;
            #[cfg(unix)]
            File::open(
                path.parent()
                    .filter(|p| !p.as_os_str().is_empty())
                    .unwrap_or(Path::new(".")),
            )
            .and_then(|directory| directory.sync_all())
            .map_err(io_error)?;
            file.set_len(valid_bytes).map_err(io_error)?;
            file.sync_all().map_err(io_error)?;
            Some(backup_path)
        } else {
            // A complete JSON value whose final newline was lost is retained.
            if needs_newline {
                file.write_all(b"\n").map_err(io_error)?;
                file.sync_data().map_err(io_error)?;
            }
            None
        };
        events.sort_by_key(|event| event.sequence);
        let next_sequence = events
            .iter()
            .map(|event| event.sequence)
            .max()
            .map_or(0, |sequence| sequence + 1);

        Ok(Self {
            path,
            recovered_tail,
            state: Mutex::new(JsonlState {
                file,
                next_sequence,
                events,
                write_failed: false,
            }),
        })
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Raw incomplete tail archived during this open, if any.
    #[must_use]
    pub fn recovered_tail_path(&self) -> Option<&Path> {
        self.recovered_tail.as_deref()
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
        if state.write_failed {
            return Err(StoreError::Io(
                "a previous append failed; reopen the event store before writing".to_owned(),
            ));
        }
        let envelope = make_envelope(state.next_sequence, thread_id, turn_id, event);
        let encoded = serde_json::to_vec(&envelope)
            .map_err(|error| StoreError::Serialization(error.to_string()))?;
        // Any I/O failure may leave a partial record. Do not append behind it.
        state.write_failed = true;
        state.file.write_all(&encoded).map_err(io_error)?;
        state.file.write_all(b"\n").map_err(io_error)?;
        state.file.flush().map_err(io_error)?;
        state.file.sync_data().map_err(io_error)?;
        state.write_failed = false;
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
