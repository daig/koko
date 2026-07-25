//! Process-level interrupt bridge using only signal-safe atomics in the handler.

use std::sync::LazyLock;
use std::sync::atomic::{AtomicU64, Ordering};

static INTERRUPT_EPOCH: AtomicU64 = AtomicU64::new(0);
static INSTALL_RESULT: LazyLock<Result<(), String>> = LazyLock::new(|| {
    ctrlc::set_handler(|| {
        INTERRUPT_EPOCH.fetch_add(1, Ordering::Release);
    })
    .map_err(|error| error.to_string())
});

pub fn install() -> Result<(), String> {
    (*INSTALL_RESULT).clone()
}

#[derive(Debug, Clone, Copy)]
pub struct InterruptCursor {
    observed: u64,
}

impl InterruptCursor {
    pub fn current() -> Self {
        Self {
            observed: INTERRUPT_EPOCH.load(Ordering::Acquire),
        }
    }

    pub fn take(&mut self) -> bool {
        let current = INTERRUPT_EPOCH.load(Ordering::Acquire);
        if current == self.observed {
            return false;
        }
        self.observed = current;
        true
    }
}

#[cfg(test)]
pub fn trigger_for_test() {
    INTERRUPT_EPOCH.fetch_add(1, Ordering::Release);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursor_coalesces_interrupts_and_isolates_late_epochs() {
        let mut cursor = InterruptCursor::current();
        assert!(!cursor.take());
        trigger_for_test();
        trigger_for_test();
        assert!(cursor.take());
        assert!(!cursor.take());
        let mut late = InterruptCursor::current();
        assert!(!late.take());
        trigger_for_test();
        assert!(late.take());
    }
}
