//! Worlds: where one execution may change things, and how that comes back.
//!
//! The kernel reserves a world before a run and settles it after. Two worlds
//! never run against each other, so parallel executions stay separate accounts
//! of the same identity. The host decides what isolation means: the shared
//! project, one git worktree per run, or something else entirely.
use super::process;
use crate::{Control, Result};
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    process::Command,
};

/// A project larger than this is reported as truncated instead of scanned.
const ENTRIES: usize = 20_000;
/// Names shown per change class before the rest are counted.
const NAMES: usize = 6;

type Inventory = BTreeMap<String, (u64, i64)>;

/// What one run did to one world: the host's measurement, never the runner's claim.
#[derive(Debug, Default)]
pub struct ProjectDelta {
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

    pub fn line(&self) -> String {
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
                .map(|time| time.as_nanos() as i64)
                .unwrap_or(0);
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

/// One world reserved for one execution, with the state it started from.
pub struct World {
    pub label: String,
    pub root: PathBuf,
    before: Inventory,
    complete: bool,
    skip: PathBuf,
}

impl World {
    /// What this world now contains, measured against what it held at reservation.
    pub fn change(&self, control: &Control) -> ProjectDelta {
        let (after, truncated) = snapshot(&self.root, &self.skip, control);
        ProjectDelta::between(&self.before, &after, self.complete, !truncated)
    }
}

/// Reserve, settle, reconcile and release the world one execution may change.
pub trait Worlds: Send + Sync {
    fn reserve(&self, task: u64, control: &Control) -> Result<World>;
    /// The host's account of a finished run, including any backend step that
    /// makes the world durable (a commit, a snapshot).
    fn settle(&self, world: &World, change: &ProjectDelta, control: &Control) -> Result<Value>;
    /// Bring one run's world back into the shared world.
    fn reconcile(&self, task: u64, control: &Control) -> Result<Value>;
    fn release(&self, world: &World);
}

fn prepare(label: String, root: PathBuf, skip: &Path, control: &Control) -> Result<World> {
    // snapshot reports whether the walk was truncated, not whether it was whole.
    let (before, truncated) = snapshot(&root, skip, control);
    Ok(World {
        label,
        root,
        before,
        complete: !truncated,
        skip: skip.to_path_buf(),
    })
}

/// The default: every execution shares one world, so only one may run at a time.
pub struct Shared {
    pub project: PathBuf,
    pub private: PathBuf,
}

impl Worlds for Shared {
    fn reserve(&self, _task: u64, control: &Control) -> Result<World> {
        prepare(
            "shared".to_string(),
            self.project.clone(),
            &self.private,
            control,
        )
    }
    fn settle(&self, _world: &World, change: &ProjectDelta, _control: &Control) -> Result<Value> {
        Ok(json!({"world": "shared", "change": change.line()}))
    }
    fn reconcile(&self, _task: u64, _control: &Control) -> Result<Value> {
        Ok(
            json!({"world": "shared", "merged": true, "detail": "the run happened in the shared world"}),
        )
    }
    fn release(&self, _world: &World) {}
}

/// One git worktree and one branch per execution: parallel runs cannot overwrite
/// each other, and the result is a ref the host may reconcile explicitly.
pub struct Worktrees {
    project: PathBuf,
    root: PathBuf,
    private: PathBuf,
}

fn git(cwd: &Path, args: &[&str], control: &Control) -> Result<String> {
    let mut command = Command::new("git");
    command
        .arg("-C")
        .arg(cwd)
        .args([
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_AUTHOR_NAME", "eja")
        .env("GIT_AUTHOR_EMAIL", "eja@local")
        .env("GIT_COMMITTER_NAME", "eja")
        .env("GIT_COMMITTER_EMAIL", "eja@local");
    let bytes = process::output(&mut command, control, crate::aja::FRAME_LIMIT)?;
    Ok(String::from_utf8_lossy(&bytes).trim().to_string())
}

impl Worktrees {
    /// A world per run needs a committed project: a worktree branches from HEAD.
    pub fn new(
        project: impl Into<PathBuf>,
        root: impl Into<PathBuf>,
        private: impl Into<PathBuf>,
        control: &Control,
    ) -> Result<Self> {
        let project = project.into();
        let root = root.into();
        let top = git(&project, &["rev-parse", "--show-toplevel"], control)
            .map_err(|_| "isolated worlds need a git project")?;
        git(&project, &["rev-parse", "--verify", "HEAD"], control)
            .map_err(|_| "isolated worlds need one commit to branch from")?;
        std::fs::create_dir_all(&root)?;
        Ok(Self {
            project: PathBuf::from(top),
            root,
            private: private.into(),
        })
    }

    fn branch(task: u64) -> String {
        format!("eja/aja-{task}")
    }
}

impl Worlds for Worktrees {
    fn reserve(&self, task: u64, control: &Control) -> Result<World> {
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
        let branch = Self::branch(task);
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
        let label = format!("worktree {branch}");
        let mut world = prepare(label, root, &self.private, control)?;
        // An isolated world starts at a commit: uncommitted project changes stay
        // behind, and the digest says so because the label carries the commit.
        world.label = format!("{} at {}", world.label, &head[..head.len().min(12)]);
        Ok(world)
    }

    fn settle(&self, world: &World, change: &ProjectDelta, control: &Control) -> Result<Value> {
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
        Ok(json!({
            "world": world.label,
            "change": change.line(),
            "commit": commit,
        }))
    }

    fn reconcile(&self, task: u64, control: &Control) -> Result<Value> {
        let branch = Self::branch(task);
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
        let control = crate::Control::new(std::time::Duration::from_secs(30), Default::default());
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
