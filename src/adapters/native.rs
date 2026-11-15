//! Single native worker path: same binary, private AJA mode, host-owned effects.
use super::{Control, Model, Shared, ToolContext, ToolGateway, World, Worlds};
use crate::fold;
use crate::{
    aja::{self, Limits, Request, Start},
    Assignment, Result, Worker,
};
use serde_json::{json, Value};
use std::{
    collections::HashSet,
    fmt::Write as FmtWrite,
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

/// How long one wait for the aja's next frame blocks before the control is read.
const WORKER_POLL: Duration = Duration::from_millis(20);

/// The aja's complete context, appended as it happens and frozen with the aja.
/// The host sees every model response and tool call, so freezing the record never
/// depends on the aja cooperating.
struct ContextLog {
    file: fs::File,
    assistants: usize,
    entries: usize,
}

impl ContextLog {
    fn create(aja: &Path) -> Result<Self> {
        let file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(aja.join("context.jsonl"))?;
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
        if crate::measure::bytes(&start)? > aja::FRAME_LIMIT {
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
        let receiver = self.receiver.as_ref().ok_or("worker closed")?;
        loop {
            // The wait is bounded by the run's own remaining budget, so a spent
            // deadline is reported by the control instead of one tick later.
            let wait = control.remaining()?.min(WORKER_POLL);
            match receiver.recv_timeout(wait) {
                Ok(Ok(request)) => return Ok(request),
                Ok(Err(error)) => return Err(error.into()),
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(error) => return Err(error.into()),
            }
        }
    }
}

/// What one run of a aja produced: the transcript's account of it.
struct Outcome<'a> {
    text: &'a str,
    reason: &'a str,
    steps: usize,
    counts: &'a std::collections::BTreeMap<String, usize>,
}

struct Lease<'a> {
    active: &'a Mutex<Vec<ActiveRun>>,
    id: u64,
}

impl Drop for Lease<'_> {
    fn drop(&mut self) {
        let mut active = self.active.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(index) = active.iter().position(|run| run.id == self.id) {
            active.remove(index).cancelled.store(true, Ordering::SeqCst);
        }
    }
}

/// A reserved world released on every exit path, panic included.
struct Held<'a> {
    worlds: &'a dyn Worlds,
    world: World,
}

