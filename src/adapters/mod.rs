//! Language, retrieval, artifact and process adapters.
//!
//! These are outside the core state machine. The core only sees typed ports.

mod artifact;
mod brain;
mod retriever;
mod worker;

pub use artifact::FileArtifactReader;
pub use brain::{parse_decision, render_context, OpenAiBrain, SYSTEM_PROMPT};
pub use retriever::ModelRetriever;
pub use worker::{MissingWorker, PiWorker, SessionWorker};

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
