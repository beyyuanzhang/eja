//! Single native worker path: same binary, private AJA mode, host-owned effects.
use super::{tools::relative_file, Control, Model, ToolContext, ToolGateway};
use crate::{
    aja::{self, Limits, Request, Start},
    Assignment, Report, Result, Worker,
};
use serde_json::json;
use std::{
    collections::{HashMap, HashSet},
    fs,
    io::BufReader,
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

struct Lease<'a> {
    active: &'a Mutex<HashMap<u64, Arc<AtomicBool>>>,
    id: u64,
    flag: Arc<AtomicBool>,
}

impl Drop for Lease<'_> {
    fn drop(&mut self) {
        self.flag.store(true, Ordering::SeqCst);
        self.active.lock().expect("active nodes").remove(&self.id);
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
            active: Mutex::new(HashMap::new()),
        })
    }
    pub fn with_limits(mut self, limits: Limits) -> Result<Self> {
        limits.validate()?;
        self.limits = limits;
        Ok(self)
    }
    pub fn cancel(&self, node_id: u64) -> bool {
        if let Some(flag) = self.active.lock().expect("active nodes").get(&node_id) {
            flag.store(true, Ordering::SeqCst);
            true
        } else {
            false
        }
    }
    pub fn active_count(&self) -> usize {
        self.active.lock().expect("active nodes").len()
    }

    fn run(
        &self,
        assignment: &Assignment,
        node: &Path,
        control: &Control,
        artifacts: &mut HashSet<String>,
    ) -> Result<Report> {
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
                    steps += 1;
                    if steps > self.limits.steps
                        || serde_json::to_vec(&messages)?.len() > self.limits.context_bytes
                    {
                        return Err("AJA model budget exceeded".into());
                    }
                    let value =
                        self.model
                            .complete(&messages, &definitions, &self.limits, control)?;
                    if serde_json::to_vec(&value)?.len() > self.limits.response_bytes {
                        return Err("AJA model output limit reached".into());
                    }
                    value
                }
                Request::Tool { name, arguments } => {
                    control.check()?;
                    if !allowed.contains(&name) {
                        json!({"error":"tool not allowed"})
                    } else {
                        match self.tools.call(&context, &name, &arguments) {
                            Ok(output) => {
                                artifacts.extend(output.artifacts);
                                output.value
                            }
                            Err(error) => json!({"error":error.to_string()}),
                        }
                    }
                }
                Request::Finish { report } => {
                    control.check()?;
                    return Ok(report);
                }
            };
            control.check()?;
            if serde_json::to_vec(&response)?.len() > self.limits.response_bytes {
                return Err("AJA tool output limit reached".into());
            }
            aja::send(process.input.as_mut().ok_or("worker closed")?, &response)?;
        }
    }

    fn seal(
        &self,
        node: &Path,
        assignment: &Assignment,
        mut report: Report,
        paths: HashSet<String>,
    ) -> Result<Report> {
        let mut sealed = Vec::new();
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
                Err(error) => {
                    report.status = crate::ReportStatus::Failed;
                    report.summary = format!("artifact could not be sealed: {error}");
                }
            }
        }
        report.artifacts = sealed;
        fs::remove_dir_all(node.join("work"))?;
        let manifest = json!({"assignment":assignment,"report":report,"frozen":true});
        let temp = node.join("node.json.tmp");
        fs::write(&temp, serde_json::to_vec_pretty(&manifest)?)?;
        fs::File::open(&temp)?.sync_all()?;
        fs::rename(temp, node.join("node.json"))?;
        Ok(report)
    }
}

impl Worker for NativeWorker {
    fn execute(&self, assignment: &Assignment) -> Report {
        let node = self.root.join(assignment.task_id.to_string());
        // Reserve once. Restart or replay cannot overwrite an existing node, even after failure.
        if assignment.task_id == 0 {
            return Report::failed("invalid node id");
        }
        let cancelled = Arc::new(AtomicBool::new(false));
        {
            let mut active = self.active.lock().expect("active nodes");
            if !active.is_empty() {
                return Report {
                    status: crate::ReportStatus::Blocked,
                    summary: "execution slot is busy".into(),
                    artifacts: vec![],
                    evidence: vec![],
                    open_questions: vec![],
                };
            }
            if let Err(e) = fs::create_dir(&node) {
                return Report::failed(format!("node already reserved or unavailable: {e}"));
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
        let result = fs::create_dir(node.join("work"))
            .map_err(Into::into)
            .and_then(|_| self.run(assignment, &node, &control, &mut paths));
        cancelled.store(true, Ordering::SeqCst); // revoke this run before publishing anything
        let report = result.unwrap_or_else(|e| Report::failed(e.to_string()));
        paths.extend(report.artifacts.iter().cloned());
        let result = self.seal(&node, assignment, report, paths);
        result.unwrap_or_else(|e| Report::failed(format!("node could not be frozen: {e}")))
    }
}
