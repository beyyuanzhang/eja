//! The worlds a run may change: one shared, one per run, settled and reconciled.

mod support;

use eja::adapters::{
    Ports, Shared, WorkspaceWorlds, Worktrees, World, WorldKind, WorldRecord, Worlds,
};
use eja::{
    turn_at_with_options, Action, Assignment, Control, Kind, TurnOptions, Worker, WorkspaceGrant,
};
use serde_json::{json, Value};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
use std::time::Duration;
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};
use support::{control, Done, Script, State};

#[derive(Default)]
struct ReconcileProbe {
    calls: AtomicUsize,
}

impl Worlds for ReconcileProbe {
    fn reserve(&self, _: u64, _: Option<&Path>, _: &Control) -> eja::Result<World> {
        Err("probe does not reserve worlds".into())
    }

    fn settle(&self, _: &World, _: &Control) -> eja::Result<Value> {
        Err("probe does not settle worlds".into())
    }

    fn reconcile(&self, _: u64, _: Option<&Path>, _: &Control) -> eja::Result<Value> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(json!({"merged": true}))
    }

    fn reconcile_recorded(
        &self,
        _: u64,
        _: Option<&Path>,
        _: Option<&WorldRecord>,
        _: &Control,
    ) -> eja::Result<Value> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(json!({"merged": true}))
    }

    fn release(&self, _: &World) {}
}

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
        workspace: None,
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
    let world = worlds.reserve(1, None, &control).expect("reserve");
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

    let outcome = worlds.reconcile(1, None, &control).expect("reconcile");
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
    let world = worlds.reserve(2, None, &control).expect("reserve");
    std::fs::write(world.root.join("base.txt"), "from the run\n").unwrap();
    worlds.settle(&world, &control).expect("settle");
    // The shared world moved on in the meantime.
    std::fs::write(project.join("base.txt"), "from the host\n").unwrap();
    git(&project, &["add", "-A"]);
    git(&project, &["commit", "--quiet", "-m", "host move"]);

    let outcome = worlds.reconcile(2, None, &control).expect("reconcile");
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
fn a_recorded_shared_run_never_enters_the_current_isolated_backend() {
    let state = State::new("recorded-shared");
    let project = tree(&state);
    let probe = Arc::new(ReconcileProbe::default());
    let current: Arc<dyn Worlds> = probe.clone();
    let worlds = WorkspaceWorlds::new(
        current.clone(),
        BTreeMap::from([(project.clone(), current)]),
    );
    let record = WorldRecord {
        kind: WorldKind::Shared,
        root: project.display().to_string(),
        ..WorldRecord::default()
    };
    let outcome = worlds
        .reconcile_recorded(7, Some(&project), Some(&record), &control())
        .expect("a shared run needs no current backend work");
    assert_eq!(outcome["merged"], json!(true), "{outcome}");
    assert_eq!(
        probe.calls.load(Ordering::SeqCst),
        0,
        "today's isolated default must not process a run recorded as shared"
    );
}

#[test]
fn a_recorded_worktree_without_a_git_service_is_refused_without_touching_the_project() {
    let state = State::new("recorded-no-git");
    let project = project_with_one_commit(&state);
    let shared: Arc<dyn Worlds> = Arc::new(Shared::new(project.clone(), state.path().join(".eja")));
    let worlds = WorkspaceWorlds::new(shared.clone(), BTreeMap::from([(project.clone(), shared)]));
    let record = WorldRecord {
        kind: WorldKind::Worktree,
        root: project.display().to_string(),
        branch: Some("eja/aja-8-deadbeef".into()),
        commit: None,
    };
    let error = worlds
        .reconcile_recorded(8, Some(&project), Some(&record), &control())
        .expect_err("absent Git history cannot land a worktree");
    assert!(
        error.to_string().contains("no Git world service"),
        "{error}"
    );
    assert_eq!(
        std::fs::read_to_string(project.join("base.txt")).unwrap(),
        "one\n"
    );
    assert!(!project.join("out.txt").exists());
}

