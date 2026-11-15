use eja::adapters::Ports;
use eja::{
    turn_at, Action, Assignment, Brain, Decision, Edge, EjaContext, RelationKind, Store, Worker,
};
use std::collections::VecDeque;
use std::sync::Mutex;

struct QueueBrain {
    queue: Mutex<VecDeque<Decision>>,
}

impl Brain for QueueBrain {
    fn decide(&self, _context: &EjaContext<'_>, _: &eja::Control) -> Decision {
        self.queue
            .lock()
            .expect("brain")
            .pop_front()
            .unwrap_or_else(|| Decision {
                reply: String::new(),
                actions: Vec::new(),
                refused: Vec::new(),
            })
    }
}

struct DoneWorker;

impl Worker for DoneWorker {
    fn execute(&self, _assignment: &Assignment, _: &eja::Control) -> String {
        format!(
            "1 steps, 1 tool calls, project: unchanged, ended: message\n{}",
            "done"
        )
    }
}

#[test]
fn parent_and_typed_relation_are_folded_into_the_graph() {
    let dir = std::env::temp_dir().join(format!("eja-graph-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let store = Store::new(&dir).expect("store");
    let brain = QueueBrain {
        queue: Mutex::new(VecDeque::from(vec![Decision {
            reply: "structure".to_string(),
            actions: vec![
                Action::OpenGroup {
                    title: "long task".to_string(),
                    acceptance: "child done".to_string(),
                },
                Action::OpenTask {
                    title: "child task".to_string(),
                    request: "child".to_string(),
                    acceptance: "child output".to_string(),
                    evidence: None,
                    run: false,
                },
                Action::LinkTasks {
                    from: 2,
                    to: 1,
                    kind: RelationKind::PartOf,
                },
            ],
            refused: Vec::new(),
        }])),
    };
    let outcome = turn_at(
        &store,
        "build a long task",
        &brain,
        &DoneWorker,
        &Ports::default(),
        eja::now_utc(),
    )
    .expect("turn");

    assert_eq!(
        outcome.context().relations,
        vec![Edge {
            from: 2,
            to: 1,
            kind: RelationKind::PartOf,
        },]
    );
    // The edge is one event, scoped to its source task, not two task mutations.
    assert!(store
        .events()
        .unwrap()
        .iter()
        .any(|event| event.kind == eja::Kind::TaskRelation && event.scope == "task:2"));
}

fn store(tag: &str) -> Store {
    let dir = std::env::temp_dir().join(format!("eja-graph-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    Store::new(&dir).expect("store")
}

fn run(store: &Store, decisions: Vec<Decision>) -> eja::TurnOutcome<'static> {
    let brain = QueueBrain {
        queue: Mutex::new(VecDeque::from(decisions)),
    };
    turn_at(
        store,
        "structure",
        &brain,
        &DoneWorker,
        &Ports::default(),
        eja::now_utc(),
    )
    .expect("turn")
}

fn group(title: &str) -> Action {
    Action::OpenGroup {
        title: title.to_string(),
        acceptance: "members done".to_string(),
    }
}

fn task(title: &str, run: bool) -> Action {
    Action::OpenTask {
        title: title.to_string(),
        request: title.to_string(),
        acceptance: "output".to_string(),
        evidence: None,
        run,
    }
}

fn decision(actions: Vec<Action>) -> Decision {
    Decision {
        reply: "structure".to_string(),
        actions,
        refused: Vec::new(),
    }
}

#[test]
fn containment_needs_an_open_group() {
    let store = store("containment");
    // A aja is not a container, and a task belongs to one group.
    let outcome = run(
        &store,
        vec![
            decision(vec![task("solo", false), group("first"), group("second")]),
            decision(vec![
                Action::LinkTasks {
                    from: 1,
                    to: 1,
                    kind: RelationKind::PartOf,
                },
                Action::LinkTasks {
                    from: 1,
                    to: 1,
                    kind: RelationKind::DependsOn,
                },
                Action::LinkTasks {
                    from: 1,
                    to: 2,
                    kind: RelationKind::PartOf,
                },
                Action::LinkTasks {
                    from: 1,
                    to: 3,
                    kind: RelationKind::PartOf,
                },
            ]),
        ],
    );
    let context = outcome.context();
    assert!(context
        .refused
        .iter()
        .any(|note| note.contains("self relation")));
    // Only the first containment is recorded; the second is refused, and neither
    // renamed the aja into a group.
    assert_eq!(
        context.relations,
        vec![Edge {
            from: 1,
            to: 2,
            kind: RelationKind::PartOf,
        }]
    );
    assert!(context
        .refused
        .iter()
        .any(|note| note.contains("already belongs to a group")));
}

#[test]
fn relating_the_same_edge_twice_is_one_fact() {
    let store = store("idempotent");
    let outcome = run(
        &store,
        vec![
            decision(vec![group("g"), task("member", false)]),
            decision(vec![
                Action::LinkTasks {
                    from: 2,
                    to: 1,
                    kind: RelationKind::PartOf,
                },
                Action::LinkTasks {
                    from: 2,
                    to: 1,
                    kind: RelationKind::PartOf,
                },
            ]),
        ],
    );
    assert_eq!(outcome.context().relations.len(), 1);
    assert_eq!(
        store
            .events()
            .expect("events")
            .iter()
            .filter(|event| event.kind == eja::Kind::TaskRelation)
            .count(),
        1,
        "a relation stated twice is one fact"
    );
}

#[test]
fn a_group_with_an_active_member_refuses_to_close() {
    let store = store("group-close");
    let outcome = run(
        &store,
        vec![
            decision(vec![group("g"), task("member", false)]),
            // One decision: a successful relation alone ends the turn.
            decision(vec![
                Action::LinkTasks {
                    from: 2,
                    to: 1,
                    kind: RelationKind::PartOf,
                },
                Action::CloseTask {
                    task_id: 1,
                    accepted: true,
                    note: "done".to_string(),
                },
            ]),
        ],
    );
    let context = outcome.context();
    // The member is still open, so the container cannot complete.
    assert!(context
        .refused
        .iter()
        .any(|note| note.contains("still has active children")));
    assert_eq!(context.tasks[0].status, eja::TaskStatus::Open);
}

#[test]
fn a_group_closed_unaccepted_is_failed() {
    let store = store("group-reject");
    let outcome = run(
        &store,
        vec![
            decision(vec![group("g")]),
            decision(vec![Action::CloseTask {
                task_id: 1,
                accepted: false,
                note: "not wanted".to_string(),
            }]),
        ],
    );
    let context = outcome.context();
    assert_eq!(context.tasks[0].status, eja::TaskStatus::Failed);
    assert_eq!(
        context.tasks[0].close.as_ref().expect("close").note,
        "not wanted"
    );
}

#[test]
fn a_dependency_cycle_is_refused() {
    let store = store("cycle");
    let outcome = run(
        &store,
        vec![
            decision(vec![task("a", false), task("b", false)]),
            decision(vec![
                Action::LinkTasks {
                    from: 1,
                    to: 2,
                    kind: RelationKind::DependsOn,
                },
                Action::LinkTasks {
                    from: 2,
                    to: 1,
                    kind: RelationKind::DependsOn,
                },
            ]),
        ],
    );
    let context = outcome.context();
    assert_eq!(context.relations.len(), 1);
    assert!(context
        .refused
        .iter()
        .any(|note| note.contains("dependency cycle")));
}
