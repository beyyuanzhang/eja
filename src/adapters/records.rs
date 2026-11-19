//! Frozen records: one manifest type, one path resolver, one reader.
//!
//! A record is `K` in the contract: the immutable evidence one run left behind.
//! Everything that reads it — the worker's own tool, a continuation's replay, and
//! the host's `record:` address — resolves the same paths and reads the same
//! manifest through this module, so a record has one shape and one owner.

use super::{EnvironmentRecord, RecordReader};
use crate::{fold, Assignment, Control, Result, Selector};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::{HashSet, VecDeque},
    fs::{self, File, OpenOptions, TryLockError},
    io::{self, BufRead, BufReader, Read},
    path::{Path, PathBuf},
    process::{Child, ChildStdout, Stdio},
    time::Duration,
};

/// The tool a aja uses to read a passage of its own or an inherited record.
pub const READ_TOOL: &str = "eja_read_record";

const FREEZE_LOCK: &str = "eja-freeze.lock";
const FREEZE_POLL: Duration = Duration::from_millis(2);

pub fn definition() -> Value {
    json!({"type":"function","function":{"name":READ_TOOL,"description":"Read a bounded byte passage of a full tool result in this aja's record or an inherited record. Use the record aja/entry address from a tool result. Offsets address UTF-8 bytes.","parameters":{"type":"object","properties":{"aja":{"type":"integer"},"entry":{"type":"integer"},"offset":{"type":"integer","default":0},"bytes":{"type":"integer","default":1024}},"required":["aja","entry"],"additionalProperties":false}}})
}

/// The state's shadow repository: where a run's evidence is frozen.
///
/// It is the record store's own mechanism, not the worker's: the worker hands it
/// a run and receives a commit, and everything that reads record:<aja>/<path>
/// resolves that same commit. One place creates the repository, appends to it and
/// names the tree it excludes.
#[derive(Debug, Clone)]
pub struct Checkpoints {
    git_dir: PathBuf,
    /// The host's private state, excluded from every frozen tree.
    private: PathBuf,
    /// One repository-wide lock file. A process-local mutex would only
    /// serialize clones of this value, not the CLI, HTTP host or a restarted
    /// process that opens the same checkpoint repository.
    lock_path: PathBuf,
}

impl Checkpoints {
    #[must_use]
    pub fn new(git_dir: impl AsRef<Path>, private: impl AsRef<Path>) -> Self {
        let git_dir = super::environments::canonical_boundary(git_dir.as_ref());
        Self {
            lock_path: git_dir.join(FREEZE_LOCK),
            git_dir,
            private: super::environments::canonical_boundary(private.as_ref()),
        }
    }

    /// Hold the repository lock for one whole freeze. The file is closed when
    /// the returned guard drops; the kernel releases its lock then.
    fn lock(&self, control: &Control) -> Result<File> {
        fs::create_dir_all(&self.git_dir)?;
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&self.lock_path)?;
        loop {
            control.check()?;
            match file.try_lock() {
                Ok(()) => return Ok(file),
                Err(TryLockError::WouldBlock) => std::thread::sleep(FREEZE_POLL),
                Err(TryLockError::Error(error)) => {
                    return Err(format!(
                        "checkpoint freeze lock {} cannot be held: {error}",
                        self.lock_path.display()
                    )
                    .into())
                }
            }
        }
    }

    /// Freeze one run's environment as a commit and return its id.
    pub(crate) fn freeze(
        &self,
        task_id: u64,
        issued_at: i64,
        environment: &Path,
        control: &Control,
    ) -> Result<String> {
        let _freeze = self.lock(control)?;
        let git = |args: &[&str]| {
            super::environments::git_at(Some(&self.git_dir), environment, args, control)
        };
        if !self.git_dir.join("HEAD").exists() {
            fs::create_dir_all(&self.git_dir)?;
            git(&["init", "--quiet"])?;
        }
        let mut spec = vec![
            "add".to_string(),
            "-A".into(),
            "--".into(),
            ".".into(),
            ":!.git".into(),
        ];
        // A state directory inside the project is the host's, never evidence.
        if let Ok(relative) = self.private.strip_prefix(environment) {
            let relative = relative.to_string_lossy().to_string();
            if !relative.is_empty() {
                spec.push(format!(":!{relative}"));
            }
        }
        git(&spec.iter().map(String::as_str).collect::<Vec<_>>())?;
        let message = format!("aja {task_id} at {issued_at}");
        git(&["commit", "--quiet", "--allow-empty", "-m", &message])?;
        git(&["rev-parse", "HEAD"])
    }
}

