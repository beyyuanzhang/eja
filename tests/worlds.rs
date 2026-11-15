//! The worlds a run may change: one shared, one per run, settled and reconciled.

mod support;

use eja::adapters::{Ports, Shared, Worktrees, Worlds};
use eja::{turn_at_with_options, Action, Assignment, Control, Kind, TurnOptions, Worker};
use std::path::PathBuf;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
use std::time::Duration;
use support::{control, Done, Script, State};

/// Records how many runs were alive at once. A run waits for a sibling so a
/// serial host is reported as serial instead of being hidden by timing.
#[derive(Default)]
struct Overlap {
    live: AtomicUsize,
    arrived: AtomicUsize,
    peak: AtomicUsize,
}

/// The one worker these tests need: a run that can prove it was alive beside a
/// sibling, and that reports which task it was.
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

/// The task these tests dispatch: the worktree account is measured against this
/// stated contract, so the request and acceptance are named here.
fn open(index: u64) -> Action {
    Action::OpenTask {
        title: format!("job {index}"),
        request: format!("request {index}"),
        acceptance: "it holds".into(),
        evidence: None,
        run: false,
    }
}

/// The fixture's one directory, resolved: a git world records absolute paths, so
/// a test must name the tree the way git reports it.
fn tree(state: &State) -> PathBuf {
    std::fs::create_dir_all(state.path()).expect("dir");
    state.path().canonicalize().expect("canonical")
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

fn project_with_one_commit(state: &State) -> PathBuf {
    let project = tree(state);
    git(&project, &["init", "--quiet"]);
    std::fs::create_dir_all(project.join(".eja")).unwrap();
    std::fs::write(project.join(".eja/.gitignore"), "*\n").unwrap();
    std::fs::write(project.join("base.txt"), "one\n").unwrap();
    git(&project, &["add", "-A"]);
    git(&project, &["commit", "--quiet", "-m", "base"]);
    project
}

#[test]
fn parallel_slots_run_at_once_and_the_log_keeps_action_order() {
    let state = State::new("parallel");
    let store = state.store();
    let overlap = Arc::new(Overlap::default());
    let brain = Script::once(vec![
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
        &Ports::default(),
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
    let reports: Vec<u64> = state
        .events()
        .into_iter()
        .filter(|event| event.kind == Kind::WorkerReport)
        .filter_map(|event| event.scope.strip_prefix("task:")?.parse().ok())
        .collect();
    assert_eq!(reports, vec![1, 2], "reports are recorded in action order");
}

#[test]
fn one_slot_stays_sequential() {
    let state = State::new("serial");
    let store = state.store();
    let overlap = Arc::new(Overlap::default());
    let brain = Script::once(vec![
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
        &Ports::default(),
        eja::now_utc(),
        &TurnOptions::default(),
    )
    .expect("turn");
    assert_eq!(overlap.peak.load(Ordering::SeqCst), 1);
}

#[test]
fn a_worktree_world_is_settled_and_reconciled_explicitly() {
    let state = State::new("worktree");
    let project = project_with_one_commit(&state);
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
    let settled = worlds.settle(&world, &control).expect("settle");
    assert!(
        settled["change"].as_str().unwrap_or("").contains("out.txt"),
        "{settled}"
    );
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
    let state = State::new("conflict");
    let project = project_with_one_commit(&state);
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
    worlds.settle(&world, &control).expect("settle");
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
    let state = State::new("dirty");
    let project = project_with_one_commit(&state);
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
    // The run's own change is still the whole account, measured by the backend.
    std::fs::write(world.root.join("mine.txt"), "made here\n").unwrap();
    let settled = worlds.settle(&world, &control).expect("settle");
    assert_eq!(
        settled["change"],
        serde_json::json!("+1 created (mine.txt)")
    );
    worlds.release(&world);
}

#[test]
fn a_shared_world_needs_no_reconciliation() {
    let state = State::new("shared");
    let project = tree(&state);
    let shared = Shared::new(project.clone(), project.join(".eja"));
    let control = control();
    let world = shared.reserve(1, &control).expect("reserve");
    assert_eq!(world.root, project);
    let account = shared.settle(&world, &control).expect("settle");
    assert_eq!(account["change"], serde_json::json!("unchanged"));
    let outcome = shared.reconcile(1, &control).expect("reconcile");
    assert_eq!(outcome["merged"], serde_json::json!(true));
    shared.release(&world);
}

/// A run that left an isolated world comes back only when asked, and the host's
/// account is one recorded observation — never a silent merge.
#[test]
fn reconciling_a_run_is_one_recorded_observation() {
    let state = State::new("reconcile");
    let project = state.path().join("project");
    std::fs::create_dir_all(&project).unwrap();
    let store = state.store();
    let worlds = Shared::new(project, state.path().join(".eja"));
    let ports = Ports {
        worlds: Some(&worlds),
        ..Default::default()
    };
    let options = TurnOptions::default();

    // Turn one: the aja runs; its record exists but the world is still shared.
    let brain = Script::once(vec![open(1), Action::Dispatch { task_id: 1 }]);
    turn_at_with_options(
        &store,
        "run",
        &brain,
        &Done::default(),
        &ports,
        eja::now_utc(),
        &options,
    )
    .expect("dispatch");

    // Turn two: EJA asks for the world to come back.
    let brain = Script::once(vec![Action::Reconcile { task_id: 1 }]);
    let out = turn_at_with_options(
        &store,
        "bring it back",
        &brain,
        &Done::default(),
        &ports,
        eja::now_utc(),
        &options,
    )
    .expect("reconcile");
    let context = out.context();
    let account = context
        .evidence
        .iter()
        .find(|view| view.kind == eja::ObservationKind::Reconcile)
        .expect("the account is evidence EJA sees");
    assert_eq!(
        account.result["merged"],
        serde_json::json!(true),
        "{account:?}"
    );
    assert!(
        state.events().iter().any(|event| {
            event.kind == Kind::Observed
                && event
                    .data
                    .as_ref()
                    .is_some_and(|data| data["kind"] == serde_json::json!("reconcile"))
        }),
        "the account is one immutable fact"
    );

    // A task with no run has no world to bring back.
    let brain = Script::once(vec![Action::Reconcile { task_id: 2 }]);
    let refused = turn_at_with_options(
        &store,
        "nothing to reconcile",
        &brain,
        &Done::default(),
        &ports,
        eja::now_utc(),
        &options,
    )
    .expect("refusal");
    assert!(
        refused
            .context()
            .refused
            .iter()
            .any(|note| note.contains("reconcile refused")),
        "{:?}",
        refused.context().refused
    );
}

#[test]
fn two_state_directories_on_one_project_keep_separate_branches() {
    let state = State::new("two-instances");
    let project = project_with_one_commit(&state);
    let control = control();
    let first = Worktrees::new(
        &project,
        project.join(".eja/worlds"),
        project.join(".eja"),
        &control,
    )
    .expect("first instance");
    let second = Worktrees::new(
        &project,
        project.join(".other/worlds"),
        project.join(".other"),
        &control,
    )
    .expect("second instance");
    let a = first.reserve(1, &control).expect("first reserve");
    let b = second.reserve(1, &control).expect("second reserve");
    // Both instances reserved task 1. Each owns a branch of its own, so the
    // second reservation cannot resign the first instance's refs.
    let listed = std::process::Command::new("git")
        .current_dir(&project)
        .args(["branch", "--list", "eja/aja-1-*"])
        .output()
        .expect("git branch");
    let listed = String::from_utf8_lossy(&listed.stdout);
    assert_eq!(
        listed.lines().count(),
        2,
        "one branch per state directory: {listed}"
    );
    assert!(a.root.join("base.txt").exists());
    assert!(b.root.join("base.txt").exists());
    first.release(&a);
    second.release(&b);
}

#[test]
fn the_shared_world_has_one_owner() {
    let state = State::new("owner");
    let project = state.path().join("project");
    std::fs::create_dir_all(&project).unwrap();
    let shared = Shared::new(project, state.path().join(".eja"));
    let control = control();
    let first = shared.reserve(1, &control).expect("first reservation");
    // A second run may not write the same directory while the first holds it.
    let second = shared.reserve(2, &control);
    assert!(second.is_err(), "a shared world has one owner");
    shared.settle(&first, &control).expect("settle");
    shared.release(&first);
    // Once settled, the world is free again.
    let third = shared.reserve(3, &control).expect("after settlement");
    shared.release(&third);
}

#[test]
fn settling_a_world_twice_is_refused() {
    let state = State::new("twice");
    let project = state.path().join("project");
    std::fs::create_dir_all(&project).unwrap();
    let shared = Shared::new(project, state.path().join(".eja"));
    let control = control();
    let world = shared.reserve(1, &control).expect("reserve");
    shared.settle(&world, &control).expect("settle");
    assert!(
        shared.settle(&world, &control).is_err(),
        "one reservation settles once"
    );
}

#[test]
fn a_world_per_run_needs_a_git_project() {
    let state = State::new("no-git");
    let dir = tree(&state);
    let refused = Worktrees::new(&dir, dir.join("worlds"), dir.join(".eja"), &control());
    let error = refused.err().expect("a plain directory cannot be forked");
    assert!(error.to_string().contains("git"), "{error}");
}

#[test]
fn failed_settlement_preserves_the_world_and_a_saved_branch_cannot_be_replaced() {
    let state = State::new("preserved-world");
    let project = project_with_one_commit(&state);
    let worlds = Worktrees::new(
        &project,
        project.join(".eja/worlds"),
        project.join(".eja"),
        &control(),
    )
    .unwrap();
    let world = worlds.reserve(1, &control()).unwrap();
    std::fs::write(world.root.join("out"), "recover").unwrap();
    let cancelled = Control::new(
        Duration::from_secs(10),
        Arc::new(std::sync::atomic::AtomicBool::new(true)),
    );
    assert!(worlds.settle(&world, &cancelled).is_err());
    worlds.release(&world);
    assert_eq!(
        std::fs::read_to_string(world.root.join("out")).unwrap(),
        "recover"
    );
    assert!(worlds.reserve(1, &control()).is_err());
    let broken = worlds.reserve(3, &control()).unwrap();
    std::fs::write(broken.root.join("out"), "git failure").unwrap();
    std::fs::remove_file(broken.root.join(".git")).unwrap();
    assert!(worlds.settle(&broken, &control()).is_err());
    worlds.release(&broken);
    assert_eq!(
        std::fs::read_to_string(broken.root.join("out")).unwrap(),
        "git failure"
    );
    let saved = worlds.reserve(2, &control()).unwrap();
    std::fs::write(saved.root.join("out"), "saved").unwrap();
    let commit = worlds.settle(&saved, &control()).unwrap()["commit"]
        .as_str()
        .unwrap()
        .to_string();
    worlds.release(&saved);
    assert!(!saved.root.exists());
    assert!(worlds.reserve(2, &control()).is_err());
    assert_eq!(worlds.reconcile(2, &control()).unwrap()["merged"], true);
    assert_eq!(
        std::fs::read_to_string(project.join("out")).unwrap(),
        "saved"
    );
    assert!(!commit.is_empty());
}

#[test]
fn relative_roots_resolve_before_project_is_rebased_to_git_top() {
    if std::env::var_os("EJA_RELATIVE_WORLD_CHILD").is_some() {
        std::fs::create_dir_all(".eja").unwrap();
        let worlds = Worktrees::new(".", ".eja/worlds", ".eja", &control()).unwrap();
        let world = worlds.reserve(41, &control()).unwrap();
        let expected = std::env::current_dir().unwrap().join(".eja/worlds/41");
        assert_eq!(world.root, expected.canonicalize().unwrap());
        worlds.settle(&world, &control()).unwrap();
        worlds.release(&world);
        let alias = Worktrees::new(".", ".eja/worlds/../worlds", ".eja", &control()).unwrap();
        assert!(alias.reserve(41, &control()).is_err());
        return;
    }
    let state = State::new("relative-world");
    let project = project_with_one_commit(&state);
    let sub = project.join("sub");
    std::fs::create_dir_all(&sub).unwrap();
    let result = std::process::Command::new(std::env::current_exe().unwrap())
        .current_dir(&sub)
        .env("EJA_RELATIVE_WORLD_CHILD", "1")
        .args([
            "--exact",
            "relative_roots_resolve_before_project_is_rebased_to_git_top",
            "--nocapture",
        ])
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{} {}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
}

#[test]
fn cancelled_world_setup_does_not_create_the_state_root() {
    let state = State::new("cancel-world-setup");
    let project = project_with_one_commit(&state);
    let root = project.join(".eja/new-worlds");
    let cancelled = Control::new(
        Duration::from_secs(10),
        Arc::new(std::sync::atomic::AtomicBool::new(true)),
    );
    assert!(Worktrees::new(&project, &root, project.join(".eja"), &cancelled).is_err());
    assert!(!root.exists());
}
