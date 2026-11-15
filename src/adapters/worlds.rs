//! Worlds: where one execution may change things, and how that comes back.
//!
//! The kernel reserves a world before a run and settles it after. A world is a
//! lease — where this run may write — and nothing else: what the run did is the
//! backend's account, measured against the reservation the backend kept.
use super::process;
use crate::{Control, Result};
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    path::{Path, PathBuf},
    process::Command,
    sync::Mutex,
};

/// A project larger than this is reported as truncated instead of scanned.
const ENTRIES: usize = 20_000;
/// Names shown per change class before the rest are counted.
const NAMES: usize = 6;
/// Releasing a world is cleanup, and it runs after the run's own budget may have
/// expired: it gets this declared bound of its own instead of inheriting a spent
/// control. Cleanup never decides whether a run succeeded.
const RELEASE: std::time::Duration = std::time::Duration::from_secs(30);

type Inventory = BTreeMap<String, (u64, i64)>;

/// What one run did to one world: the host's measurement, never the runner's claim.
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

/// Every regular file in one world, with its size and modification time.
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

/// One world reserved for one execution: where it may write.
pub struct World {
    task: u64,
    pub label: String,
    pub root: PathBuf,
}

/// Reserve, settle, reconcile and release the world one execution may change.
pub trait Worlds: Send + Sync {
    fn reserve(&self, task: u64, control: &Control) -> Result<World>;
    /// The host's account of a finished run, including whatever makes the world
    /// durable. The account carries a one-line `change`, measured against the
    /// reservation this backend kept — not against the runner's claims.
    fn settle(&self, world: &World, control: &Control) -> Result<Value>;
    /// Bring one run's world back into the shared world.
    fn reconcile(&self, task: u64, control: &Control) -> Result<Value>;
    fn release(&self, world: &World);
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
    /// truncated run still gets a full measurement of the world it left.
    fn delta(&self, root: &Path, skip: &Path, control: &Control) -> ProjectDelta {
        let (after, truncated) = snapshot(root, skip, control);
        ProjectDelta::between(&self.before, &after, self.complete, !truncated)
    }
}

type Open = Mutex<HashMap<u64, Reservation>>;

fn keep(open: &Open, task: u64, reservation: Reservation) {
    open.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(task, reservation);
}

fn take(open: &Open, task: u64) -> Result<Reservation> {
    open.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .remove(&task)
        .ok_or_else(|| "this world has no open reservation".into())
}

fn forget(open: &Open, task: u64) {
    open.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .remove(&task);
}

/// The default: every execution shares one world, so only one may run at a time.
pub struct Shared {
    project: PathBuf,
    private: PathBuf,
    open: Open,
}

impl Shared {
    pub fn new(project: impl Into<PathBuf>, private: impl Into<PathBuf>) -> Self {
        Self {
            project: project.into(),
            private: private.into(),
            open: Mutex::new(HashMap::new()),
        }
    }
}

impl Worlds for Shared {
    fn reserve(&self, task: u64, control: &Control) -> Result<World> {
        let mut open = self
            .open
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // One mutable world has one owner. A host that wants two runs at once
        // must give them isolated worlds, so this refuses instead of letting two
        // runs write the same directory.
        if !open.is_empty() {
            return Err(
                "the shared world has one owner; a second run needs an isolated world".into(),
            );
        }
        open.insert(
            task,
            Reservation::start(&self.project, &self.private, control),
        );
        // The reservation is recorded; the map must not stay locked while the
        // world is handed back.
        drop(open);
        Ok(World {
            task,
            label: "shared".to_string(),
            root: self.project.clone(),
        })
    }

    fn settle(&self, world: &World, control: &Control) -> Result<Value> {
        let reservation = take(&self.open, world.task)?;
        let delta = reservation.delta(&world.root, &self.private, control);
        Ok(json!({"world": "shared", "change": delta.line()}))
    }

    fn reconcile(&self, _task: u64, _control: &Control) -> Result<Value> {
        Ok(json!({"world": "shared", "merged": true,
                  "detail": "the run happened in the shared world"}))
    }

    fn release(&self, world: &World) {
        forget(&self.open, world.task);
    }
}

