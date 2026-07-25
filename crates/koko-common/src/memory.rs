//! Database-owned tracked-memory accounting.

use crate::{Error, Result};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

/// A point-in-time view of one database's tracked memory.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MemoryUsage {
    /// Bytes currently reserved by live tracked allocations.
    pub current: u64,
    /// Highest observed `current` value since database construction.
    pub peak: u64,
    /// Configured limit in bytes, or `None` when unrestricted.
    pub limit: Option<u64>,
}

/// Stable hook for integrating database reservations with an application-owned
/// memory resource.
///
/// Rust's collection allocator API is not stable on the workspace's minimum
/// toolchain, so Koko's `Vec`/`Box` storage still uses the process allocator.
/// This hook provides the stable part of allocator injection: admission before
/// tracked growth and exact release notifications for every admitted byte.
pub trait MemoryResource: std::fmt::Debug + Send + Sync {
    /// Admit `bytes` before Koko grows a tracked allocation.
    fn try_reserve(&self, bytes: u64) -> Result<()>;

    /// Release bytes previously admitted by [`try_reserve`](Self::try_reserve).
    fn release(&self, bytes: u64);
}

#[derive(Debug)]
struct MemoryState {
    current: AtomicU64,
    peak: AtomicU64,
    limit: Option<u64>,
    resource: Option<Arc<dyn MemoryResource>>,
}

/// Shared accounting owner for one database instance.
///
/// Reservations are admitted atomically before an allocation grows. Dropping a
/// [`MemoryReservation`] releases its bytes, so temporary and result allocations
/// cannot leak accounting on early returns.
#[derive(Debug, Clone)]
pub struct MemoryTracker {
    state: Arc<MemoryState>,
}

impl Default for MemoryTracker {
    fn default() -> Self {
        Self::new(None)
    }
}

impl MemoryTracker {
    /// Construct an independent tracker with an optional byte limit.
    pub fn new(limit: Option<u64>) -> Self {
        Self::with_resource(limit, None)
    }

    /// Construct a tracker backed by an optional application memory resource.
    pub fn with_resource(limit: Option<u64>, resource: Option<Arc<dyn MemoryResource>>) -> Self {
        Self {
            state: Arc::new(MemoryState {
                current: AtomicU64::new(0),
                peak: AtomicU64::new(0),
                limit,
                resource,
            }),
        }
    }

    /// Reserve `bytes` before growing a tracked allocation.
    pub fn try_reserve(&self, bytes: u64) -> Result<MemoryReservation> {
        self.acquire(bytes)?;
        Ok(MemoryReservation {
            tracker: self.clone(),
            bytes,
        })
    }

    /// Current, peak, and configured-limit counters.
    pub fn usage(&self) -> MemoryUsage {
        MemoryUsage {
            current: self.state.current.load(Ordering::Acquire),
            peak: self.state.peak.load(Ordering::Acquire),
            limit: self.state.limit,
        }
    }

    fn acquire(&self, bytes: u64) -> Result<()> {
        if bytes == 0 {
            return Ok(());
        }
        if let Some(resource) = &self.state.resource {
            resource.try_reserve(bytes)?;
        }
        let mut current = self.state.current.load(Ordering::Acquire);
        loop {
            let Some(next) = current.checked_add(bytes) else {
                if let Some(resource) = &self.state.resource {
                    resource.release(bytes);
                }
                return Err(Error::buffer_manager());
            };
            if self.state.limit.is_some_and(|limit| next > limit) {
                if let Some(resource) = &self.state.resource {
                    resource.release(bytes);
                }
                return Err(Error::buffer_manager());
            }
            match self.state.current.compare_exchange_weak(
                current,
                next,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    self.state.peak.fetch_max(next, Ordering::AcqRel);
                    return Ok(());
                }
                Err(observed) => current = observed,
            }
        }
    }

    fn release(&self, bytes: u64) {
        if bytes == 0 {
            return;
        }
        let previous = self.state.current.fetch_sub(bytes, Ordering::AcqRel);
        debug_assert!(previous >= bytes, "memory reservation underflow");
        if let Some(resource) = &self.state.resource {
            resource.release(bytes);
        }
    }
}

/// RAII ownership of bytes admitted by a [`MemoryTracker`].
#[derive(Debug)]
#[must_use = "dropping the reservation immediately releases its tracked bytes"]
pub struct MemoryReservation {
    tracker: MemoryTracker,
    bytes: u64,
}

impl MemoryReservation {
    /// Reserved bytes currently owned by this handle.
    pub const fn bytes(&self) -> u64 {
        self.bytes
    }

    /// Grow or shrink this reservation to `new_bytes`.
    ///
    /// Growth is admitted before the caller grows its allocation. Shrinkage
    /// releases accounting immediately.
    pub fn resize(&mut self, new_bytes: u64) -> Result<()> {
        if new_bytes > self.bytes {
            self.tracker.acquire(new_bytes - self.bytes)?;
        } else {
            self.tracker.release(self.bytes - new_bytes);
        }
        self.bytes = new_bytes;
        Ok(())
    }
}

impl Drop for MemoryReservation {
    fn drop(&mut self) {
        self.tracker.release(self.bytes);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reservations_enforce_limit_and_preserve_peak() {
        let tracker = MemoryTracker::new(Some(10));
        let mut first = tracker.try_reserve(4).unwrap();
        let second = tracker.try_reserve(6).unwrap();
        assert_eq!(
            tracker.usage(),
            MemoryUsage {
                current: 10,
                peak: 10,
                limit: Some(10)
            }
        );
        let error = tracker.try_reserve(1).unwrap_err();
        assert!(matches!(error, Error::BufferManager));
        assert_eq!(
            error.to_string(),
            "Buffer manager exception: Unable to allocate memory! The buffer pool is full and no memory could be freed!"
        );
        drop(second);
        first.resize(2).unwrap();
        assert_eq!(
            tracker.usage(),
            MemoryUsage {
                current: 2,
                peak: 10,
                limit: Some(10)
            }
        );
    }

    #[derive(Debug)]
    struct DenyingResource;

    impl MemoryResource for DenyingResource {
        fn try_reserve(&self, _bytes: u64) -> Result<()> {
            Err(Error::buffer_manager())
        }

        fn release(&self, _bytes: u64) {
            panic!("a rejected reservation must not be released");
        }
    }

    #[test]
    fn injected_resource_denial_does_not_change_accounting() {
        let tracker = MemoryTracker::with_resource(None, Some(Arc::new(DenyingResource)));
        let error = tracker.try_reserve(1).unwrap_err();
        assert!(matches!(error, Error::BufferManager));
        assert_eq!(
            error.to_string(),
            "Buffer manager exception: Unable to allocate memory! The buffer pool is full and no memory could be freed!"
        );
        assert_eq!(tracker.usage(), MemoryUsage::default());
    }
}
