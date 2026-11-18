//! Execution environments: where one run may change things, and how that comes back.
//!
//! The host leases an environment before a run and settles it after. The
//! provider owns the lease and measures the run against what it reserved.
use super::process;
use crate::{Control, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, HashMap},
    path::{Path, PathBuf},
    process::Command,
    sync::{Arc, Mutex},
};

/// A project larger than this is reported as truncated instead of scanned.
pub const ENTRIES: usize = 20_000;
/// Names shown per change class before the rest are counted.
const NAMES: usize = 6;
/// Releasing an environment is cleanup, and it runs after the run's own budget may have
/// expired: it gets this declared bound of its own instead of inheriting a spent
/// control. Cleanup never decides whether a run succeeded.
const RELEASE: std::time::Duration = std::time::Duration::from_secs(30);

type Inventory = BTreeMap<String, (u64, i64)>;

/// Canonicalize the existing prefix of a path even when its final directory has
/// not been created yet. Platform aliases such as `/var` and `/private/var` must
/// not produce two spellings for the same environment boundary.
pub fn canonical_boundary(path: &Path) -> PathBuf {
    if let Ok(canonical) = path.canonicalize() {
        return canonical;
    }
    let (Some(parent), Some(name)) = (path.parent(), path.file_name()) else {
        return path.to_path_buf();
    };
    canonical_boundary(parent).join(name)
}

/// What one run did to one environment: the host's measurement, never the runner's claim.
#[derive(Debug, Default)]
struct ProjectDelta {
    created: Vec<String>,
    modified: Vec<String>,
    deleted: Vec<String>,
    truncated: bool,
}

impl ProjectDelta {
    /// A change is claimed only when the inventory that could observe it is
    /// complete: an inventory cut short by budget, cancellation or the entry cap
    /// is expressible as "unobserved", never as a created or deleted path.
    fn between(
        before: &Inventory,
        after: &Inventory,
        before_complete: bool,
        after_complete: bool,
    ) -> Self {
        let mut delta = Self {
            truncated: !(before_complete && after_complete),
            ..Self::default()
        };
        for (name, stamp) in after {
            match before.get(name) {
                None if before_complete => delta.created.push(name.clone()),
                Some(was) if was != stamp => delta.modified.push(name.clone()),
                _ => {}
            }
        }
        if after_complete {
            for name in before.keys() {
                if !after.contains_key(name) {
                    delta.deleted.push(name.clone());
                }
            }
        }
        delta
    }

    fn line(&self) -> String {
        let names = |list: &[String]| {
            let shown: Vec<&str> = list.iter().take(NAMES).map(String::as_str).collect();
            let rest = list.len().saturating_sub(shown.len());
            let more = if rest == 0 {
                String::new()
            } else {
                format!(", …{rest} more")
            };
            format!(" ({}{more})", shown.join(", "))
        };
        let mut parts = Vec::new();
        if !self.created.is_empty() {
            parts.push(format!(
                "+{} created{}",
                self.created.len(),
                names(&self.created)
            ));
        }
        if !self.modified.is_empty() {
            parts.push(format!(
                "~{} modified{}",
                self.modified.len(),
                names(&self.modified)
            ));
        }
        if !self.deleted.is_empty() {
            parts.push(format!(
                "-{} deleted{}",
                self.deleted.len(),
                names(&self.deleted)
            ));
        }
        if parts.is_empty() {
            return if self.truncated {
                "no change observed (inventory truncated)".into()
            } else {
                "unchanged".into()
            };
        }
        let tail = if self.truncated {
            " (inventory truncated)"
        } else {
            ""
        };
        format!("{}{tail}", parts.join(", "))
    }
}

