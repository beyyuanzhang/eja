//! The command line is a thin shell over the library; these tests run it, so a
//! broken flag or an unreadable target fails here rather than in a user's hands.
use eja::adapters::Ports;
use eja::{turn_at, Action, Assignment, Brain, Decision, EjaContext, Store, TaskStatus, Worker};
use std::{
    collections::VecDeque,
    path::PathBuf,
    process::Command,
    sync::{Mutex, MutexGuard, OnceLock},
};

fn serial() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

const fn binary() -> &'static str {
    env!("CARGO_BIN_EXE_eja")
}

fn root(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("eja-cli-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

fn run(dir: &PathBuf, args: &[&str]) -> (bool, String) {
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

struct Script(Mutex<VecDeque<Decision>>);

impl Brain for Script {
    fn decide(&self, _: &EjaContext<'_>, _: &eja::Control) -> Decision {
        self.0
            .lock()
            .unwrap()
            .pop_front()
            .expect("scripted decision")
    }
}

struct Done;

impl Worker for Done {
    fn execute(&self, _: &Assignment, _: &eja::Control) -> String {
        "1 steps, 0 tool calls, project: unchanged, ended: message\nwork".to_string()
    }
}

fn seeded(dir: &PathBuf) {
    let store = Store::new(dir).expect("store");
    let brain = Script(Mutex::new(VecDeque::from(vec![
        Decision {
            reply: "planned".to_string(),
            actions: vec![
                Action::OpenGroup {
                    title: "a group".to_string(),
                    acceptance: "members done".to_string(),
                },
                Action::OpenTask {
                    title: "a member".to_string(),
                    request: "do the work".to_string(),
                    acceptance: "output".to_string(),
                    evidence: None,
                    run: false,
                },
                Action::Dispatch { task_id: 2 },
            ],
            refused: Vec::new(),
        },
        // A turn that only commits continues; a decision that commits nothing is
        // its final reply.
        Decision {
            reply: "done".to_string(),
            actions: Vec::new(),
            refused: Vec::new(),
        },
    ])));
    turn_at(
        &store,
        "plan the work",
        &brain,
        &Done,
        &Ports::default(),
        eja::now_utc(),
    )
    .expect("turn");
}

#[test]
fn every_show_target_the_usage_line_offers_answers() {
    let _lock = serial();
    let dir = root("show");
    seeded(&dir);
    for (target, expected) in [
        ("context", "CURRENT INPUT"),
        ("log", "\"kind\":\"user\""),
        ("tasks", "a member"),
        ("tools", "["),
        ("task:2", "a member"),
        ("audit:2", "audit component"),
    ] {
        let (ok, text) = run(&dir, &["show", target]);
        assert!(ok, "show {target} failed: {text}");
        assert!(text.contains(expected), "show {target}: {text}");
    }
    let state = Store::new(&dir).expect("store");
    let tasks = eja::State::load(&state).expect("state");
    assert_eq!(tasks.tasks()[1].status, TaskStatus::Waiting);
}

#[test]
fn an_unknown_show_target_is_refused_with_the_usage_line() {
    let _lock = serial();
    let dir = root("usage");
    seeded(&dir);
    let (ok, text) = run(&dir, &["show", "everything"]);
    assert!(!ok);
    assert!(text.contains("unknown show target"), "{text}");
}

#[test]
fn a_directory_written_before_the_rename_is_refused_not_read() {
    let _lock = serial();
    let dir = root("legacy");
    std::fs::create_dir_all(dir.join("nodes")).expect("legacy dir");
    let (ok, text) = run(&dir, &["show", "log"]);
    assert!(!ok, "a legacy directory must not be read: {text}");
    assert!(text.contains("nodes/"), "{text}");
    assert!(text.contains("no migration"), "{text}");
}

#[test]
fn a_confirm_flag_reaches_the_turn_instead_of_being_ignored() {
    let _lock = serial();
    let dir = root("confirm");
    seeded(&dir);
    // There is no model configured here, so the turn stops before deciding — and
    // says so. The point is that the approval was parsed and carried into the
    // turn's options, not that a model was reachable.
    let (ok, text) = run(&dir, &["--confirm", "7", "turn", "approve it"]);
    assert!(!ok, "{text}");
    assert!(text.to_lowercase().contains("model"), "{text}");
    let (ok, text) = run(&dir, &["--confirm", "not-a-number", "turn", "x"]);
    assert!(!ok);
    assert!(text.contains("invalid id for --confirm"), "{text}");
}