/// One frozen run, as written by the worker and read back by everything else.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub(crate) format: String,
    pub(crate) version: u64,
    /// The activation contract this run was given.
    pub(crate) assignment: Assignment,
    /// The host's account of what the run left behind.
    #[serde(default)]
    pub(crate) digest: String,
    /// The provider's complete environment record, when the run had one.
    #[serde(default)]
    pub(crate) environment: Option<EnvironmentRecord>,
    /// The commit a `record:` file is read from, when one was taken.
    #[serde(default)]
    pub(crate) checkpoint: Option<String>,
    #[serde(default)]
    pub(crate) checkpoint_error: Option<String>,
}

const FORMAT: &str = "eja.aja";
const VERSION: u64 = 1;

impl Manifest {
    fn path(root: &Path, aja: u64) -> Result<PathBuf> {
        Ok(directory(root, aja)?.join("aja.json"))
    }

    pub(crate) fn read(root: &Path, aja: u64) -> Result<Self> {
        let manifest: Self = serde_json::from_slice(&fs::read(Self::path(root, aja)?)?)?;
        if manifest.format != FORMAT || manifest.version != VERSION {
            return Err(format!(
                "aja {aja} uses an unsupported record format; expected {FORMAT} version {VERSION}"
            )
            .into());
        }
        Ok(manifest)
    }
}

/// One aja's directory under a record root.
fn directory(root: &Path, aja: u64) -> Result<PathBuf> {
    if aja == 0 {
        return Err("invalid record aja".into());
    }
    super::tools::relative_file(root, &aja.to_string())
}

/// One aja's transcript. The protocol fixes this name, so it is read without the
/// manifest: the worker's own tool reads its record before the record is frozen.
fn context(root: &Path, aja: u64) -> Result<PathBuf> {
    super::tools::relative_file(root, &format!("{aja}/context.jsonl"))
}

/// The commit a `record:` file is read from, validated so it is only a revision.
fn checkpoint(manifest: &Manifest) -> Result<&str> {
    let commit = manifest
        .checkpoint
        .as_deref()
        .ok_or("this aja left no checkpoint")?;
    if commit.len() != 40 && commit.len() != 64
        || !commit.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return Err("invalid checkpoint id".into());
    }
    Ok(commit)
}

pub struct RecordFiles {
    root: PathBuf,
    checkpoints: Option<PathBuf>,
    max_bytes: usize,
}

impl RecordFiles {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            checkpoints: None,
            max_bytes: crate::aja::FRAME_LIMIT,
        }
    }
    pub fn with_checkpoints(root: impl Into<PathBuf>, checkpoints: impl Into<PathBuf>) -> Self {
        Self::with_checkpoints_and_limit(root, checkpoints, crate::aja::FRAME_LIMIT)
    }
    pub fn with_checkpoints_and_limit(
        root: impl Into<PathBuf>,
        checkpoints: impl Into<PathBuf>,
        max_bytes: usize,
    ) -> Self {
        Self {
            root: root.into(),
            checkpoints: Some(checkpoints.into()),
            max_bytes,
        }
    }
    pub fn with_limit(root: impl Into<PathBuf>, max_bytes: usize) -> Self {
        Self {
            root: root.into(),
            checkpoints: None,
            max_bytes,
        }
    }

    /// Resolve one address inside the record authority. A transcript or manifest
    /// is a file in the aja's directory; anything else is a path inside the
    /// run's environment
    /// the run left behind, read from its checkpoint commit.
    fn source(&self, address: &str) -> Result<Source> {
        let Some((id, rest)) = address.split_once('/') else {
            return Ok(Source::File(open(super::tools::relative_file(
                &self.root, address,
            )?)?));
        };
        let aja: u64 = id.parse().map_err(|_| "aja path needs a numeric id")?;
        if rest != "context.jsonl" && rest != "aja.json" {
            if let Some(checkpoints) = &self.checkpoints {
                let manifest = Manifest::read(&self.root, aja)?;
                let commit = checkpoint(&manifest)?;
                // The same hermetic git every other path uses: a frozen file is
                // read under the environment that wrote it.
                let mut child = super::environments::hermetic(Some(checkpoints), &self.root)
                    .args(["show", &format!("{commit}:{rest}")])
                    .stdout(Stdio::piped())
                    .stderr(Stdio::null())
                    .spawn()?;
                let output = child.stdout.take().ok_or("missing checkpoint reader")?;
                return Ok(Source::Git { child, output });
            }
        }
        Ok(Source::File(open(super::tools::relative_file(
            &self.root, address,
        )?)?))
    }
}

