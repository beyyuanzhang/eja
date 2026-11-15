mod support;

use eja::adapters::Ports;
use eja::{Action, TaskStatus};
use support::{aja, decision, task, Done, Script, State};

#[test]
fn single_waiting_task_is_focused_without_the_extra_initial_call() {
    let fixture = State::new("fast-waiting");
    let store = fixture.store();
    let open = Script::new(vec![decision(vec![aja("one")]), decision(vec![])]);
    let first =
        eja::turn(&store, "open", &open, &Done::default(), &Ports::default()).expect("open");
    assert_eq!(first.context().tasks[0].status, TaskStatus::Waiting);

    let close = Script::new(vec![decision(vec![Action::CloseTask {
        task_id: 1,
        accepted: true,
        note: "ok".to_string(),
    }])]);
    let second = eja::turn(
        &store,
        "continue",
        &close,
        &Done::default(),
        &Ports::default(),
    )
    .expect("close");
    assert_eq!(second.decisions, 1);
    assert_eq!(
        close
            .seen()
            .iter()
            .map(|seen| seen.focus)
            .collect::<Vec<_>>(),
        vec![Some(1)]
    );
    assert_eq!(second.context().tasks[0].status, TaskStatus::Done);
}

#[test]
fn multiple_active_tasks_do_not_use_the_fast_path() {
    let fixture = State::new("fast-multiple");
    let store = fixture.store();
    let open = Script::new(vec![decision(vec![task("a"), task("b")])]);
    eja::turn(
        &store,
        "open two",
        &open,
        &Done::default(),
        &Ports::default(),
    )
    .expect("open");

    let close = Script::new(vec![decision(vec![Action::CloseTask {
        task_id: 1,
        accepted: false,
        note: "no".to_string(),
    }])]);
    eja::turn(&store, "close", &close, &Done::default(), &Ports::default()).expect("close");
    // Cancelling an open aja needs no worker execution or extra observation.
    assert_eq!(
        close
            .seen()
            .iter()
            .map(|seen| seen.focus)
            .collect::<Vec<_>>(),
        vec![None]
    );
}
