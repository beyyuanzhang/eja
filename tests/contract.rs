use eja::{
    turn_at, Action, Assignment, Brain, Decision, EjaContext, NoopArtifactReader, NoopRetriever,
    Report, ReportStatus, Store, TaskStatus, Worker,
};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

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
            })
    }
}

struct CaptureWorker {
    seen: Arc<Mutex<Vec<Assignment>>>,
}

impl Worker for CaptureWorker {
    fn execute(&self, assignment: &Assignment) -> Report {
        self.seen.lock().expect("seen").push(assignment.clone());
        Report {
            status: ReportStatus::Done,
            summary: "done".to_string(),
            artifacts: Vec::new(),
            evidence: Vec::new(),
            open_questions: Vec::new(),
        }
    }
}

#[test]
fn worker_receives_its_node_task_and_origin_remains_in_eja() {
    let dir = std::env::temp_dir().join(format!("eja-contract-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let store = Store::new(&dir).expect("store");
    let seen = Arc::new(Mutex::new(Vec::new()));
    let worker = CaptureWorker { seen: seen.clone() };
    let brain = QueueBrain {
        queue: Mutex::new(VecDeque::from(vec![
            Decision {
                reply: "open".to_string(),
                actions: vec![Action::OpenTask {
                    title: "expanded title".to_string(),
                    request: "EJA expanded request".to_string(),
                    acceptance: "EJA acceptance only".to_string(),
                    run: true,
                }],
            },
            Decision {
                reply: "done".to_string(),
                actions: vec![Action::CloseTask {
                    task_id: 1,
                    accepted: true,
                    note: "ok".to_string(),
                }],
            },
        ])),
    };
    let outcome = turn_at(
        &store,
        "raw user task",
        &brain,
        &worker,
        &NoopRetriever,
        &NoopArtifactReader,
        eja::now_utc(),
    )
    .expect("turn");

    let assignments = seen.lock().expect("seen");
    assert_eq!(assignments.len(), 1);
    assert_eq!(assignments[0].objective, "EJA expanded request");
    let task = &outcome.tasks[0];
    assert_eq!(task.status, TaskStatus::Done);
    assert_eq!(task.origin, "raw user task");
    assert_eq!(task.request, "EJA expanded request");
    assert_eq!(task.acceptance, "EJA acceptance only");

    let assigned = store
        .events()
        .expect("events")
        .into_iter()
        .find(|event| event.kind == eja::Kind::TaskAssigned)
        .and_then(|event| event.data)
        .expect("assigned data");
    assert!(assigned.get("acceptance").is_none());
}
