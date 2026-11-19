//! Host tools. Connectors implement this same boundary; ajas never load plugins.
use super::{
    cut_chars,
    environments::{canonical_boundary, ENTRIES},
    Capabilities, Control,
};
use crate::{aja::FRAME_LIMIT, Result, WitnessContext};
use serde_json::{json, Value};
use std::{
    collections::HashSet,
    fs,
    io::Read,
    path::{Component, Path, PathBuf},
    time::Duration,
};

/// A search pattern is an argument, not output: it is echoed only up to the
/// room left for the results, so a huge pattern cannot overflow the frame.
const PATTERN_CAP: usize = 1024;

/// The frame bounds the serialized result, not the raw text: escaping grows
/// it, so a view is measured as JSON and halved until it fits.
fn fit_frame(view: Value) -> Result<Value> {
    let mut view = view;
    loop {
        if crate::measure::value(&view) < FRAME_LIMIT {
            return Ok(view);
        }
        if !shrink(&mut view) {
            return Err("tool output does not fit the frame".into());
        }
        if let Value::Object(map) = &mut view {
            map.insert("truncated".to_owned(), Value::Bool(true));
        }
    }
}

/// Halve the largest string in the view; with none left, drop array entries.
fn shrink(value: &mut Value) -> bool {
    match value {
        Value::String(text) if !text.is_empty() => {
            *text = cut_chars(text, text.len() / 2).to_owned();
            true
        }
        Value::Array(items) => {
            let largest = items
                .iter()
                .enumerate()
                .max_by_key(|(_, item)| crate::measure::value(item))
                .map(|(index, _)| index);
            largest.is_some_and(|index| {
                if !shrink(&mut items[index]) {
                    items.remove(index);
                }
                true
            })
        }
        Value::Object(map) => {
            let largest = map
                .iter()
                .max_by_key(|(_, item)| crate::measure::value(item))
                .map(|(key, _)| key.clone());
            largest.is_some_and(|key| shrink(map.get_mut(&key).expect("key just listed")))
        }
        _ => false,
    }
}

pub struct ToolContext<'a> {
    pub aja_id: u64,
    /// The workspace this run may touch. A host may assemble connectors without
    /// one; a local file tool then has nothing to resolve against.
    pub workspace: Option<&'a Path>,
    /// The host's own state, which no aja may read or write.
    pub private: &'a Path,
    pub control: &'a Control,
}

impl ToolContext<'_> {
    pub fn workspace(&self) -> Result<&Path> {
        self.workspace
            .ok_or_else(|| "this host assembled no workspace".into())
    }

    // A reserved worktree can live beneath the state directory. Protect private
    // state inside a granted workspace while the workspace resolver keeps an
    // isolated run within its own root.
    fn private_in_workspace(&self) -> Option<PathBuf> {
        let workspace = canonical_boundary(self.workspace?);
        let private = canonical_boundary(self.private);
        private.starts_with(&workspace).then_some(private)
    }

    fn is_private(&self, path: &Path) -> bool {
        self.private_in_workspace()
            .is_some_and(|private| canonical_boundary(path).starts_with(private))
    }
}

