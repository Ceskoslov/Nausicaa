use agent_harness_core::{CallId, ToolReceipt};
use agent_harness_task::{ArtifactRef, Budget, Criterion, TaskJournal, TaskSpec, TaskStatus};

struct Fixture(std::path::PathBuf);
impl Fixture {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("nausicaa-task-{}", CallId::new()));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }
    fn log(&self) -> std::path::PathBuf {
        self.0.join("task.jsonl")
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).unwrap();
    }
}
fn spec(max_attempts: u32) -> TaskSpec {
    TaskSpec {
        objective: "Fix addition".into(),
        criteria: vec![Criterion {
            id: "addition".into(),
            artifact_id: "lib.rs".into(),
            expected: "a + b".into(),
        }],
        budget: Budget { max_attempts },
    }
}
fn artifacts(content: &str) -> Vec<ArtifactRef> {
    vec![ArtifactRef {
        id: "lib.rs".into(),
        content: content.into(),
    }]
}

#[test]
fn failure_repair_and_success_survive_replay() {
    let fixture = Fixture::new();
    let mut task = TaskJournal::create(fixture.log(), spec(2)).unwrap();
    task.start_attempt("turn-1").unwrap();
    task.verify(artifacts("a - b"), vec![]).unwrap();
    assert_eq!(task.state().status, TaskStatus::NeedsRepair);
    assert!(task.state().remaining_work[0].contains("addition"));
    let snapshot = task.state().clone();
    drop(task);
    let mut task = TaskJournal::open(fixture.log()).unwrap();
    assert_eq!(task.state(), &snapshot);
    assert!(task.start_attempt("turn-1").is_err());
    task.start_attempt("turn-2").unwrap();
    task.verify(artifacts("a + b"), vec![]).unwrap();
    assert_eq!(task.state().status, TaskStatus::Succeeded);
    assert_eq!(task.state().evidence[1].artifacts[0].content, "a + b");
    assert!(task.start_attempt("turn-3").is_err());
    assert!(task.block("overwrite success").is_err());
    let snapshot = task.state().clone();
    drop(task);
    assert_eq!(TaskJournal::open(fixture.log()).unwrap().state(), &snapshot);
}

#[test]
fn missing_duplicate_and_failing_artifacts_cannot_complete() {
    for input in [
        vec![],
        artifacts("wrong"),
        [artifacts("a + b"), artifacts("a + b")].concat(),
    ] {
        let fixture = Fixture::new();
        let mut task = TaskJournal::create(fixture.log(), spec(1)).unwrap();
        assert!(task.verify(input.clone(), vec![]).is_err());
        task.start_attempt("turn-1").unwrap();
        task.verify(input, vec![]).unwrap();
        assert_eq!(task.state().status, TaskStatus::BudgetExhausted);
        assert!(task.start_attempt("turn-2").is_err());
    }
}

#[test]
fn unknown_blocks_and_denied_receipts_are_retained() {
    for unknown in [false, true] {
        let fixture = Fixture::new();
        let mut task = TaskJournal::create(fixture.log(), spec(2)).unwrap();
        task.start_attempt("turn-1").unwrap();
        let mut receipt = ToolReceipt::denied(CallId::new(), "shell", None, "policy denied");
        if unknown {
            receipt.status = agent_harness_core::ReceiptStatus::Unknown;
        }
        task.verify(artifacts("a + b"), vec![receipt.clone()])
            .unwrap();
        assert_eq!(
            task.state().status,
            if unknown {
                TaskStatus::Blocked
            } else {
                TaskStatus::Succeeded
            }
        );
        assert_eq!(task.state().evidence[0].receipts, vec![receipt]);
        assert!(task.start_attempt("retry").is_err());
        let snapshot = task.state().clone();
        drop(task);
        assert_eq!(TaskJournal::open(fixture.log()).unwrap().state(), &snapshot);
    }
}

#[test]
fn interrupted_attempt_requires_host_resolution() {
    let fixture = Fixture::new();
    let mut task = TaskJournal::create(fixture.log(), spec(2)).unwrap();
    task.start_attempt("turn-1").unwrap();
    drop(task);
    let mut task = TaskJournal::open(fixture.log()).unwrap();
    assert_eq!(task.state().attempts, 1);
    assert_eq!(task.state().status, TaskStatus::Running);
    assert!(task.start_attempt("turn-2").is_err());
    task.block("host must reconcile interrupted turn").unwrap();
    assert_eq!(task.state().status, TaskStatus::Blocked);
}

#[test]
fn invalid_specs_and_corrupt_journals_fail_closed() {
    let fixture = Fixture::new();
    for invalid in [
        spec(0),
        TaskSpec {
            criteria: vec![],
            ..spec(1)
        },
        TaskSpec {
            criteria: vec![spec(1).criteria[0].clone(); 2],
            ..spec(1)
        },
    ] {
        assert!(TaskJournal::create(fixture.log(), invalid).is_err());
        assert!(!fixture.log().exists());
    }
    drop(TaskJournal::create(fixture.log(), spec(1)).unwrap());
    let valid = std::fs::read_to_string(fixture.log()).unwrap();
    for corrupted in [
        valid.trim_end().to_owned(),
        valid.replace("\"version\":1", "\"version\":2"),
        valid.replace("\"sequence\":0", "\"sequence\":1"),
        format!("{valid}{valid}"),
    ] {
        std::fs::write(fixture.log(), &corrupted).unwrap();
        assert!(TaskJournal::open(fixture.log()).is_err());
        assert_eq!(std::fs::read_to_string(fixture.log()).unwrap(), corrupted);
    }
}

#[test]
fn task_contract_survives_compaction_without_changing_call_receipt_groups() {
    use agent_harness_context_fs::{FsContextCompiler, FsContextConfig};
    use agent_harness_core::{
        ContextCompiler, ContextInput, ThreadId, ToolCall, TranscriptMessage, TurnId,
    };
    use agent_harness_task::TaskContextCompiler;
    use std::sync::Arc;
    let fixture = Fixture::new();
    let mut config = FsContextConfig::new(&fixture.0, &fixture.0);
    config.max_transcript_groups = Some(1);
    let original = spec(2);
    let compiler =
        TaskContextCompiler::new(Arc::new(FsContextCompiler::new(config)), &original).unwrap();
    let call = ToolCall::new("hidden", serde_json::json!({}));
    let receipt = ToolReceipt::denied(call.id.clone(), "hidden", None, "denied");
    let group = vec![
        TranscriptMessage::Assistant {
            content: String::new(),
            tool_calls: vec![call],
        },
        TranscriptMessage::Tool { receipt },
    ];
    let mut transcript = vec![TranscriptMessage::User {
        content: "old objective that will be dropped".repeat(1000),
    }];
    transcript.extend(group.clone());
    let context = compiler
        .compile(ContextInput {
            thread_id: ThreadId::new(),
            turn_id: TurnId::new(),
            transcript,
        })
        .unwrap();
    assert_eq!(context.messages, group);
    let pinned = context
        .prompt
        .iter()
        .find(|p| p.name == "task-contract-v1")
        .unwrap();
    let text = pinned.text.split_once('\n').unwrap().1;
    assert_eq!(serde_json::from_str::<TaskSpec>(text).unwrap(), original);
    assert!(
        !context
            .messages
            .iter()
            .any(|m| matches!(m, TranscriptMessage::User { .. }))
    );
}