impl Drop for Held<'_> {
    fn drop(&mut self) {
        self.worlds.release(&self.world);
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
    model: Arc<dyn Model>,
    tools: Arc<dyn ToolGateway>,
    limits: Limits,
    /// The shadow repository that keeps one commit per aja: the fossils.
    checkpoints: PathBuf,
    /// The host's own state, never part of what a aja did to the project.
    state: PathBuf,
    /// Where each run may change things. Isolation is the host's decision, and
    /// the backend refuses a world that already has an owner.
    worlds: Arc<dyn Worlds>,
    active: Mutex<Vec<ActiveRun>>,
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
            .ok_or("aja root has no state directory")?
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
            model,
            tools,
            limits: Limits::default(),
            checkpoints,
            worlds: Arc::new(Shared::new(project.clone(), state.clone())),
            state,
            active: Mutex::new(Vec::new()),
        })
    }
    /// The worlds this host provides. Without one, every run shares the project.
    pub fn with_worlds(mut self, worlds: Arc<dyn Worlds>) -> Self {
        self.worlds = worlds;
        self
    }
    pub fn with_limits(mut self, limits: Limits) -> Result<Self> {
        limits.validate()?;
        self.limits = limits;
        Ok(self)
    }
    pub fn cancel(&self, aja_id: u64) -> bool {
        let active = self.active.lock().unwrap_or_else(|e| e.into_inner());
        match active.iter().find(|run| run.id == aja_id) {
            Some(run) => {
                run.cancelled.store(true, Ordering::SeqCst);
                true
            }
            None => false,
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
        world: &World,
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
        let mut process = Process::start(&self.binary, &world.root, start)?;
        let context = ToolContext {
            aja_id: assignment.task_id,
            project: &world.root,
            private: self.root.parent().ok_or("missing state root")?,
            control,
        };
        let mut steps = 0;
        loop {
            let response = match process.next(control)? {
                Request::Model { messages } => {
                    steps += 1;
                    if let Some(limit) = self.limits.steps.filter(|limit| steps > *limit) {
                        return Ok((
                            format!(
                                "AJA step budget of {limit} turns exhausted; continue in a new aja"
                            ),
                            "budget".into(),
                        ));
                    }
                    if crate::measure::bytes(&messages)? > self.limits.context_bytes {
                        return Ok((
                            "AJA context budget exhausted; continue in a new aja".into(),
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
                    if crate::measure::bytes(&value)? > self.limits.response_bytes {
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
                    if crate::measure::bytes(&value)? > aja::FRAME_LIMIT {
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
                        return Err("invalid aja finish reason".into());
                    }
                    return Ok((text, reason));
                }
            };
            control.check()?;
            // A tool result is bounded by the frame budget, not the model's reply
            // budget: the tool owns its own output bound, the record keeps the
            // whole result, and the transcript folds against its context budget.
            if crate::measure::bytes(&response)? > aja::FRAME_LIMIT {
                return Err("AJA tool output limit reached".into());
            }
            aja::send(process.input.as_mut().ok_or("worker closed")?, &response)?;
        }
    }

    /// Freeze the world this aja left: one commit in the state's shadow
    /// repository. The commit is the fossil EJA reads a aja's files from.
    fn checkpoint(
        &self,
        assignment: &Assignment,
        root: &Path,
        control: &Control,
    ) -> Result<String> {
        let git = |args: &[&str]| -> Result<String> {
            let mut command = std::process::Command::new("git");
            command
                .arg("--git-dir")
                .arg(&self.checkpoints)
                .arg("--work-tree")
                .arg(root)
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
        if let Ok(relative) = self.state.strip_prefix(root) {
            let relative = relative.to_string_lossy().to_string();
            if !relative.is_empty() {
                let exclude = format!(":!{relative}");
                spec.push(exclude);
            }
        }
        git(&spec.iter().map(String::as_str).collect::<Vec<_>>())?;
        let message = format!("aja {} at {}", assignment.task_id, assignment.issued_at);
        git(&["commit", "--quiet", "--allow-empty", "-m", &message])?;
        git(&["rev-parse", "HEAD"])
    }

    fn seal(&self, run: Run<'_>, control: &Control) -> Result<String> {
        let Run {
            aja,
            assignment,
            outcome,
            world,
            world_line,
        } = run;
        let (checkpoint, checkpoint_error) = match self.checkpoint(assignment, &world.root, control)
        {
            Ok(commit) => (Some(commit), None),
            Err(error) => (None, Some(error.to_string())),
        };
        let context = aja.join("context.jsonl");
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
            "{steps} steps, {calls} tool calls{}, ended: {reason}\n{world_line}\n{text}",
            if histogram.is_empty() {
                String::new()
            } else {
                format!(" ({})", histogram.join(" "))
            },
        );
        if let Some(error) = &checkpoint_error {
            let _ = write!(digest, "\ncheckpoint unavailable: {error}");
        }
        let manifest = json!({
            "assignment": assignment,
            "digest": digest,
            "world": world.label,
            "checkpoint": checkpoint,
            "checkpoint_error": checkpoint_error,
            "context": "context.jsonl",
            "context_bytes": context_bytes,
            "frozen": true,
        });
        let temp = aja.join("aja.json.tmp");
        fs::write(&temp, serde_json::to_vec_pretty(&manifest)?)?;
        fs::File::open(&temp)?.sync_all()?;
        fs::rename(temp, aja.join("aja.json"))?;
        // The manifest is part of the frozen record, like the context and the checkpoint.
        let manifest = aja.join("aja.json");
        if let Ok(file) = fs::OpenOptions::new().write(true).open(&manifest) {
            let mut permissions = file.metadata()?.permissions();
            permissions.set_readonly(true);
            fs::set_permissions(&manifest, permissions)?;
        }
        Ok(digest)
    }
}

/// Everything one run produced, in one place: the record is sealed from this.
struct Run<'a> {
    aja: &'a Path,
    assignment: &'a Assignment,
    outcome: Outcome<'a>,
    world: &'a World,
    world_line: String,
}

impl Worker for NativeWorker {
    fn execute(&self, assignment: &Assignment, parent: &Control) -> String {
        if let Err(error) = parent.check() {
            return format!("0 steps, 0 tool calls, ended: budget\n{error}");
        }
        let aja = self.root.join(assignment.task_id.to_string());
        // Reserve once. Restart or replay cannot overwrite an existing aja, even after failure.
        if assignment.task_id == 0 {
            return "0 steps, 0 tool calls, ended: error\ninvalid aja id".into();
        }
        let cancelled = Arc::new(AtomicBool::new(false));
        let control = parent
            .child(Duration::from_millis(self.limits.timeout_ms))
            .cancelled_by(cancelled.clone());
        // The world comes first: a run that cannot get one has started nothing, so
        // it does not burn its aja id either. Isolation is the backend's rule, and
        // it is the only place that decides how many runs may exist at once.
        let world = match self.worlds.reserve(assignment.task_id, &control) {
            Ok(world) => Held {
                worlds: &*self.worlds,
                world,
            },
            Err(error) => return format!("0 steps, 0 tool calls, ended: error\nno world: {error}"),
        };
        {
            let mut active = self.active.lock().unwrap_or_else(|e| e.into_inner());
            if let Err(e) = fs::create_dir(&aja) {
                return format!(
                    "0 steps, 0 tool calls, ended: error\naja already reserved or unavailable: {e}"
                );
            }
            active.push(ActiveRun {
                id: assignment.task_id,
                cancelled: cancelled.clone(),
            });
        }
        let _lease = Lease {
            active: &self.active,
            id: assignment.task_id,
        };
        let world = &world.world;
        let mut counts = std::collections::BTreeMap::new();
        let mut log = match ContextLog::create(&aja) {
            Ok(log) => log,
            Err(error) => {
                return format!(
                    "0 steps, 0 tool calls, ended: error\naja context is unavailable: {error}"
                )
            }
        };
        let result: Result<(String, String)> =
            self.run(assignment, &control, &mut log, &mut counts, world);
        let steps = log.assistants;
        // The world's account is measured before the run is revoked: an expired
        // deadline makes it honest ("unobserved"), not optimistic.
        cancelled.store(true, Ordering::SeqCst);
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
        // The account is the host's: measured against the reservation, and made
        // durable before the aja is sealed.
        let account = self
            .worlds
            .settle(world, &parent.child(Duration::from_secs(60)));
        let world_line = match &account {
            Ok(value) => {
                let mut line = format!(
                    "world: {}: {}",
                    world.label,
                    value["change"].as_str().unwrap_or("unreported")
                );
                if let Some(commit) = value["commit"].as_str() {
                    let _ = write!(line, " at {commit}");
                }
                line
            }
            Err(error) => format!("world: {}: account unavailable ({error})", world.label),
        };
        self.seal(
            Run {
                aja: &aja,
                assignment,
                outcome: Outcome {
                    text: &text,
                    reason: &reason,
                    steps,
                    counts: &counts,
                },
                world,
                world_line,
            },
            parent,
        )
        .unwrap_or_else(|e| {
            format!("{steps} steps, 0 tool calls, ended: error\naja could not be frozen: {e}")
        })
    }
}
