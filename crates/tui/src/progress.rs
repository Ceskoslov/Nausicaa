use agent_harness_core::{ModelProgress, ModelProgressEvent, ModelProgressObserver};
use std::collections::VecDeque;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::sync::Mutex;

/// Coalesces previews instead of queuing every token. Diagnostics are separate
/// from the runtime journal and never store text deltas or request bodies.
#[derive(Default)]
pub struct ModelProgressBuffer {
    state: Mutex<Snapshot>,
    metrics: Mutex<Option<File>>,
}
#[derive(Default)]
pub(crate) struct Snapshot {
    pub draft: Option<ModelProgressEvent>,
    pub completed: VecDeque<ModelProgressEvent>,
    pub error: Option<String>,
}
impl ModelProgressBuffer {
    pub fn open(path: impl AsRef<Path>) -> std::io::Result<Self> {
        Ok(Self {
            metrics: Mutex::new(Some(
                OpenOptions::new().create(true).append(true).open(path)?,
            )),
            ..Self::default()
        })
    }
    pub(crate) fn take(&self) -> Snapshot {
        std::mem::take(&mut *self.state.lock().unwrap_or_else(|e| e.into_inner()))
    }
}
impl ModelProgressObserver for ModelProgressBuffer {
    fn on_progress(&self, event: &ModelProgressEvent) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        match &event.progress {
            ModelProgress::Started => state.draft = Some(event.clone()),
            ModelProgress::TextDelta { text } => {
                let existing = state.draft.as_mut().filter(|old| {
                    old.turn_id == event.turn_id
                        && old.iteration == event.iteration
                        && old.thread_id == event.thread_id
                });
                if let Some(ModelProgressEvent {
                    progress: ModelProgress::TextDelta { text: previous },
                    ..
                }) = existing
                {
                    append_bounded(previous, text);
                } else {
                    let mut event = event.clone();
                    if let ModelProgress::TextDelta { text } = &mut event.progress {
                        truncate(text);
                    }
                    state.draft = Some(event);
                }
            }
            ModelProgress::Finished { .. } => {
                if let Some(file) = &mut *self.metrics.lock().unwrap_or_else(|e| e.into_inner()) {
                    let record = serde_json::json!({"version":1,"request":event});
                    if let Err(error) = writeln!(file, "{record}") {
                        state.error = Some(format!("request metrics write failed: {error}"));
                    }
                }
                state.completed.push_back(event.clone());
                while state.completed.len() > 16 {
                    state.completed.pop_front();
                }
            }
        }
    }
}
pub(crate) fn append_bounded(previous: &mut String, text: &str) {
    previous.push_str(text);
    truncate(previous);
}
fn truncate(text: &mut String) {
    if text.len() > 65536 {
        let mut start = text.len() - 65536;
        while !text.is_char_boundary(start) {
            start += 1;
        }
        text.drain(..start);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_harness_core::{RequestTimings, ThreadId, TurnId};
    #[test]
    fn previews_are_bounded_and_metrics_never_store_text() {
        let path = std::env::temp_dir().join(format!("nausicaa-metrics-{}", TurnId::new()));
        let buffer = ModelProgressBuffer::open(&path).unwrap();
        let mut event = ModelProgressEvent {
            thread_id: ThreadId::new(),
            turn_id: TurnId::new(),
            iteration: 0,
            progress: ModelProgress::Started,
        };
        buffer.on_progress(&event);
        for _ in 0..100 {
            event.progress = ModelProgress::TextDelta {
                text: "秘密".repeat(1024),
            };
            buffer.on_progress(&event);
        }
        let snapshot = buffer.take();
        if let ModelProgress::TextDelta { text } = snapshot.draft.unwrap().progress {
            assert!(text.len() <= 65536);
            assert!(text.ends_with("秘密"));
        } else {
            panic!("missing draft");
        }
        event.progress = ModelProgress::Finished {
            timings: RequestTimings::default(),
            outcome: "cancelled".into(),
        };
        buffer.on_progress(&event);
        let file = std::fs::read_to_string(&path).unwrap();
        assert!(!file.contains("秘密"));
        let json: serde_json::Value = serde_json::from_str(file.trim()).unwrap();
        assert_eq!(json["version"], 1);
        assert_eq!(json["request"]["progress"]["outcome"], "cancelled");
        drop(buffer);
        std::fs::remove_file(path).unwrap();
    }
}
