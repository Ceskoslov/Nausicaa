use std::fs;

use agent_harness_core::{
    DirectoryRuleLoader, EventStore, JsonlEventStore, PromptLayer, RuntimeEvent, ThreadId,
};

fn test_directory(label: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "agent-harness-{label}-{}",
        ThreadId::new().as_str()
    ))
}

#[test]
fn jsonl_store_round_trips_events() {
    let directory = test_directory("jsonl");
    fs::create_dir_all(&directory).unwrap();
    let path = directory.join("events.jsonl");
    let thread = ThreadId::new();
    {
        let store = JsonlEventStore::open(&path).unwrap();
        store
            .append(thread.clone(), None, RuntimeEvent::ThreadStarted)
            .unwrap();
    }

    let reopened = JsonlEventStore::open(&path).unwrap();
    let events = reopened.load_thread(&thread).unwrap();
    assert_eq!(events.len(), 1);
    assert!(matches!(events[0].event, RuntimeEvent::ThreadStarted));
    drop(reopened);
    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn directory_rules_load_root_to_leaf_with_override_precedence() {
    let root = test_directory("rules");
    let child = root.join("services/api");
    fs::create_dir_all(&child).unwrap();
    fs::write(root.join("AGENTS.md"), "root rules").unwrap();
    fs::write(root.join("services/AGENTS.md"), "ignored normal rules").unwrap();
    fs::write(
        root.join("services/AGENTS.override.md"),
        "service override rules",
    )
    .unwrap();
    fs::write(child.join("AGENTS.md"), "api rules").unwrap();

    let segments = DirectoryRuleLoader::default().load(&root, &child).unwrap();

    assert_eq!(segments.len(), 3);
    assert!(
        segments
            .iter()
            .all(|segment| segment.layer == PromptLayer::Rules)
    );
    assert_eq!(segments[0].text, "root rules");
    assert_eq!(segments[1].text, "service override rules");
    assert_eq!(segments[2].text, "api rules");
    fs::remove_dir_all(root).unwrap();
}
