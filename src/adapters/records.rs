//! Frozen records: one manifest type, one path resolver, one reader.
//!
//! A record is `K` in the contract: the immutable evidence one run left behind.
//! Everything that reads it — the worker's own tool, a continuation's replay, and
//! the host's `record:` address — resolves the same paths and reads the same
//! manifest through this module, so a record has one shape and one owner.

use super::{EnvironmentKind, EnvironmentRecord, RecordReader};
use crate::{fold, Assignment, Control, Result, Selector};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::{HashSet, VecDeque},
    fs,
    io::{self, BufRead, BufReader, Read},
    path::{Path, PathBuf},
    process::{Child, ChildStdout, Stdio},
};

/// The tool a aja uses to read a passage of its own or an inherited record.
pub const READ_TOOL: &str = "eja_read_record";

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
}

impl Checkpoints {
    #[must_use]
    pub fn new(git_dir: PathBuf, private: impl AsRef<Path>) -> Self {
        Self {
            git_dir,
            private: super::environments::canonical_boundary(private.as_ref()),
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
pub struct Manifest {
    /// The activation contract this run was given.
    pub(crate) assignment: Assignment,
    /// The host's account of what the run left behind.
    #[serde(default)]
    pub(crate) digest: String,
    /// Where it ran. Older records call this field `world`.
    #[serde(default, alias = "world")]
    pub(crate) environment: String,
    /// The commit a `record:` file is read from, when one was taken.
    #[serde(default)]
    pub(crate) checkpoint: Option<String>,
    #[serde(default)]
    pub(crate) checkpoint_error: Option<String>,
    /// The environment that measured this run. `world_*` remains accepted for
    /// records written before the environment terminology was introduced.
    #[serde(default, alias = "world_kind", skip_serializing_if = "Option::is_none")]
    pub(crate) environment_kind: Option<String>,
    #[serde(
        default,
        alias = "world_root",
        skip_serializing_if = "String::is_empty"
    )]
    pub(crate) environment_root: String,
    #[serde(
        default,
        alias = "world_branch",
        skip_serializing_if = "Option::is_none"
    )]
    pub(crate) environment_branch: Option<String>,
    #[serde(
        default,
        alias = "world_commit",
        skip_serializing_if = "Option::is_none"
    )]
    pub(crate) environment_commit: Option<String>,
    /// The transcript's file name in this aja's directory. The protocol fixes it;
    /// it is recorded so a record states its own layout for a reader that only
    /// has the file.
    #[serde(default = "default_context")]
    pub(crate) context: String,
    #[serde(default)]
    pub(crate) context_bytes: usize,
    /// Absent in records written before the field existed: an unfrozen record is
    /// never replayed, and the default says so.
    #[serde(default)]
    pub(crate) frozen: bool,
}

fn default_context() -> String {
    "context.jsonl".to_string()
}

impl Manifest {
    fn path(root: &Path, aja: u64) -> Result<PathBuf> {
        Ok(directory(root, aja)?.join("aja.json"))
    }

    pub(crate) fn read(root: &Path, aja: u64) -> Result<Self> {
        Ok(serde_json::from_slice(&fs::read(Self::path(root, aja)?)?)?)
    }

