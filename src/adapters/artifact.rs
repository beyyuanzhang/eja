use crate::{ArtifactContent, ArtifactReader};

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

    pub fn from_env() -> Self {
        let root = std::env::var("EJA_ARTIFACT_ROOT").unwrap_or_else(|_| ".".to_string());
        Self::new(root)
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
        let relative = std::path::Path::new(path);
        if relative.is_absolute()
            || relative
                .components()
                .any(|component| matches!(component, std::path::Component::ParentDir))
        {
            return ArtifactContent::failed(path, "rejected path");
        }
        let full = self.root.join(relative);
        let bytes = match std::fs::read(&full) {
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