pub trait ToolProvider: Send + Sync {
    fn definitions(&self, assignment: &crate::Assignment) -> Vec<Value>;
    /// Trusted host implementations must honor control and bound their output/resources.
    fn call(&self, context: &ToolContext<'_>, name: &str, arguments: &Value) -> Result<Value>;
    /// The host's own read of this tool. A provider resolves its own paths and
    /// authority; the registry only routes to the provider that owns the name.
    fn inspect(
        &self,
        context: &ToolContext<'_>,
        name: &str,
        arguments: &Value,
        witness: bool,
    ) -> Result<Value> {
        if witness {
            if !self.witnesses(name) {
                return Err(format!("{name} is not a declared witness").into());
            }
        } else if !self.observes(name) {
            return Err("EJA may only observe read-only host tools".into());
        }
        self.call(context, name, arguments)
    }
    /// Whether the host itself may call this tool to inspect an environment. An
    /// effect is never an observation; connectors declare their own read-only
    /// tools instead.
    fn observes(&self, _name: &str) -> bool {
        false
    }
    fn witnesses(&self, name: &str) -> bool {
        self.observes(name)
    }
    /// Whether this provider declares the name: the registry routes by it.
    fn owns(&self, name: &str) -> bool;
}

/// The reserved name of the built-in workspace group.
pub const WORKSPACE_GROUP: &str = "workspace";

/// The built-in group's tools, in presentation order. Definitions, routing and
/// the settings view all derive from this one list.
pub const WORKSPACE_TOOLS: [&str; 7] = [
    "read_file",
    "list_dir",
    "edit_file",
    "write_file",
    "glob",
    "grep",
    "shell",
];

impl<T: ToolProvider + ?Sized> ToolProvider for std::sync::Arc<T> {
    fn definitions(&self, assignment: &crate::Assignment) -> Vec<Value> {
        (**self).definitions(assignment)
    }
    fn call(&self, context: &ToolContext<'_>, name: &str, arguments: &Value) -> Result<Value> {
        (**self).call(context, name, arguments)
    }
    fn inspect(
        &self,
        context: &ToolContext<'_>,
        name: &str,
        arguments: &Value,
        witness: bool,
    ) -> Result<Value> {
        (**self).inspect(context, name, arguments, witness)
    }
    fn observes(&self, name: &str) -> bool {
        (**self).observes(name)
    }
    fn witnesses(&self, name: &str) -> bool {
        (**self).witnesses(name)
    }
    fn owns(&self, name: &str) -> bool {
        (**self).owns(name)
    }
}

/// No absolute paths, parent traversal, or symlinks in a aja's file capabilities.
pub fn relative_file(root: &Path, name: &str) -> Result<PathBuf> {
    if name.is_empty() || name.len() > 4096 {
        return Err("invalid path".into());
    }
    let path = Path::new(name);
    let mut full = root.to_path_buf();
    for part in path.components() {
        let Component::Normal(part) = part else {
            return Err("path must be relative without traversal".into());
        };
        full.push(part);
        if full
            .symlink_metadata()
            .is_ok_and(|m| m.file_type().is_symlink())
        {
            return Err("symlink paths are not allowed".into());
        }
    }
    Ok(full)
}

/// Resolve a workspace path: relative to the project, or absolute inside it.
/// Anything else is refused, so the canonical path forms a caller sees in a task
/// still address the workspace.
pub fn workspace_path(root: &Path, name: &str) -> Result<PathBuf> {
    if name.is_empty() || name == "." {
        return Ok(root.to_path_buf());
    }
    let path = Path::new(name);
    if path.is_absolute() {
        let inside = path
            .strip_prefix(root)
            .map_err(|_| "absolute paths must stay inside the workspace")?;
        return if inside.as_os_str().is_empty() {
            Ok(root.to_path_buf())
        } else {
            relative_file(root, &inside.to_string_lossy())
        };
    }
    relative_file(root, name)
}

/// Resolve a path against several granted roots, first match wins.
///
/// An absolute path must stay inside one grant; a relative one is tried in
/// each. This is the host's read boundary: every workspace it granted, and
/// nothing else.
pub fn granted_path(roots: &[PathBuf], name: &str) -> Result<PathBuf> {
    let path = Path::new(name);
    if path.is_absolute() {
        for root in roots {
            if let Ok(inside) = path.strip_prefix(root) {
                return if inside.as_os_str().is_empty() {
                    Ok(root.clone())
                } else {
                    relative_file(root, &inside.to_string_lossy())
                };
            }
        }
        return Err("absolute paths must stay inside a granted workspace".into());
    }
    // A relative name is tried in each root; a root that merely lacks the file
    // or refuses it (traversal, symlink) does not end the search: the name may
    // still address a clean file in a later grant.
    let mut missing = false;
    for root in roots {
        match relative_file(root, name) {
            Ok(full) if full.exists() => return Ok(full),
            Ok(_) => missing = true,
            Err(_) => {}
        }
    }
    if missing {
        return Err(std::io::Error::from(std::io::ErrorKind::NotFound).into());
    }
    Err("path is outside every granted workspace".into())
}

pub struct FileTools;

/// The workspace provider.
///
/// It owns the local path authority: a run's tool calls stay in the environment
/// it was given, while the host's own observations resolve across every granted
/// root and never into private state.
pub struct WorkspaceTools {
    files: bool,
    shell: bool,
    workspace: Option<PathBuf>,
    roots: Vec<PathBuf>,
    private: PathBuf,
}

impl WorkspaceTools {
    pub fn new(
        files: bool,
        shell: bool,
        workspace: Option<PathBuf>,
        roots: Vec<PathBuf>,
        private: impl Into<PathBuf>,
    ) -> Self {
        Self {
            files,
            shell,
            workspace,
            roots,
            private: private.into(),
        }
    }

