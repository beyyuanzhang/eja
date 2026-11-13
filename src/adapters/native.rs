//! Single native worker path: same binary, private AJA mode, host-owned effects.
use super::{Control, Model, ToolContext, ToolGateway};
use crate::{
    aja::{self, Limits, Request, Start},
    Assignment, Result, Worker,
};
use serde_json::{json, Value};
use std::{
    collections::{HashMap, HashSet},
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
    started: bool,
    assistants: usize,
}

impl ContextLog {
    fn create(node: &Path) -> Result<Self> {
        let file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(node.join("context.jsonl"))?;
        Ok(Self {
            file,
            started: false,
            assistants: 0,
        })
    }

    fn assistants(&self) -> usize {
        self.assistants
    }

    fn record(&mut self, entry: Value) -> Result<()> {
        if entry.get("kind").and_then(Value::as_str) == Some("assistant") {
            self.assistants += 1;
        }
        let mut line = serde_json::to_vec(&entry)?;
        line.push(b'\n');
        self.file.write_all(&line)?;
        self.file.flush()?;
        Ok(())
    }
}

/// Set by the host on SIGINT: cancellation is a host capability, not a model action.
pub static INTERRUPTED: AtomicBool = AtomicBool::new(false);

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
            if INTERRUPTED.load(Ordering::SeqCst) {
                return Err("AJA interrupted".into());
            }
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
fn snapshot_project(root: &Path, skip: &Path) -> (ProjectState, bool) {
    let mut state = ProjectState::new();
    let mut truncated = false;
    let mut pending = vec![root.to_path_buf()];
    while let Some(dir) = pending.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            if state.len() >= PROJECT_ENTRIES {
                truncated = true;
                break;
            }
            let path = entry.path();
            let Ok(metadata) = entry.metadata() else {
                continue;
            };
            if metadata.is_dir() {
                if path != skip {
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
#[derive(Default)]
struct ProjectDelta {
    created: Vec<String>,
    modified: Vec<String>,
    deleted: Vec<String>,
    truncated: bool,
}

impl ProjectDelta {
    fn between(before: &ProjectState, after: &ProjectState, truncated: bool) -> Self {
        let mut delta = Self {
            truncated,
            ..Self::default()
        };
        for (name, stamp) in after {
            match before.get(name) {
                None => delta.created.push(name.clone()),
                Some(was) if was != stamp => delta.modified.push(name.clone()),
                Some(_) => {}
            }
        }
        for name in before.keys() {
            if !after.contains_key(name) {
                delta.deleted.push(name.clone());
            }
        }
        delta
    }
    fn line(&self) -> String {
        if self.created.is_empty() && self.modified.is_empty() && self.deleted.is_empty() {
            return "project: unchanged".into();
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
        if self.truncated {
            parts.push("(truncated)".into());
        }
        format!("project: {}", parts.join(", "))
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
    active: &'a Mutex<HashMap<u64, Arc<AtomicBool>>>,
    id: u64,
    flag: Arc<AtomicBool>,
}

impl Drop for Lease<'_> {
    fn drop(&mut self) {
        self.flag.store(true, Ordering::SeqCst);
        self.active
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&self.id);
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

pub struct NativeWorker {
    binary: PathBuf,
    root: PathBuf,
    project: PathBuf,
    model: Arc<dyn Model>,
    tools: Arc<dyn ToolGateway>,
    limits: Limits,
    /// Declared concurrent node capacity. Nodes are independent processes, so the
    /// host decides how many run at once.
    slots: usize,
    /// The shadow repository that keeps one commit per node: the fossils.
    checkpoints: PathBuf,
    /// The host's own state, never part of what a node did to the project.
    state: PathBuf,
    checkpoint_lock: std::sync::Mutex<()>,
    active: Mutex<HashMap<u64, Arc<AtomicBool>>>,
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
        Ok(Self {
            binary: binary.into().canonicalize()?,
            root: root.canonicalize()?,
            project: project.into().canonicalize()?,
            model,
            tools,
            limits: Limits::default(),
            slots: 1,
            checkpoints: root.parent().unwrap_or(&root).join("checkpoints"),
            state: root.parent().unwrap_or(&root).to_path_buf(),
            checkpoint_lock: std::sync::Mutex::new(()),
            active: Mutex::new(HashMap::new()),
        })
    }
    pub fn with_slots(mut self, slots: usize) -> Result<Self> {
        if slots == 0 {
            return Err("the node capacity must be at least one".into());
        }
        self.slots = slots;
        Ok(self)
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
            .get(&node_id)
        {
            flag.store(true, Ordering::SeqCst);
            true
        } else {
            false
        }
    }
    pub fn active_count(&self) -> usize {
        self.active.lock().unwrap_or_else(|e| e.into_inner()).len()
    }

    fn run(
        &self,
        assignment: &Assignment,
        control: &Control,
        log: &mut ContextLog,
        counts: &mut std::collections::BTreeMap<String, usize>,
    ) -> Result<(String, String)> {
        let definitions = self.tools.definitions(assignment);
        let allowed: HashSet<String> = definitions
            .iter()
            .filter_map(|d| d["function"]["name"].as_str().map(String::from))
            .collect();
        if allowed.len() != definitions.len() {
            return Err("invalid or duplicate host tool definitions".into());
        }
        let start = Start {
            assignment: assignment.clone(),
            limits: self.limits.clone(),
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
                    if !log.started {
                        log.started = true;
                        log.record(json!({"kind": "start", "messages": messages}))?;
                    }
                    steps += 1;
                    if self.limits.steps.is_some_and(|limit| steps > limit)
                        || serde_json::to_vec(&messages)?.len() > self.limits.context_bytes
                    {
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
                    log.record(json!({
                        "kind": "assistant",
                        "message": value
                            .pointer("/choices/0/message")
                            .cloned()
                            .unwrap_or_else(|| value.clone()),
                    }))?;
                    value
                }
                Request::Tool { name, arguments } => {
                    control.check()?;
                    *counts.entry(name.clone()).or_insert(0) += 1;
                    let value = if !allowed.contains(&name) {
                        json!({"error":"tool not allowed"})
                    } else {
                        match self.tools.call(&context, &name, &arguments) {
                            Ok(output) => output.value,
                            Err(error) => {
                                json!({"error":error.to_string()})
                            }
                        }
                    };
                    log.record(json!({
                        "kind": "tool",
                        "name": name,
                        "arguments": arguments,
                        "result": value,
                    }))?;
                    value
                }
                Request::Finish { text, reason } => {
                    control.check()?;
                    return Ok((text, reason));
                }
            };
            control.check()?;
            // A tool result is bounded by the frame budget, not the model's reply
            // budget: the tool owns its own output bound, and the node's transcript
            // compacts against its context budget.
            if serde_json::to_vec(&response)?.len() > aja::FRAME_LIMIT {
                return Err("AJA tool output limit reached".into());
            }
            aja::send(process.input.as_mut().ok_or("worker closed")?, &response)?;
        }
    }

    /// Freeze the world this node left: one commit in the state's shadow
    /// repository. The commit is the fossil EJA reads a node's files from.
    fn checkpoint(&self, assignment: &Assignment) -> Option<String> {
        let _guard = self
            .checkpoint_lock
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let git = |args: &[&str]| -> Result<String> {
            let output = std::process::Command::new("git")
                .arg("--git-dir")
                .arg(&self.checkpoints)
                .arg("--work-tree")
                .arg(&self.project)
                .args(args)
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("GIT_AUTHOR_NAME", "eja")
                .env("GIT_AUTHOR_EMAIL", "eja@local")
                .env("GIT_COMMITTER_NAME", "eja")
                .env("GIT_COMMITTER_EMAIL", "eja@local")
                .output()?;
            if !output.status.success() {
                return Err(String::from_utf8_lossy(&output.stderr)
                    .trim()
                    .to_string()
                    .into());
            }
            Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
        };
        if !self.checkpoints.join("HEAD").exists() {
            fs::create_dir_all(&self.checkpoints).ok()?;
            git(&["init", "--quiet"]).ok()?;
        }
        let mut spec = vec!["add", "-A", "--", ".", ":!.git"];
        if let Ok(relative) = self.state.strip_prefix(&self.project) {
            let relative = relative.to_string_lossy().to_string();
            if !relative.is_empty() {
                let exclude = format!(":!{relative}");
                spec.push(Box::leak(exclude.into_boxed_str()));
            }
        }
        git(&spec).ok()?;
        let message = format!("node {} at {}", assignment.task_id, assignment.issued_at);
        git(&["commit", "--quiet", "--allow-empty", "-m", &message]).ok()?;
        git(&["rev-parse", "HEAD"]).ok()
    }

    fn seal(
        &self,
        node: &Path,
        assignment: &Assignment,
        outcome: Outcome<'_>,
        delta: &ProjectDelta,
    ) -> Result<String> {
        let checkpoint = self.checkpoint(assignment);
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
        let digest = format!(
            "{steps} steps, {calls} tool calls{}, {}, ended: {reason}\n{text}",
            if histogram.is_empty() {
                String::new()
            } else {
                format!(" ({})", histogram.join(" "))
            },
            delta.line(),
        );
        let manifest = json!({
            "assignment": assignment,
            "digest": digest,
            "checkpoint": checkpoint,
            "context": "context.jsonl",
            "context_bytes": context_bytes,
            "frozen": true,
        });
        let temp = node.join("node.json.tmp");
        fs::write(&temp, serde_json::to_vec_pretty(&manifest)?)?;
        fs::File::open(&temp)?.sync_all()?;
        fs::rename(temp, node.join("node.json"))?;
        // The manifest is part of the frozen record, like the context and artifacts.
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
    fn capacity(&self) -> usize {
        self.slots
    }

    fn execute(&self, assignment: &Assignment) -> String {
        let node = self.root.join(assignment.task_id.to_string());
        // Reserve once. Restart or replay cannot overwrite an existing node, even after failure.
        if assignment.task_id == 0 {
            return "0 steps, 0 tool calls, ended: error\ninvalid node id".into();
        }
        if INTERRUPTED.load(Ordering::SeqCst) {
            return "0 steps, 0 tool calls, ended: budget\ninterrupted before the node started"
                .into();
        }
        let cancelled = Arc::new(AtomicBool::new(false));
        {
            let mut active = self.active.lock().unwrap_or_else(|e| e.into_inner());
            if !active.is_empty() {
                return "0 steps, 0 tool calls, ended: budget\nexecution slot is busy".into();
            }
            if let Err(e) = fs::create_dir(&node) {
                return format!(
                    "0 steps, 0 tool calls, ended: error\nnode already reserved or unavailable: {e}"
                );
            }
            active.insert(assignment.task_id, cancelled.clone());
        }
        let _lease = Lease {
            active: &self.active,
            id: assignment.task_id,
            flag: cancelled.clone(),
        };
        let control = Control::new(
            Duration::from_millis(self.limits.timeout_ms),
            cancelled.clone(),
        );
        let mut counts = std::collections::BTreeMap::new();
        let mut log = match ContextLog::create(&node) {
            Ok(log) => log,
            Err(error) => {
                return format!(
                    "0 steps, 0 tool calls, ended: error\nnode context is unavailable: {error}"
                )
            }
        };
        let before = snapshot_project(&self.project, &self.state);
        let result: Result<(String, String)> =
            self.run(assignment, &control, &mut log, &mut counts);
        let steps = log.assistants();
        let (after, truncated) = snapshot_project(&self.project, &self.state);
        let delta = ProjectDelta::between(&before.0, &after, truncated || before.1);
        cancelled.store(true, Ordering::SeqCst); // revoke this run before publishing anything
        let (text, reason) = match result {
            Ok(outcome) => outcome,
            Err(error) => (error.to_string(), "error".to_string()),
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
        )
        .unwrap_or_else(|e| {
            format!("{steps} steps, 0 tool calls, ended: error\nnode could not be frozen: {e}")
        })
    }
}
