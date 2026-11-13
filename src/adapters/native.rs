//! Single native worker path: same binary, private AJA mode, host-owned effects.
use super::{tools::relative_file, Control, Model, ToolContext, ToolGateway};
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
        node: &Path,
        control: &Control,
        artifacts: &mut HashSet<String>,
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
        let mut process = Process::start(&self.binary, &node.join("work"), start)?;
        let context = ToolContext {
            node_id: assignment.task_id,
            work: &node.join("work"),
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
                            Ok(output) => {
                                artifacts.extend(output.artifacts);
                                output.value
                            }
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

    fn seal(
        &self,
        node: &Path,
        assignment: &Assignment,
        outcome: (&str, &str),
        steps: usize,
        counts: &std::collections::BTreeMap<String, usize>,
        paths: HashSet<String>,
    ) -> Result<String> {
        let mut sealed = Vec::new();
        let mut failed = Vec::new();
        let output = node.join("artifacts");
        fs::create_dir(&output)?;
        let mut paths: Vec<_> = paths.into_iter().collect();
        paths.sort();
        for name in paths {
            let copied = (|| -> Result<()> {
                let source = relative_file(&node.join("work"), &name)?;
                let target = relative_file(&output, &name)?;
                let metadata = source.metadata()?;
                if !metadata.is_file() || metadata.len() > aja::FRAME_LIMIT as u64 {
                    return Err("artifact must be a bounded regular file".into());
                }
                fs::create_dir_all(target.parent().ok_or("missing artifact parent")?)?;
                fs::copy(&source, &target)?;
                let mut permissions = target.metadata()?.permissions();
                permissions.set_readonly(true);
                fs::set_permissions(&target, permissions)?;
                fs::File::open(&target)?.sync_all()?;
                Ok(())
            })();
            match copied {
                Ok(()) => sealed.push(format!("{}/artifacts/{name}", assignment.task_id)),
                Err(error) => failed.push(format!("{name} could not be sealed: {error}")),
            }
        }
        fs::remove_dir_all(node.join("work"))?;
        let context = node.join("context.jsonl");
        if let Ok(file) = fs::OpenOptions::new().write(true).open(&context) {
            file.sync_all()?;
            let mut permissions = file.metadata()?.permissions();
            permissions.set_readonly(true);
            fs::set_permissions(&context, permissions)?;
        }
        let context_bytes = fs::metadata(&context).map(|m| m.len()).unwrap_or(0);
        let calls: usize = counts.values().sum();
        let histogram: Vec<String> = {
            let mut pairs: Vec<_> = counts.iter().collect();
            pairs.sort_by(|a, b| b.1.cmp(a.1).then(a.0.cmp(b.0)));
            pairs
                .iter()
                .map(|(name, count)| format!("{name}x{count}"))
                .collect()
        };
        let (text, reason) = outcome;
        let digest = format!(
            "{steps} steps, {calls} tool calls{}, {} artifacts{}{}, ended: {reason}\n{text}",
            if histogram.is_empty() {
                String::new()
            } else {
                format!(" ({})", histogram.join(" "))
            },
            sealed.len(),
            if sealed.is_empty() {
                String::new()
            } else {
                format!(" [{}]", sealed.join(", "))
            },
            if failed.is_empty() {
                String::new()
            } else {
                format!("; seal failed: {}", failed.join("; "))
            }
        );
        let manifest = json!({
            "assignment": assignment,
            "digest": digest,
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
            return "0 steps, 0 tool calls, 0 artifacts, ended: error\ninvalid node id".into();
        }
        if INTERRUPTED.load(Ordering::SeqCst) {
            return "0 steps, 0 tool calls, 0 artifacts, ended: budget\ninterrupted before the node started".into();
        }
        let cancelled = Arc::new(AtomicBool::new(false));
        {
            let mut active = self.active.lock().unwrap_or_else(|e| e.into_inner());
            if !active.is_empty() {
                return "0 steps, 0 tool calls, 0 artifacts, ended: budget\nexecution slot is busy"
                    .into();
            }
            if let Err(e) = fs::create_dir(&node) {
                return format!(
                    "0 steps, 0 tool calls, 0 artifacts, ended: error\nnode already reserved or unavailable: {e}"
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
        let mut paths = HashSet::new();
        let mut counts = std::collections::BTreeMap::new();
        let mut log = match ContextLog::create(&node) {
            Ok(log) => log,
            Err(error) => {
                return format!(
                    "0 steps, 0 tool calls, 0 artifacts, ended: error\nnode context is unavailable: {error}"
                )
            }
        };
        let result: Result<(String, String)> = fs::create_dir(node.join("work"))
            .map_err(Into::into)
            .and_then(|_| {
                let outcome = self.run(
                    assignment,
                    &node,
                    &control,
                    &mut paths,
                    &mut log,
                    &mut counts,
                )?;
                Ok(outcome)
            });
        let steps = log.assistants();
        cancelled.store(true, Ordering::SeqCst); // revoke this run before publishing anything
        let (text, reason) = match result {
            Ok(outcome) => outcome,
            Err(error) => (error.to_string(), "error".to_string()),
        };
        let _ = log.record(json!({"kind": "finish", "text": &text, "reason": &reason}));
        drop(log);
        self.seal(&node, assignment, (&text, &reason), steps, &counts, paths)
            .unwrap_or_else(|e| {
                format!(
                    "{steps} steps, 0 tool calls, 0 artifacts, ended: error\nnode could not be frozen: {e}"
                )
            })
    }
}
