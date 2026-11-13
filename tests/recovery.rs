use eja::adapters::Ports;
use eja::{
    recover, turn_at, Assignment, Brain, Decision, EjaContext, EventMeta, Kind, Store, TaskStatus,
    Worker,
};
use std::collections::VecDeque;
use std::sync::Mutex;

struct QueueBrain {
    queue: Mutex<VecDeque<Decision>>,
}

impl QueueBrain {
    fn new(decisions: Vec<Decision>) -> Self {
        Self {
            queue: Mutex::new(decisions.into()),
        }
    }
}

impl Brain for QueueBrain {
    fn decide(&self, _context: &EjaContext) -> Decision {
        self.queue
            .lock()
            .expect("brain")
            .pop_front()
            .unwrap_or_else(|| Decision {
                reply: "continued".to_string(),
                actions: Vec::new(),
                refused: Vec::new(),
            })
    }
}

struct NoWorker;

impl Worker for NoWorker {
    fn execute(&self, _assignment: &Assignment) -> String {
        format!(
            "0 steps, 0 tool calls, 0 artifacts, ended: error\n{}",
            "no worker"
        )
    }
}

static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn temp_store() -> std::path::PathBuf {
    let serial = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("eja-recovery-{}-{}", std::process::id(), serial));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("dir");
    dir
}

#[test]
fn interrupted_assignment_is_reconciled_without_duplicate_dispatch() {
    let dir = temp_store();
    let store = Store::new(&dir).expect("store");
    let assignment = Assignment {
        task_id: 1,
        objective: "do it".to_string(),
        issued_at: 100,
        acceptance: String::new(),
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
    let task = eja::tasks(&events).remove(0);
    assert_eq!(task.status, TaskStatus::Waiting);
    assert!(
        task.digest.as_deref().unwrap_or("").contains("interrupted"),
        "{:?}",
        task.digest
    );
    assert!(recover(&store, 3, 300).expect("idempotent").is_empty());

    let brain = QueueBrain::new(vec![]);
    turn_at(
        &store,
        "continue",
        &brain,
        &NoWorker,
        &Ports::default(),
        chrono::Utc::now(),
    )
    .expect("turn");
}

#[test]
fn torn_tail_is_ignored_by_the_reader() {
    let dir = temp_store();
    let store = Store::new(&dir).expect("store");
    store
        .append(Kind::User, "eja", "one", 1, EventMeta::new(1))
        .expect("append");
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(&store.log_path)
        .expect("open");
    use std::io::Write;
    file.write_all(b"{\"id\":").expect("torn");
    let events = store.events().expect("events");
    assert_eq!(events.len(), 1);
}
