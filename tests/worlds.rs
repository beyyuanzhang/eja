mod support;
use eja::adapters::{Shared, Worktrees, Worlds};
use eja::{
    turn_at_with_options, Action, Assignment, Brain, Control, Decision, EjaContext, Kind, Store,
    TurnOptions, Worker,
};
use std::collections::VecDeque;
use std::sync::{
    atomic::{AtomicU64, AtomicUsize, Ordering},
    Arc, Mutex,
};
use std::time::Duration;
use support::control;

struct Queue(Mutex<VecDeque<Decision>>);
impl Queue {
    fn one(actions: Vec<Action>) -> Self {
        Self(Mutex::new(VecDeque::from(vec![Decision {
            reply: "ok".into(),
            actions,
            refused: Vec::new(),
        }])))
    }
}
impl Brain for Queue {
    fn decide(&self, _: &EjaContext, _: &eja::Control) -> Decision {
        self.0
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

/// Records how many runs were alive at once. A run waits for a sibling so a
/// serial host is reported as serial instead of being hidden by timing.
#[derive(Default)]
struct Overlap {
    live: AtomicUsize,
    arrived: AtomicUsize,
    peak: AtomicUsize,
}
struct Probe {
    inner: Arc<Overlap>,
    patience: Duration,
}
impl Worker for Probe {
    fn execute(&self, assignment: &Assignment, _: &Control) -> String {
        let live = self.inner.live.fetch_add(1, Ordering::SeqCst) + 1;
        self.inner.peak.fetch_max(live, Ordering::SeqCst);
        self.inner.arrived.fetch_add(1, Ordering::SeqCst);
        let deadline = std::time::Instant::now() + self.patience;
        while self.inner.arrived.load(Ordering::SeqCst) < 2 && std::time::Instant::now() < deadline
        {
            std::thread::sleep(Duration::from_millis(2));
        }
        self.inner.live.fetch_sub(1, Ordering::SeqCst);
        format!(
            "1 steps, 0 tool calls, ended: message\nran {}",
            assignment.task_id
        )
    }
}

fn open(index: u64) -> Action {
    Action::OpenTask {
        title: format!("job {index}"),
        request: format!("request {index}"),
        acceptance: "it holds".into(),
        evidence: None,
        run: false,
    }
}

static NEXT: AtomicU64 = AtomicU64::new(0);
fn temp_dir(name: &str) -> std::path::PathBuf {
    let serial = NEXT.fetch_add(1, Ordering::Relaxed);
    let dir =
        std::env::temp_dir().join(format!("eja-worlds-{name}-{}-{serial}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("dir");
    dir.canonicalize().expect("canonical")
}

#[test]
fn parallel_slots_run_at_once_and_the_log_keeps_action_order() {
    let dir = temp_dir("parallel");
    let store = Store::new(&dir).expect("store");
    let overlap = Arc::new(Overlap::default());
    let brain = Queue::one(vec![
        open(1),
        open(2),
        Action::Dispatch { task_id: 1 },
        Action::Dispatch { task_id: 2 },
    ]);
    let options = TurnOptions {
        limits: eja::TurnLimits {
            slots: 2,
            ..Default::default()
        },
        ..Default::default()
    };
    let out = turn_at_with_options(
        &store,
        "run both",
        &brain,
        &Probe {
            inner: overlap.clone(),
            patience: Duration::from_secs(5),
        },
        &eja::adapters::Ports::default(),
        eja::now_utc(),
        &options,
    )
    .expect("turn");
    assert_eq!(out.dispatched, vec![1, 2]);
    assert_eq!(
        overlap.peak.load(Ordering::SeqCst),
        2,
        "both worlds were live"
    );
    let reports: Vec<u64> = store
        .events()
        .unwrap()
        .into_iter()
        .filter(|event| event.kind == Kind::WorkerReport)
        .filter_map(|event| event.scope.strip_prefix("task:")?.parse().ok())
        .collect();
    assert_eq!(reports, vec![1, 2], "reports are recorded in action order");
}

#[test]
fn one_slot_stays_sequential() {
    let dir = temp_dir("serial");
    let store = Store::new(&dir).expect("store");
    let overlap = Arc::new(Overlap::default());
    let brain = Queue::one(vec![
        open(1),
        open(2),
        Action::Dispatch { task_id: 1 },
        Action::Dispatch { task_id: 2 },
    ]);
    turn_at_with_options(
        &store,
        "run both",
        &brain,
        &Probe {
            inner: overlap.clone(),
            patience: Duration::from_millis(200),
        },
        &eja::adapters::Ports::default(),
        eja::now_utc(),
        &TurnOptions::default(),
    )
    .expect("turn");
    assert_eq!(overlap.peak.load(Ordering::SeqCst), 1);
}

fn git(cwd: &std::path::Path, args: &[&str]) {
    let status = std::process::Command::new("git")
        .current_dir(cwd)
        .args(["-c", "user.name=eja", "-c", "user.email=eja@local"])
        .args(args)
        .status()
        .expect("git");
    assert!(status.success(), "git {args:?}");
}

fn project_with_one_commit(name: &str) -> std::path::PathBuf {
    let project = temp_dir(name);
    git(&project, &["init", "--quiet"]);
    std::fs::create_dir_all(project.join(".eja")).unwrap();
    std::fs::write(project.join(".eja/.gitignore"), "*\n").unwrap();
    std::fs::write(project.join("base.txt"), "one\n").unwrap();
    git(&project, &["add", "-A"]);
    git(&project, &["commit", "--quiet", "-m", "base"]);
    project
}

#[test]
fn a_worktree_world_is_settled_and_reconciled_explicitly() {
    let project = project_with_one_commit("worktree");
    let control = control();
    let worlds = Worktrees::new(
        &project,
        project.join(".eja/worlds"),
        project.join(".eja"),
        &control,
    )
    .expect("worlds");
    let world = worlds.reserve(1, &control).expect("reserve");
    assert!(world.root.join("base.txt").exists());

    std::fs::write(world.root.join("out.txt"), "result\n").unwrap();
    let change = world.change(&control);
    assert!(change.line().contains("out.txt"), "{}", change.line());
    let settled = worlds.settle(&world, &change, &control).expect("settle");
    assert!(
        !settled["commit"].as_str().unwrap_or("").is_empty(),
        "{settled}"
    );

    let outcome = worlds.reconcile(1, &control).expect("reconcile");
    assert_eq!(outcome["merged"], serde_json::json!(true), "{outcome}");
    assert!(
        project.join("out.txt").exists(),
        "reconciliation lands the run"
    );
    worlds.release(&world);
}

#[test]
fn a_conflicting_world_is_reported_and_never_merged_silently() {
    let project = project_with_one_commit("conflict");
    let control = control();
    let worlds = Worktrees::new(
        &project,
        project.join(".eja/worlds"),
        project.join(".eja"),
        &control,
    )
    .expect("worlds");
    let world = worlds.reserve(2, &control).expect("reserve");
    std::fs::write(world.root.join("base.txt"), "from the run\n").unwrap();
    let change = world.change(&control);
    worlds.settle(&world, &change, &control).expect("settle");
    // The shared world moved on in the meantime.
    std::fs::write(project.join("base.txt"), "from the host\n").unwrap();
    git(&project, &["add", "-A"]);
    git(&project, &["commit", "--quiet", "-m", "host move"]);

    let outcome = worlds.reconcile(2, &control).expect("reconcile");
    assert_eq!(outcome["merged"], serde_json::json!(false), "{outcome}");
    assert!(
        !outcome["conflicts"]
            .as_array()
            .expect("conflicts")
            .is_empty(),
        "{outcome}"
    );
    assert_eq!(
        std::fs::read_to_string(project.join("base.txt")).unwrap(),
        "from the host\n",
        "a conflict leaves the shared world untouched"
    );
    worlds.release(&world);
}

#[test]
fn an_isolated_world_starts_at_the_commit_not_at_the_working_tree() {
    let project = project_with_one_commit("dirty");
    std::fs::write(project.join("base.txt"), "uncommitted\n").unwrap();
    let control = control();
    let worlds = Worktrees::new(
        &project,
        project.join(".eja/worlds"),
        project.join(".eja"),
        &control,
    )
    .expect("worlds");
    let world = worlds.reserve(3, &control).expect("reserve");
    assert_eq!(
        std::fs::read_to_string(world.root.join("base.txt")).unwrap(),
        "one\n",
        "the world is a commit, not the operator's working tree"
    );
    // The run's own change is still the whole account.
    std::fs::write(world.root.join("mine.txt"), "made here\n").unwrap();
    let change = world.change(&control);
    assert_eq!(change.line(), "+1 created (mine.txt)");
    worlds.release(&world);
}

#[test]
fn a_shared_world_needs_no_reconciliation() {
    let project = temp_dir("shared");
    let shared = Shared {
        project: project.clone(),
        private: project.join(".eja"),
    };
    let control = control();
    let world = shared.reserve(1, &control).expect("reserve");
    assert_eq!(world.root, project);
    let change = world.change(&control);
    assert_eq!(change.line(), "unchanged");
    let outcome = shared.reconcile(1, &control).expect("reconcile");
    assert_eq!(outcome["merged"], serde_json::json!(true));
    shared.release(&world);
}