/// Every regular file in one environment, with its size and modification time.
fn snapshot(root: &Path, skip: &Path, control: &Control) -> (Inventory, bool) {
    let mut state = Inventory::new();
    let mut truncated = false;
    let mut pending = vec![root.to_path_buf()];
    while let Some(dir) = pending.pop() {
        if control.check().is_err() {
            truncated = true;
            break;
        }
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            if control.check().is_err() || state.len() >= ENTRIES {
                truncated = true;
                break;
            }
            let path = entry.path();
            let Ok(metadata) = entry.path().symlink_metadata() else {
                continue;
            };
            if metadata.file_type().is_symlink() {
                continue;
            }
            if metadata.is_dir() {
                if path != skip && entry.file_name() != ".git" {
                    pending.push(path);
                }
                continue;
            }
            if !metadata.is_file() {
                continue;
            }
            let Ok(relative) = path.strip_prefix(root) else {
                continue;
            };
            let stamp = metadata
                .modified()
                .ok()
                .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
                .map_or(0, |time| i64::try_from(time.as_nanos()).unwrap_or(i64::MAX));
            state.insert(
                relative.to_string_lossy().replace('\\', "/"),
                (metadata.len(), stamp),
            );
        }
        if truncated {
            break;
        }
    }
    (state, truncated)
}

/// One environment reserved for one execution: where it may write.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EnvironmentKind {
    /// A record written before the kind was stated.
    #[default]
    Unknown,
    Shared,
    Worktree,
}

/// The durable part of a reserved environment: what kind it was, the
/// granted root it belonged to, and the ref that holds the run's result.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnvironmentRecord {
    #[serde(default)]
    pub kind: EnvironmentKind,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub root: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
}

impl EnvironmentRecord {
    #[must_use]
    pub fn of(kind: EnvironmentKind, root: &Path) -> Self {
        Self {
            kind,
            root: root
                .canonicalize()
                .unwrap_or_else(|_| root.to_path_buf())
                .display()
                .to_string(),
            ..Self::default()
        }
    }
}

/// One provider-owned reservation for a single run.
pub struct EnvironmentLease {
    task: u64,
    pub label: String,
    pub root: PathBuf,
    /// What reconciliation needs to return to this run, not to today's default.
    pub record: EnvironmentRecord,
}

/// Reserve, settle, reconcile and release the environment one execution may change.
pub trait EnvironmentProvider: Send + Sync {
    /// `root` names the granted workspace this run works in; `None` is the
    /// host's default. A backend that owns several roots isolates them apart.
    fn reserve(
        &self,
        task: u64,
        root: Option<&Path>,
        control: &Control,
    ) -> Result<EnvironmentLease>;
    /// The host's account of a finished run, including whatever makes the environment
    /// durable. The account carries a one-line `change`, measured against the
    /// reservation this backend kept — not against the runner's claims.
    fn settle(&self, environment: &EnvironmentLease, control: &Control) -> Result<Value>;
    /// Bring one run's recorded environment back into its base environment.
    fn reconcile(
        &self,
        root: Option<&Path>,
        record: &EnvironmentRecord,
        control: &Control,
    ) -> Result<Value>;
    fn release(&self, environment: &EnvironmentLease);
}

/// What a backend remembers between reserve and settle.
struct Reservation {
    before: Inventory,
    complete: bool,
}

impl Reservation {
    fn start(root: &Path, skip: &Path, control: &Control) -> Self {
        // snapshot reports whether the walk was truncated, not whether it was whole.
        let (before, truncated) = snapshot(root, skip, control);
        Self {
            before,
            complete: !truncated,
        }
    }

    /// The account of one run. Settlement runs under its own deadline, so a
    /// truncated run still gets a full measurement of the environment it left.
    fn delta(&self, root: &Path, skip: &Path, control: &Control) -> ProjectDelta {
        let (after, truncated) = snapshot(root, skip, control);
        ProjectDelta::between(&self.before, &after, self.complete, !truncated)
    }
}

/// What a backend holds per run: the root it owns, and the inventory it keeps.
type Held = (PathBuf, Reservation);
type Open = Mutex<HashMap<u64, Held>>;