    fn observed_path(
        &self,
        ctx: &ToolContext<'_>,
        arguments: &Value,
        witness: bool,
    ) -> Result<(PathBuf, PathBuf)> {
        let path = arguments["path"].as_str().ok_or("missing path")?;
        // A witness is bound to the task's fixed workspace. This keeps two
        // granted roots with the same relative filename from answering for one
        // another.
        if let Some(workspace) = ctx.workspace {
            let full = workspace_path(workspace, path)?;
            if canonical_boundary(&full).starts_with(canonical_boundary(&self.private)) {
                return Err("the host's private state is not readable".into());
            }
            return Ok((full, workspace.to_path_buf()));
        }
        if witness {
            return Err("a file witness needs the task's fixed workspace".into());
        }
        let full = granted_path(&self.roots, path)?;
        if canonical_boundary(&full).starts_with(canonical_boundary(&self.private)) {
            return Err("the host's private state is not readable".into());
        }
        let root = if let Some(root) = self
            .roots
            .iter()
            .filter(|root| full.starts_with(root))
            .max_by_key(|root| root.components().count())
        {
            root.clone()
        } else {
            self.workspace
                .clone()
                .ok_or("this host assembled no workspace")?
        };
        Ok((full, root))
    }
}

impl ToolProvider for WorkspaceTools {
    fn definitions(&self, _: &crate::Assignment) -> Vec<Value> {
        WORKSPACE_TOOLS
            .iter()
            .filter(|name| self.owns(name))
            .map(|name| workspace_definition(name))
            .collect()
    }

    fn call(&self, ctx: &ToolContext<'_>, name: &str, arguments: &Value) -> Result<Value> {
        if !self.owns(name) {
            return Err("local tool is disabled".into());
        }
        FileTools.call(ctx, name, arguments)
    }

    fn inspect(
        &self,
        ctx: &ToolContext<'_>,
        name: &str,
        arguments: &Value,
        witness: bool,
    ) -> Result<Value> {
        let allowed = if witness {
            self.witnesses(name)
        } else {
            self.observes(name)
        };
        if !allowed {
            return Err(if witness {
                format!("{name} is not a declared witness").into()
            } else {
                "EJA only observes; edits belong to ajas".into()
            });
        }
        let (full, root) = self.observed_path(ctx, arguments, witness)?;
        let context = ToolContext {
            aja_id: ctx.aja_id,
            workspace: Some(&root),
            private: &self.private,
            control: ctx.control,
        };
        FileTools.call(
            &context,
            name,
            &json!({"path": full.to_string_lossy().into_owned()}),
        )
    }

    fn observes(&self, name: &str) -> bool {
        self.files && FileTools.observes(name)
    }

    fn witnesses(&self, name: &str) -> bool {
        self.files && FileTools.witnesses(name)
    }