/// One git worktree and one branch per execution: parallel runs cannot overwrite
/// each other, and the result is a ref the host may reconcile explicitly.
pub struct Worktrees {
    project: PathBuf,
    root: PathBuf,
    private: PathBuf,
    open: Open,
    saved: Mutex<HashSet<u64>>,
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

impl Worktrees {
    /// A world per run needs a committed project: a worktree branches from HEAD.
    pub fn new(
        project: impl Into<PathBuf>,
        root: impl Into<PathBuf>,
        private: impl Into<PathBuf>,
        control: &Control,
    ) -> Result<Self> {
        control.check()?;
        let project = project.into();
        let root = root.into();
        let private = private.into();
        std::fs::create_dir_all(&root)?;
        let root = root.canonicalize()?;
        let private = private
            .canonicalize()
            .or_else(|_| std::path::absolute(&private))?;
        let top = git(&project, &["rev-parse", "--show-toplevel"], control)
            .map_err(|_| "isolated worlds need a git project")?;
        git(&project, &["rev-parse", "--verify", "HEAD"], control)
            .map_err(|_| "isolated worlds need one commit to branch from")?;
        Ok(Self {
            project: PathBuf::from(top),
            root,
            private,
            open: Mutex::new(HashMap::new()),
            saved: Mutex::new(HashSet::new()),
        })
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

impl Worlds for Worktrees {
    fn reserve(&self, task: u64, control: &Control) -> Result<World> {
        let root = self.root.join(task.to_string());
        if root.exists() {
            return Err(format!("world already exists; recover it at {}", root.display()).into());
        }
        let branch = self.branch(task);
        let head = git(&self.project, &["rev-parse", "--verify", "HEAD"], control)?;
        // Never replace a previous run's durable branch, even after its worktree
        // was released. A new execution must use a new task id.
        if git(
            &self.project,
            &[
                "show-ref",
                "--verify",
                "--quiet",
                &format!("refs/heads/{branch}"),
            ],
            control,
        )
        .is_ok()
        {
            return Err(format!("world branch already exists: {branch}").into());
        }
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
            Reservation::start(&root, &self.private, control),
        );
        // An isolated world starts at a commit: uncommitted project changes stay
        // behind, and the label carries the commit the run started from.
        Ok(World {
            task,
            label: format!("worktree {branch} at {}", &head[..head.len().min(12)]),
            root,
        })
    }

    fn settle(&self, world: &World, control: &Control) -> Result<Value> {
        let reservation = take(&self.open, world.task)?;
        let delta = reservation.delta(&world.root, &self.private, control);
        git(&world.root, &["add", "-A", "--", "."], control)?;
        git(
            &world.root,
            &[
                "commit",
                "--quiet",
                "--allow-empty",
                "-m",
                "eja: settle world",
            ],
            control,
        )?;
        let commit = git(&world.root, &["rev-parse", "HEAD"], control)?;
        self.saved
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(world.task);
        Ok(json!({
            "world": world.label,
            "change": delta.line(),
            "commit": commit,
        }))
    }

    fn reconcile(&self, task: u64, control: &Control) -> Result<Value> {
        let branch = self.branch(task);
        match git(
            &self.project,
            &["merge", "--no-ff", "--no-edit", &branch],
            control,
        ) {
            Ok(output) => Ok(json!({
                "world": branch,
                "merged": true,
                "detail": output.lines().next().unwrap_or_default(),
            })),
            Err(error) => {
                let conflicts = git(
                    &self.project,
                    &["diff", "--name-only", "--diff-filter=U"],
                    control,
                )
                .unwrap_or_default();
                let _ = git(&self.project, &["merge", "--abort"], control);
                Ok(json!({
                    "world": branch,
                    "merged": false,
                    "conflicts": conflicts.lines().take(20).collect::<Vec<_>>(),
                    "detail": error.to_string(),
                }))
            }
        }
    }

    fn release(&self, world: &World) {
        // The branch keeps the work; the directory is only a workspace.
        forget(&self.open, world.task);
        if !self
            .saved
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&world.task)
        {
            return;
        }
        let control = crate::Control::new(RELEASE, std::sync::Arc::default());
        let text = world.root.to_string_lossy().to_string();
        let _ = git(
            &self.project,
            &["worktree", "remove", "--force", &text],
            &control,
        );
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
        // The second inventory never saw the world: it proves nothing.
        let delta = ProjectDelta::between(&before, &empty, true, false);
        assert!(delta.deleted.is_empty(), "{delta:?}");
        assert!(delta.created.is_empty());
        assert!(delta.truncated);
        assert!(
            delta.line().contains("inventory truncated"),
            "{}",
            delta.line()
        );
        // An unobserved earlier world cannot invent creations either.
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
}
