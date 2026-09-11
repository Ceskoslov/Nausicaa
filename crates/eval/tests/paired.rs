use agent_harness_core::{CallId, RuntimeEvent};
use agent_harness_eval::{Mode, fixtures, run_scripted};
use agent_harness_task::{TaskJournal, TaskStatus};

#[test]
fn paired_fixtures_measure_false_completion_repair_and_authority() {
    let root = std::env::temp_dir().join(format!("nausicaa-eval-{}", CallId::new()));
    std::fs::create_dir(&root).unwrap();
    struct Cleanup(std::path::PathBuf);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.0).unwrap();
        }
    }
    let _cleanup = Cleanup(root.clone());
    let mut core_false = 0;
    let mut task_false = 0;
    for fixture in fixtures().unwrap() {
        let core = run_scripted(
            &fixture,
            Mode::CoreOnly,
            &root.join(format!("{}-core", fixture.id)),
        )
        .unwrap();
        let task_dir = root.join(format!("{}-task", fixture.id));
        let task = run_scripted(&fixture, Mode::Task, &task_dir).unwrap();
        core_false += usize::from(core.false_completion);
        task_false += usize::from(task.false_completion);
        assert!(!task.live_model);
        assert_eq!(task.tokens.input_tokens, 0);
        assert!(task.model_iterations <= fixture.max_attempts as usize * 4);
        let replay = TaskJournal::open(task_dir.join("task.jsonl")).unwrap();
        assert_eq!(Some(&replay.state().status), task.task_status.as_ref());
        assert_eq!(replay.state().attempts, task.attempts);
        match fixture.id.as_str() {
            "repair" => {
                assert!(core.false_completion);
                assert!(task.reported_success && task.verified_success);
                assert_eq!(task.attempts, 2);
                assert_eq!(task.model_iterations, 4);
                assert!(!replay.state().evidence[0].checks[0].passed);
                assert!(replay.state().evidence[1].checks[0].passed);
                let events: Vec<agent_harness_core::EventEnvelope> =
                    std::fs::read_to_string(task_dir.join("events.jsonl"))
                        .unwrap()
                        .lines()
                        .map(|s| serde_json::from_str(s).unwrap())
                        .collect();
                assert!(events.iter().any(|e| matches!(&e.event, RuntimeEvent::UserMessage { content } if content.contains("Verification feedback:"))));
            }
            "exhausted" | "denied" => {
                assert_eq!(task.task_status, Some(TaskStatus::BudgetExhausted))
            }
            "identity" => assert!(core.verified_success && task.verified_success),
            "unknown" => {
                assert_eq!(task.task_status, Some(TaskStatus::Blocked));
                assert_eq!(task.unknown_receipts, 1);
                assert_eq!(core.unknown_receipts, 1);
                assert_eq!(task.attempts, 1);
                assert_eq!(task.interventions, 1);
                assert_eq!(
                    std::fs::read_to_string(task_dir.join("workspace/lib.rs")).unwrap(),
                    fixture.initial
                );
            }
            _ => panic!("fixture needs expectations"),
        }
        if fixture.deny_write {
            assert_eq!(task.denied_receipts, 2);
            assert_eq!(core.denied_receipts, 1);
            assert!(
                replay
                    .state()
                    .evidence
                    .iter()
                    .all(|e| e.receipts.len() == 1)
            );
        }
        assert!(run_scripted(&fixture, Mode::Task, &task_dir).is_err());
    }
    assert_eq!(core_false, 3);
    assert_eq!(task_false, 0);
}
