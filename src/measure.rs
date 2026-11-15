//! Counting a serialized value without building it.
//!
//! Every declared budget in this kernel is a byte budget, so the code has to ask
//! "how many bytes would this serialize to?" far more often than it asks for the
//! bytes themselves. Materializing a `Vec` or a `String` just to read `.len()`
//! allocates for a number; a sink that only counts turns that into a walk.

use crate::Result;
use serde::Serialize;
use std::io;

/// A writer that keeps no bytes, only their count.
#[derive(Debug, Default)]
pub struct Len(pub usize);

impl io::Write for Len {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0 += bytes.len();
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// The serialized size of any value, without allocating the serialized form.
pub fn bytes(value: &impl Serialize) -> Result<usize> {
    let mut len = Len::default();
    serde_json::to_writer(&mut len, value)?;
    Ok(len.0)
}

/// The serialized size of a JSON value. A `Value` serializes totally and `Len`
/// cannot fail, so there is no error to report; the count is never guessed.
pub fn value(value: &serde_json::Value) -> usize {
    let mut len = Len::default();
    let _ = serde_json::to_writer(&mut len, value);
    len.0
}

/// The same idea for formatted text: count what a writer would receive.
#[derive(Debug, Default)]
pub struct Count(pub usize);

impl std::fmt::Write for Count {
    fn write_str(&mut self, text: &str) -> std::fmt::Result {
        self.0 += text.len();
        Ok(())
    }
}
