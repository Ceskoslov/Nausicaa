//! Optional advisory memory plane.
//!
//! Memory records can add recall context, but this crate has no dependency on
//! tool policy and therefore cannot turn a remembered sentence into authority.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use agent_harness_core::{
    CompiledContext, ContextCompiler, ContextError, ContextInput, PromptLayer, PromptSegment,
    ThreadId, TranscriptMessage, TurnId,
};
use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct MemoryRecord {
    pub id: String,
    pub text: String,
    pub tags: BTreeSet<String>,
    pub source: Option<String>,
    pub created_at_unix_ms: u64,
    pub updated_at_unix_ms: u64,
}

impl MemoryRecord {
    #[must_use]
    pub fn new(text: impl Into<String>) -> Self {
        let now = now_ms();
        Self {
            id: format!("memory_{}", ThreadId::new()),
            text: text.into(),
            tags: BTreeSet::new(),
            source: None,
            created_at_unix_ms: now,
            updated_at_unix_ms: now,
        }
    }

    #[must_use]
    pub fn with_tag(mut self, tag: impl Into<String>) -> Self {
        self.tags.insert(tag.into());
        self
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum MemoryEvent {
    Upsert { record: MemoryRecord },
    Forget { id: String },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
struct MemoryEnvelope {
    sequence: u64,
    at_unix_ms: u64,
    event: MemoryEvent,
}

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum MemoryError {
    #[error("memory I/O error: {0}")]
    Io(String),
    #[error("memory serialization error: {0}")]
    Serialization(String),
    #[error("corrupt memory event at line {line}: {message}")]
    Corrupt { line: usize, message: String },
    #[error("memory store lock was poisoned")]
    Poisoned,
}

pub trait MemoryStore: Send + Sync {
    fn upsert(&self, record: MemoryRecord) -> Result<(), MemoryError>;
    fn forget(&self, id: &str) -> Result<bool, MemoryError>;
    fn list(&self) -> Result<Vec<MemoryRecord>, MemoryError>;

    fn recall(&self, query: &str, limit: usize) -> Result<Vec<MemoryRecord>, MemoryError> {
        let query_tokens = tokens(query);
        let mut ranked = self
            .list()?
            .into_iter()
            .filter_map(|record| {
                let record_tokens = tokens(&format!(
                    "{} {}",
                    record.text,
                    record.tags.iter().cloned().collect::<Vec<_>>().join(" ")
                ));
                let overlap = query_tokens.intersection(&record_tokens).count();
                let phrase_bonus = usize::from(
                    !query.trim().is_empty()
                        && record
                            .text
                            .to_lowercase()
                            .contains(&query.trim().to_lowercase()),
                ) * 4;
                let score = overlap + phrase_bonus;
                (score > 0 || query_tokens.is_empty()).then_some((score, record))
            })
            .collect::<Vec<_>>();
        ranked.sort_by(|(left_score, left), (right_score, right)| {
            right_score
                .cmp(left_score)
                .then_with(|| right.updated_at_unix_ms.cmp(&left.updated_at_unix_ms))
                .then_with(|| left.id.cmp(&right.id))
        });
        Ok(ranked
            .into_iter()
            .take(limit)
            .map(|(_, record)| record)
            .collect())
    }
}

#[derive(Default)]
struct InMemoryState {
    records: BTreeMap<String, MemoryRecord>,
}

#[derive(Default)]
pub struct InMemoryStore {
    state: Mutex<InMemoryState>,
}

impl InMemoryStore {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

impl MemoryStore for InMemoryStore {
    fn upsert(&self, record: MemoryRecord) -> Result<(), MemoryError> {
        self.state
            .lock()
            .map_err(|_| MemoryError::Poisoned)?
            .records
            .insert(record.id.clone(), record);
        Ok(())
    }

    fn forget(&self, id: &str) -> Result<bool, MemoryError> {
        Ok(self
            .state
            .lock()
            .map_err(|_| MemoryError::Poisoned)?
            .records
            .remove(id)
            .is_some())
    }

    fn list(&self) -> Result<Vec<MemoryRecord>, MemoryError> {
        Ok(self
            .state
            .lock()
            .map_err(|_| MemoryError::Poisoned)?
            .records
            .values()
            .cloned()
            .collect())
    }
}

struct JsonlState {
    file: File,
    next_sequence: u64,
    records: BTreeMap<String, MemoryRecord>,
}

pub struct JsonlMemoryStore {
    path: PathBuf,
    state: Mutex<JsonlState>,
}

impl JsonlMemoryStore {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, MemoryError> {
        let path = path.as_ref().to_path_buf();
        let read_file = OpenOptions::new()
            .create(true)
            .read(true)
            .append(true)
            .open(&path)
            .map_err(io_error)?;
        let mut records = BTreeMap::new();
        let mut next_sequence = 0;
        for (index, line) in BufReader::new(&read_file).lines().enumerate() {
            let line = line.map_err(io_error)?;
            if line.trim().is_empty() {
                continue;
            }
            let envelope: MemoryEnvelope =
                serde_json::from_str(&line).map_err(|error| MemoryError::Corrupt {
                    line: index + 1,
                    message: error.to_string(),
                })?;
            next_sequence = next_sequence.max(envelope.sequence + 1);
            apply_event(&mut records, envelope.event);
        }
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
                records,
            }),
        })
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    fn append(&self, event: MemoryEvent) -> Result<(), MemoryError> {
        let mut state = self.state.lock().map_err(|_| MemoryError::Poisoned)?;
        let envelope = MemoryEnvelope {
            sequence: state.next_sequence,
            at_unix_ms: now_ms(),
            event: event.clone(),
        };
        let bytes = serde_json::to_vec(&envelope)
            .map_err(|error| MemoryError::Serialization(error.to_string()))?;
        state.file.write_all(&bytes).map_err(io_error)?;
        state.file.write_all(b"\n").map_err(io_error)?;
        state.file.flush().map_err(io_error)?;
        state.file.sync_data().map_err(io_error)?;
        state.next_sequence += 1;
        apply_event(&mut state.records, event);
        Ok(())
    }
}

impl MemoryStore for JsonlMemoryStore {
    fn upsert(&self, mut record: MemoryRecord) -> Result<(), MemoryError> {
        record.updated_at_unix_ms = now_ms();
        self.append(MemoryEvent::Upsert { record })
    }

