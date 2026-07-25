//! Connection-owned warning collection (`IGNORE_ERRORS=true` skip reports,
//! read by `CALL show_warnings()`).
//!
//! A connection owns one [`WarningRegistry`]. Each statement creates a
//! [`WarningSink`] carrying that statement's query id and warning limit.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// One skipped-row warning.
#[derive(Debug, Clone)]
pub struct Warning {
    pub query_id: u64,
    pub message: String,
    pub file_path: String,
    pub line_number: u64,
    pub skipped_line_or_record: String,
}

#[derive(Debug, Default)]
struct RegistryInner {
    warnings: Vec<Warning>,
    /// Per-query TOTAL warning counts — unlimited. The COPY summary reports
    /// these even when the stored warning list is capped.
    totals: HashMap<u64, u64>,
}

/// Warning history for one connection.
#[derive(Debug, Clone, Default)]
pub struct WarningRegistry {
    inner: Arc<Mutex<RegistryInner>>,
}

impl WarningRegistry {
    /// Create the sink used by one statement.
    pub fn sink(&self, query_id: u64, limit: u64) -> WarningSink {
        WarningSink {
            registry: self.clone(),
            query_id,
            limit,
        }
    }

    /// Snapshot every warning retained by this connection.
    pub fn all(&self) -> Vec<Warning> {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .warnings
            .clone()
    }

    /// Clear retained warnings and totals, preserving the connection's query-id stream.
    pub fn clear(&self) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.warnings.clear();
        inner.totals.clear();
    }
}

/// Statement-local warning writer. Clones share the owning connection's registry.
#[derive(Debug, Clone)]
pub struct WarningSink {
    registry: WarningRegistry,
    query_id: u64,
    limit: u64,
}

impl WarningSink {
    pub fn query_id(&self) -> u64 {
        self.query_id
    }

    /// Record one skipped row under this statement's query id.
    pub fn push(&self, message: String, file_path: String, line_number: u64, record: String) {
        let mut inner = self
            .registry
            .inner
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        *inner.totals.entry(self.query_id).or_insert(0) += 1;
        if (inner.warnings.len() as u64) >= self.limit {
            return;
        }
        inner.warnings.push(Warning {
            query_id: self.query_id,
            message,
            file_path,
            line_number,
            skipped_line_or_record: record,
        });
    }

    /// Total warnings raised by this statement, including warnings beyond the
    /// retained-warning limit.
    pub fn count(&self) -> usize {
        self.registry
            .inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .totals
            .get(&self.query_id)
            .copied()
            .unwrap_or(0) as usize
    }

    /// Warning records retained for this statement only.
    pub fn retained(&self) -> Vec<Warning> {
        self.registry
            .inner
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .warnings
            .iter()
            .filter(|warning| warning.query_id == self.query_id)
            .cloned()
            .collect()
    }

    pub fn all(&self) -> Vec<Warning> {
        self.registry.all()
    }

    pub fn clear(&self) {
        self.registry.clear();
    }
}
