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

    fn record(&mut self, entry: &Value) -> Result<()> {
        if entry.get("kind").and_then(Value::as_str) == Some("assistant") {
            self.assistants += 1;
        }
        let mut line = serde_json::to_vec(entry)?;
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
    fn start(binary: &Path, work: &Path, start: &Start) -> Result<Self> {
        if crate::measure::bytes(start)? > aja::FRAME_LIMIT {
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
        let mut active = self
            .active
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
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
    /// The optional shadow repository that keeps one commit per aja. A host
    /// without Git evidence leaves this absent; the transcript manifest remains.
    records: Option<super::records::Checkpoints>,
    /// Where each run may change things. Isolation is the host's decision, and
    /// the backend refuses a world that already has an owner.
    worlds: Option<Arc<dyn Worlds>>,
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
        let project = project.into();
        Self::for_workspace(binary, root, Some(&project), model, tools)
    }

    /// Build a worker with the host's optional workspace.
    pub fn for_workspace(
        binary: impl Into<PathBuf>,
        root: impl Into<PathBuf>,
        project: Option<&Path>,
        model: Arc<dyn Model>,
        tools: Arc<dyn ToolGateway>,
    ) -> Result<Self> {
        let root = root.into();
        fs::create_dir_all(&root)?;
        let root = root.canonicalize()?;
        let project = project.map(Path::canonicalize).transpose()?;
        let state = root.parent().ok_or("aja root has no state directory")?;
        if project
            .as_ref()
            .is_some_and(|project| project.starts_with(state))
        {
            return Err(
                "the private state directory must not contain the project workspace".into(),
            );
        }
        let records = project.as_ref().map(|_| {
            super::records::Checkpoints::new(state.join("checkpoints"), state.to_path_buf())
        });
        let worlds = project
            .map(|project| Arc::new(Shared::new(project, state.to_path_buf())) as Arc<dyn Worlds>);
        Ok(Self {
            binary: binary.into().canonicalize()?,
            root,
            model,
            tools,
            limits: Limits::default(),
            records,
            worlds,
            active: Mutex::new(Vec::new()),
        })
    }

    /// Replace the constructor's default world backend.
    #[must_use]
    pub fn with_worlds(mut self, worlds: Arc<dyn Worlds>) -> Self {
        self.worlds = Some(worlds);
        self
    }
    pub fn with_limits(mut self, limits: Limits) -> Result<Self> {
        limits.validate()?;
        self.limits = limits;
        Ok(self)
    }

    #[must_use]
    pub fn without_checkpoints(mut self) -> Self {
        self.records = None;
        self
    }
    pub fn cancel(&self, aja_id: u64) -> bool {
        let active = self
            .active
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        active
            .iter()
            .find(|run| run.id == aja_id)
            .is_some_and(|run| {
                run.cancelled.store(true, Ordering::SeqCst);
                true
            })
    }
    pub fn active_count(&self) -> usize {
        self.active
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len()
    }

    fn run(
        &self,
        assignment: &Assignment,
        control: &Control,
        log: &mut ContextLog,
        counts: &mut std::collections::BTreeMap<String, usize>,
        workspace: Option<&Path>,
    ) -> Result<(String, String)> {
        let mut definitions = self.tools.definitions(assignment);
        definitions.push(super::records::definition());
        let allowed: HashSet<String> = definitions
            .iter()
            .filter_map(|d| d["function"]["name"].as_str().map(String::from))
            .collect();
        if allowed.len() != definitions.len() {
            return Err("invalid or duplicate host tool definitions".into());
        }
        log.record(&json!({"kind":"start", "view_version":1, "messages":fold::fresh_messages(assignment,&self.limits)}))?;
        let ancestors = super::records::lineage(&self.root, assignment.resume, control)?;
        let inherited =
            super::records::replay(&self.root, &ancestors, self.limits.context_bytes, control)?;
        let messages = fold::prepare_messages(assignment, &self.limits, inherited)?;
        let start = Start {
            context_bytes: self.limits.context_bytes,
            messages,
        };
        let mut process = Process::start(&self.binary, workspace.unwrap_or(&self.root), &start)?;
        let context = ToolContext {
            aja_id: assignment.task_id,
            workspace,
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
                        log.record(&json!({"kind":"model_error","message":value,"error":error.to_string()}))?;
                        return Err(error);
                    }
                    log.record(&json!({
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
                    } else if name == super::records::READ_TOOL {
                        super::records::read(
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
                    log.record(&json!({
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

    fn seal(&self, run: Seal<'_>, control: &Control) -> Result<String> {
        let Seal {
            aja,
            assignment,
            outcome,
            world,
            world_line,
        } = run;
        // The record store freezes the world only when this host assembled Git
        // evidence. The transcript/manifest record is independent of it.
        let (checkpoint, checkpoint_error) =
            self.records
                .as_ref()
                .zip(world)
                .map_or((None, None), |(records, world)| {
                    match records.freeze(
                        assignment.task_id,
                        assignment.issued_at,
                        &world.root,
                        control,
                    ) {
                        Ok(commit) => (Some(commit), None),
                        Err(error) => (None, Some(error.to_string())),
                    }
                });
        let context = aja.join("context.jsonl");
        if let Ok(file) = fs::OpenOptions::new().write(true).open(&context) {
            file.sync_all()?;
            let mut permissions = file.metadata()?.permissions();
            permissions.set_readonly(true);
            fs::set_permissions(&context, permissions)?;
        }
        let context_bytes = fs::metadata(&context).map_or(0, |m| m.len());
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
        let manifest = super::records::Manifest {
            assignment: assignment.clone(),
            digest: digest.clone(),
            world: world.map_or_else(|| "none".to_owned(), |world| world.label.clone()),
            checkpoint,
            checkpoint_error,
            context: "context.jsonl".to_string(),
            context_bytes: usize::try_from(context_bytes).unwrap_or(usize::MAX),
            frozen: true,
        };
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
struct Seal<'a> {
    aja: &'a Path,
    assignment: &'a Assignment,
    outcome: Outcome<'a>,
    world: Option<&'a World>,
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
        let root = (!assignment.workspace.is_empty())
            .then_some(std::path::Path::new(assignment.workspace.as_str()));
        let held_world = if let Some(worlds) = &self.worlds {
            match worlds.reserve(assignment.task_id, root, &control) {
                Ok(world) => Some(Held {
                    worlds: worlds.as_ref(),
                    world,
                }),
                Err(error) => {
                    return format!("0 steps, 0 tool calls, ended: error\nno world: {error}")
                }
            }
        } else if root.is_some() {
            return "0 steps, 0 tool calls, ended: error\nno world: this deployment grants no workspace".into();
        } else {
            None
        };
        {
            let mut active = self
                .active
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
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
        let world = held_world.as_ref().map(|held| &held.world);
        let mut counts = std::collections::BTreeMap::new();
        let mut log = match ContextLog::create(&aja) {
            Ok(log) => log,
            Err(error) => {
                return format!(
                    "0 steps, 0 tool calls, ended: error\naja context is unavailable: {error}"
                )
            }
        };
        let result = self.run(
            assignment,
            &control,
            &mut log,
            &mut counts,
            world.map(|world| world.root.as_path()),
        );
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
        let _ = log.record(&json!({"kind": "finish", "text": &text, "reason": &reason}));
        drop(log);
        // The account is the host's: measured against the reservation, and made
        // durable before the aja is sealed.
        let world_line = held_world.as_ref().map_or_else(
            || "world: none: no workspace".into(),
            |held| match held
                .worlds
                .settle(&held.world, &parent.child(Duration::from_secs(60)))
            {
                Ok(value) => {
                    let change = value["change"].as_str().unwrap_or("unreported");
                    let mut line = format!("world: {}: {change}", held.world.label);
                    if let Some(commit) = value["commit"].as_str() {
                        let _ = write!(line, " at {commit}");
                    }
                    line
                }
                Err(error) => format!("world: {}: account unavailable ({error})", held.world.label),
            },
        );
        self.seal(
            Seal {
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