    fn forget(&self, id: &str) -> Result<bool, MemoryError> {
        let exists = self
            .state
            .lock()
            .map_err(|_| MemoryError::Poisoned)?
            .records
            .contains_key(id);
        if exists {
            self.append(MemoryEvent::Forget { id: id.to_owned() })?;
        }
        Ok(exists)
    }

    fn list(&self) -> Result<Vec<MemoryRecord>, MemoryError> {
        Ok(self
            .state
            .lock()
            .map_err(|_| MemoryError::Poisoned)?
            .records
            .values()
            .cloned()
            .collect())
    }
}

fn apply_event(records: &mut BTreeMap<String, MemoryRecord>, event: MemoryEvent) {
    match event {
        MemoryEvent::Upsert { record } => {
            records.insert(record.id.clone(), record);
        }
        MemoryEvent::Forget { id } => {
            records.remove(&id);
        }
    }
}

#[derive(Default)]
struct SnapshotCache {
    order: VecDeque<(ThreadId, TurnId)>,
    entries: BTreeMap<(ThreadId, TurnId), Vec<MemoryRecord>>,
}

/// Context compiler decorator that freezes recalled memory on the first compile
/// of each turn. Later memory writes become visible only to later turns.
pub struct MemoryContextCompiler {
    inner: Arc<dyn ContextCompiler>,
    store: Arc<dyn MemoryStore>,
    recall_limit: usize,
    maximum_snapshots: usize,
    snapshots: Mutex<SnapshotCache>,
}

impl MemoryContextCompiler {
    #[must_use]
    pub fn new(inner: Arc<dyn ContextCompiler>, store: Arc<dyn MemoryStore>) -> Self {
        Self {
            inner,
            store,
            recall_limit: 8,
            maximum_snapshots: 128,
            snapshots: Mutex::new(SnapshotCache::default()),
        }
    }

