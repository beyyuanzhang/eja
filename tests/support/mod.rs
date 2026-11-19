//! Shared kit for the integration tests.
//!
//! A test states a scenario and the facts it expects; everything that builds the
//! scenario lives here once. That is the whole rule: a test body is one
//! construction and its assertions, never its own copy of the harness.
#![allow(dead_code)] // each test crate compiles the whole kit and uses part of it.

use eja::{
    Action, Assignment, Brain, Control, Decision, EjaContext, Kind, RelationKind, Store,
    TurnOutcome, Worker,
};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

/// A control for one call: long enough that only the test's own limit ends it.
pub fn control() -> Control {
    Control::new(Duration::from_secs(300), std::sync::Arc::default())
}

/// A state directory that removes itself, so a run leaves nothing behind.
pub struct State(PathBuf);

impl State {
    /// A fresh directory. Two tests may pass the same tag: the counter keeps them
    /// apart within a process, so no test depends on another's leftovers.
    pub fn new(tag: &str) -> Self {
        static NEXT: OnceLock<Mutex<u64>> = OnceLock::new();
        let n = {
            let lock = NEXT.get_or_init(|| Mutex::new(0));
            let mut guard = lock
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            *guard += 1;
            *guard
        };
        let dir = std::env::temp_dir().join(format!("eja-{tag}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        Self(dir)
    }

    pub fn path(&self) -> &Path {
        &self.0
    }

    /// The journal this test writes to.
    pub fn store(&self) -> Store {
        Store::new(&self.0).expect("store")
    }

    /// A second handle on the same journal, as a restarted process would open it.
    pub fn reopen(&self) -> Store {
        Store::new(&self.0).expect("reopen")
    }

    pub fn events(&self) -> Vec<eja::Event> {
        self.store().events().expect("events")
    }
}

impl Drop for State {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A brain that plays the decisions it was given, in order.
pub struct Script {
    queue: Mutex<std::collections::VecDeque<Decision>>,
    seen: Mutex<Vec<Seen>>,
}

/// What one decision saw, kept only when a test asks for it.
#[derive(Clone, Default)]
pub struct Seen {
    pub focus: Option<u64>,
    pub tasks: Vec<eja::Task>,
    pub proposals: usize,
    pub evidence: Vec<serde_json::Value>,
    pub refused: Vec<String>,
}

impl Script {
    pub fn new(decisions: Vec<Decision>) -> Self {
        Self {
            queue: Mutex::new(decisions.into()),
            seen: Mutex::new(Vec::new()),
        }
    }

    /// One decision, one turn.
    pub fn once(actions: Vec<Action>) -> Self {
        Self::new(vec![decision(actions)])
    }

    pub fn seen(&self) -> std::sync::MutexGuard<'_, Vec<Seen>> {
        self.seen
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl Brain for Script {
    fn decide(&self, context: &EjaContext<'_>, _: &Control) -> eja::Result<Decision> {
        self.seen
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(Seen {
                focus: context.focus,
                tasks: context.tasks.to_vec(),
                proposals: context.proposals.len(),
                evidence: context
                    .evidence
                    .iter()
                    .map(|observation| {
                        serde_json::json!({
                            "target": observation.target,
                            "result": observation.result,
                        })
                    })
                    .collect(),
                refused: context
                    .refused
                    .iter()
                    .map(|text| (*text).to_string())
                    .collect(),
            });
        Ok(self
            .queue
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .pop_front()
            .unwrap_or_else(|| Decision {
                reply: "done".to_string(),
                choices: Vec::new(),
                actions: Vec::new(),
                refused: Vec::new(),
            }))
    }
}

/// A worker that answers at once with the line a test asks for.
pub struct Done(pub &'static str);

impl Default for Done {
    fn default() -> Self {
        Self("done")
    }
}

impl Worker for Done {
    fn execute(&self, _: &Assignment, _: &Control) -> String {
        format!(
            "1 steps, 1 tool calls, project: unchanged, ended: message\n{}",
            self.0
        )
    }
}

/// One decision that says nothing and does these actions.
pub fn decision(actions: Vec<Action>) -> Decision {
    Decision {
        reply: "structure".to_string(),
        choices: Vec::new(),
        actions,
        refused: Vec::new(),
    }
}

/// Run one turn against a scripted brain and a worker that answers at once.
pub fn turn(store: &Store, decisions: Vec<Decision>) -> TurnOutcome<'static> {
    turn_with(store, "go", decisions, &Done::default())
}

pub fn turn_at(
    store: &Store,
    input: &'static str,
    decisions: Vec<Decision>,
) -> TurnOutcome<'static> {
    turn_with(store, input, decisions, &Done::default())
}

pub fn turn_with<'a>(
    store: &Store,
    input: &'a str,
    decisions: Vec<Decision>,
    worker: &dyn Worker,
) -> TurnOutcome<'a> {
    let brain = Script::new(decisions);
    eja::turn(
        store,
        input,
        &brain,
        worker,
        &eja::adapters::Ports::default(),
    )
    .expect("turn")
}

/// Actions, named for what they do rather than for their variant.
pub fn group(title: &str) -> Action {
    Action::OpenGroup {
        title: title.to_string(),
        acceptance: "members done".to_string(),
    }
}

pub fn task(title: &str) -> Action {
    Action::OpenTask {
        title: title.to_string(),
        request: title.to_string(),
        acceptance: "output".to_string(),
        workspace: None,
        evidence: None,
        run: false,
    }
}

pub fn aja(title: &str) -> Action {
    Action::OpenTask {
        title: title.to_string(),
        request: title.to_string(),
        acceptance: "output".to_string(),
        workspace: None,
        evidence: None,
        run: true,
    }
}

pub const fn link(from: u64, to: u64, kind: RelationKind) -> Action {
    Action::LinkTasks { from, to, kind }
}

pub fn kinds(store: &Store, kind: Kind) -> usize {
    store
        .events()
        .expect("events")
        .iter()
        .filter(|event| event.kind == kind)
        .count()
}