    fn owns(&self, name: &str) -> bool {
        if name == "shell" {
            self.shell
        } else {
            self.files && FileTools.owns(name)
        }
    }
}

/// One tool's declaration, from the group's single name list.
fn workspace_definition(name: &str) -> Value {
    let (description, properties, required) = match name {
        "read_file" => (
            "Read a UTF-8 file from the workspace. Large files are truncated.",
            json!({"path":{"type":"string"}}),
            json!(["path"]),
        ),
        "list_dir" => (
            "List one directory level of the workspace: names, kind and size. Bounded by the output frame, reporting how many entries were kept.",
            json!({"path":{"type":"string"}}),
            json!(["path"]),
        ),
        "edit_file" => (
            "Replace one exact occurrence in a workspace file. Fails unless the find text occurs exactly once, so an edit is always unambiguous.",
            json!({"path":{"type":"string"},"find":{"type":"string"},"replace":{"type":"string"}}),
            json!(["path","find","replace"]),
        ),
        "write_file" => (
            "Write UTF-8 content into the workspace.",
            json!({"path":{"type":"string"},"text":{"type":"string"}}),
            json!(["path","text"]),
        ),
        "glob" => (
            "Find workspace files by glob pattern (*, ?, **), newest first. Skips .git, the host's private state and symlinks. Bounded: reports how many matches were kept.",
            json!({"pattern":{"type":"string"},"path":{"type":"string","description":"directory to search from; defaults to the workspace root"}}),
            json!(["pattern"]),
        ),
        "grep" => (
            "Search workspace file contents with a regular expression. Returns file:line: text matches, bounded by the output frame. Skips .git, the host's private state, symlinks and oversized files.",
            json!({"pattern":{"type":"string"},"path":{"type":"string","description":"directory to search from; defaults to the workspace root"},"include":{"type":"string","description":"optional glob the file path must match, e.g. *.rs"}}),
            json!(["pattern"]),
        ),
        "shell" => (
            "Run one shell command in the workspace (sh -c). Returns exit_code, bounded stdout/stderr and whether the run's budget ended it. A non-zero exit is an answer to read, not a tool failure. The command inherits the run's deadline and cancellation; its whole process group is killed when the run stops.",
            json!({"command":{"type":"string"},"timeout_ms":{"type":"integer","description":"optional per-command deadline, capped by the run's remaining budget"}}),
            json!(["command"]),
        ),
        _ => unreachable!("the workspace name list is the definition list"),
    };
    json!({"type":"function","function":{"name":name,"description":description,"parameters":{"type":"object","properties":properties,"required":required,"additionalProperties":false}}})
}

impl ToolProvider for FileTools {
    fn definitions(&self, _: &crate::Assignment) -> Vec<Value> {
        WORKSPACE_TOOLS
            .iter()
            .map(|name| workspace_definition(name))
            .collect()
    }
    fn call(&self, ctx: &ToolContext<'_>, name: &str, args: &Value) -> Result<Value> {
        ctx.control.check()?;
        let workspace = ctx.workspace()?;
        let value = match name {
            "read_file" => {
                let path = args["path"].as_str().ok_or("missing path")?;
                let full = workspace_path(workspace, path)?;
                if ctx.is_private(&full) {
                    return Err("the host's private state is not readable".into());
                }
                let file = fs::File::open(full)?;
                if !file.metadata()?.is_file() {
                    return Err("not a regular file".into());
                }
                super::text_view(file, FRAME_LIMIT, json!({}), ctx.control)?
            }
            "list_dir" => {
                let path = args["path"].as_str().ok_or("missing path")?;
                let full = workspace_path(workspace, path)?;
                if ctx.is_private(&full) {
                    return Err("the host's private state is not readable".into());
                }
                let mut entries = Vec::new();
                let mut total = 0usize;
                let mut bytes = 256usize;
                for entry in fs::read_dir(&full)? {
                    ctx.control.check()?;
                    let entry = entry?;
                    total += 1;
                    let metadata = entry.metadata()?;
                    let name = entry.file_name().to_string_lossy().to_string();
                    let line = if metadata.is_dir() {
                        format!("{name}/")
                    } else {
                        format!("{name} ({} bytes)", metadata.len())
                    };
                    let size = crate::measure::bytes(&line)? + 1;
                    if bytes.saturating_add(size) < FRAME_LIMIT {
                        bytes += size;
                        entries.push(line);
                    }
                }
                let shown = entries.len();
                json!({"path":path,"entries":entries,"showing":shown,"total":total,"truncated":shown<total})
            }
            "edit_file" => {
                let path = args["path"].as_str().ok_or("missing path")?;
                let find = args["find"].as_str().ok_or("missing find")?;
                let replace = args["replace"].as_str().ok_or("missing replace")?;
                let full = workspace_path(workspace, path)?;
                if ctx.is_private(&full) {
                    return Err("the host's private state is not writable".into());
                }
                if fs::metadata(&full)?.len() > FRAME_LIMIT as u64 {
                    return Err("file is too large to edit".into());
                }
                let mut bytes = Vec::new();
                fs::File::open(&full)?
                    .take(FRAME_LIMIT as u64 + 1)
                    .read_to_end(&mut bytes)?;
                let mut text = String::from_utf8(bytes)?;
                if text.len() > FRAME_LIMIT {
                    return Err("file is too large to edit".into());
                }
                let matches = text.matches(find).count();
                if matches != 1 {
                    return Err(format!("find must occur exactly once (found {matches})").into());
                }
                text = text.replacen(find, replace, 1);
                fs::write(full, text)?;
                json!({"edited":path})
            }
            "write_file" => {
                let path = args["path"].as_str().ok_or("missing path")?;
                let text = args["text"].as_str().ok_or("missing text")?;
                let full = workspace_path(workspace, path)?;
                if ctx.is_private(&full) {
                    return Err("the host's private state is not writable".into());
                }
                if let Some(parent) = full.parent() {
                    fs::create_dir_all(parent)?;
                }
                fs::write(full, text)?;
                json!({"written":path})
            }
            "glob" => return glob(ctx, args),
            "grep" => return grep(ctx, args),
            "shell" => return shell(ctx, args),
            _ => return Err("tool not allowed".into()),
        };
        Ok(value)
    }
    fn observes(&self, name: &str) -> bool {
        matches!(name, "read_file" | "list_dir")
    }
    fn owns(&self, name: &str) -> bool {
        WORKSPACE_TOOLS.contains(&name)
    }
}

/// The application's tool registry. It knows nothing about a provider's
/// protocol, workspace roots or tools: it concatenates definitions and routes a
/// name to the one provider that declares it.
pub struct ToolRegistry {
    providers: Vec<Box<dyn ToolProvider>>,
    private: PathBuf,
}

impl ToolRegistry {
    pub fn new(private: impl Into<PathBuf>, providers: Vec<Box<dyn ToolProvider>>) -> Result<Self> {
        let mut registry = Self {
            providers: Vec::new(),
            private: private.into().canonicalize()?,
        };
        for provider in providers {
            registry.register(provider)?;
        }
        Ok(registry)
    }