fn keep(open: &Open, task: u64, root: &Path, reservation: Reservation) {
    open.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(task, (root.to_path_buf(), reservation));
}

fn take(open: &Open, task: u64) -> Result<Reservation> {
    open.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .remove(&task)
        .map(|(_, reservation)| reservation)
        .ok_or_else(|| "this environment has no open reservation".into())
}

fn forget(open: &Open, task: u64) {
    open.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .remove(&task);
}

/// The default: every execution shares one environment, so only one may run at a time.
pub struct SharedEnvironment {
    project: PathBuf,
    private: PathBuf,
    open: Open,
}

impl SharedEnvironment {
    pub fn new(project: impl Into<PathBuf>, private: impl Into<PathBuf>) -> Self {
        let private = canonical_boundary(&private.into());
        Self {
            project: project.into(),
            private,
            open: Mutex::new(HashMap::new()),
        }
    }

    fn root_of<'a>(&'a self, root: Option<&'a Path>) -> &'a Path {
        root.unwrap_or(&self.project)
    }
}

impl EnvironmentProvider for SharedEnvironment {
    fn reserve(
        &self,
        task: u64,
        root: Option<&Path>,
        control: &Control,
    ) -> Result<EnvironmentLease> {
        let root = self.root_of(root);
        if !root.is_dir() {
            return Err(format!("workspace is not a directory: {}", root.display()).into());
        }
        // Tool boundaries compare paths structurally. Canonicalize at the lease
        // boundary so a workspace reached through a platform alias (for example
        // `/var` and `/private/var`) cannot make the private state appear outside
        // the environment it belongs to.
        let root = root.canonicalize()?;
        let mut open = self
            .open
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // One mutable environment has one owner per root. Two runs in two granted
        // workspaces may hold their own roots at once; a second run in a root
        // that is held is refused instead of letting two runs write it.
        if open.values().any(|(held, _)| held == &root) {
            return Err(
                "the shared environment has one owner; a second run needs an isolated environment"
                    .into(),
            );
        }
        open.insert(
            task,
            (
                root.clone(),
                Reservation::start(&root, &self.private, control),
            ),
        );
        // The reservation is recorded; the map must not stay locked while the
        // environment is handed back.
        drop(open);
        Ok(EnvironmentLease {
            task,
            label: "shared".to_string(),
            root: root.clone(),
            record: EnvironmentRecord::of(EnvironmentKind::Shared, &root),
        })
    }

    fn settle(&self, environment: &EnvironmentLease, control: &Control) -> Result<Value> {
        let reservation = take(&self.open, environment.task)?;
        let delta = reservation.delta(&environment.root, &self.private, control);
        Ok(json!({"environment": "shared", "change": delta.line()}))
    }

    fn reconcile(
        &self,
        _root: Option<&Path>,
        record: &EnvironmentRecord,
        _control: &Control,
    ) -> Result<Value> {
        if !record.root.is_empty()
            && normalize_run_root(&record.root)
                != normalize_run_root(&self.project.display().to_string())
        {
            return Err(format!(
                "reconcile refused: the run belongs to {}, not {}",
                record.root,
                self.project.display()
            )
            .into());
        }
        match record.kind {
            EnvironmentKind::Shared => Ok(json!({
                "environment": "shared",
                "merged": true,
                "detail": "the run happened in the shared environment"
            })),
            EnvironmentKind::Worktree => Err(
                "reconcile refused: the run used an isolated Git worktree, but this runtime has no Git environment provider to merge its recorded branch"
                    .into(),
            ),
            EnvironmentKind::Unknown => Err(
                "reconcile refused: the run record does not identify the environment provider it used"
                    .into(),
            ),
        }
    }

    fn release(&self, environment: &EnvironmentLease) {
        forget(&self.open, environment.task);
    }
}

/// One Git worktree and one branch per execution: parallel runs cannot overwrite
/// each other, and the result is a ref the host may reconcile explicitly.
pub struct GitWorktrees {
    project: PathBuf,
    granted: PathBuf,
    root: PathBuf,
    private: PathBuf,
    open: Open,
}