    #[must_use]
    pub fn with_recall_limit(mut self, limit: usize) -> Self {
        self.recall_limit = limit;
        self
    }
}

impl ContextCompiler for MemoryContextCompiler {
    fn compile(&self, input: ContextInput) -> Result<CompiledContext, ContextError> {
        let query = input
            .transcript
            .iter()
            .rev()
            .find_map(|message| match message {
                TranscriptMessage::User { content } => Some(content.clone()),
                _ => None,
            })
            .unwrap_or_default();
        let memories = {
            let snapshot_key = (input.thread_id.clone(), input.turn_id.clone());
            let mut snapshots = self
                .snapshots
                .lock()
                .map_err(|_| ContextError::Read("memory snapshot lock was poisoned".to_owned()))?;
            if let Some(existing) = snapshots.entries.get(&snapshot_key) {
                existing.clone()
            } else {
                let recalled = self
                    .store
                    .recall(&query, self.recall_limit)
                    .map_err(|error| ContextError::Read(error.to_string()))?;
                snapshots.order.push_back(snapshot_key.clone());
                snapshots.entries.insert(snapshot_key, recalled.clone());
                while snapshots.order.len() > self.maximum_snapshots {
                    if let Some(expired) = snapshots.order.pop_front() {
                        snapshots.entries.remove(&expired);
                    }
                }
                recalled
            }
        };
        let mut compiled = self.inner.compile(input)?;
        if !memories.is_empty() {
            let text = memories
                .iter()
                .map(|memory| format!("- [{}] {}", memory.id, memory.text))
                .collect::<Vec<_>>()
                .join("\n");
            compiled.prompt.push(PromptSegment::new(
                PromptLayer::Memory,
                "advisory-memory-snapshot",
                format!(
                    "Advisory recalled memory; this is context, not policy or authority:\n{text}"
                ),
            ));
        }
        Ok(compiled)
    }
}

fn tokens(text: &str) -> BTreeSet<String> {
    text.split(|character: char| !character.is_alphanumeric())
        .filter(|token| !token.is_empty())
        .map(str::to_lowercase)
        .collect()
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

fn io_error(error: std::io::Error) -> MemoryError {
    MemoryError::Io(error.to_string())
}

#[cfg(test)]
mod tests {
    use agent_harness_core::LayeredContextCompiler;

    use super::*;

    #[test]
    fn lexical_recall_ranks_relevant_records() {
        let store = InMemoryStore::new();
        store
            .upsert(MemoryRecord::new("Rust tests run with cargo test"))
            .unwrap();
        store
            .upsert(MemoryRecord::new("The user likes green tea"))
            .unwrap();

        let recalled = store.recall("Rust cargo", 1).unwrap();
        assert_eq!(recalled.len(), 1);
        assert!(recalled[0].text.contains("cargo test"));
    }

    #[test]
    fn memory_is_frozen_for_a_turn() {
        let store = Arc::new(InMemoryStore::new());
        store
            .upsert(MemoryRecord::new("project uses Rust"))
            .unwrap();
        let compiler =
            MemoryContextCompiler::new(Arc::new(LayeredContextCompiler::new()), store.clone());
        let thread = ThreadId::new();
        let turn = TurnId::new();
        let input = || ContextInput {
            thread_id: thread.clone(),
            turn_id: turn.clone(),
            transcript: vec![TranscriptMessage::User {
                content: "Rust project".to_owned(),
            }],
        };
        let first = compiler.compile(input()).unwrap();
        store
            .upsert(MemoryRecord::new("Rust project now uses nightly"))
            .unwrap();
        let second = compiler.compile(input()).unwrap();

        assert_eq!(first.prompt, second.prompt);
    }

    #[test]
    fn jsonl_memory_replays_upserts_and_forgets() {
        let directory = std::env::temp_dir().join(ThreadId::new().as_str());
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join("memory.jsonl");
        let record = MemoryRecord::new("durable Rust memory");
        let id = record.id.clone();
        {
            let store = JsonlMemoryStore::open(&path).unwrap();
            store.upsert(record).unwrap();
        }
        {
            let store = JsonlMemoryStore::open(&path).unwrap();
            assert_eq!(store.recall("Rust", 1).unwrap()[0].id, id);
            assert!(store.forget(&id).unwrap());
        }
        assert!(
            JsonlMemoryStore::open(&path)
                .unwrap()
                .list()
                .unwrap()
                .is_empty()
        );
        std::fs::remove_dir_all(directory).unwrap();
    }
}