    pub fn register(&mut self, provider: Box<dyn ToolProvider>) -> Result<()> {
        let assignment = crate::Assignment {
            task_id: 0,
            objective: String::new(),
            workspace: String::new(),
            acceptance: String::new(),
            input: String::new(),
            resume: None,
            issued_at: 0,
        };
        let mut names = self
            .providers
            .iter()
            .flat_map(|provider| provider.definitions(&assignment))
            .filter_map(|definition| {
                definition["function"]["name"]
                    .as_str()
                    .map(ToOwned::to_owned)
            })
            .collect::<HashSet<_>>();
        for definition in provider.definitions(&assignment) {
            let name = definition["function"]["name"]
                .as_str()
                .ok_or("host tool definition has no name")?;
            if !names.insert(name.to_owned()) {
                return Err(format!("host tool name is declared twice: {name}").into());
            }
        }
        self.providers.push(provider);
        Ok(())
    }

    fn provider(&self, tool: &str) -> Result<&dyn ToolProvider> {
        self.providers
            .iter()
            .find(|provider| provider.owns(tool))
            .map(Box::as_ref)
            .ok_or_else(|| format!("{tool} is not a declared host tool").into())
    }

    fn inspect(
        &self,
        tool: &str,
        arguments: &Value,
        control: &Control,
        witness: bool,
    ) -> Result<Value> {
        let context = ToolContext {
            aja_id: 0,
            workspace: None,
            private: &self.private,
            control,
        };
        self.provider(tool)?
            .inspect(&context, tool, arguments, witness)
    }
}

impl ToolProvider for ToolRegistry {
    fn definitions(&self, assignment: &crate::Assignment) -> Vec<Value> {
        self.providers
            .iter()
            .flat_map(|provider| provider.definitions(assignment))
            .collect()
    }

    fn call(&self, ctx: &ToolContext<'_>, name: &str, args: &Value) -> Result<Value> {
        self.provider(name)?.call(ctx, name, args)
    }

    fn inspect(
        &self,
        ctx: &ToolContext<'_>,
        name: &str,
        args: &Value,
        witness: bool,
    ) -> Result<Value> {
        let _ = ctx;
        self.provider(name)?.inspect(ctx, name, args, witness)
    }

    fn observes(&self, name: &str) -> bool {
        self.provider(name)
            .is_ok_and(|provider| provider.observes(name))
    }

    fn witnesses(&self, name: &str) -> bool {
        self.provider(name)
            .is_ok_and(|provider| provider.witnesses(name))
    }

    fn owns(&self, name: &str) -> bool {
        self.providers.iter().any(|provider| provider.owns(name))
    }
}

impl Capabilities for ToolRegistry {
    fn observe(&self, tool: &str, arguments: &Value, parent: &Control) -> Result<Value> {
        self.inspect(
            tool,
            arguments,
            &parent.child(Duration::from_secs(60)),
            false,
        )
    }

