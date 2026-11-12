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
    fn decide(&self, _context: &EjaContext) -> Decision {
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
    fn execute(&self, _assignment: &Assignment) -> String {
        format!(
            "1 steps, 1 tool calls, 0 artifacts, ended: message\n{}",
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
                Action::OpenTask {
                    title: "long task".to_string(),
                    request: "parent".to_string(),
                    acceptance: "child done".to_string(),
                    run: false,
                },
                Action::OpenTask {
                    title: "child task".to_string(),
                    request: "child".to_string(),
                    acceptance: "child output".to_string(),
                    run: false,
                },
                Action::LinkTasks {
                    from: 2,
                    to: 1,
                    kind: RelationKind::PartOf,
                },
                Action::LinkTasks {
                    from: 2,
                    to: 1,
                    kind: RelationKind::DependsOn,
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
        outcome.context.relations,
        vec![
            Edge {
                from: 2,
                to: 1,
                kind: RelationKind::PartOf,
            },
            Edge {
                from: 2,
                to: 1,
                kind: RelationKind::DependsOn,
            },
        ]
    );
}
