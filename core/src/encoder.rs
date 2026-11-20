//! An adapter supplies a shared vector space for passages and queries.
use std::error::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputKind {
    Passage,
    Query,
}

/// The identity must change with weights, tokenizer, pooling, or input formatting.
/// Return one finite, nonzero vector per input, in the same order.
pub trait Encoder: Send {
    fn identity(&self) -> &str;
    fn dimensions(&self) -> usize;
    fn encode(
        &mut self,
        kind: InputKind,
        texts: &[&str],
    ) -> Result<Vec<Vec<f32>>, Box<dyn Error + Send + Sync>>;
}