/// One hermetic git invocation. Both durability paths — a run's fossil and a
/// worktree's settle — commit a tree through this, so a project's hooks, signing
/// settings and identity can never reach a run.
pub fn git_at(
    git_dir: Option<&Path>,
    tree: &Path,
    args: &[&str],
    control: &Control,
) -> Result<String> {
    let mut command = hermetic(git_dir, tree);
    command.args(args);
    let bytes = process::output(&mut command, control, crate::aja::FRAME_LIMIT)?;
    Ok(String::from_utf8_lossy(&bytes).trim().to_string())
}

/// One git invocation a project cannot influence.
///
/// Every path that runs git — freezing a run, settling a worktree, reading a
/// frozen file — starts here, so a project's hooks, signing settings, identity
/// and global configuration reach none of them.
pub fn hermetic(git_dir: Option<&Path>, tree: &Path) -> Command {
    let mut command = Command::new("git");
    match git_dir {
        Some(dir) => command
            .arg("--git-dir")
            .arg(dir)
            .arg("--work-tree")
            .arg(tree),
        None => command.arg("-C").arg(tree),
    };
    command
        .args([
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "commit.gpgsign=false",
        ])
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_AUTHOR_NAME", "eja")
        .env("GIT_AUTHOR_EMAIL", "eja@local")
        .env("GIT_COMMITTER_NAME", "eja")
        .env("GIT_COMMITTER_EMAIL", "eja@local");
    command
}

fn git(cwd: &Path, args: &[&str], control: &Control) -> Result<String> {
    git_at(None, cwd, args, control)
}

impl GitWorktrees {
    /// An isolated environment per run needs a committed project.
    pub fn new(
        project: impl Into<PathBuf>,
        root: impl Into<PathBuf>,
        private: impl Into<PathBuf>,
        control: &Control,
    ) -> Result<Self> {
        let project = project.into();
        let project = project.canonicalize().unwrap_or(project);
        let root = root.into();
        let top = git(&project, &["rev-parse", "--show-toplevel"], control)
            .map_err(|error| format!("isolated environments need a Git project: {error}"))?;
        git(&project, &["rev-parse", "--verify", "HEAD"], control).map_err(|error| {
            format!("isolated environments need one commit to branch from: {error}")
        })?;
        std::fs::create_dir_all(&root)?;
        let root = root.canonicalize()?;
        let private = canonical_boundary(&private.into());
        Ok(Self {
            project: PathBuf::from(top),
            granted: project,
            root,
            private,
            open: Mutex::new(HashMap::new()),
        })
    }

    /// A grant inside this git project belongs to it; a foreign root does not.
    fn owns(&self, root: Option<&Path>) -> bool {
        root.is_none_or(|root| root.starts_with(&self.project) || root == self.granted)
    }

