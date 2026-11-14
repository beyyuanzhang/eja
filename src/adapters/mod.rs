//! Language, retrieval, artifact and process adapters.
//!
//! These are outside the core state machine. The core only sees typed ports.

mod artifact;
mod brain;
mod mcp;
mod model;
mod native;
mod ports;
mod process;
mod record;
mod tools;

pub use artifact::FileArtifactReader;
pub use brain::{
    parse_decision, render_context, render_context_within, OpenAiBrain, SYSTEM_PROMPT,
};
pub use mcp::{HostTools, McpTools};
pub use model::{Control, Model, OpenAiModel};
pub use native::{NativeWorker, INTERRUPTED};
pub use ports::{ArtifactReader, NoopArtifactReader, NoopObserver, Observer, Ports};
pub use tools::{FileTools, ToolContext, ToolGateway, ToolOutput};

use crate::Kind;

fn label(kind: Kind) -> String {
    serde_json::to_value(kind)
        .expect("unit event kinds serialize")
        .as_str()
        .expect("event kind is a string")
        .to_owned()
}

fn llm_env() -> Option<(String, String, String)> {
    let base = std::env::var("LNA_BASE_URL")
        .or_else(|_| std::env::var("JIYUAN_BASE_URL"))
        .or_else(|_| std::env::var("OPENAI_BASE_URL"))
        .ok()?;
    let key = std::env::var("LNA_API_KEY")
        .or_else(|_| std::env::var("JIYUAN_API_KEY"))
        .or_else(|_| std::env::var("OPENAI_API_KEY"))
        .ok()?;
    let model = std::env::var("LNA_MODEL")
        .or_else(|_| std::env::var("JIYUAN_MODEL"))
        .or_else(|_| std::env::var("OPENAI_MODEL"))
        .unwrap_or_else(|_| "gpt-4.1-mini".into());
    Some((base, key, model))
}

/// Cut a line at a byte budget without splitting a character.
pub(crate) fn cut_chars(line: &str, limit: usize) -> &str {
    let mut end = limit.min(line.len());
    while !line.is_char_boundary(end) {
        end -= 1;
    }
    &line[..end]
}

/// Advance through a UTF-8 record even when the requested window is smaller
/// than one character. Returned offsets always refer to the original text.
pub(crate) fn text_page(text: &str, from: usize, bytes: usize, cap: usize) -> serde_json::Value {
    let mut offset = from.min(text.len());
    while !text.is_char_boundary(offset) {
        offset += 1;
    }
    let mut end = offset.saturating_add(bytes.clamp(1, cap)).min(text.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    if end == offset && offset < text.len() {
        end += text[offset..].chars().next().unwrap().len_utf8();
    }
    serde_json::json!({"offset":offset,"next_offset":end,"total_bytes":text.len(),"truncated":end<text.len(),"text":&text[offset..end]})
}

/// One cooperative wait for adapter futures. A deadline and cancellation cover
/// request enqueueing as well as waiting for the eventual response.
pub(crate) async fn controlled<T>(
    control: &crate::Control,
    future: impl std::future::Future<Output = crate::Result<T>>,
) -> crate::Result<T> {
    control.check()?;
    tokio::pin!(future);
    loop {
        tokio::select! {
            result = &mut future => {control.check()?;return result;},
            _ = tokio::time::sleep(std::time::Duration::from_millis(10)) => control.check()?,
        }
    }
}
