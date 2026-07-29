//! Cooperative query cancellation and deadline state.

use crate::{Error, Result};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

/// Cooperative cancellation/deadline state checked at execution boundaries.
#[derive(Clone, Copy, Default)]
pub struct QueryControl<'a> {
    interrupt_epoch: Option<&'a AtomicU64>,
    captured_epoch: u64,
    deadline: Option<Instant>,
}

impl<'a> QueryControl<'a> {
    pub fn new(
        interrupt_epoch: &'a AtomicU64,
        captured_epoch: u64,
        deadline: Option<Instant>,
    ) -> Self {
        Self {
            interrupt_epoch: Some(interrupt_epoch),
            captured_epoch,
            deadline,
        }
    }

    #[inline]
    pub fn check(self) -> Result<()> {
        if self
            .interrupt_epoch
            .is_some_and(|epoch| epoch.load(Ordering::Acquire) != self.captured_epoch)
            || self
                .deadline
                .is_some_and(|deadline| Instant::now() >= deadline)
        {
            return Err(Error::interrupt());
        }
        Ok(())
    }
}