#[test]
fn a_recorded_worktree_merges_its_stable_commit_when_history_is_available() {
    let state = State::new("recorded-worktree");
    let project = project_with_one_commit(&state);
    let control = control();
    let backend = Arc::new(
        Worktrees::new(
            &project,
            state.path().join(".eja/worlds"),
            state.path().join(".eja"),
            &control,
        )
        .expect("worktrees"),
    );
    let world = backend.reserve(9, None, &control).expect("reserve");
    std::fs::write(world.root.join("out.txt"), "recorded result\n").unwrap();
    let settled = backend.settle(&world, &control).expect("settle");
    let record: WorldRecord =
        serde_json::from_value(settled["world_record"].clone()).expect("recorded world");
    assert!(record.commit.is_some(), "{settled}");
    backend.release(&world);

    let shared: Arc<dyn Worlds> = Arc::new(Shared::new(project.clone(), state.path().join(".eja")));
    let history: Arc<dyn Worlds> = backend;
    let worlds = WorkspaceWorlds::new(shared.clone(), BTreeMap::from([(project.clone(), shared)]))
        .with_history(BTreeMap::from([(project.clone(), history)]));
    let outcome = worlds
        .reconcile_recorded(9, Some(&project), Some(&record), &control)
        .expect("recorded branch merges");
    assert_eq!(outcome["merged"], json!(true), "{outcome}");
    assert_eq!(
        std::fs::read_to_string(project.join("out.txt")).unwrap(),
        "recorded result\n"
    );
}

#[test]
fn a_bare_legacy_worktree_uses_the_only_available_history() {
    let state = State::new("recorded-legacy-worktree");
    let project = project_with_one_commit(&state);
    let control = control();
    let backend = Arc::new(
        Worktrees::new(
            &project,
            state.path().join(".eja/worlds"),
            state.path().join(".eja"),
            &control,
        )
        .expect("worktrees"),
    );
    let world = backend.reserve(11, None, &control).expect("reserve");
    std::fs::write(world.root.join("legacy.txt"), "legacy result\n").unwrap();
    let settled = backend.settle(&world, &control).expect("settle");
    let mut record: WorldRecord =
        serde_json::from_value(settled["world_record"].clone()).expect("recorded world");
    record.root.clear();
    backend.release(&world);

    let shared: Arc<dyn Worlds> = Arc::new(Shared::new(project.clone(), state.path().join(".eja")));
    let history: Arc<dyn Worlds> = backend;
    let worlds = WorkspaceWorlds::new(shared, BTreeMap::new())
        .with_history(BTreeMap::from([(project.clone(), history)]));
    let outcome = worlds
        .reconcile_recorded(11, None, Some(&record), &control)
        .expect("an unambiguous legacy record still reconciles");
    assert_eq!(outcome["merged"], json!(true), "{outcome}");
    assert_eq!(
        std::fs::read_to_string(project.join("legacy.txt")).unwrap(),
        "legacy result\n"
    );
}

#[test]
fn a_recorded_world_for_another_workspace_is_refused() {
    let state = State::new("recorded-wrong-root");
    let project = tree(&state);
    let other = tree(&State::new("recorded-wrong-root-other"));
    let shared: Arc<dyn Worlds> = Arc::new(Shared::new(project.clone(), state.path().join(".eja")));
    let worlds = WorkspaceWorlds::new(shared.clone(), BTreeMap::from([(project.clone(), shared)]));
    let record = WorldRecord {
        kind: WorldKind::Worktree,
        root: other.display().to_string(),
        branch: Some("eja/aja-10-deadbeef".into()),
        commit: None,
    };
    let error = worlds
        .reconcile_recorded(10, Some(&project), Some(&record), &control())
        .expect_err("a task keeps its originally assigned workspace");
    assert!(error.to_string().contains("belongs to"), "{error}");
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
    let world = worlds.reserve(3, None, &control).expect("reserve");
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
    let world = shared.reserve(1, None, &control).expect("reserve");
    assert_eq!(world.root, project);
    let account = shared.settle(&world, &control).expect("settle");
    assert_eq!(account["change"], serde_json::json!("unchanged"));
    let outcome = shared.reconcile(1, None, &control).expect("reconcile");
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
    let a = first.reserve(1, None, &control).expect("first reserve");
    let b = second.reserve(1, None, &control).expect("second reserve");
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
    let first = shared
        .reserve(1, None, &control)
        .expect("first reservation");
    // A second run may not write the same directory while the first holds it.
    let second = shared.reserve(2, None, &control);
    assert!(second.is_err(), "a shared world has one owner");
    shared.settle(&first, &control).expect("settle");
    shared.release(&first);
    // Once settled, the world is free again.
    let third = shared.reserve(3, None, &control).expect("after settlement");
    shared.release(&third);
}

