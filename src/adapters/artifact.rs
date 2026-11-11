use crate::{ArtifactContent, ArtifactReader};
use std::io::Read;

pub struct FileArtifactReader {
    root: std::path::PathBuf,
    max_bytes: usize,
}

impl FileArtifactReader {
    pub fn new(root: impl Into<std::path::PathBuf>) -> Self {
        Self {
            root: root.into(),
            max_bytes: 16 * 1024,
        }
    }

    pub fn with_limit(root: impl Into<std::path::PathBuf>, max_bytes: usize) -> Self {
        Self {
            root: root.into(),
            max_bytes,
        }
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
}
