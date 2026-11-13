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

pub struct ToolOutput {
    pub value: Value,
}

pub trait ToolGateway: Send + Sync {
    fn definitions(&self, assignment: &crate::Assignment) -> Vec<Value>;
    /// Trusted host implementations must honor control and bound their output/resources.
    fn call(&self, context: &ToolContext<'_>, name: &str, arguments: &Value) -> Result<ToolOutput>;
}

/// No absolute paths, parent traversal, or symlinks in a node's file capabilities.
pub(crate) fn relative_file(root: &Path, name: &str) -> Result<PathBuf> {
    if name.is_empty() || name.len() > 4096 {
        return Err("invalid artifact path".into());
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
    fn call(&self, ctx: &ToolContext<'_>, name: &str, args: &Value) -> Result<ToolOutput> {
        ctx.control.check()?;
        let path = args["path"].as_str().ok_or("missing path")?;
        let value = match name {
            "read_file" => {
                let full = relative_file(ctx.project, path)?;
                if full.starts_with(ctx.private) {
                    return Err("the host's private state is not readable".into());
                }
                let file = fs::File::open(full)?;
                if !file.metadata()?.is_file() {
                    return Err("not a regular file".into());
                }
                let mut bytes = Vec::new();
                file.take(FRAME_LIMIT as u64 + 1).read_to_end(&mut bytes)?;
                let truncated = bytes.len() > FRAME_LIMIT;
                bytes.truncate(FRAME_LIMIT);
                json!({"text":String::from_utf8_lossy(&bytes),"truncated":truncated})
            }
            "list_dir" => {
                let full = if path == "." {
                    ctx.project.to_path_buf()
                } else {
                    relative_file(ctx.project, path)?
                };
                if full.starts_with(ctx.private) {
                    return Err("the host's private state is not readable".into());
                }
                let mut entries: Vec<String> = Vec::new();
                let mut shown = 0usize;
                let mut bytes = 0usize;
                let listing: Vec<_> = fs::read_dir(&full)?.collect::<std::io::Result<Vec<_>>>()?;
                let total = listing.len();
                for entry in listing {
                    let metadata = entry.metadata()?;
                    let name = entry.file_name().to_string_lossy().to_string();
                    let line = if metadata.is_dir() {
                        format!("{name}/")
                    } else {
                        format!("{name} ({} bytes)", metadata.len())
                    };
                    bytes += line.len() + 1;
                    if bytes > FRAME_LIMIT {
                        break;
                    }
                    entries.push(line);
                    shown += 1;
                }
                json!({"path":path,"entries":entries,"showing":shown,"total":total,"truncated":shown < total})
            }
            "edit_file" => {
                let find = args["find"].as_str().ok_or("missing find")?;
                let replace = args["replace"].as_str().ok_or("missing replace")?;
                let full = relative_file(ctx.project, path)?;
                if full.starts_with(ctx.private) {
                    return Err("the host's private state is not writable".into());
                }
                let mut text = fs::read_to_string(&full)?;
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
                let full = relative_file(ctx.project, path)?;
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
        Ok(ToolOutput { value })
    }
}
