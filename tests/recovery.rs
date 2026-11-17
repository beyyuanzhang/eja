mod support;

use eja::{recover, EventMeta, Kind, State, TaskStatus};
use std::io::Write;
use support::{turn_at, State as Fixture};

#[test]
fn interrupted_assignment_is_reconciled_without_duplicate_dispatch() {
    let fixture = Fixture::new("recovery");
    let store = fixture.store();
    let assignment = eja::Assignment {
        task_id: 1,
        objective: "do it".to_string(),
        issued_at: 100,
        workspace: String::new(),
        acceptance: String::new(),
        input: String::new(),
        resume: None,
    };
    store
        .append(Kind::User, "eja", "run", 1, EventMeta::new(100))
        .expect("user");
    store
        .append(
            Kind::TaskOpened,
            "task:1",
            "interrupted",
            1,
            EventMeta::data(
                100,
                serde_json::json!({
                "title": "interrupted",
                "request": "do it",
                "origin": "do it",
                "acceptance": "done",
                        }),
            ),
        )
        .expect("open");
    store
        .append(
            Kind::TaskAssigned,
            "task:1",
            "",
            1,
            EventMeta::data(100, serde_json::to_value(&assignment).expect("assignment")),
        )
        .expect("assign");

    let recovered = recover(&store, 2, 200).expect("recover");
    assert_eq!(recovered, vec![1]);
    let events = store.events().expect("events");
    assert_eq!(
        events
            .iter()
            .filter(|event| event.kind == Kind::TaskAssigned)
            .count(),
        1
    );
    let task = State::fold(events).tasks()[0].clone();
    assert_eq!(task.status, TaskStatus::Waiting);
    assert!(
        task.digest().unwrap_or("").contains("interrupted"),
        "{:?}",
        task.run
    );
    assert!(recover(&store, 3, 300).expect("idempotent").is_empty());

    turn_at(&store, "continue", vec![]);
}

#[test]
fn torn_tail_is_ignored_by_the_reader() {
    let fixture = Fixture::new("recovery-torn");
    let store = fixture.store();
    store
        .append(Kind::User, "eja", "one", 1, EventMeta::new(1))
        .expect("append");
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(&store.log_path)
        .expect("open");
    file.write_all(b"{\"id\":").expect("torn");
    let events = store.events().expect("events");
    assert_eq!(events.len(), 1);
}
