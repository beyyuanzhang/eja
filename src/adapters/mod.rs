//! Language, retrieval, artifact and process adapters.
//!
//! These are outside the core state machine. The core only sees typed ports.

mod artifact;
mod brain;
mod mcp;
mod model;
mod native;
mod retriever;
mod tools;

pub use artifact::FileArtifactReader;
pub use brain::{
    parse_decision, render_context, render_context_within, OpenAiBrain, SYSTEM_PROMPT,
};
pub use mcp::{HostTools, McpTools};
pub use model::{Control, Model, OpenAiModel};
pub use native::{NativeWorker, INTERRUPTED};
pub use retriever::ModelRetriever;
pub use tools::{FileTools, ToolContext, ToolGateway, ToolOutput};

use crate::{Kind, TaskStatus};

fn label(kind: Kind) -> String {
    format!("{kind:?}").to_lowercase()
}

fn status(status: TaskStatus) -> String {
    format!("{status:?}").to_lowercase()
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
