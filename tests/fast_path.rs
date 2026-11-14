use eja::adapters::Ports;
use eja::{turn_at, Action, Assignment, Brain, Decision, EjaContext, Store, TaskStatus, Worker};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

struct CountingBrain {
    queue: Mutex<VecDeque<Decision>>,
    focus: Arc<Mutex<Vec<Option<u64>>>>,
}

impl Brain for CountingBrain {
    fn decide(&self, context: &EjaContext) -> Decision {
        self.focus
            .lock()
            .expect("focus")
            .push(context.focus.as_ref().map(|task| task.id));
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

fn store(name: &str) -> Store {
    let dir = std::env::temp_dir().join(format!("eja-fast-{}-{}", std::process::id(), name));
    let _ = std::fs::remove_dir_all(&dir);
    Store::new(&dir).expect("store")
}

fn open_task(title: &str) -> Action {
    Action::OpenTask {
        title: title.to_string(),
        request: format!("request {title}"),
        acceptance: format!("accept {title}"),
        run: true,
    }
}

#[test]
fn single_waiting_task_is_focused_without_the_extra_initial_call() {
    let store = store("waiting");
    let open = CountingBrain {
        queue: Mutex::new(VecDeque::from(vec![
            Decision {
                reply: "open".to_string(),
                actions: vec![open_task("one")],
                refused: Vec::new(),
            },
            Decision {
                reply: "waiting".to_string(),
                actions: Vec::new(),
                refused: Vec::new(),
            },
        ])),
        focus: Arc::new(Mutex::new(Vec::new())),
    };
    let first = turn_at(
        &store,
        "open",
        &open,
        &DoneWorker,
        &Ports::default(),
        eja::now_utc(),
    )
    .expect("open");
    assert_eq!(first.tasks[0].status, TaskStatus::Waiting);

    let focus = Arc::new(Mutex::new(Vec::new()));
    let close = CountingBrain {
        queue: Mutex::new(VecDeque::from(vec![Decision {
            reply: "closed".to_string(),
            actions: vec![Action::CloseTask {
                task_id: 1,
                accepted: true,
                note: "ok".to_string(),
            }],
            refused: Vec::new(),
        }])),
        focus: focus.clone(),
    };
    let second = turn_at(
        &store,
        "continue",
        &close,
        &DoneWorker,
        &Ports::default(),
        eja::now_utc(),
    )
    .expect("close");
    assert_eq!(second.decisions, 1);
    assert_eq!(*focus.lock().expect("focus"), vec![Some(1)]);
    assert_eq!(second.tasks[0].status, TaskStatus::Done);
}

#[test]
fn multiple_active_tasks_do_not_use_the_fast_path() {
    let store = store("multiple");
    let open = CountingBrain {
        queue: Mutex::new(VecDeque::from(vec![Decision {
            reply: "open".to_string(),
            actions: vec![
                Action::OpenTask {
                    title: "a".to_string(),
                    request: "a".to_string(),
                    acceptance: "a".to_string(),
                    run: false,
                },
                Action::OpenTask {
                    title: "b".to_string(),
                    request: "b".to_string(),
                    acceptance: "b".to_string(),
                    run: false,
                },
            ],
            refused: Vec::new(),
        }])),
        focus: Arc::new(Mutex::new(Vec::new())),
    };
    turn_at(
        &store,
        "open two",
        &open,
        &DoneWorker,
        &Ports::default(),
        eja::now_utc(),
    )
    .expect("open");

    let focus = Arc::new(Mutex::new(Vec::new()));
    let close = CountingBrain {
        queue: Mutex::new(VecDeque::from(vec![Decision {
            reply: "close".to_string(),
            actions: vec![Action::CloseTask {
                task_id: 1,
                accepted: false,
                note: "no".to_string(),
            }],
            refused: Vec::new(),
        }])),
        focus: focus.clone(),
    };
    turn_at(
        &store,
        "close",
        &close,
        &DoneWorker,
        &Ports::default(),
        eja::now_utc(),
    )
    .expect("close");
    // Cancelling an open node needs no worker execution or extra observation.
    assert_eq!(*focus.lock().expect("focus"), vec![None]);
}
