mod support;

use eja::{Action, Assignment, TaskStatus, Worker};
use std::sync::{Arc, Mutex};
use support::{decision, turn_with, State};

struct CaptureWorker {
    seen: Arc<Mutex<Vec<Assignment>>>,
}

impl Worker for CaptureWorker {
    fn execute(&self, assignment: &Assignment, _: &eja::Control) -> String {
        self.seen.lock().expect("seen").push(assignment.clone());
        format!(
            "1 steps, 1 tool calls, project: unchanged, ended: message\n{}",
            "done"
        )
    }
}

#[test]
fn worker_receives_the_task_contract_and_original_input() {
    let fixture = State::new("contract");
    let store = fixture.store();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let worker = CaptureWorker { seen: seen.clone() };
    let outcome = turn_with(
        &store,
        "raw user task",
        vec![
            decision(vec![Action::OpenTask {
                title: "expanded title".to_string(),
                request: "EJA expanded request".to_string(),
                acceptance: "EJA acceptance only".to_string(),
                workspace: None,
                evidence: None,
                run: true,
            }]),
            decision(vec![Action::CloseTask {
                task_id: 1,
                accepted: true,
                note: "ok".to_string(),
            }]),
        ],
        &worker,
    );

    let assignments = seen.lock().expect("seen");
    assert_eq!(assignments.len(), 1);
    assert_eq!(assignments[0].objective, "EJA expanded request");
    assert_eq!(assignments[0].input, "raw user task");
    let task = &outcome.context().tasks[0];
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
    // The aja gets the task's contract, not just its prose objective.
    assert_eq!(assigned["acceptance"], "EJA acceptance only");
}