    fn witness(
        &self,
        request: &Value,
        task: WitnessContext<'_>,
        control: &Control,
    ) -> Result<Value> {
        let tool = request["tool"]
            .as_str()
            .filter(|name| !name.is_empty())
            .ok_or("a witness names one host tool")?;
        let empty = json!({});
        let arguments = request.get("arguments").unwrap_or(&empty);
        let control = control.child(Duration::from_secs(120));
        let workspace = (!task.workspace.is_empty()).then(|| std::path::Path::new(task.workspace));
        let context = ToolContext {
            aja_id: task.task_id,
            workspace,
            private: &self.private,
            control: &control,
        };
        self.provider(tool)?
            .inspect(&context, tool, arguments, true)
    }
}

/// Every regular file under one root, skipping the host's private state, `.git`
/// and symlinks. The walk is bounded by the run's control and the entry cap.
fn walk(root: &Path, private: Option<&Path>, control: &Control) -> Result<(Vec<PathBuf>, bool)> {
    let mut files = Vec::new();
    let mut truncated = false;
    let mut pending = vec![root.to_path_buf()];
    let private = private.map(canonical_boundary);
    while let Some(dir) = pending.pop() {
        control.check()?;
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            control.check()?;
            if files.len() >= ENTRIES {
                truncated = true;
                break;
            }
            let path = entry.path();
            if private
                .as_ref()
                .is_some_and(|private| canonical_boundary(&path).starts_with(private))
            {
                continue;
            }
            let Ok(metadata) = path.symlink_metadata() else {
                continue;
            };
            if metadata.file_type().is_symlink() {
                continue;
            }
            if metadata.is_dir() {
                if entry.file_name() != ".git" {
                    pending.push(path);
                }
            } else if metadata.is_file() {
                files.push(path);
            }
        }
        if truncated {
            break;
        }
    }
    Ok((files, truncated))
}

/// One glob segment against one name: `*` and `?` only, no path separators.
fn segment_matches(pattern: &str, name: &str) -> bool {
    let pattern: Vec<char> = pattern.chars().collect();
    let name: Vec<char> = name.chars().collect();
    let (mut pi, mut ni) = (0, 0);
    let (mut star, mut mark) = (usize::MAX, 0);
    while ni < name.len() {
        if pi < pattern.len() && (pattern[pi] == '?' || pattern[pi] == name[ni]) {
            pi += 1;
            ni += 1;
        } else if pi < pattern.len() && pattern[pi] == '*' {
            star = pi;
            mark = ni;
            pi += 1;
        } else if star != usize::MAX {
            pi = star + 1;
            mark += 1;
            ni = mark;
        } else {
            return false;
        }
    }
    while pi < pattern.len() && pattern[pi] == '*' {
        pi += 1;
    }
    pi == pattern.len()
}

/// A glob over a relative path: `**` spans directories, other segments match
/// one component each. A pattern without a separator also matches by file
/// name, the way a harness's include glob does.
fn glob_matches(pattern: &str, path: &str) -> bool {
    let segments: Vec<&str> = pattern.split('/').filter(|part| !part.is_empty()).collect();
    let parts: Vec<&str> = path.split('/').filter(|part| !part.is_empty()).collect();
    if !pattern.contains('/') {
        return parts
            .last()
            .is_some_and(|name| segment_matches(pattern, name));
    }
    glob_walk(&segments, &parts)
}

fn glob_walk(segments: &[&str], parts: &[&str]) -> bool {
    let Some((first, rest)) = segments.split_first() else {
        return parts.is_empty();
    };
    if *first == "**" {
        return (0..=parts.len()).any(|skip| glob_walk(rest, &parts[skip..]));
    }
    parts
        .split_first()
        .is_some_and(|(name, tail)| segment_matches(first, name) && glob_walk(rest, tail))
}

fn search_root(ctx: &ToolContext<'_>, args: &Value) -> Result<PathBuf> {
    let path = args["path"].as_str().unwrap_or(".");
    let full = workspace_path(ctx.workspace()?, path)?;
    if ctx.is_private(&full) {
        return Err("the host's private state is not searchable".into());
    }
    if !full.is_dir() {
        return Err("search path is not a directory".into());
    }
    Ok(full)
}

