use crate::{Assignment, Report, ReportStatus, Worker};
use std::io::Read;
use std::path::PathBuf;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant, SystemTime};

struct WorkerBox {
    child: Child,
    pgid: i32,
    reaped: bool,
    grace: Duration,
}

impl WorkerBox {
    fn spawn(mut command: Command, grace: Duration) -> std::io::Result<Self> {
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }
        let child = command.spawn()?;
        let pgid = child.id() as i32;
        Ok(Self {
            child,
            pgid,
            reaped: false,
            grace,
        })
    }

    fn signal(&self, signal: i32) {
        #[cfg(unix)]
        unsafe {
            libc::killpg(self.pgid, signal);
        }
        #[cfg(not(unix))]
        let _ = signal;
    }

    #[cfg(unix)]
    fn terminate(&mut self) {
        if self.reaped {
            self.signal(libc::SIGKILL);
            return;
        }
        self.signal(libc::SIGTERM);
        if self.wait_until(self.grace).is_none() {
            self.signal(libc::SIGKILL);
        }
        self.signal(libc::SIGKILL);
        if !self.reaped {
            let _ = self.child.wait();
            self.reaped = true;
        }
    }

    #[cfg(not(unix))]
    fn terminate(&mut self) {
        if !self.reaped {
            let _ = self.child.kill();
            let _ = self.child.wait();
            self.reaped = true;
        }
    }

    fn wait_until(&mut self, limit: Duration) -> Option<ExitStatus> {
        let started = Instant::now();
        loop {
            match self.child.try_wait() {
                Ok(Some(status)) => {
                    self.reaped = true;
                    return Some(status);
                }
                Ok(None) => {}
                Err(_) => return None,
            }
            if started.elapsed() >= limit {
                return None;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn finish(&mut self) {
        // The leader exited; kill anything it left in the group.
        self.signal(if cfg!(unix) { 9 } else { 0 });
    }
}

impl Drop for WorkerBox {
    fn drop(&mut self) {
        self.terminate();
    }
}

fn env_value(names: &[&str]) -> Option<String> {
    names.iter().find_map(|name| std::env::var(name).ok())
}

fn timeout_from_env() -> Duration {
    let seconds = env_value(&["EJA_AJA_TIMEOUT_SECS", "EJA_AJAS_TIMEOUT_SECS"])
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|seconds| *seconds > 0)
        .unwrap_or(300);
    Duration::from_secs(seconds)
}

fn grace_from_env() -> Duration {
    let millis = env_value(&["EJA_AJA_KILL_GRACE_MS", "EJA_AJAS_KILL_GRACE_MS"])
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|millis| *millis > 0)
        .unwrap_or(500);
    Duration::from_millis(millis)
}

fn read_streams(
    mut worker_box: WorkerBox,
) -> (
    WorkerBox,
    std::thread::JoinHandle<Vec<u8>>,
    std::thread::JoinHandle<Vec<u8>>,
) {
    let stdout = worker_box.child.stdout.take().expect("stdout");
    let stderr = worker_box.child.stderr.take().expect("stderr");
    let out_reader = std::thread::spawn(move || {
        let mut buffer = Vec::new();
        let mut stdout = stdout;
        let _ = stdout.read_to_end(&mut buffer);
        buffer
    });
    let err_reader = std::thread::spawn(move || {
        let mut buffer = Vec::new();
        let mut stderr = stderr;
        let _ = stderr.read_to_end(&mut buffer);
        buffer
    });
    (worker_box, out_reader, err_reader)
}

type WaitResult = (
    WorkerBox,
    ExitStatus,
    std::thread::JoinHandle<Vec<u8>>,
    std::thread::JoinHandle<Vec<u8>>,
);

