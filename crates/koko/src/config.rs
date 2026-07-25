use koko_common::{Error, MemoryResource, Result};
use std::sync::Arc;

/// Resource limits for an in-memory [`crate::Database`].
///
/// Limits are disabled by default. Builder methods reject zero so an explicit
/// limit is always meaningful; omit the limit to retain the unrestricted default.
#[derive(Debug, Clone, Default)]
pub struct DatabaseConfig {
    max_workers: Option<usize>,
    memory_limit: Option<u64>,
    memory_resource: Option<Arc<dyn MemoryResource>>,
}

impl DatabaseConfig {
    /// Start with the unrestricted, backwards-compatible defaults.
    pub const fn new() -> Self {
        Self {
            max_workers: None,
            memory_limit: None,
            memory_resource: None,
        }
    }

    /// Cap the workers an individual query may use.
    pub fn with_max_workers(mut self, max_workers: usize) -> Result<Self> {
        if max_workers == 0 {
            return Err(Error::configuration(
                "Database max_workers must be greater than zero.",
            ));
        }
        self.max_workers = Some(max_workers);
        Ok(self)
    }

    /// Cap database-owned tracked memory in bytes.
    pub fn with_memory_limit(mut self, memory_limit: u64) -> Result<Self> {
        if memory_limit == 0 {
            return Err(Error::configuration(
                "Database memory_limit must be greater than zero.",
            ));
        }
        self.memory_limit = Some(memory_limit);
        Ok(self)
    }

    /// Route every tracked reservation through an application-owned memory
    /// resource. The underlying collections continue to use Rust's process
    /// allocator until the standard allocator API stabilizes.
    pub fn with_memory_resource(mut self, resource: Arc<dyn MemoryResource>) -> Self {
        self.memory_resource = Some(resource);
        self
    }

    /// Configured per-query worker cap, or `None` when unrestricted.
    pub const fn max_workers(&self) -> Option<usize> {
        self.max_workers
    }

    /// Configured tracked-memory cap in bytes, or `None` when unrestricted.
    pub const fn memory_limit(&self) -> Option<u64> {
        self.memory_limit
    }

    /// Configured application memory resource, if any.
    pub fn memory_resource(&self) -> Option<Arc<dyn MemoryResource>> {
        self.memory_resource.clone()
    }
}
