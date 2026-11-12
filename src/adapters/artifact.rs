use super::ArtifactReader;
use crate::{ArtifactContent, Selector};
use serde_json::json;
use std::io::Read;

pub struct FileArtifactReader {
    root: std::path::PathBuf,
    max_bytes: usize,
}

impl FileArtifactReader {
    pub fn new(root: impl Into<std::path::PathBuf>) -> Self {
        Self {
            root: root.into(),
            max_bytes: crate::aja::FRAME_LIMIT,
        }
    }

    /// A smaller page than the frame, when the caller wants one.
    pub fn with_limit(root: impl Into<std::path::PathBuf>, max_bytes: usize) -> Self {
        Self {
            root: root.into(),
            max_bytes,
        }
    }

}

impl FileArtifactReader {
    /// Full text for a selective look; bounded so one call cannot read without limit.
    fn text(&self, path: &str) -> crate::Result<String> {
        let full = super::tools::relative_file(&self.root, path)?;
        let metadata = std::fs::metadata(&full)?;
        if !metadata.is_file() {
            return Err("not a regular file".into());
        }
        Ok(std::fs::read(&full).map(|bytes| String::from_utf8_lossy(&bytes).to_string())?)
    }
}

impl ArtifactReader for FileArtifactReader {
    fn read(&self, path: &str) -> ArtifactContent {
        let bytes = match (|| -> crate::Result<Vec<u8>> {
            let full = super::tools::relative_file(&self.root, path)?;
            let file = std::fs::File::open(full)?;
            if !file.metadata()?.is_file() {
                return Err("not a regular file".into());
            }
            let mut bytes = Vec::new();
            file.take(self.max_bytes.saturating_add(1) as u64)
                .read_to_end(&mut bytes)?;
            Ok(bytes)
        })() {
            Ok(bytes) => bytes,
            Err(error) => return ArtifactContent::failed(path, error.to_string()),
        };
        let truncated = bytes.len() > self.max_bytes;
        let slice = if truncated {
            &bytes[..self.max_bytes]
        } else {
            &bytes[..]
        };
        ArtifactContent {
            path: path.to_string(),
            text: String::from_utf8_lossy(slice).to_string(),
            truncated,
            error: None,
        }
    }

    /// Node files answer whole, range and match: EJA reads the passage it needs.
    fn slice(&self, path: &str, selector: &Selector) -> crate::Result<serde_json::Value> {
        match selector {
            Selector::Whole => Ok(serde_json::to_value(self.read(path))?),
            Selector::Range { from, lines } => {
                let text = self.text(path)?;
                let all: Vec<&str> = text.lines().collect();
                let total = all.len();
                let start = (*from).min(total);
                let end = start.saturating_add(*lines).min(total);
                let window: Vec<serde_json::Value> = all[start..end]
                    .iter()
                    .enumerate()
                    .map(|(offset, line)| json!({"line": start + offset, "text": line}))
                    .collect();
                Ok(json!({
                    "path": path,
                    "total_lines": total,
                    "from": start,
                    "lines": window,
                }))
            }
            Selector::Match {
                text,
                before,
                after,
            } => {
                let content = self.text(path)?;
                let all: Vec<&str> = content.lines().collect();
                let mut matches = Vec::new();
                for (index, line) in all.iter().enumerate() {
                    if !line.contains(text.as_str()) {
                        continue;
                    }
                    let start = index.saturating_sub(*before);
                    let end = (index + after + 1).min(all.len());
                    let context: Vec<serde_json::Value> = all[start..end]
                        .iter()
                        .enumerate()
                        .map(|(offset, line)| json!({"line": start + offset, "text": line}))
                        .collect();
                    matches.push(json!({"line": index, "context": context}));
                }
                Ok(json!({
                    "path": path,
                    "total_lines": all.len(),
                    "matches": matches,
                }))
            }
            Selector::Search { .. } => Err("a node look does not search".into()),
        }
    }
}