    fn environment(&self) -> EnvironmentRecord {
        let old_worktree = self.environment.strip_prefix("worktree ");
        let kind = match self.environment_kind.as_deref() {
            Some("shared") => EnvironmentKind::Shared,
            Some("worktree") => EnvironmentKind::Worktree,
            None if self.environment == "shared" => EnvironmentKind::Shared,
            None if old_worktree.is_some() => EnvironmentKind::Worktree,
            Some(_) | None => EnvironmentKind::Unknown,
        };
        let branch = self.environment_branch.clone().or_else(|| {
            old_worktree
                .and_then(|rest| rest.split_once(" at "))
                .map(|(branch, _)| branch.to_owned())
        });
        let root = if self.environment_root.is_empty() {
            self.assignment.workspace.clone()
        } else {
            self.environment_root.clone()
        };
        EnvironmentRecord {
            kind,
            root,
            branch,
            commit: self.environment_commit.clone(),
        }
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
        Self {
            root: root.into(),
            checkpoints: Some(checkpoints.into()),
            max_bytes: crate::aja::FRAME_LIMIT,
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
    fn environment(&self, aja: u64) -> Result<Option<EnvironmentRecord>> {
        match Manifest::read(&self.root, aja) {
            Ok(manifest) => Ok(Some(manifest.environment())),
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
        || entry["messages"]
            .as_array()
            .is_none_or(std::vec::Vec::is_empty)
    {
        return Err("record has no valid start entry".into());
    }
    Ok(entry)
}

/// New records store only a parent address and this run's start messages. Legacy
/// records already contain their whole starting view and form an ancestry root.
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
        if !manifest.frozen {
            return Err("predecessor record is not frozen".into());
        }
        chain.push(aja);
        let initial = start(root, aja, control)?;
        next = if initial["view_version"] == 1 {
            manifest.assignment.resume
        } else {
            None
        };
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
                    let fresh = entry["messages"]
                        .as_array()
                        .ok_or("record start has no messages")?;
                    if entry["view_version"] != 1 {
                        messages.clear();
                    }
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

    fn manifest(environment: &str) -> Manifest {
        Manifest {
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
            environment: environment.into(),
            environment_kind: None,
            environment_root: String::new(),
            environment_branch: None,
            environment_commit: None,
            checkpoint: None,
            checkpoint_error: None,
            context: default_context(),
            context_bytes: 0,
            frozen: true,
        }
    }

    #[test]
    fn existing_records_infer_their_environment_and_new_records_read_what_was_frozen() {
        let shared = manifest("shared").environment();
        assert_eq!(shared.kind, EnvironmentKind::Shared);
        assert_eq!(shared.root, "/workspace/project");

        let old_worktree = manifest("worktree eja/aja-3-deadbeef at abcdef123456").environment();
        assert_eq!(old_worktree.kind, EnvironmentKind::Worktree);
        assert_eq!(old_worktree.branch.as_deref(), Some("eja/aja-3-deadbeef"));
        assert_eq!(old_worktree.root, "/workspace/project");

        assert_eq!(
            manifest("none").environment().kind,
            EnvironmentKind::Unknown
        );

        let mut recorded = manifest("worktree eja/aja-3-deadbeef at abcdef123456");
        let commit = "a".repeat(40);
        recorded.environment_kind = Some("worktree".into());
        recorded.environment_root = "/recorded/project".into();
        recorded.environment_branch = Some("eja/aja-3-deadbeef".into());
        recorded.environment_commit = Some(commit.clone());
        let record = recorded.environment();
        assert_eq!(record.root, "/recorded/project");
        assert_eq!(record.commit.as_deref(), Some(commit.as_str()));
    }

    #[test]
    fn legacy_manifest_field_names_decode_into_the_environment_record() {
        let mut old = manifest("worktree eja/aja-3-deadbeef at abcdef123456");
        old.environment_kind = Some("worktree".into());
        old.environment_root = "/recorded/project".into();
        old.environment_branch = Some("eja/aja-3-deadbeef".into());
        old.environment_commit = Some("a".repeat(40));
        let mut value = serde_json::to_value(old).expect("manifest value");
        let fields = value.as_object_mut().expect("manifest object");
        let environment = fields.remove("environment").expect("environment");
        fields.insert("world".into(), environment);
        for (current, legacy) in [
            ("environment_kind", "world_kind"),
            ("environment_root", "world_root"),
            ("environment_branch", "world_branch"),
            ("environment_commit", "world_commit"),
        ] {
            let value = fields.remove(current).expect(current);
            fields.insert(legacy.into(), value);
        }

        let decoded: Manifest = serde_json::from_value(value).expect("legacy manifest");
        let record = decoded.environment();
        assert_eq!(record.kind, EnvironmentKind::Worktree);
        assert_eq!(record.root, "/recorded/project");
        assert_eq!(record.branch.as_deref(), Some("eja/aja-3-deadbeef"));
        assert_eq!(record.commit.as_deref(), Some("a".repeat(40).as_str()));
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
}
