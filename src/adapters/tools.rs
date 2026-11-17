//! Host tools. Connectors implement this same boundary; ajas never load plugins.
use super::Control;
use crate::{aja::FRAME_LIMIT, Result};
use serde_json::{json, Value};
use std::{
    fs,
    io::Read,
    path::{Component, Path, PathBuf},
    time::Duration,
};

pub struct ToolContext<'a> {
    pub aja_id: u64,
    /// The one workspace: the project. There is no second place to write.
    pub project: &'a Path,
    /// The host's own state, which no aja may read or write.
    pub private: &'a Path,
    pub control: &'a Control,
}

pub trait ToolGateway: Send + Sync {
    fn definitions(&self, assignment: &crate::Assignment) -> Vec<Value>;
    /// Trusted host implementations must honor control and bound their output/resources.
    fn call(&self, context: &ToolContext<'_>, name: &str, arguments: &Value) -> Result<Value>;
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
    // does not end the search, but a traversal or symlink refusal does.
    let mut missing = false;
    for root in roots {
        match relative_file(root, name) {
            Ok(full) if full.exists() => return Ok(full),
            Ok(_) => missing = true,
            Err(error) => return Err(error),
        }
    }
    if missing {
        return Err(std::io::Error::from(std::io::ErrorKind::NotFound).into());
    }
    Err("path is outside every granted workspace".into())
}

pub struct FileTools;

impl ToolGateway for FileTools {
    fn definitions(&self, _: &crate::Assignment) -> Vec<Value> {
        vec![
            json!({"type":"function","function":{"name":"read_file","description":"Read a UTF-8 file from the workspace. Large files are truncated.","parameters":{"type":"object","properties":{"path":{"type":"string"}},"required":["path"],"additionalProperties":false}}}),
            json!({"type":"function","function":{"name":"list_dir","description":"List one directory level of the workspace: names, kind and size. Bounded by the output frame, reporting how many entries were kept.","parameters":{"type":"object","properties":{"path":{"type":"string"}},"required":["path"],"additionalProperties":false}}}),
            json!({"type":"function","function":{"name":"edit_file","description":"Replace one exact occurrence in a workspace file. Fails unless the find text occurs exactly once, so an edit is always unambiguous.","parameters":{"type":"object","properties":{"path":{"type":"string"},"find":{"type":"string"},"replace":{"type":"string"}},"required":["path","find","replace"],"additionalProperties":false}}}),
            json!({"type":"function","function":{"name":"write_file","description":"Write UTF-8 content into the workspace.","parameters":{"type":"object","properties":{"path":{"type":"string"},"text":{"type":"string"}},"required":["path","text"],"additionalProperties":false}}}),
            json!({"type":"function","function":{"name":"glob","description":"Find workspace files by glob pattern (*, ?, **), newest first. Skips .git, the host's private state and symlinks. Bounded: reports how many matches were kept.","parameters":{"type":"object","properties":{"pattern":{"type":"string"},"path":{"type":"string","description":"directory to search from; defaults to the workspace root"}},"required":["pattern"],"additionalProperties":false}}}),
            json!({"type":"function","function":{"name":"grep","description":"Search workspace file contents with a regular expression. Returns file:line: text matches, bounded by the output frame. Skips .git, the host's private state, symlinks and oversized files.","parameters":{"type":"object","properties":{"pattern":{"type":"string"},"path":{"type":"string","description":"directory to search from; defaults to the workspace root"},"include":{"type":"string","description":"optional glob the file path must match, e.g. *.rs"}},"required":["pattern"],"additionalProperties":false}}}),
            json!({"type":"function","function":{"name":"shell","description":"Run one shell command in the workspace (sh -c). Returns exit_code, bounded stdout/stderr and whether the run's budget ended it. A non-zero exit is an answer to read, not a tool failure. The command inherits the run's deadline and cancellation; its whole process group is killed when the run stops.","parameters":{"type":"object","properties":{"command":{"type":"string"},"timeout_ms":{"type":"integer","description":"optional per-command deadline, capped by the run's remaining budget"}},"required":["command"],"additionalProperties":false}}}),
        ]
    }
    fn call(&self, ctx: &ToolContext<'_>, name: &str, args: &Value) -> Result<Value> {
        ctx.control.check()?;
        let value = match name {
            "read_file" => {
                let path = args["path"].as_str().ok_or("missing path")?;
                let full = workspace_path(ctx.project, path)?;
                if full.starts_with(ctx.private) {
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
                let full = workspace_path(ctx.project, path)?;
                if full.starts_with(ctx.private) {
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
                let full = workspace_path(ctx.project, path)?;
                if full.starts_with(ctx.private) {
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
                let full = workspace_path(ctx.project, path)?;
                if full.starts_with(ctx.private) {
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
}

/// Every regular file under one root, skipping the host's private state, `.git`
/// and symlinks. The walk is bounded by the run's control and the entry cap.
fn walk(root: &Path, private: &Path, control: &Control) -> Result<(Vec<PathBuf>, bool)> {
    let mut files = Vec::new();
    let mut truncated = false;
    let mut pending = vec![root.to_path_buf()];
    while let Some(dir) = pending.pop() {
        control.check()?;
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            if control.check().is_err() || files.len() >= 20_000 {
                truncated = true;
                break;
            }
            let path = entry.path();
            if path.starts_with(private) {
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
    let full = workspace_path(ctx.project, path)?;
    if full.starts_with(ctx.private) {
        return Err("the host's private state is not searchable".into());
    }
    if !full.is_dir() {
        return Err("search path is not a directory".into());
    }
    Ok(full)
}

fn glob(ctx: &ToolContext<'_>, args: &Value) -> Result<Value> {
    let pattern = args["pattern"].as_str().ok_or("missing pattern")?;
    let root = search_root(ctx, args)?;
    let (files, mut truncated) = walk(&root, ctx.private, ctx.control)?;
    let mut matched: Vec<(String, i64)> = Vec::new();
    for file in files {
        let Ok(relative) = file.strip_prefix(ctx.project) else {
            continue;
        };
        let name = relative.to_string_lossy().replace('\\', "/");
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
    Ok(
        json!({"pattern":pattern,"matches":kept,"showing":kept.len(),"total":total,"truncated":truncated}),
    )
}

fn grep(ctx: &ToolContext<'_>, args: &Value) -> Result<Value> {
    let pattern = args["pattern"].as_str().ok_or("missing pattern")?;
    let regex =
        regex_lite::Regex::new(pattern).map_err(|error| format!("invalid pattern: {error}"))?;
    let include = args["include"].as_str().filter(|glob| !glob.is_empty());
    let root = search_root(ctx, args)?;
    let (files, mut truncated) = walk(&root, ctx.private, ctx.control)?;
    let mut matches = Vec::new();
    let mut budget = crate::measure::Budget::new(FRAME_LIMIT);
    budget.admit(256);
    'files: for file in files {
        ctx.control.check()?;
        let Ok(relative) = file.strip_prefix(ctx.project) else {
            continue;
        };
        let name = relative.to_string_lossy().replace('\\', "/");
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
    Ok(json!({"pattern":pattern,"matches":matches,"showing":matches.len(),"truncated":truncated}))
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
        .current_dir(ctx.project)
        .stdin(std::process::Stdio::null());
    super::process::shell_output(&mut process, &control, FRAME_LIMIT)
}