#[test]
fn settling_a_world_twice_is_refused() {
    let state = State::new("twice");
    let project = state.path().join("project");
    std::fs::create_dir_all(&project).unwrap();
    let shared = Shared::new(project, state.path().join(".eja"));
    let control = control();
    let world = shared.reserve(1, None, &control).expect("reserve");
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

/// A worker that writes one marker into the world it was given, so a test can
/// see which root a run actually held.
struct Marker;

impl Worker for Marker {
    fn execute(&self, assignment: &Assignment, _: &Control) -> String {
        let root = std::path::Path::new(&assignment.workspace);
        std::fs::write(
            root.join(format!("marker-{}.txt", assignment.task_id)),
            "here\n",
        )
        .expect("marker");
        format!(
            "1 steps, 0 tool calls, ended: message\nmarked {}",
            assignment.task_id
        )
    }
}

#[test]
fn two_granted_workspaces_hold_their_own_roots_at_once() {
    let state = State::new("two-roots");
    let store = state.store();
    let root = tree(&state);
    let alpha = root.join("alpha");
    let beta = root.join("beta");
    std::fs::create_dir_all(&alpha).unwrap();
    std::fs::create_dir_all(&beta).unwrap();
    let worlds = Arc::new(Shared::new(alpha.clone(), root.join(".eja")));
    let brain = Script::once(vec![
        Action::OpenTask {
            title: "in alpha".into(),
            request: "mark alpha".into(),
            acceptance: "marked".into(),
            workspace: Some("alpha".into()),
            evidence: None,
            run: true,
        },
        Action::OpenTask {
            title: "in beta".into(),
            request: "mark beta".into(),
            acceptance: "marked".into(),
            workspace: Some("beta".into()),
            evidence: None,
            run: true,
        },
    ]);
    let options = TurnOptions {
        workspaces: vec![
            WorkspaceGrant {
                id: "default".into(),
                label: "default".into(),
                root: alpha.display().to_string(),
            },
            WorkspaceGrant {
                id: "alpha".into(),
                label: "alpha".into(),
                root: alpha.display().to_string(),
            },
            WorkspaceGrant {
                id: "beta".into(),
                label: "beta".into(),
                root: beta.display().to_string(),
            },
        ],
        limits: eja::TurnLimits {
            slots: 2,
            ..Default::default()
        },
        ..Default::default()
    };
    let out = turn_at_with_options(
        &store,
        "mark both",
        &brain,
        &Marker,
        &Ports {
            worlds: Some(worlds.as_ref()),
            ..Default::default()
        },
        eja::now_utc(),
        &options,
    )
    .expect("turn");
    assert_eq!(out.dispatched, vec![1, 2]);
    assert!(
        alpha.join("marker-1.txt").exists(),
        "alpha holds its own run"
    );
    assert!(beta.join("marker-2.txt").exists(), "beta holds its own run");
    assert!(!alpha.join("marker-2.txt").exists());
    let tasks = out.context().tasks;
    assert_eq!(tasks[0].workspace, alpha.display().to_string());
    assert_eq!(tasks[1].workspace, beta.display().to_string());
    // The root is fixed in the journal, not re-resolved from the grant.
    let reopened = eja::State::load(&state.reopen()).expect("reload");
    assert_eq!(
        reopened.task(2).expect("task").workspace,
        beta.display().to_string()
    );
}

#[test]
fn a_task_naming_an_unavailable_workspace_is_refused() {
    let state = State::new("ungranted");
    let store = state.store();
    let brain = Script::once(vec![Action::OpenTask {
        title: "elsewhere".into(),
        request: "work elsewhere".into(),
        acceptance: "done".into(),
        workspace: Some("nowhere".into()),
        evidence: None,
        run: true,
    }]);
    let out = turn_at_with_options(
        &store,
        "go",
        &brain,
        &Marker,
        &Ports::default(),
        eja::now_utc(),
        &TurnOptions {
            workspaces: vec![WorkspaceGrant {
                id: "default".into(),
                label: "default".into(),
                root: "/tmp".into(),
            }],
            ..Default::default()
        },
    )
    .expect("turn");
    assert!(out.dispatched.is_empty());
    assert!(
        out.context().tasks.is_empty(),
        "no task opens on a refused name"
    );
    assert!(
        out.context()
            .refused
            .iter()
            .any(|note| note.contains("nowhere")),
        "{:?}",
        out.context().refused
    );
}

#[test]
fn a_task_cannot_dispatch_after_its_workspace_is_no_longer_available() {
    let state = State::new("stale-workspace");
    let store = state.store();
    let root = tree(&state);
    let old = root.join("old");
    let current = root.join("current");
    std::fs::create_dir_all(&old).unwrap();
    std::fs::create_dir_all(&current).unwrap();

    let open = Script::once(vec![Action::OpenTask {
        title: "old target".into(),
        request: "mark the old target".into(),
        acceptance: "marked".into(),
        workspace: Some("old".into()),
        evidence: None,
        run: false,
    }]);
    let old_grant = TurnOptions {
        workspaces: vec![WorkspaceGrant {
            id: "old".into(),
            label: "old".into(),
            root: old.display().to_string(),
        }],
        ..Default::default()
    };
    let opened = turn_at_with_options(
        &store,
        "open an old target",
        &open,
        &Done::default(),
        &Ports::default(),
        eja::now_utc(),
        &old_grant,
    )
    .expect("open");
    assert_eq!(opened.dispatched, Vec::<u64>::new());

    let dispatch = Script::once(vec![Action::Dispatch { task_id: 1 }]);
    let current_grant = TurnOptions {
        workspaces: vec![WorkspaceGrant {
            id: "current".into(),
            label: "current".into(),
            root: current.display().to_string(),
        }],
        ..Default::default()
    };
    let out = turn_at_with_options(
        &store,
        "dispatch the old target",
        &dispatch,
        &Marker,
        &Ports::default(),
        eja::now_utc(),
        &current_grant,
    )
    .expect("dispatch");

    assert!(out.dispatched.is_empty());
    assert!(!old.join("marker-1.txt").exists());
    assert!(
        out.context()
            .refused
            .iter()
            .any(|note| note.contains("not available")),
        "{:?}",
        out.context().refused
    );
}

#[test]
fn each_granted_git_root_reconciles_into_its_own_project() {
    let state = State::new("two-git-roots");
    let root = tree(&state);
    let control = control();
    let mut projects = Vec::new();
    let mut backends: std::collections::BTreeMap<PathBuf, Arc<dyn Worlds>> =
        std::collections::BTreeMap::new();
    for name in ["alpha", "beta"] {
        let project = root.join(name);
        std::fs::create_dir_all(&project).unwrap();
        git(&project, &["init", "--quiet"]);
        std::fs::write(project.join("base.txt"), "one\n").unwrap();
        git(&project, &["add", "-A"]);
        git(&project, &["commit", "--quiet", "-m", "base"]);
        let worlds: Arc<dyn Worlds> = Arc::new(
            Worktrees::new(
                &project,
                root.join(".eja/worlds").join(name),
                root.join(".eja"),
                &control,
            )
            .expect("worktrees"),
        );
        backends.insert(project.clone(), worlds);
        projects.push(project);
    }
    let default = backends[&projects[0]].clone();
    let worlds = WorkspaceWorlds::new(default, backends);
    let first = worlds
        .reserve(1, Some(&projects[0]), &control)
        .expect("alpha");
    let second = worlds
        .reserve(2, Some(&projects[1]), &control)
        .expect("beta");
    std::fs::write(first.root.join("out.txt"), "alpha\n").unwrap();
    std::fs::write(second.root.join("out.txt"), "beta\n").unwrap();
    worlds.settle(&first, &control).expect("settle alpha");
    worlds.settle(&second, &control).expect("settle beta");
    let merged = worlds
        .reconcile(1, Some(&projects[0]), &control)
        .expect("reconcile alpha");
    assert_eq!(merged["merged"], serde_json::json!(true), "{merged}");
    let merged = worlds
        .reconcile(2, Some(&projects[1]), &control)
        .expect("reconcile beta");
    assert_eq!(merged["merged"], serde_json::json!(true), "{merged}");
    assert_eq!(
        std::fs::read_to_string(projects[0].join("out.txt")).unwrap(),
        "alpha\n"
    );
    assert_eq!(
        std::fs::read_to_string(projects[1].join("out.txt")).unwrap(),
        "beta\n"
    );
    worlds.release(&first);
    worlds.release(&second);
    assert!(
        !first.root.exists(),
        "release after settle removes the worktree of the owning backend"
    );
    assert!(
        !second.root.exists(),
        "release after settle removes the worktree of the owning backend"
    );
}
