//! Shared storage ownership and database-wide commit clock.

use crate::engine::InMemStorage;
use koko_common::Ts;
use std::sync::{
    Arc, RwLock, RwLockReadGuard, RwLockWriteGuard,
    atomic::{AtomicU64, Ordering},
};

/// Database-wide monotonically increasing committed-version clock.
#[derive(Debug, Clone, Default)]
pub struct CommitClock {
    current: Arc<AtomicU64>,
}

impl CommitClock {
    pub fn current(&self) -> Ts {
        self.current.load(Ordering::Acquire)
    }

    pub(crate) fn next(&self) -> Ts {
        self.current.fetch_add(1, Ordering::AcqRel) + 1
    }
}

/// Shared ownership for the concrete in-memory engine.
#[derive(Default)]
pub struct SharedStorage {
    inner: RwLock<InMemStorage>,
}

impl SharedStorage {
    pub fn new(storage: InMemStorage) -> Self {
        Self {
            inner: RwLock::new(storage),
        }
    }

    #[inline]
    pub fn read(&self) -> RwLockReadGuard<'_, InMemStorage> {
        self.inner
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    #[inline]
    pub fn write(&self) -> RwLockWriteGuard<'_, InMemStorage> {
        self.inner
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl From<InMemStorage> for SharedStorage {
    fn from(storage: InMemStorage) -> Self {
        Self::new(storage)
    }
}
