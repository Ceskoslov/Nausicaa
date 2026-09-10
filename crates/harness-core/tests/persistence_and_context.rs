use std::fs;
use std::io::Write;

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
fn incomplete_tail_is_archived_and_new_events_remain_replayable() {
    let directory = test_directory("torn-tail");
    fs::create_dir_all(&directory).unwrap();
    let path = directory.join("events.jsonl");
    let thread = ThreadId::new();
    let store = JsonlEventStore::open(&path).unwrap();
    let first = store
        .append(thread.clone(), None, RuntimeEvent::ThreadStarted)
        .unwrap();
    drop(store);
    let torn = b"{\"id\":\"partial";
    fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap()
        .write_all(torn)
        .unwrap();
    let store = JsonlEventStore::open(&path).unwrap();
    assert_eq!(
        fs::read(store.recovered_tail_path().unwrap()).unwrap(),
        torn
    );
    assert_eq!(store.load_thread(&thread).unwrap(), vec![first]);
    let second = store
        .append(thread.clone(), None, RuntimeEvent::TurnCancelled)
        .unwrap();
    assert_eq!(second.sequence, 1);
    drop(store);
    let store = JsonlEventStore::open(&path).unwrap();
    assert!(store.recovered_tail_path().is_none());
    assert_eq!(store.load_thread(&thread).unwrap().len(), 2);
    drop(store);
    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn complete_record_without_newline_is_preserved_before_append() {
    let directory = test_directory("lost-newline");
    fs::create_dir_all(&directory).unwrap();
    let path = directory.join("events.jsonl");
    let thread = ThreadId::new();
    let store = JsonlEventStore::open(&path).unwrap();
    store
        .append(thread.clone(), None, RuntimeEvent::ThreadStarted)
        .unwrap();
    drop(store);
    let mut bytes = fs::read(&path).unwrap();
    assert_eq!(bytes.pop(), Some(b'\n'));
    fs::write(&path, bytes).unwrap();
    let store = JsonlEventStore::open(&path).unwrap();
    assert!(store.recovered_tail_path().is_none());
    store
        .append(thread.clone(), None, RuntimeEvent::TurnCancelled)
        .unwrap();
    drop(store);
    let store = JsonlEventStore::open(&path).unwrap();
    assert_eq!(store.load_thread(&thread).unwrap().len(), 2);
    drop(store);
    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn corrupt_records_are_rejected_without_changing_the_log() {
    let directory = test_directory("corrupt");
    fs::create_dir_all(&directory).unwrap();
    let path = directory.join("events.jsonl");
    for bytes in [
        b"{\"id\":\n".as_slice(),
        b"not-json",
        b"{\"id\":\n{}\n",
        b"{}",
    ] {
        fs::write(&path, bytes).unwrap();
        assert!(matches!(
            JsonlEventStore::open(&path),
            Err(agent_harness_core::StoreError::Corrupt { .. })
        ));
        assert_eq!(fs::read(&path).unwrap(), bytes);
    }
    assert_eq!(fs::read_dir(&directory).unwrap().count(), 1);
    fs::remove_dir_all(directory).unwrap();
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
