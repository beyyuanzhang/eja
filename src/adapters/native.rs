//! Single native worker path: same binary, private AJA mode, host-owned effects.
use super::{Control, Model, ToolContext, ToolGateway};
use crate::fold;
use crate::{
    aja::{self, Limits, Request, Start},
    Assignment, Result, Worker,
};
use serde_json::{json, Value};
use std::{
    collections::HashSet,
    fs,
    io::{BufReader, Write},
    path::{Path, PathBuf},
    process::{Child, ChildStdin, Command, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver},
        Arc, Mutex,
    },
    thread::JoinHandle,
    time::Duration,
};

/// The node's complete context, appended as it happens and frozen with the node.
/// The host sees every model response and tool call, so freezing the record never
/// depends on the node cooperating.
struct ContextLog {
    file: fs::File,
    assistants: usize,
    entries: usize,
}

impl ContextLog {
    fn create(node: &Path) -> Result<Self> {
        let file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(node.join("context.jsonl"))?;
        Ok(Self {
            file,
            assistants: 0,
            entries: 0,
        })
    }

    fn record(&mut self, entry: Value) -> Result<()> {
        if entry.get("kind").and_then(Value::as_str) == Some("assistant") {
            self.assistants += 1;
        }
        let mut line = serde_json::to_vec(&entry)?;
        line.push(b'\n');
        self.file.write_all(&line)?;
        self.file.flush()?;
        self.entries += 1;
        Ok(())
    }
}

/// Set by the host on SIGINT: cancellation is a host capability, not a model action.
pub use crate::INTERRUPTED;

struct Process {
    child: Child,
    input: Option<ChildStdin>,
    receiver: Option<Receiver<std::result::Result<Request, String>>>,
    reader: Option<JoinHandle<()>>,
}

impl Process {
    fn start(binary: &Path, work: &Path, start: Start) -> Result<Self> {
        if serde_json::to_vec(&start)?.len() > aja::FRAME_LIMIT {
            return Err("AJA start frame exceeds limit".into());
        }
        let child = Command::new(binary)
            .arg("--aja-worker")
            .env_clear()
            .current_dir(work)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?;
        let mut process = Self {
            child,
            input: None,
            receiver: None,
            reader: None,
        };
        process.input = process.child.stdin.take();
        let output = process.child.stdout.take().ok_or("missing worker stdout")?;
        let (sender, receiver) = mpsc::sync_channel(1);
        let reader = std::thread::spawn(move || {
            let mut reader = BufReader::new(output);
            loop {
                let request: std::result::Result<Request, String> =
                    aja::receive(&mut reader).map_err(|e| e.to_string());
                let stop = matches!(request, Ok(Request::Finish { .. }) | Err(_));
                if sender.send(request).is_err() || stop {
                    break;
                }
            }
        });
        process.receiver = Some(receiver);
        process.reader = Some(reader);
        aja::send(
            process.input.as_mut().ok_or("missing worker stdin")?,
            &start,
        )?;
        Ok(process)
    }
    fn next(&self, control: &Control) -> Result<Request> {
        loop {
            control.check()?;
            match self
                .receiver
                .as_ref()
                .ok_or("worker closed")?
                .recv_timeout(Duration::from_millis(10))
            {
                Ok(Ok(request)) => return Ok(request),
                Ok(Err(e)) => return Err(e.into()),
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(e) => return Err(e.into()),
            }
        }
    }
}

/// A project larger than this is reported as truncated instead of scanned.
const PROJECT_ENTRIES: usize = 20_000;
/// Names shown per change class before the rest are counted.
const PROJECT_NAMES: usize = 6;

type ProjectState = std::collections::BTreeMap<String, (u64, i64)>;