fn open(path: PathBuf) -> Result<fs::File> {
    let file = fs::File::open(path)?;
    if !file.metadata()?.is_file() {
        return Err("not a regular file".into());
    }
    Ok(file)
}

enum Source {
    File(fs::File),
    Git { child: Child, output: ChildStdout },
}

impl Read for Source {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        match self {
            Self::File(file) => file.read(bytes),
            Self::Git { child, output } => {
                let count = output.read(bytes)?;
                if count == 0 && !child.wait()?.success() {
                    return Err(io::Error::other("checkpoint path does not exist"));
                }
                Ok(count)
            }
        }
    }
}

impl Drop for Source {
    fn drop(&mut self) {
        if let Self::Git { child, .. } = self {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

impl RecordReader for RecordFiles {
    fn continuation_ready(&self, aja: u64, control: &Control) -> Result<()> {
        lineage(&self.root, Some(aja), control).map(|_| ())
    }

    fn environment(&self, aja: u64) -> Result<Option<EnvironmentRecord>> {
        match Manifest::read(&self.root, aja) {
            Ok(manifest) => Ok(manifest.environment),
            Err(error)
                if error
                    .downcast_ref::<io::Error>()
                    .is_some_and(|error| error.kind() == io::ErrorKind::NotFound) =>
            {
                Ok(None)
            }
            Err(error) => Err(error),
        }
    }

    fn slice(&self, path: &str, selector: &Selector, control: &Control) -> Result<Value> {
        control.check()?;
        if matches!(selector, Selector::Bytes { .. }) {
            return Err("aja files use whole, range or match".into());
        }
        if matches!(selector, Selector::Whole) {
            return super::text_view(
                self.source(path)?,
                self.max_bytes,
                json!({"path":path}),
                control,
            );
        }
        let mut reader = BufReader::new(self.source(path)?);
        // The before-window keeps each line's serialized size beside it, so the
        // window's byte budget is maintained instead of re-measured per line.
        let mut window: VecDeque<(usize, Value)> = VecDeque::new();
        let mut window_bytes = 0usize;
        let mut selected = Vec::new();
        let mut hits: Vec<(usize, Vec<Value>)> = Vec::new();
        let mut total = 0usize;
        let mut budget = crate::measure::Budget::new(crate::aja::FRAME_LIMIT);
        // Room for the view's own envelope.
        budget.admit(256);
        loop {
            control.check()?;
            let mut raw = Vec::new();
            let read = reader
                .by_ref()
                .take((crate::aja::FRAME_LIMIT * 6 + 1) as u64)
                .read_until(b'\n', &mut raw)?;
            if read == 0 {
                break;
            }
            if raw.last() != Some(&b'\n') && raw.len() > crate::aja::FRAME_LIMIT * 6 {
                budget.stop();
                loop {
                    let rest = reader.fill_buf()?;
                    if rest.is_empty() {
                        break;
                    }
                    let end = rest.iter().position(|b| *b == b'\n');
                    let used = end.map_or(rest.len(), |end| end + 1);
                    reader.consume(used);
                    if end.is_some() {
                        break;
                    }
                    control.check()?;
                }
            }
            while raw
                .last()
                .is_some_and(|byte| *byte == b'\n' || *byte == b'\r')
            {
                raw.pop();
            }
            let text = String::from_utf8_lossy(&raw).into_owned();
            let line = json!({"line":total,"text":text});
            let size = crate::measure::value(&line) + 1;
            match selector {
                Selector::Range { from, lines } => {
                    if total >= *from && total < from.saturating_add(*lines) && budget.admit(size) {
                        selected.push(line);
                    }
                }
                Selector::Match {
                    text: needle,
                    before,
                    after,
                } => {
                    // The after-window of a hit that still covers this line.
                    for (_, context) in hits
                        .iter_mut()
                        .rev()
                        .take_while(|(at, _)| total <= at.saturating_add(*after))
                    {
                        if budget.admit(size) {
                            context.push(line.clone());
                        }
                    }
                    if text.contains(needle.as_str()) {
                        let mut context: Vec<Value> =
                            window.iter().map(|(_, line)| line.clone()).collect();
                        context.push(line.clone());
                        let hit = crate::measure::bytes(&context)? + 64;
                        if budget.admit(hit) {
                            hits.push((total, context));
                        }
                    }
                    // The before-window is bounded by lines and by bytes.
                    window.push_back((size, line));
                    window_bytes += size;
                    while window.len() > *before || window_bytes > crate::aja::FRAME_LIMIT {
                        let Some((dropped, _)) = window.pop_front() else {
                            break;
                        };
                        window_bytes = window_bytes.saturating_sub(dropped);
                    }
                }
                _ => {}
            }
            total += 1;
        }
        let value = if matches!(selector, Selector::Range { .. }) {
            let from = match selector {
                Selector::Range { from, .. } => *from,
                _ => 0,
            };
            json!({"path":path,"total_lines":total,"from":from.min(total),"lines":selected,"truncated":budget.truncated()})
        } else {
            let hits: Vec<_> = hits
                .into_iter()
                .map(|(line, context)| json!({"line":line,"context":context}))
                .collect();
            json!({"path":path,"total_lines":total,"matches":hits,"truncated":budget.truncated()})
        };
        if crate::measure::value(&value) > crate::aja::FRAME_LIMIT {
            return Err("selected record view exceeds output budget; narrow the selector".into());
        }
        Ok(value)
    }
}

/// The one way a record is walked: its manifest, then its transcript line by line.
fn start(root: &Path, aja: u64, control: &Control) -> Result<Value> {
    control.check()?;
    let line = BufReader::new(fs::File::open(context(root, aja)?)?)
        .lines()
        .next()
        .ok_or("record has no start entry")??;
    let entry: Value = serde_json::from_str(&line)?;
    if entry["kind"] != "start"
        || entry["view_version"].as_u64() != Some(1)
        || entry["messages"]
            .as_array()
            .is_none_or(std::vec::Vec::is_empty)
    {
        return Err("record has no valid start entry".into());
    }
    Ok(entry)
}

/// A record stores one parent address and this run's starting view.
pub fn lineage(root: &Path, predecessor: Option<u64>, control: &Control) -> Result<Vec<u64>> {
    let mut chain = Vec::new();
    let mut next = predecessor;
    let mut seen = HashSet::new();
    while let Some(aja) = next {
        control.check()?;
        if !seen.insert(aja) {
            return Err("record ancestry cycle".into());
        }
        let manifest = Manifest::read(root, aja)?;
        chain.push(aja);
        start(root, aja, control)?;
        next = manifest.assignment.resume;
    }
    Ok(chain)
}

/// Rebuild the bounded transcript a continuation inherits.
pub fn replay(root: &Path, chain: &[u64], budget: usize, control: &Control) -> Result<Vec<Value>> {
    let mut messages = Vec::new();
    for &aja in chain.iter().rev() {
        let mut pending = VecDeque::new();
        let mut unfinished = None;
        // Fold only when the working set outgrows the budget: folding after every
        // line would re-measure the whole transcript once per record entry.
        let mut size = 0usize;
        for (index, line) in BufReader::new(fs::File::open(context(root, aja)?)?)
            .lines()
            .enumerate()
        {
            control.check()?;
            let entry: Value = serde_json::from_str(&line?)?;
            match entry["kind"].as_str() {
                Some("start") => {
                    if entry["view_version"].as_u64() != Some(1) {
                        return Err("record start has an unsupported view version".into());
                    }
                    let fresh = entry["messages"]
                        .as_array()
                        .ok_or("record start has no messages")?;
                    messages.retain(|message: &Value| message["role"] != "system");
                    for message in fresh.iter().rev() {
                        if message["role"] == "system" {
                            messages.insert(0, message.clone());
                        }
                    }
                    messages.extend(
                        fresh
                            .iter()
                            .filter(|message| message["role"] != "system")
                            .cloned(),
                    );
                    size = crate::measure::messages(&messages);
                }
                Some("assistant") => {
                    if !pending.is_empty() {
                        return Err(
                            "record starts a message before completing its tool group".into()
                        );
                    }
                    let message = entry["message"].clone();
                    crate::aja::validate_assistant(&message)?;
                    if let Some(calls) = message["tool_calls"]
                        .as_array()
                        .filter(|calls| !calls.is_empty())
                    {
                        unfinished = Some(messages.len());
                        for call in calls {
                            pending.push_back(
                                call["id"]
                                    .as_str()
                                    .ok_or("record tool call lacks id")?
                                    .to_string(),
                            );
                        }
                    }
                    size += crate::measure::value(&message);
                    messages.push(message);
                }
                Some("tool") => {
                    let id = pending
                        .pop_front()
                        .ok_or("record has an unmatched tool result")?;
                    if entry
                        .get("id")
                        .and_then(Value::as_str)
                        .is_some_and(|actual| !actual.is_empty() && actual != id)
                    {
                        return Err("record tool ids do not match".into());
                    }
                    let result = fold::tool_result(&entry["result"], aja, index).to_string();
                    size += result.len();
                    messages.push(json!({"role":"tool","tool_call_id":id,"content":result}));
                    if pending.is_empty() {
                        unfinished = None;
                    }
                }
                _ => {}
            }
            if size > budget {
                fold::transcript(&mut messages, budget);
                size = crate::measure::messages(&messages);
            }
        }
        // Keep the complete prefix of an interrupted call group. It may have had
        // effects; its full, partial record remains separately inspectable.
        if let Some(index) = unfinished {
            messages.truncate(index);
        }
        fold::transcript(&mut messages, budget);
    }
    Ok(messages)
}

/// Read one bounded passage of a frozen tool result, at an aja/entry address.
pub fn read(
    root: &Path,
    current: u64,
    ancestors: &[u64],
    arguments: &Value,
    control: &Control,
) -> Result<Value> {
    let aja = arguments["aja"].as_u64().ok_or("record aja is required")?;
    if aja != current && !ancestors.contains(&aja) {
        return Err("record is outside this aja's ancestry".into());
    }
    let wanted = arguments["entry"]
        .as_u64()
        .ok_or("record entry is required")?;
    for (index, line) in BufReader::new(fs::File::open(context(root, aja)?)?)
        .lines()
        .enumerate()
    {
        control.check()?;
        let line = line?;
        if index as u64 != wanted {
            continue;
        }
        let entry: Value = serde_json::from_str(&line)?;
        if entry["kind"] != "tool" {
            return Err("record entry is not a tool result".into());
        }
        let text = entry["result"].to_string();
        let mut page = super::text_page(
            &text,
            usize::try_from(arguments["offset"].as_u64().unwrap_or(0)).unwrap_or(usize::MAX),
            usize::try_from(arguments["bytes"].as_u64().unwrap_or(1024)).unwrap_or(usize::MAX),
            1024,
        );
        page["aja"] = json!(aja);
        page["entry"] = json!(wanted);
        return Ok(page);
    }
    Err("record entry does not exist".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Control;
    use std::fmt::Write as _;
    use std::time::Duration;

    fn control() -> Control {
        Control::new(Duration::from_secs(30), std::sync::Arc::default())
    }

    fn manifest() -> Manifest {
        Manifest {
            format: FORMAT.to_owned(),
            version: VERSION,
            assignment: Assignment {
                task_id: 3,
                objective: String::new(),
                workspace: "/workspace/project".into(),
                acceptance: String::new(),
                input: String::new(),
                resume: None,
                issued_at: 1,
            },
            digest: String::new(),
            environment: Some(EnvironmentRecord::new(
                "git",
                Path::new("/recorded/project"),
                json!({
                    "branch": "eja/aja-3-deadbeef",
                    "commit": "a".repeat(40),
                }),
            )),
            checkpoint: None,
            checkpoint_error: None,
        }
    }

    #[test]
    fn a_manifest_round_trips_the_environment_owned_by_its_provider() {
        let value = serde_json::to_value(manifest()).expect("manifest value");
        let decoded: Manifest = serde_json::from_value(value).expect("manifest");
        let record = decoded.environment.expect("environment record");
        assert_eq!(record.provider, "git");
        assert_eq!(record.root, "/recorded/project");
        assert_eq!(record.state["branch"].as_str(), Some("eja/aja-3-deadbeef"));
        assert_eq!(record.state["commit"].as_str().map(str::len), Some(40));
    }

    #[test]
    fn freezing_a_run_ignores_the_projects_hooks_and_signing_settings() {
        let root = std::env::temp_dir().join(format!("eja-freeze-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let project = root.join("project");
        let state = root.join("state");
        fs::create_dir_all(&project).expect("project");
        fs::write(project.join("note.txt"), "left behind").expect("file");
        let checkpoints = Checkpoints::new(state.join("checkpoints"), state.clone());

        // First freeze creates the repository and one commit.
        checkpoints
            .freeze(1, 0, &project, &control())
            .expect("first");

        // Now make the repository hostile: it asks git to run a hook that refuses
        // every commit, and to require a signature this run does not have. A run
        // must inherit neither from the project it froze.
        let hooks = state.join("checkpoints").join("hooks");
        fs::create_dir_all(&hooks).expect("hooks");
        let hook = hooks.join("pre-commit");
        fs::write(&hook, "#!/bin/sh\nexit 1\n").expect("hook");
        // Git only runs an executable hook, so the hostile setting must be one it
        // would honour if the run did not override it.
        let mut permissions = fs::metadata(&hook).expect("hook metadata").permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut permissions, 0o755);
        fs::set_permissions(&hook, permissions).expect("hook mode");
        let config = state.join("checkpoints").join("config");
        let mut text = fs::read_to_string(&config).unwrap_or_default();
        write!(
            text,
            "\n[core]\n\thooksPath = {}\n[commit]\n\tgpgsign = true\n",
            hooks.display()
        )
        .expect("string writer");
        fs::write(&config, text).expect("config");

        let commit = checkpoints
            .freeze(2, 1, &project, &control())
            .expect("a run is never blocked by the projects own policy");
        assert_eq!(commit.len(), 40, "a commit id, not an error");

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn concurrent_freezes_keep_each_runs_commit_and_contents_together() {
        let root = std::env::temp_dir().join(format!("eja-freeze-parallel-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let alpha = root.join("alpha");
        let beta = root.join("beta");
        fs::create_dir_all(&alpha).unwrap();
        fs::create_dir_all(&beta).unwrap();
        fs::write(alpha.join("result.txt"), "alpha result\n").unwrap();
        fs::write(beta.join("result.txt"), "beta result\n").unwrap();
        let together = std::sync::Arc::new(std::sync::Barrier::new(2));
        let mut threads = Vec::new();
        for (task, tree, expected) in [(1, alpha, "alpha result"), (2, beta, "beta result")] {
            let together = together.clone();
            let git_dir = root.join("checkpoints");
            let tree_root = root.clone();
            threads.push(std::thread::spawn(move || {
                let checkpoints = Checkpoints::new(git_dir.clone(), tree_root.clone());
                together.wait();
                let commit = checkpoints
                    .freeze(task, i64::try_from(task).unwrap(), &tree, &control())
                    .expect("freeze");
                let shown = crate::adapters::environments::git_at(
                    Some(&git_dir),
                    &tree_root,
                    &["show", &format!("{commit}:result.txt")],
                    &control(),
                )
                .expect("read frozen content");
                (commit, expected.to_owned(), shown)
            }));
        }
        let frozen: Vec<_> = threads
            .into_iter()
            .map(|thread| thread.join().expect("freeze thread"))
            .collect();
        assert_ne!(frozen[0].0, frozen[1].0, "each run has its own commit");
        for (_, expected, shown) in frozen {
            assert_eq!(shown, expected);
        }
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn independent_checkpoint_owners_share_one_freeze_lock() {
        let root =
            std::env::temp_dir().join(format!("eja-freeze-independent-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let tree = root.join("tree");
        fs::create_dir_all(&tree).unwrap();
        fs::write(tree.join("result.txt"), "serialized\n").unwrap();
        let git_dir = root.join("checkpoints");
        let first = Checkpoints::new(&git_dir, root.clone());
        let held = first.lock(&control()).expect("repository lock");

        let (sender, receiver) = std::sync::mpsc::channel();
        let worker_root = root.clone();
        let worker_tree = tree;
        let worker = std::thread::spawn(move || {
            let second = Checkpoints::new(worker_root.join("checkpoints"), worker_root);
            sender
                .send(second.freeze(1, 1, &worker_tree, &control()))
                .expect("freeze result");
        });
        assert!(
            receiver.recv_timeout(Duration::from_millis(250)).is_err(),
            "another Checkpoints value cannot enter the same freeze"
        );
        drop(held);
        let commit = receiver
            .recv_timeout(Duration::from_secs(10))
            .expect("freeze after the lock is released")
            .expect("freeze");
        worker.join().expect("freeze thread");
        assert_eq!(commit.len(), 40);
        let _ = fs::remove_dir_all(&root);
    }
}
