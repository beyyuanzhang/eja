//! Host tools. Connectors implement this same boundary; nodes never load plugins.
use super::Control;
use crate::{aja::FRAME_LIMIT, Result};
use serde_json::{json, Value};
use std::{
    fs,
    io::Read,
    path::{Component, Path, PathBuf},
};

pub struct ToolContext<'a> {
    pub node_id: u64,
    /// The one workspace: the project. There is no second place to write.
    pub project: &'a Path,
    /// The host's own state, which no node may read or write.
    pub private: &'a Path,
    pub control: &'a Control,
}

pub trait ToolGateway: Send + Sync {
    fn definitions(&self, assignment: &crate::Assignment) -> Vec<Value>;
    /// Trusted host implementations must honor control and bound their output/resources.
    fn call(&self, context: &ToolContext<'_>, name: &str, arguments: &Value) -> Result<Value>;
}

/// No absolute paths, parent traversal, or symlinks in a node's file capabilities.
pub(crate) fn relative_file(root: &Path, name: &str) -> Result<PathBuf> {
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
pub(crate) fn workspace_path(root: &Path, name: &str) -> Result<PathBuf> {
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

pub struct FileTools;

impl ToolGateway for FileTools {
    fn definitions(&self, _: &crate::Assignment) -> Vec<Value> {
        vec![
            json!({"type":"function","function":{"name":"read_file","description":"Read a UTF-8 file from the workspace. Large files are truncated.","parameters":{"type":"object","properties":{"path":{"type":"string"}},"required":["path"],"additionalProperties":false}}}),
            json!({"type":"function","function":{"name":"list_dir","description":"List one directory level of the workspace: names, kind and size. Bounded by the output frame, reporting how many entries were kept.","parameters":{"type":"object","properties":{"path":{"type":"string"}},"required":["path"],"additionalProperties":false}}}),
            json!({"type":"function","function":{"name":"edit_file","description":"Replace one exact occurrence in a workspace file. Fails unless the find text occurs exactly once, so an edit is always unambiguous.","parameters":{"type":"object","properties":{"path":{"type":"string"},"find":{"type":"string"},"replace":{"type":"string"}},"required":["path","find","replace"],"additionalProperties":false}}}),
            json!({"type":"function","function":{"name":"write_file","description":"Write UTF-8 content into the workspace.","parameters":{"type":"object","properties":{"path":{"type":"string"},"text":{"type":"string"}},"required":["path","text"],"additionalProperties":false}}}),
        ]
    }
    fn call(&self, ctx: &ToolContext<'_>, name: &str, args: &Value) -> Result<Value> {
        ctx.control.check()?;
        let path = args["path"].as_str().ok_or("missing path")?;
        let value = match name {
            "read_file" => {
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
                    let size = serde_json::to_vec(&line)?.len() + 1;
                    if bytes.saturating_add(size) < FRAME_LIMIT {
                        bytes += size;
                        entries.push(line);
                    }
                }
                let shown = entries.len();
                json!({"path":path,"entries":entries,"showing":shown,"total":total,"truncated":shown<total})
            }
            "edit_file" => {
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
            _ => return Err("tool not allowed".into()),
        };
        Ok(value)
    }
}