fn wait_for(
    worker_box: WorkerBox,
    timeout: Duration,
) -> std::result::Result<WaitResult, (WorkerBox, String)> {
    let (mut worker_box, out_reader, err_reader) = read_streams(worker_box);
    let started = Instant::now();
    loop {
        match worker_box.child.try_wait() {
            Ok(Some(status)) => {
                worker_box.reaped = true;
                return Ok((worker_box, status, out_reader, err_reader));
            }
            Ok(None) => {
                if started.elapsed() > timeout {
                    worker_box.terminate();
                    let _ = out_reader.join();
                    let _ = err_reader.join();
                    return Err((
                        worker_box,
                        format!("AJA timed out after {}s", timeout.as_secs()),
                    ));
                }
                std::thread::sleep(Duration::from_millis(25));
            }
            Err(error) => {
                worker_box.terminate();
                let _ = out_reader.join();
                let _ = err_reader.join();
                return Err((worker_box, format!("wait AJA: {error}")));
            }
        }
    }
}

fn finish_box(mut worker_box: WorkerBox) {
    worker_box.finish();
}

pub struct SessionWorker {
    command: String,
    timeout: Duration,
    grace: Duration,
}

impl SessionWorker {
    pub fn new(command: impl Into<String>) -> Self {
        Self {
            command: command.into(),
            timeout: timeout_from_env(),
            grace: grace_from_env(),
        }
    }

    pub fn with_timeout(command: impl Into<String>, timeout: Duration) -> Self {
        Self {
            command: command.into(),
            timeout,
            grace: Duration::from_millis(500),
        }
    }

    pub fn with_grace(mut self, grace: Duration) -> Self {
        self.grace = grace;
        self
    }

    pub fn from_env() -> Option<Self> {
        env_value(&["EJA_AJA_COMMAND", "EJA_AJAS_COMMAND"])
            .filter(|command| !command.trim().is_empty())
            .map(Self::new)
    }
}

impl Worker for SessionWorker {
    fn execute(&self, assignment: &Assignment) -> Report {
        let payload = match serde_json::to_string(assignment) {
            Ok(payload) => payload,
            Err(error) => return Report::failed(format!("serialize assignment: {error}")),
        };
        let mut command = Command::new("sh");
        command
            .arg("-c")
            .arg(&self.command)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut worker_box = match WorkerBox::spawn(command, self.grace) {
            Ok(worker_box) => worker_box,
            Err(error) => return Report::failed(format!("spawn AJA: {error}")),
        };
        if let Some(mut stdin) = worker_box.child.stdin.take() {
            if let Err(error) = std::io::Write::write_all(&mut stdin, payload.as_bytes()) {
                worker_box.terminate();
                return Report::failed(format!("write assignment: {error}"));
            }
        }
        let (worker_box, status, out_reader, err_reader) = match wait_for(worker_box, self.timeout)
        {
            Ok(result) => result,
            Err((_, error)) => return Report::failed(error),
        };
        finish_box(worker_box);
        let stdout = out_reader.join().unwrap_or_default();
        let stderr = err_reader.join().unwrap_or_default();
        let stderr = String::from_utf8_lossy(&stderr);
        if !status.success() {
            return Report::failed(format!("AJA exited {status}: {stderr}"));
        }
        let stdout = String::from_utf8_lossy(&stdout);
        match serde_json::from_str::<Report>(stdout.trim()) {
            Ok(report) => report,
            Err(error) => Report::failed(format!("AJA invalid report: {error}; stdout: {stdout}")),
        }
    }
}

pub struct PiWorker {
    command: String,
    model: Option<String>,
    system_prompt: Option<String>,
    cwd: Option<PathBuf>,
    timeout: Duration,
    grace: Duration,
}

impl PiWorker {
    pub fn from_env() -> Option<Self> {
        let model = std::env::var("EJA_PI_MODEL")
            .ok()
            .filter(|value| !value.trim().is_empty())?;
        let system_prompt = std::env::var("EJA_PI_SYSTEM_PROMPT_FILE")
            .ok()
            .and_then(|path| std::fs::read_to_string(path).ok())
            .or_else(|| std::env::var("EJA_PI_SYSTEM_PROMPT").ok());
        let cwd = env_value(&["EJA_AJA_WORKSPACE", "EJA_AJAS_WORKSPACE"]).map(PathBuf::from);
        Some(Self {
            command: std::env::var("EJA_PI_BIN").unwrap_or_else(|_| "pi".to_string()),
            model: Some(model),
            system_prompt,
            cwd,
            timeout: timeout_from_env(),
            grace: grace_from_env(),
        })
    }