fn glob(ctx: &ToolContext<'_>, args: &Value) -> Result<Value> {
    let pattern = args["pattern"].as_str().ok_or("missing pattern")?;
    let shown = cut_chars(pattern, PATTERN_CAP);
    let root = search_root(ctx, args)?;
    let workspace = ctx.workspace()?;
    let private = ctx.private_in_workspace();
    let (files, mut truncated) = walk(&root, private.as_deref(), ctx.control)?;
    let mut matched: Vec<(String, i64)> = Vec::new();
    for file in files {
        let Ok(relative) = file.strip_prefix(workspace) else {
            continue;
        };
        let name = relative.to_string_lossy().into_owned();
        if !glob_matches(pattern, &name) {
            continue;
        }
        let modified = file
            .metadata()
            .and_then(|meta| meta.modified())
            .ok()
            .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
            .map_or(0, |duration| i64::try_from(duration.as_secs()).unwrap_or(0));
        matched.push((name, modified));
    }
    matched.sort_by(|left, right| right.1.cmp(&left.1).then_with(|| left.0.cmp(&right.0)));
    let total = matched.len();
    let mut kept = Vec::new();
    let mut budget = crate::measure::Budget::new(FRAME_LIMIT);
    budget.admit(256);
    for (name, _) in &matched {
        if !budget.admit(crate::measure::bytes(name)? + 1) {
            truncated = true;
            break;
        }
        kept.push(name.clone());
    }
    fit_frame(
        json!({"pattern":shown,"matches":kept,"showing":kept.len(),"total":total,"truncated":truncated}),
    )
}

fn grep(ctx: &ToolContext<'_>, args: &Value) -> Result<Value> {
    let pattern = args["pattern"].as_str().ok_or("missing pattern")?;
    let shown = cut_chars(pattern, PATTERN_CAP);
    let regex =
        regex_lite::Regex::new(pattern).map_err(|error| format!("invalid pattern: {error}"))?;
    let include = args["include"].as_str().filter(|glob| !glob.is_empty());
    let root = search_root(ctx, args)?;
    let workspace = ctx.workspace()?;
    let private = ctx.private_in_workspace();
    let (files, mut truncated) = walk(&root, private.as_deref(), ctx.control)?;
    let mut matches = Vec::new();
    let mut budget = crate::measure::Budget::new(FRAME_LIMIT);
    budget.admit(256);
    'files: for file in files {
        ctx.control.check()?;
        let Ok(relative) = file.strip_prefix(workspace) else {
            continue;
        };
        let name = relative.to_string_lossy().into_owned();
        if include.is_some_and(|glob| !glob_matches(glob, &name)) {
            continue;
        }
        let Ok(metadata) = file.metadata() else {
            continue;
        };
        if metadata.len() > FRAME_LIMIT as u64 {
            continue;
        }
        let Ok(bytes) = fs::read(&file) else {
            continue;
        };
        let Ok(text) = String::from_utf8(bytes) else {
            continue;
        };
        for (index, line) in text.lines().enumerate() {
            if !regex.is_match(line) {
                continue;
            }
            let entry = format!("{}:{}: {}", name, index + 1, line.trim_end());
            if !budget.admit(crate::measure::bytes(&entry)? + 1) {
                truncated = true;
                break 'files;
            }
            matches.push(entry);
        }
    }
    fit_frame(
        json!({"pattern":shown,"matches":matches,"showing":matches.len(),"truncated":truncated}),
    )
}

fn shell(ctx: &ToolContext<'_>, args: &Value) -> Result<Value> {
    let command = args["command"].as_str().ok_or("missing command")?;
    if command.trim().is_empty() {
        return Err("command is empty".into());
    }
    let mut control = ctx.control.clone();
    if let Some(timeout) = args["timeout_ms"].as_u64().filter(|ms| *ms > 0) {
        control = control.child(Duration::from_millis(timeout));
    }
    let mut process = std::process::Command::new("sh");
    process
        .arg("-c")
        .arg(command)
        .current_dir(ctx.workspace()?)
        .env_remove("LNA_API_KEY")
        .env_remove("JIYUAN_API_KEY")
        .env_remove("OPENAI_API_KEY")
        .stdin(std::process::Stdio::null());
    fit_frame(super::process::shell_output(
        &mut process,
        &control,
        FRAME_LIMIT / 8,
    )?)
}