    /// One branch per (state directory, task). Two instances that share a
    /// project would otherwise both own `eja/aja-1`, and resigning the branch
    /// would delete the other instance's refs. The suffix is derived from the
    /// state directory itself so the name never depends on the Rust version.
    fn branch(&self, task: u64) -> String {
        let mut hash = 0xcbf2_9ce4_8422_2325_u64;
        for byte in self.root.to_string_lossy().as_bytes() {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
        // The branch name carries the low 32 bits: enough to separate two state
        // directories over one project, and short enough for a ref.
        format!(
            "eja/aja-{task}-{:08x}",
            u32::try_from(hash & 0xffff_ffff).unwrap_or(0)
        )
    }
}

impl EnvironmentProvider for GitWorktrees {
    fn reserve(
        &self,
        task: u64,
        root: Option<&Path>,
        control: &Control,
    ) -> Result<EnvironmentLease> {
        if !self.owns(root) {
            return Err("an isolated environment belongs to its own Git project".into());
        }
        let root = self.root.join(task.to_string());
        if root.exists() {
            let text = root.to_string_lossy().to_string();
            let _ = git(
                &self.project,
                &["worktree", "remove", "--force", &text],
                control,
            );
            std::fs::remove_dir_all(&root).ok();
        }
        let branch = self.branch(task);
        let head = git(&self.project, &["rev-parse", "--verify", "HEAD"], control)?;
        // A restart must not reuse a branch: the new execution gets a new id.
        let _ = git(&self.project, &["branch", "-D", &branch], control);
        let root_text = root.to_string_lossy().to_string();
        git(
            &self.project,
            &[
                "worktree", "add", "--quiet", "-b", &branch, &root_text, &head,
            ],
            control,
        )?;
        keep(
            &self.open,
            task,
            &self.project,
            Reservation::start(&root, &self.private, control),
        );
        // An isolated environment starts at a commit: uncommitted project changes stay
        // behind, and the label carries the commit the run started from.
        Ok(EnvironmentLease {
            task,
            label: format!("worktree {branch} at {}", &head[..head.len().min(12)]),
            root,
            record: EnvironmentRecord {
                kind: EnvironmentKind::Worktree,
                root: self.granted.display().to_string(),
                branch: Some(branch),
                commit: None,
            },
        })
    }

    fn settle(&self, environment: &EnvironmentLease, control: &Control) -> Result<Value> {
        let reservation = take(&self.open, environment.task)?;
        let delta = reservation.delta(&environment.root, &self.private, control);
        git(&environment.root, &["add", "-A", "--", "."], control)?;
        git(
            &environment.root,
            &[
                "commit",
                "--quiet",
                "--allow-empty",
                "-m",
                "eja: settle environment",
            ],
            control,
        )?;
        let commit = git(&environment.root, &["rev-parse", "HEAD"], control)?;
        let mut record = environment.record.clone();
        record.commit = Some(commit.clone());
        Ok(json!({
            "environment": environment.label,
            "change": delta.line(),
            "commit": commit,
            "kind": record.kind,
            "root": record.root,
            "branch": record.branch,
            "environment_record": record,
        }))
    }

    fn reconcile(
        &self,
        root: Option<&Path>,
        record: &EnvironmentRecord,
        control: &Control,
    ) -> Result<Value> {
        if !self.owns(root) {
            return Err("an isolated environment belongs to its own Git project".into());
        }
        if !record.root.is_empty()
            && normalize_run_root(&record.root)
                != normalize_run_root(&self.granted.display().to_string())
        {
            return Err(format!(
                "reconcile refused: the run belongs to {}, not {}",
                record.root,
                self.granted.display()
            )
            .into());
        }
        match record.kind {
            EnvironmentKind::Shared => Ok(json!({
                "environment": "shared",
                "merged": true,
                "detail": "the run happened in the shared environment"
            })),
            EnvironmentKind::Worktree => {
                let branch = record
                    .branch
                    .as_deref()
                    .ok_or("reconcile refused: the isolated run record names no branch artifact")?;
                merge_branch(&self.project, branch, record.commit.as_deref(), control)
            }
            EnvironmentKind::Unknown => Err(
                "reconcile refused: the run record does not identify the environment provider it used"
                    .into(),
            ),
        }
    }

