//! Shared tool contract types between capability execution and model formatting.
use eja_core::Outcome;
use serde_json::Value;

pub const MAX_TOOL_IMAGE_BYTES: usize = 2 * 1024 * 1024;

pub struct ModelImage {
    pub media_type: &'static str,
    pub bytes: Vec<u8>,
}

impl ModelImage {
    pub fn receipt(&self) -> String {
        format!(
            "工具返回图像（{}，{} 字节）",
            self.media_type,
            self.bytes.len()
        )
    }

    pub fn media_type(bytes: &[u8]) -> Option<&'static str> {
        if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
            Some("image/jpeg")
        } else if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
            Some("image/png")
        } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
            Some("image/gif")
        } else if bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP") {
            Some("image/webp")
        } else {
            None
        }
    }
}

pub struct ToolExecution {
    pub outcome: Outcome,
    pub images: Vec<ModelImage>,
}

impl From<Outcome> for ToolExecution {
    fn from(outcome: Outcome) -> Self {
        Self {
            outcome,
            images: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ToolSpec {
    pub capability: String,
    pub name: String,
    pub description: String,
    pub schema: Value,
    pub target: ToolTarget,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolTarget {
    Kernel,
    Host,
    Capability(u64),
}