    pub fn with_command(command: impl Into<String>) -> Self {
        Self {
            command: command.into(),
            model: None,
            system_prompt: None,
            cwd: None,
            timeout: Duration::from_secs(300),
            grace: Duration::from_millis(500),
        }
    }

    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.model = Some(model.into());
        self
    }

    pub fn with_system_prompt(mut self, prompt: impl Into<String>) -> Self {
        self.system_prompt = Some(prompt.into());
        self
    }

    pub fn with_cwd(mut self, cwd: impl Into<PathBuf>) -> Self {
        self.cwd = Some(cwd.into());
        self
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    fn command_for(&self, assignment: &Assignment) -> Command {
        let mut command = Command::new(&self.command);
        command
            .arg("-p")
            .arg("--no-session")
            .arg("--no-context-files");
        if let Some(model) = &self.model {
            command.arg("--model").arg(model);
        }
        if let Some(prompt) = &self.system_prompt {
            command.arg("--append-system-prompt").arg(prompt);
        }
        if let Some(cwd) = &self.cwd {
            command.current_dir(cwd);
        }
        command.arg(&assignment.objective);
        command
            .stdin(Stdio::inherit())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        command
    }
}

fn changed_artifacts(root: &std::path::Path, since: SystemTime) -> Vec<String> {
    fn walk(
        root: &std::path::Path,
        dir: &std::path::Path,
        since: SystemTime,
        out: &mut Vec<String>,
        depth: usize,
    ) {
        if depth > 5 || out.len() >= 32 {
            return;
        }
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name == ".git"
                || name == "node_modules"
                || name == "__pycache__"
                || name.starts_with(".eja")
            {
                continue;
            }
            let Ok(meta) = entry.metadata() else {
                continue;
            };
            if meta.is_dir() {
                walk(root, &path, since, out, depth + 1);
            } else if meta.is_file() {
                let modified = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
                if modified >= since {
                    if let Ok(relative) = path.strip_prefix(root) {
                        out.push(relative.to_string_lossy().to_string());
                    }
                }
            }
        }
    }
    let mut out = Vec::new();
    walk(root, root, since, &mut out, 0);
    out.sort();
    out.dedup();
    out
}

impl Worker for PiWorker {
    fn execute(&self, assignment: &Assignment) -> Report {
        let started = SystemTime::now();
        let command = self.command_for(assignment);
        let worker_box = match WorkerBox::spawn(command, self.grace) {
            Ok(worker_box) => worker_box,
            Err(error) => return Report::failed(format!("spawn Pi session: {error}")),
        };
        let (worker_box, status, out_reader, err_reader) = match wait_for(worker_box, self.timeout)
        {
            Ok(result) => result,
            Err((_, error)) => return Report::failed(error),
        };
        finish_box(worker_box);
        let stdout = String::from_utf8_lossy(&out_reader.join().unwrap_or_default()).to_string();
        let stderr = String::from_utf8_lossy(&err_reader.join().unwrap_or_default()).to_string();
        if !status.success() {
            return Report::failed(format!("Pi exited {status}: {stderr}"));
        }
        let root = self
            .cwd
            .clone()
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_else(|| PathBuf::from("."));
        let artifacts = changed_artifacts(&root, started);
        Report {
            status: ReportStatus::Done,
            summary: stdout.trim().to_string(),
            artifacts,
            evidence: Vec::new(),
            open_questions: Vec::new(),
        }
    }
}

pub struct MissingWorker;

impl Worker for MissingWorker {
    fn execute(&self, _assignment: &Assignment) -> Report {
        Report::failed("no AJA configured: set EJA_PI_MODEL or EJA_AJA_COMMAND")
    }
}