    fn release(&self, environment: &EnvironmentLease) {
        // The branch keeps the work; the directory is only a workspace.
        forget(&self.open, environment.task);
        let control = crate::Control::new(RELEASE, std::sync::Arc::default());
        let text = environment.root.to_string_lossy().to_string();
        let _ = git(
            &self.project,
            &["worktree", "remove", "--force", &text],
            &control,
        );
    }
}

fn merge_branch(
    project: &Path,
    branch: &str,
    commit: Option<&str>,
    control: &Control,
) -> Result<Value> {
    let target = if let Some(commit) = commit {
        if !matches!(commit.len(), 40 | 64) || !commit.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            return Err("reconcile refused: the run record names an invalid commit".into());
        }
        commit
    } else if branch.starts_with("eja/aja-")
        && branch
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'-' | b'_'))
    {
        branch
    } else {
        return Err("reconcile refused: the run record names an invalid branch".into());
    };
    match git(project, &["merge", "--no-ff", "--no-edit", target], control) {
        Ok(output) => Ok(json!({
            "environment": branch,
            "merged": true,
            "detail": output.lines().next().unwrap_or_default(),
        })),
        Err(error) => {
            let conflicts = git(
                project,
                &["diff", "--name-only", "--diff-filter=U"],
                control,
            )
            .unwrap_or_default();
            let _ = git(project, &["merge", "--abort"], control);
            Ok(json!({
                "environment": branch,
                "merged": false,
                "conflicts": conflicts.lines().take(20).collect::<Vec<_>>(),
                "detail": error.to_string(),
            }))
        }
    }
}

fn normalize_run_root(root: &str) -> PathBuf {
    PathBuf::from(root)
        .canonicalize()
        .unwrap_or_else(|_| PathBuf::from(root))
}

/// One backend per granted root.
///
/// A run reserves the environment of the workspace its task named, and every root
/// keeps the isolation rule of its own backend. Roots without a backend of
/// their own share the default.
pub struct WorkspaceEnvironments {
    default: Arc<dyn EnvironmentProvider>,
    roots: BTreeMap<PathBuf, Arc<dyn EnvironmentProvider>>,
    /// Backends that can land a historical isolated run. A shared execution
    /// backend still gives a restart one way to merge a branch recorded by an
    /// earlier isolated composition.
    history: BTreeMap<PathBuf, Arc<dyn EnvironmentProvider>>,
    /// The provider that reserved each run: an isolated environment's root is a
    /// worktree path, not the granted root, so settle and release must return
    /// to the backend that owns the reservation.
    held: Mutex<HashMap<u64, Arc<dyn EnvironmentProvider>>>,
}

impl WorkspaceEnvironments {
    pub fn new(
        default: Arc<dyn EnvironmentProvider>,
        roots: BTreeMap<PathBuf, Arc<dyn EnvironmentProvider>>,
    ) -> Self {
        let history = roots.clone();
        Self {
            default,
            roots,
            history,
            held: Mutex::new(HashMap::new()),
        }
    }

    #[must_use]
    pub fn with_history(
        mut self,
        history: BTreeMap<PathBuf, Arc<dyn EnvironmentProvider>>,
    ) -> Self {
        self.history = history;
        self
    }

    fn backend(&self, root: Option<&Path>) -> &Arc<dyn EnvironmentProvider> {
        root.and_then(|root| self.roots.get(root))
            .unwrap_or(&self.default)
    }

    fn reserved(&self, environment: &EnvironmentLease) -> Arc<dyn EnvironmentProvider> {
        self.held
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&environment.task)
            .cloned()
            .unwrap_or_else(|| self.backend(Some(&environment.root)).clone())
    }
}

impl EnvironmentProvider for WorkspaceEnvironments {
    fn reserve(
        &self,
        task: u64,
        root: Option<&Path>,
        control: &Control,
    ) -> Result<EnvironmentLease> {
        let backend = self.backend(root);
        let environment = backend.reserve(task, root, control)?;
        self.held
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(task, backend.clone());
        Ok(environment)
    }

    fn settle(&self, environment: &EnvironmentLease, control: &Control) -> Result<Value> {
        let backend = self.reserved(environment);
        // The reservation stays held through release: settle alone must not
        // hand the environment back to a root lookup that no longer knows it.
        backend.settle(environment, control)
    }

