//! The command line is a thin shell over the library; these tests run it, so a
//! broken flag or an unreadable target fails here rather than in a user's hands.
mod support;

use eja::{Action, TaskStatus};
use std::{
    path::Path,
    process::Command,
    sync::{Mutex, MutexGuard, OnceLock},
};
use support::{decision, group, task, turn_with, Done, State};

fn serial() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

const fn binary() -> &'static str {
    env!("CARGO_BIN_EXE_eja")
}

fn run(dir: &Path, args: &[&str]) -> (bool, String) {
    let output = Command::new(binary())
        .arg("--dir")
        .arg(dir)
        .args(args)
        .output()
        .expect("run the cli");
    let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&output.stderr));
    (output.status.success(), text)
}

/// The journal every test here starts from, written through one library turn:
/// a group with one member, and a dispatch that runs it.
fn seeded(fixture: &State) {
    let store = fixture.store();
    // A turn that only commits continues; a decision that commits nothing is its
    // final reply, which is why one decision is enough here.
    turn_with(
        &store,
        "plan the work",
        vec![decision(vec![
            group("a group"),
            task("a member"),
            Action::Dispatch { task_id: 2 },
        ])],
        &Done("work"),
    );
}

#[test]
fn every_show_target_the_usage_line_offers_answers() {
    let _lock = serial();
    let fixture = State::new("cli-show");
    seeded(&fixture);
    for (target, expected) in [
        ("context", "INPUT"),
        ("log", "\"kind\":\"user\""),
        ("tasks", "a member"),
        ("tools", "["),
        ("task:2", "a member"),
        ("audit:2", "audit component"),
    ] {
        let (ok, text) = run(fixture.path(), &["show", target]);
        assert!(ok, "show {target} failed: {text}");
        assert!(text.contains(expected), "show {target}: {text}");
    }
    let store = fixture.reopen();
    let tasks = eja::State::load(&store).expect("state");
    assert_eq!(tasks.tasks()[1].status, TaskStatus::Waiting);
}

#[test]
fn an_unknown_show_target_is_refused_with_the_usage_line() {
    let _lock = serial();
    let fixture = State::new("cli-usage");
    seeded(&fixture);
    let (ok, text) = run(fixture.path(), &["show", "everything"]);
    assert!(!ok);
    assert!(text.contains("unknown show target"), "{text}");
}

#[test]
fn a_confirm_flag_reaches_the_turn_instead_of_being_ignored() {
    let _lock = serial();
    let fixture = State::new("cli-confirm");
    seeded(&fixture);
    // There is no model configured here, so the turn stops before deciding — and
    // says so. The point is that the approval was parsed and carried into the
    // turn's options, not that a model was reachable.
    let (ok, text) = run(fixture.path(), &["--confirm", "7", "turn", "approve it"]);
    assert!(!ok, "{text}");
    assert!(text.to_lowercase().contains("model"), "{text}");
    let (ok, text) = run(fixture.path(), &["--confirm", "not-a-number", "turn", "x"]);
    assert!(!ok);
    assert!(text.contains("invalid id for --confirm"), "{text}");
}
