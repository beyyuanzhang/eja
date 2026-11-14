//! Host resource control shared by every boundary; it carries no task tactics.
use crate::Result;
use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

pub static INTERRUPTED: AtomicBool = AtomicBool::new(false);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControlError {
    Cancelled,
    Deadline,
}
impl std::fmt::Display for ControlError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Cancelled => "execution cancelled",
            Self::Deadline => "execution timed out",
        })
    }
}
impl std::error::Error for ControlError {}

#[derive(Clone, Debug)]
pub struct Control {
    deadline: Instant,
    cancellations: Vec<Arc<AtomicBool>>,
}
impl Control {
    pub fn new(timeout: Duration, cancelled: Arc<AtomicBool>) -> Self {
        Self {
            deadline: Instant::now()
                .checked_add(timeout)
                .unwrap_or_else(Instant::now),
            cancellations: vec![cancelled],
        }
    }
    pub fn child(&self, timeout: Duration) -> Self {
        Self {
            deadline: self.deadline.min(
                Instant::now()
                    .checked_add(timeout)
                    .unwrap_or_else(Instant::now),
            ),
            cancellations: self.cancellations.clone(),
        }
    }
    pub fn cancelled_by(mut self, flag: Arc<AtomicBool>) -> Self {
        self.cancellations.push(flag);
        self
    }
    pub fn check(&self) -> std::result::Result<(), ControlError> {
        if INTERRUPTED.load(Ordering::SeqCst)
            || self
                .cancellations
                .iter()
                .any(|flag| flag.load(Ordering::SeqCst))
        {
            return Err(ControlError::Cancelled);
        }
        if Instant::now() >= self.deadline {
            return Err(ControlError::Deadline);
        }
        Ok(())
    }
    pub fn remaining(&self) -> Result<Duration> {
        self.check()?;
        Ok(self.deadline.saturating_duration_since(Instant::now()))
    }
}