    fn reconcile(
        &self,
        root: Option<&Path>,
        record: &EnvironmentRecord,
        control: &Control,
    ) -> Result<Value> {
        let task_root = root.map(|root| normalize_run_root(&root.display().to_string()));
        let recorded_root = (!record.root.is_empty()).then(|| normalize_run_root(&record.root));
        if let (Some(task_root), Some(recorded_root)) = (&task_root, &recorded_root) {
            if task_root != recorded_root {
                return Err(format!(
                    "reconcile refused: the run belongs to {}, not the task's fixed workspace {}",
                    record.root,
                    task_root.display()
                )
                .into());
            }
        }
        match record.kind {
            EnvironmentKind::Shared => Ok(json!({
                "environment": "shared",
                "merged": true,
                "detail": "the run happened in the shared environment"
            })),
            EnvironmentKind::Worktree => {
                let key = recorded_root.or(task_root).or_else(|| {
                    if self.history.len() == 1 {
                        self.history.keys().next().cloned()
                    } else {
                        None
                    }
                }).ok_or(
                    "reconcile refused: the run record does not bind its isolated environment to a workspace",
                )?;
                self.history.get(&key).map_or_else(
                    || {
                        Err(format!(
                            "reconcile refused: no Git environment provider is configured for {}; the run's recorded branch cannot be merged",
                            key.display()
                        )
                        .into())
                    },
                    |backend| backend.reconcile(root, record, control),
                )
            }
            EnvironmentKind::Unknown => Err(
                "reconcile refused: the run record does not identify the environment provider it used"
                    .into(),
            ),
        }
    }

    fn release(&self, environment: &EnvironmentLease) {
        let backend = self.reserved(environment);
        self.held
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&environment.task);
        backend.release(environment);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inventory(entries: &[(&str, u64)]) -> Inventory {
        entries
            .iter()
            .map(|(name, size)| ((*name).to_string(), (*size, 0)))
            .collect()
    }

    #[test]
    fn a_partial_inventory_reports_only_what_it_observed() {
        let before = inventory(&[("kept.txt", 1), ("other.txt", 1)]);
        let empty = Inventory::new();
        // The second inventory never saw the environment: it proves nothing.
        let delta = ProjectDelta::between(&before, &empty, true, false);
        assert!(delta.deleted.is_empty(), "{delta:?}");
        assert!(delta.created.is_empty());
        assert!(delta.truncated);
        assert!(
            delta.line().contains("inventory truncated"),
            "{}",
            delta.line()
        );
        // An unobserved earlier environment cannot invent creations either.
        let delta = ProjectDelta::between(&empty, &before, false, true);
        assert!(delta.created.is_empty(), "{delta:?}");
        assert!(delta.truncated);
        // What was genuinely compared survives the truncation notice.
        let after = inventory(&[("kept.txt", 2)]);
        let delta = ProjectDelta::between(&before, &after, true, false);
        assert_eq!(delta.modified, vec!["kept.txt".to_string()]);
        assert!(delta.created.is_empty());
        assert!(delta.deleted.is_empty());
        assert!(delta.line().starts_with("~1 modified"), "{}", delta.line());
    }

    #[test]
    fn two_complete_inventories_report_every_change() {
        let before = inventory(&[("kept.txt", 1), ("gone.txt", 1)]);
        let after = inventory(&[("kept.txt", 2), ("new.txt", 1)]);
        let delta = ProjectDelta::between(&before, &after, true, true);
        assert_eq!(delta.created, vec!["new.txt".to_string()]);
        assert_eq!(delta.modified, vec!["kept.txt".to_string()]);
        assert_eq!(delta.deleted, vec!["gone.txt".to_string()]);
        assert!(!delta.truncated);
        assert_eq!(
            delta.line(),
            "+1 created (new.txt), ~1 modified (kept.txt), -1 deleted (gone.txt)"
        );
    }

    #[test]
    fn an_invalid_stable_commit_is_never_replaced_by_the_branch() {
        let control = Control::new(std::time::Duration::from_secs(1), Arc::default());
        let error = merge_branch(
            Path::new("."),
            "eja/aja-1-deadbeef",
            Some("not-a-commit"),
            &control,
        )
        .unwrap_err();
        assert!(error.to_string().contains("invalid commit"), "{error}");
    }
}