/// Every regular file in the project, with its size and modification time. The
/// worker's own state (`skip`) is not part of what the node did to the project.
fn snapshot_project(root: &Path, skip: &Path, control: &Control) -> (ProjectState, bool) {
    let mut state = ProjectState::new();
    let mut truncated = false;
    let mut pending = vec![root.to_path_buf()];
    while let Some(dir) = pending.pop() {
        if control.check().is_err() {
            truncated = true;
            break;
        }
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            if control.check().is_err() || state.len() >= PROJECT_ENTRIES {
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

/// What the node did to the project: the world's answer, not the node's claim.
#[derive(Debug, Default)]
struct ProjectDelta {
    created: Vec<String>,
    modified: Vec<String>,
    deleted: Vec<String>,
    truncated: bool,
}

impl ProjectDelta {
    /// A change is claimed only when the inventory that could observe it is
    /// complete: an inventory that was cut short (budget, cancellation, entry
    /// cap) is expressible as "unobserved", never as a created/deleted file.
    fn between(
        before: &ProjectState,
        after: &ProjectState,
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
        if self.created.is_empty() && self.modified.is_empty() && self.deleted.is_empty() {
            return if self.truncated {
                "project: no change observed (inventory truncated)".into()
            } else {
                "project: unchanged".into()
            };
        }
        let names = |list: &[String]| {
            let shown: Vec<&str> = list
                .iter()
                .take(PROJECT_NAMES)
                .map(String::as_str)
                .collect();
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
        let tail = if self.truncated {
            " (inventory truncated)"
        } else {
            ""
        };
        format!("project: {}{tail}", parts.join(", "))
    }
}

/// What one run of a node produced: the transcript's account of it.
struct Outcome<'a> {
    text: &'a str,
    reason: &'a str,
    steps: usize,
    counts: &'a std::collections::BTreeMap<String, usize>,
}

struct Lease<'a> {
    active: &'a Mutex<Option<ActiveRun>>,
}

impl Drop for Lease<'_> {
    fn drop(&mut self) {
        if let Some(run) = self.active.lock().unwrap_or_else(|e| e.into_inner()).take() {
            run.cancelled.store(true, Ordering::SeqCst);
        }
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        self.receiver.take();
        // Native AJA never spawns tool processes: all effects stay in the host.
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}

struct ActiveRun {
    id: u64,
    cancelled: Arc<AtomicBool>,
}

pub struct NativeWorker {
    binary: PathBuf,
    root: PathBuf,
    project: PathBuf,
    model: Arc<dyn Model>,
    tools: Arc<dyn ToolGateway>,
    limits: Limits,
    /// The shadow repository that keeps one commit per node: the fossils.
    checkpoints: PathBuf,
    /// The host's own state, never part of what a node did to the project.
    state: PathBuf,
    active: Mutex<Option<ActiveRun>>,
}

impl NativeWorker {
    pub fn new(
        binary: impl Into<PathBuf>,
        root: impl Into<PathBuf>,
        project: impl Into<PathBuf>,
        model: Arc<dyn Model>,
        tools: Arc<dyn ToolGateway>,
    ) -> Result<Self> {
        let root = root.into();
        fs::create_dir_all(&root)?;
        let root = root.canonicalize()?;
        let project = project.into().canonicalize()?;
        let state = root
            .parent()
            .ok_or("node root has no state directory")?
            .to_path_buf();
        if project.starts_with(&state) {
            return Err(
                "the private state directory must not contain the project workspace".into(),
            );
        }
        let checkpoints = state.join("checkpoints");
        Ok(Self {
            binary: binary.into().canonicalize()?,
            root,
            project,
            model,
            tools,
            limits: Limits::default(),
            checkpoints,
            state,
            active: Mutex::new(None),
        })
    }
    pub fn with_limits(mut self, limits: Limits) -> Result<Self> {
        limits.validate()?;
        self.limits = limits;
        Ok(self)
    }
    pub fn cancel(&self, node_id: u64) -> bool {
        if let Some(flag) = self
            .active
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .filter(|run| run.id == node_id)
        {
            flag.cancelled.store(true, Ordering::SeqCst);
            true
        } else {
            false
        }
    }
    pub fn active_count(&self) -> usize {
        usize::from(
            self.active
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .is_some(),
        )
    }

    fn run(
        &self,
        assignment: &Assignment,
        control: &Control,
        log: &mut ContextLog,
        counts: &mut std::collections::BTreeMap<String, usize>,
    ) -> Result<(String, String)> {
        let mut definitions = self.tools.definitions(assignment);
        definitions.push(super::record::definition());
        let allowed: HashSet<String> = definitions
            .iter()
            .filter_map(|d| d["function"]["name"].as_str().map(String::from))
            .collect();
        if allowed.len() != definitions.len() {
            return Err("invalid or duplicate host tool definitions".into());
        }
        log.record(json!({"kind":"start", "view_version":1, "messages":fold::fresh_messages(assignment,&self.limits)}))?;
        let ancestors = super::record::lineage(&self.root, assignment.resume, control)?;
        let inherited =
            super::record::replay(&self.root, &ancestors, self.limits.context_bytes, control)?;
        let messages = fold::prepare_messages(assignment, &self.limits, inherited)?;
        let start = Start {
            context_bytes: self.limits.context_bytes,
            messages,
        };
        let mut process = Process::start(&self.binary, &self.project, start)?;
        let context = ToolContext {
            node_id: assignment.task_id,
            project: &self.project,
            private: self.root.parent().ok_or("missing state root")?,
            control,
        };
        let mut steps = 0;
        loop {
            let response = match process.next(control)? {
                Request::Model { messages } => {
                    steps += 1;
                    if let Some(limit) = self.limits.steps.filter(|limit| steps > *limit) {
                        return Ok((format!("AJA step budget of {limit} turns exhausted; continue in a new node"), "budget".into()));
                    }
                    if serde_json::to_vec(&messages)?.len() > self.limits.context_bytes {
                        return Ok((
                            "AJA context budget exhausted; continue in a new node".into(),
                            "budget".into(),
                        ));
                    }
                    let value = self.model.complete(
                        &messages,
                        &definitions,
                        &self.limits,
                        control,
                        false,
                    )?;
                    if serde_json::to_vec(&value)?.len() > self.limits.response_bytes {
                        return Err("AJA model output limit reached".into());
                    }
                    if let Err(error) = aja::validate_assistant(&value) {
                        log.record(
                            json!({"kind":"model_error","message":value,"error":error.to_string()}),
                        )?;
                        return Err(error);
                    }
                    log.record(json!({
                        "kind": "assistant",
                        "message": value,
                    }))?;
                    value
                }
                Request::Tool {
                    id,
                    name,
                    arguments,
                } => {
                    control.check()?;
                    *counts.entry(name.clone()).or_insert(0) += 1;
                    let value = if !allowed.contains(&name) {
                        json!({"error":"tool not allowed"})
                    } else if name == super::record::READ_TOOL {
                        super::record::read(
                            &self.root,
                            assignment.task_id,
                            &ancestors,
                            &arguments,
                            control,
                        )
                        .unwrap_or_else(|error| json!({"error":error.to_string()}))
                    } else {
                        match self.tools.call(&context, &name, &arguments) {
                            Ok(output) => output,
                            Err(error) => {
                                json!({"error":error.to_string()})
                            }
                        }
                    };
                    if serde_json::to_vec(&value)?.len() > aja::FRAME_LIMIT {
                        return Err("AJA tool output limit reached".into());
                    }
                    let entry = log.entries;
                    log.record(json!({
                        "kind": "tool",
                        "id": id,
                        "name": name,
                        "arguments": arguments,
                        "result": value,
                    }))?;
                    crate::fold::tool_result(&value, assignment.task_id, entry)
                }
                Request::Finish { text, reason } => {
                    control.check()?;
                    if !matches!(reason.as_str(), "message" | "budget" | "error") {
                        return Err("invalid node finish reason".into());
                    }
                    return Ok((text, reason));
                }
            };
            control.check()?;
            // A tool result is bounded by the frame budget, not the model's reply
            // budget: the tool owns its own output bound, the record keeps the
            // whole result, and the transcript folds against its context budget.
            if serde_json::to_vec(&response)?.len() > aja::FRAME_LIMIT {
                return Err("AJA tool output limit reached".into());
            }
            aja::send(process.input.as_mut().ok_or("worker closed")?, &response)?;
        }
    }

    /// Freeze the world this node left: one commit in the state's shadow
    /// repository. The commit is the fossil EJA reads a node's files from.
    fn checkpoint(&self, assignment: &Assignment, control: &Control) -> Result<String> {
        let git = |args: &[&str]| -> Result<String> {
            let mut command = std::process::Command::new("git");
            command
                .arg("--git-dir")
                .arg(&self.checkpoints)
                .arg("--work-tree")
                .arg(&self.project)
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
            let bytes = super::process::output(&mut command, control, aja::FRAME_LIMIT)?;
            Ok(String::from_utf8_lossy(&bytes).trim().to_string())
        };
        if !self.checkpoints.join("HEAD").exists() {
            fs::create_dir_all(&self.checkpoints)?;
            git(&["init", "--quiet"])?;
        }
        let mut spec = vec![
            "add".to_string(),
            "-A".into(),
            "--".into(),
            ".".into(),
            ":!.git".into(),
        ];
        if let Ok(relative) = self.state.strip_prefix(&self.project) {
            let relative = relative.to_string_lossy().to_string();
            if !relative.is_empty() {
                let exclude = format!(":!{relative}");
                spec.push(exclude);
            }
        }
        git(&spec.iter().map(String::as_str).collect::<Vec<_>>())?;
        let message = format!("node {} at {}", assignment.task_id, assignment.issued_at);
        git(&["commit", "--quiet", "--allow-empty", "-m", &message])?;
        git(&["rev-parse", "HEAD"])
    }

    fn seal(
        &self,
        node: &Path,
        assignment: &Assignment,
        outcome: Outcome<'_>,
        delta: &ProjectDelta,
        control: &Control,
    ) -> Result<String> {
        let (checkpoint, checkpoint_error) = match self.checkpoint(assignment, control) {
            Ok(commit) => (Some(commit), None),
            Err(error) => (None, Some(error.to_string())),
        };
        let context = node.join("context.jsonl");
        if let Ok(file) = fs::OpenOptions::new().write(true).open(&context) {
            file.sync_all()?;
            let mut permissions = file.metadata()?.permissions();
            permissions.set_readonly(true);
            fs::set_permissions(&context, permissions)?;
        }
        let context_bytes = fs::metadata(&context).map(|m| m.len()).unwrap_or(0);
        let calls: usize = outcome.counts.values().sum();
        let histogram: Vec<String> = {
            let mut pairs: Vec<_> = outcome.counts.iter().collect();
            pairs.sort_by(|a, b| b.1.cmp(a.1).then(a.0.cmp(b.0)));
            pairs
                .iter()
                .map(|(name, count)| format!("{name}x{count}"))
                .collect()
        };
        let (text, reason) = (outcome.text, outcome.reason);
        let steps = outcome.steps;
        let mut digest = format!(
            "{steps} steps, {calls} tool calls{}, {}, ended: {reason}\n{text}",
            if histogram.is_empty() {
                String::new()
            } else {
                format!(" ({})", histogram.join(" "))
            },
            delta.line(),
        );
        if let Some(error) = &checkpoint_error {
            digest.push_str(&format!("\ncheckpoint unavailable: {error}"));
        }
        let manifest = json!({
            "assignment": assignment,
            "digest": digest,
            "checkpoint": checkpoint,
            "checkpoint_error": checkpoint_error,
            "context": "context.jsonl",
            "context_bytes": context_bytes,
            "frozen": true,
        });
        let temp = node.join("node.json.tmp");
        fs::write(&temp, serde_json::to_vec_pretty(&manifest)?)?;
        fs::File::open(&temp)?.sync_all()?;
        fs::rename(temp, node.join("node.json"))?;
        // The manifest is part of the frozen record, like the context and the checkpoint.
        let manifest = node.join("node.json");
        if let Ok(file) = fs::OpenOptions::new().write(true).open(&manifest) {
            let mut permissions = file.metadata()?.permissions();
            permissions.set_readonly(true);
            fs::set_permissions(&manifest, permissions)?;
        }
        Ok(digest)
    }
}

impl Worker for NativeWorker {
    fn execute(&self, assignment: &Assignment, parent: &Control) -> String {
        if let Err(error) = parent.check() {
            return format!("0 steps, 0 tool calls, ended: budget\n{error}");
        }
        let node = self.root.join(assignment.task_id.to_string());
        // Reserve once. Restart or replay cannot overwrite an existing node, even after failure.
        if assignment.task_id == 0 {
            return "0 steps, 0 tool calls, ended: error\ninvalid node id".into();
        }
        let cancelled = Arc::new(AtomicBool::new(false));
        {
            let mut active = self.active.lock().unwrap_or_else(|e| e.into_inner());
            if active.is_some() {
                return "0 steps, 0 tool calls, ended: budget\nexecution owner is busy".into();
            }
            if let Err(e) = fs::create_dir(&node) {
                return format!(
                    "0 steps, 0 tool calls, ended: error\nnode already reserved or unavailable: {e}"
                );
            }
            *active = Some(ActiveRun {
                id: assignment.task_id,
                cancelled: cancelled.clone(),
            });
        }
        let _lease = Lease {
            active: &self.active,
        };
        let control = parent
            .child(Duration::from_millis(self.limits.timeout_ms))
            .cancelled_by(cancelled.clone());
        let mut counts = std::collections::BTreeMap::new();
        let mut log = match ContextLog::create(&node) {
            Ok(log) => log,
            Err(error) => {
                return format!(
                    "0 steps, 0 tool calls, ended: error\nnode context is unavailable: {error}"
                )
            }
        };
        let before = snapshot_project(&self.project, &self.state, &control);
        let result: Result<(String, String)> =
            self.run(assignment, &control, &mut log, &mut counts);
        let steps = log.assistants;
        let (after, truncated) = snapshot_project(&self.project, &self.state, &control);
        let delta = ProjectDelta::between(&before.0, &after, !before.1, !truncated);
        cancelled.store(true, Ordering::SeqCst); // revoke this run before publishing anything
        let (text, reason) = match result {
            Ok(outcome) => outcome,
            Err(error) => {
                let reason =
                    if error.is::<fold::ContextBudget>() || error.is::<crate::ControlError>() {
                        "budget"
                    } else {
                        "error"
                    };
                (error.to_string(), reason.to_string())
            }
        };
        let _ = log.record(json!({"kind": "finish", "text": &text, "reason": &reason}));
        drop(log);
        self.seal(
            &node,
            assignment,
            Outcome {
                text: &text,
                reason: &reason,
                steps,
                counts: &counts,
            },
            &delta,
            parent,
        )
        .unwrap_or_else(|e| {
            format!("{steps} steps, 0 tool calls, ended: error\nnode could not be frozen: {e}")
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inventory(entries: &[(&str, u64)]) -> ProjectState {
        entries
            .iter()
            .map(|(name, size)| ((*name).to_string(), (*size, 0)))
            .collect()
    }

    #[test]
    fn a_partial_inventory_reports_only_what_it_observed() {
        let before = inventory(&[("kept.txt", 1), ("other.txt", 1)]);
        let empty = ProjectState::new();
        // The second inventory never saw the project: it proves nothing.
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
        assert!(
            delta.line().starts_with("project: ~1 modified"),
            "{}",
            delta.line()
        );
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
        assert!(!delta.line().contains("truncated"), "{}", delta.line());
    }
}
