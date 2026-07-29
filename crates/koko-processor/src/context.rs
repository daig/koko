use super::*;

/// Statement-view relationship visibility decisions shared by serial operators
/// and parallel morsels for one execution.
#[derive(Default)]
pub(crate) struct ReadVisibilityCache {
    pub(crate) rel_rows: Mutex<HashMap<TableId, bool>>,
}

impl ReadVisibilityCache {
    pub(crate) fn rel_rows_all_visible(
        &self,
        storage: &InMemStorage,
        read: StorageReadHandle,
        table: TableId,
    ) -> bool {
        let mut rows = self
            .rel_rows
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *rows
            .entry(table)
            .or_insert_with(|| storage.rel_rows_all_visible(read, table))
    }

    pub(crate) fn clear(&self) {
        self.rel_rows
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clear();
    }
}

/// Statement-lifetime accounting for operator-owned temporary allocations.
///
/// Charges are monotone for statement-lifetime structures and parallel morsels, preventing one
/// worker from under-reporting allocations released on another. A fully consumed serial correlated
/// subplan restores its pre-execution checkpoint because consecutive instances are never live
/// simultaneously.
pub struct QueryMemory {
    pub(crate) tracker: MemoryTracker,
    pub(crate) reservation: Mutex<MemoryReservation>,
}

impl QueryMemory {
    pub fn new(tracker: &MemoryTracker) -> Result<Self> {
        Ok(Self {
            tracker: tracker.clone(),
            reservation: Mutex::new(tracker.try_reserve(0)?),
        })
    }

    pub fn charge(&self, bytes: u64) -> Result<()> {
        if bytes == 0 {
            return Ok(());
        }
        let mut reservation = self
            .reservation
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let total = reservation
            .bytes()
            .checked_add(bytes)
            .ok_or_else(Error::buffer_manager)?;
        reservation.resize(total)
    }

    pub(crate) fn temporary_reservation(&self, bytes: u64) -> Result<MemoryReservation> {
        self.tracker.try_reserve(bytes)
    }

    pub fn tracker(&self) -> &MemoryTracker {
        &self.tracker
    }

    pub fn bytes(&self) -> u64 {
        self.reservation
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .bytes()
    }

    pub(crate) fn release_to(&self, bytes: u64) {
        let mut reservation = self
            .reservation
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        debug_assert!(bytes <= reservation.bytes());
        reservation
            .resize(bytes)
            .expect("shrinking a query-memory reservation cannot fail");
    }
}

/// Immutable statement execution context supplied by the connection layer.
/// It carries every non-catalog input that can affect query behavior.
#[derive(Clone, Copy)]
pub struct ExecutionContext<'a> {
    pub table_functions: &'a dyn TableFunctionRuntime,
    pub random: &'a RandomState,
    pub worker_count: usize,
    pub warnings: &'a koko_common::warnings::WarningSink,
    pub storage_read: StorageReadHandle,
    pub storage_write: Option<StorageWriteHandle>,
    pub control: QueryControl<'a>,
    pub memory: &'a QueryMemory,
    pub sources: &'a IcebugQuerySources,
}
/// The read-only context threaded through the pull pipeline.
#[derive(Clone, Copy)]
pub(crate) struct OperatorContext<'a> {
    pub(crate) catalog: &'a Catalog,
    pub(crate) storage: &'a InMemStorage,
    pub(crate) layout: &'a RowLayout,
    pub(crate) table_functions: &'a dyn TableFunctionRuntime,
    pub(crate) random: &'a RandomState,
    pub(crate) worker_count: usize,
    pub(crate) warnings: &'a koko_common::warnings::WarningSink,
    pub(crate) storage_read: StorageReadHandle,
    pub(crate) control: QueryControl<'a>,
    pub(crate) memory: &'a QueryMemory,
    pub(crate) sources: &'a IcebugQuerySources,
    pub(crate) visibility: &'a ReadVisibilityCache,
}

impl<'a> OperatorContext<'a> {
    pub(crate) fn new(
        catalog: &'a Catalog,
        storage: &'a InMemStorage,
        layout: &'a RowLayout,
        execution: &'a ExecutionContext<'a>,
        visibility: &'a ReadVisibilityCache,
    ) -> Self {
        Self {
            catalog,
            storage,
            layout,
            table_functions: execution.table_functions,
            random: execution.random,
            worker_count: execution.worker_count,
            warnings: execution.warnings,
            storage_read: execution.storage_read,
            control: execution.control,
            memory: execution.memory,
            sources: execution.sources,
            visibility,
        }
    }
}
impl OperatorContext<'_> {
    #[inline]
    pub(crate) fn read(self) -> StorageReadHandle {
        self.storage_read
    }
}
