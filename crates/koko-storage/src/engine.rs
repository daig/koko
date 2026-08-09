//! `koko-storage` — the storage layer.
//!
//! Owns the typed, chunked in-memory storage engine used by the product.
//! `:memory:` is the only backend, so callers use [`InMemStorage`] directly:
//! there is no trait-object dispatch or dormant durable-backend lifecycle seam.
//! Explicit read/write handles carry every visibility decision.
//!
//! Property columns use appendable fixed-capacity typed chunks. Variable-width,
//! nested, and graph values retain owned managed payloads. Native on-disk
//! representation, page buffering, compression, checkpoint, and recovery are deferred.

use crate::handle::{StorageReadHandle, StorageWriteHandle};
use crate::index::PkKey;
use crate::node::row_bytes as node_row_bytes;
use crate::relation::{
    BatchNeighbor, EdgeDir, adjacency as adj_at, push_adjacency as adj_push,
    remove_adjacency as adj_remove, row_bytes as rel_row_bytes,
};
use crate::shared::CommitClock;
use crate::undo::{UndoEntry, UndoOp};
use crate::version::{PriorVersion, pop_prior, upgrade, upgrade_chain};
#[cfg(test)]
use koko_common::UNCOMMITTED;
use koko_common::{
    ColumnData, DataChunk, Error, ExtendDir, InternalId, LogicalType, MemoryReservation,
    MemoryTracker, ReadView, RelMultiplicity, Result, TS_INF, TableId, TableStats, Ts,
    VECTOR_CAPACITY, Value, ValueVector,
};
use std::collections::{HashMap, HashSet};

struct ColumnChunk {
    values: ValueVector,
    _memory: MemoryReservation,
}

pub(crate) struct PropertyColumn {
    logical_type: LogicalType,
    chunks: Vec<ColumnChunk>,
    len: usize,
    payload_memory: Option<MemoryReservation>,
    payload_bytes: u64,
}

#[derive(Clone, Copy)]
struct AppendPreparation {
    added_chunk: bool,
    previous_payload: u64,
}

impl PropertyColumn {
    fn new(logical_type: LogicalType) -> Self {
        Self {
            logical_type,
            chunks: Vec::new(),
            len: 0,
            payload_memory: None,
            payload_bytes: 0,
        }
    }

    fn ensure_append_capacity(&mut self, memory: &MemoryTracker) -> Result<bool> {
        if self.len % VECTOR_CAPACITY != 0 {
            return Ok(false);
        }
        let reservation = memory.try_reserve(ColumnData::allocation_bytes(
            self.logical_type.physical_type(),
        ))?;
        self.chunks.push(ColumnChunk {
            values: ValueVector::new(self.logical_type.clone()),
            _memory: reservation,
        });
        Ok(true)
    }

    fn resize_payload(&mut self, new_bytes: u64, memory: &MemoryTracker) -> Result<()> {
        match &mut self.payload_memory {
            Some(reservation) => reservation.resize(new_bytes)?,
            None if new_bytes > 0 => {
                self.payload_memory = Some(memory.try_reserve(new_bytes)?);
            }
            None => {}
        }
        self.payload_bytes = new_bytes;
        Ok(())
    }

    fn prepare_append(
        &mut self,
        value: &Value,
        memory: &MemoryTracker,
    ) -> Result<AppendPreparation> {
        let added_chunk = self.ensure_append_capacity(memory)?;
        let previous_payload = self.payload_bytes;
        let new_payload = previous_payload
            .checked_add(value_heap_bytes(value))
            .ok_or_else(Error::buffer_manager)?;
        if let Err(error) = self.resize_payload(new_payload, memory) {
            if added_chunk {
                self.chunks.pop();
            }
            return Err(error);
        }
        Ok(AppendPreparation {
            added_chunk,
            previous_payload,
        })
    }

    fn rollback_append_preparation(
        &mut self,
        preparation: AppendPreparation,
        memory: &MemoryTracker,
    ) {
        self.resize_payload(preparation.previous_payload, memory)
            .expect("shrinking a column payload reservation cannot fail");
        if preparation.added_chunk {
            debug_assert_eq!(self.len % VECTOR_CAPACITY, 0);
            self.chunks.pop();
        }
    }

    fn push(&mut self, value: Value) {
        let chunk = self.len / VECTOR_CAPACITY;
        let pos = self.len % VECTOR_CAPACITY;
        self.chunks[chunk].values.set_value_owned(pos, value);
        self.len += 1;
    }

    fn append(&mut self, value: &Value, memory: &MemoryTracker) -> Result<()> {
        self.prepare_append(value, memory)?;
        self.push(value.clone());
        Ok(())
    }

    fn get(&self, offset: usize) -> Value {
        self.chunks[offset / VECTOR_CAPACITY]
            .values
            .get_value(offset % VECTOR_CAPACITY)
    }

    fn value_heap_bytes(&self, offset: usize) -> u64 {
        let vector = &self.chunks[offset / VECTOR_CAPACITY].values;
        let pos = offset % VECTOR_CAPACITY;
        if vector.nulls.is_null(pos) {
            return 0;
        }
        match &vector.data {
            ColumnData::Str(values) => usize_bytes(values[pos].capacity()),
            ColumnData::Generic(values) => value_heap_bytes(&values[pos]),
            _ => 0,
        }
    }

    fn replace_accounted(
        &mut self,
        offset: usize,
        value: Value,
        memory: &MemoryTracker,
    ) -> Result<Value> {
        let old_payload = self.value_heap_bytes(offset);
        let new_payload = self
            .payload_bytes
            .checked_sub(old_payload)
            .and_then(|bytes| bytes.checked_add(value_heap_bytes(&value)))
            .ok_or_else(Error::buffer_manager)?;
        self.resize_payload(new_payload, memory)?;
        let vector = &mut self.chunks[offset / VECTOR_CAPACITY].values;
        let pos = offset % VECTOR_CAPACITY;
        let before = vector.take_value(pos);
        vector.set_value_owned(pos, value);
        Ok(before)
    }

    fn pop(&mut self, memory: &MemoryTracker) -> Option<Value> {
        let offset = self.len.checked_sub(1)?;
        let old_payload = self.value_heap_bytes(offset);
        self.len = offset;
        let value = self.chunks[offset / VECTOR_CAPACITY]
            .values
            .take_value(offset % VECTOR_CAPACITY);
        self.resize_payload(self.payload_bytes - old_payload, memory)
            .expect("shrinking a column payload reservation cannot fail");
        if self.len % VECTOR_CAPACITY == 0 {
            self.chunks.pop();
        }
        Some(value)
    }

    fn values(&self) -> Vec<Value> {
        (0..self.len).map(|offset| self.get(offset)).collect()
    }
}

fn prepare_column_append(
    columns: &mut [PropertyColumn],
    values: &[Value],
    memory: &MemoryTracker,
) -> Result<()> {
    let mut prepared = Vec::with_capacity(columns.len());
    for (column, value) in columns.iter_mut().zip(values) {
        match column.prepare_append(value, memory) {
            Ok(preparation) => prepared.push(preparation),
            Err(error) => {
                for (column, preparation) in columns.iter_mut().zip(prepared).rev() {
                    column.rollback_append_preparation(preparation, memory);
                }
                return Err(error);
            }
        }
    }
    Ok(())
}
fn usize_bytes(value: usize) -> u64 {
    u64::try_from(value).unwrap_or(u64::MAX)
}

fn value_heap_bytes(value: &Value) -> u64 {
    match value {
        Value::String(value) => usize_bytes(value.capacity()),
        Value::Blob(value) => usize_bytes(value.capacity()),
        Value::List(values) => usize_bytes(values.capacity())
            .saturating_mul(usize_bytes(std::mem::size_of::<Value>()))
            .saturating_add(values.iter().map(value_heap_bytes).sum()),
        Value::Struct(fields) => usize_bytes(fields.capacity())
            .saturating_mul(usize_bytes(std::mem::size_of::<(String, Value)>()))
            .saturating_add(
                fields
                    .iter()
                    .map(|(name, value)| {
                        usize_bytes(name.capacity()).saturating_add(value_heap_bytes(value))
                    })
                    .sum(),
            ),
        Value::Map(entries) => usize_bytes(entries.capacity())
            .saturating_mul(usize_bytes(std::mem::size_of::<(Value, Value)>()))
            .saturating_add(
                entries
                    .iter()
                    .map(|(key, value)| {
                        value_heap_bytes(key).saturating_add(value_heap_bytes(value))
                    })
                    .sum(),
            ),
        Value::Node(node) => usize_bytes(std::mem::size_of_val(node.as_ref()))
            .saturating_add(usize_bytes(node.label.capacity()))
            .saturating_add(
                node.props
                    .iter()
                    .map(|(name, value)| {
                        usize_bytes(std::mem::size_of::<(String, Value)>())
                            .saturating_add(usize_bytes(name.capacity()))
                            .saturating_add(value_heap_bytes(value))
                    })
                    .sum(),
            ),
        Value::Rel(rel) => usize_bytes(std::mem::size_of_val(rel.as_ref()))
            .saturating_add(usize_bytes(rel.label.capacity()))
            .saturating_add(
                rel.props
                    .iter()
                    .map(|(name, value)| {
                        usize_bytes(std::mem::size_of::<(String, Value)>())
                            .saturating_add(usize_bytes(name.capacity()))
                            .saturating_add(value_heap_bytes(value))
                    })
                    .sum(),
            ),
        Value::RecursiveRel(path) => path
            .nodes
            .iter()
            .map(|node| {
                usize_bytes(std::mem::size_of_val(node))
                    .saturating_add(usize_bytes(node.label.capacity()))
            })
            .sum::<u64>()
            .saturating_add(
                path.rels
                    .iter()
                    .map(|rel| {
                        usize_bytes(std::mem::size_of_val(rel))
                            .saturating_add(usize_bytes(rel.label.capacity()))
                    })
                    .sum(),
            ),
        Value::Union {
            variants, value, ..
        } => usize_bytes(variants.capacity())
            .saturating_mul(usize_bytes(std::mem::size_of::<(
                String,
                koko_common::LogicalType,
            )>()))
            .saturating_add(
                variants
                    .iter()
                    .map(|(name, _)| usize_bytes(name.capacity()))
                    .sum(),
            )
            .saturating_add(usize_bytes(std::mem::size_of::<Value>()))
            .saturating_add(value_heap_bytes(value)),
        _ => 0,
    }
}

pub(crate) struct NodeStore {
    /// True for the read-only columnar `icebug-disk` scan backend.
    immutable: bool,
    num_columns: usize,
    /// Appendable, fixed-capacity typed property columns.
    columns: Vec<PropertyColumn>,
    /// High-water offset count (offsets are dense and never reused, even after a
    /// delete — preserving `tableID:offset` identity).
    count: u64,
    /// Per-offset MVCC version stamps (parallel to `columns`): `begin_ts` = insert
    /// version, `end_ts` = delete version (`TS_INF` = live). Generalises the old
    /// tombstone — a row is hidden iff `end_ts != TS_INF`. Real commit timestamps
    /// and the `UNCOMMITTED` writer tag arrive with the version-aware read paths;
    /// for now every live row is `begin_ts = 0`, `end_ts = TS_INF`.
    begin_ts: Vec<Ts>,
    end_ts: Vec<Ts>,
    row_memory: Vec<MemoryReservation>,
    /// Prior property versions per `(offset, column)`, oldest→newest: a `SET` pushes
    /// the replaced value here so a reader that began before the write commits still
    /// reads it. `replaced_at` is `UNCOMMITTED` until commit upgrades it.
    updates: HashMap<(u64, usize), Vec<PriorVersion>>,
    pk_column: usize,
    pk_index: HashMap<PkKey, u64>,
    /// Per-table & per-column statistics (row count, HLL distinct, min/max, null
    /// count), folded in at commit. See [`TableStats`].
    stats: TableStats,
    /// Committed stats snapshots keyed by commit timestamp.
    stats_history: Vec<(Ts, TableStats)>,
}

pub(crate) struct RelStore {
    /// True for the read-only CSR `icebug-disk` scan backend.
    immutable: bool,
    num_columns: usize,
    /// FROM / TO node tables — the tables the dense adjacencies are keyed by (offsets
    /// are unique only within a table; one store per pair makes that a single table).
    from_table: TableId,
    to_table: TableId,
    columns: Vec<PropertyColumn>,
    src: Vec<InternalId>,
    dst: Vec<InternalId>,
    count: u64,
    /// Per-offset MVCC version stamps (see `NodeStore`). A deleted/uncommitted rel
    /// is *retained* in the adjacencies and filtered by `begin_ts`/`end_ts` at read.
    begin_ts: Vec<Ts>,
    end_ts: Vec<Ts>,
    row_memory: Vec<MemoryReservation>,
    /// Prior property versions per `(offset, column)` — see `NodeStore::updates`.
    updates: HashMap<(u64, usize), Vec<PriorVersion>>,
    /// src node offset → rel offsets (outgoing adjacency). Indexed densely by the FROM
    /// node's offset (P3 step 10b L3b — dropping the per-extend SipHash of the old
    /// `HashMap<InternalId, _>`; sound now that each store holds a single FROM/TO pair).
    fwd_adj: Vec<Vec<u64>>,
    /// dst node offset → rel offsets (incoming adjacency), indexed by the TO node's offset.
    bwd_adj: Vec<Vec<u64>>,
    /// The table name and multiplicity, for enforcing the constraint on insert
    /// (and naming it in the error). A deleted rel is removed from the adjacencies,
    /// so a node's edge count is just its adjacency length.
    name: String,
    multiplicity: RelMultiplicity,
    /// Per-table & per-column statistics, folded in at commit. See [`TableStats`].
    stats: TableStats,
    /// Committed stats snapshots keyed by commit timestamp.
    stats_history: Vec<(Ts, TableStats)>,
}

/// The in-memory storage engine.
///
/// Transaction isolation is **version-record MVCC** (P3), not cloning: per-row
/// `begin_ts`/`end_ts` plus a per-cell prior-value chain let a transaction read at a
/// timestamp and write new versions at O(changes). Both auto-commit writes and
/// explicit read-write transactions mutate this shared store in place — tagged with
/// their writer id, then published by [`commit_to`] or reverted by [`rollback_to`]
/// nothing in the hot path clones the store.
///
/// [`commit_to`]: InMemStorage::commit_to
/// [`rollback_to`]: InMemStorage::rollback_to
#[derive(Default)]
pub struct InMemStorage {
    nodes: HashMap<TableId, NodeStore>,
    rels: HashMap<TableId, RelStore>,
    /// Undo log: a stack of inverse mutations, enabling savepoint rollback. Each
    /// entry carries its writer id so multi-write transactions can commit/rollback
    /// only their own write-set while preserving other in-flight writers.
    undo: Vec<UndoEntry>,
    /// Monotone undo sequence; marks use this stable value rather than a vector
    /// index so removing one writer's entries cannot shift another writer's mark.
    next_undo_seq: usize,
    /// Database-wide committed version clock. Every graph storage shares the same
    /// source, while row versions remain isolated inside this concrete store.
    commit_clock: CommitClock,
    memory: MemoryTracker,
}

impl InMemStorage {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_memory_tracker(memory: MemoryTracker) -> Self {
        Self::with_memory_tracker_and_clock(memory, CommitClock::default())
    }

    pub fn with_memory_tracker_and_clock(memory: MemoryTracker, commit_clock: CommitClock) -> Self {
        Self {
            commit_clock,
            memory,
            ..Self::default()
        }
    }

    pub fn memory_tracker(&self) -> &MemoryTracker {
        &self.memory
    }

    fn push_undo(&mut self, writer: Ts, op: UndoOp) {
        let seq = self.next_undo_seq;
        self.next_undo_seq += 1;
        self.undo.push(UndoEntry { writer, seq, op });
    }
}

fn property_at(
    column: &PropertyColumn,
    updates: &HashMap<(u64, usize), Vec<PriorVersion>>,
    view: ReadView,
    offset: u64,
    column_id: usize,
) -> Value {
    if let Some(chain) = updates.get(&(offset, column_id)) {
        for prior in chain {
            if !view.ts_visible(prior.replaced_at) {
                return prior.value.clone();
            }
        }
    }
    column.get(offset as usize)
}

impl NodeStore {
    fn calculate_stats(&self, view: ReadView) -> TableStats {
        let mut stats = TableStats::with_types(self.columns.iter().map(|c| &c.logical_type));
        for offset in 0..self.count {
            let physical = offset as usize;
            if view.row_visible(self.begin_ts[physical], self.end_ts[physical]) {
                stats.record_owned_row(self.columns.iter().enumerate().map(
                    |(column_id, column)| {
                        property_at(column, &self.updates, view, offset, column_id)
                    },
                ));
            }
        }
        stats
    }
}

impl RelStore {
    fn calculate_stats(&self, view: ReadView) -> TableStats {
        let mut stats = TableStats::with_types(self.columns.iter().map(|c| &c.logical_type));
        for offset in 0..self.count {
            let physical = offset as usize;
            if view.row_visible(self.begin_ts[physical], self.end_ts[physical]) {
                stats.record_owned_row(self.columns.iter().enumerate().map(
                    |(column_id, column)| {
                        property_at(column, &self.updates, view, offset, column_id)
                    },
                ));
            }
        }
        stats
    }
}

const UPDATE_CONFLICT_MSG: &str = "Write-write conflict of updating the same row.";
const DELETE_CONFLICT_MSG: &str =
    "Write-write conflict: deleting a row that is already deleted by another transaction.";

fn update_conflict() -> Error {
    Error::runtime(UPDATE_CONFLICT_MSG.to_string())
}

fn delete_conflict() -> Error {
    Error::runtime(DELETE_CONFLICT_MSG.to_string())
}

fn row_has_conflicting_update(
    updates: &HashMap<(u64, usize), Vec<PriorVersion>>,
    offset: u64,
    view: ReadView,
) -> bool {
    updates.iter().any(|(&(row, _), chain)| {
        row == offset
            && chain
                .iter()
                .any(|pv| view.conflicts_with_write(pv.replaced_at))
    })
}

fn ensure_node_writable(store: &NodeStore, offset: u64, view: ReadView) -> Result<()> {
    let o = offset as usize;
    if store
        .begin_ts
        .get(o)
        .is_some_and(|&ts| view.conflicts_with_write(ts))
        || store
            .end_ts
            .get(o)
            .is_some_and(|&ts| view.conflicts_with_write(ts))
        || row_has_conflicting_update(&store.updates, offset, view)
    {
        return Err(update_conflict());
    }
    Ok(())
}

/// Whether a node delete should install a tombstone. Conflicts are checked separately from
/// adjacency so DELETE reports the writer conflict before the ordinary "connected edges" guard.
fn node_delete_needs_tombstone(store: &NodeStore, offset: u64, view: ReadView) -> Result<bool> {
    let end_ts = store.end_ts[offset as usize];
    if end_ts != TS_INF {
        if view.owns(end_ts) || !view.conflicts_with_write(end_ts) {
            return Ok(false);
        }
        return Err(delete_conflict());
    }
    ensure_node_writable(store, offset, view)?;
    Ok(true)
}

fn ensure_rel_writable(store: &RelStore, offset: u64, view: ReadView) -> Result<()> {
    let o = offset as usize;
    if store
        .begin_ts
        .get(o)
        .is_some_and(|&ts| view.conflicts_with_write(ts))
        || store
            .end_ts
            .get(o)
            .is_some_and(|&ts| view.conflicts_with_write(ts))
        || row_has_conflicting_update(&store.updates, offset, view)
    {
        return Err(update_conflict());
    }
    Ok(())
}

impl InMemStorage {
    pub fn create_node_table(
        &mut self,
        write: StorageWriteHandle,
        table_id: TableId,
        column_types: &[LogicalType],
        pk_column: usize,
    ) {
        let writer = write.writer_id();
        self.nodes.insert(
            table_id,
            NodeStore {
                immutable: false,
                num_columns: column_types.len(),
                columns: column_types
                    .iter()
                    .cloned()
                    .map(PropertyColumn::new)
                    .collect(),
                count: 0,
                begin_ts: Vec::new(),
                end_ts: Vec::new(),
                row_memory: Vec::new(),
                updates: HashMap::new(),
                pk_column,
                pk_index: HashMap::new(),
                stats: TableStats::with_types(column_types),
                stats_history: Vec::new(),
            },
        );
        self.push_undo(writer, UndoOp::UncreateTable { table: table_id });
    }

    pub fn mark_icebug_node_table(&mut self, table_id: TableId) {
        if let Some(store) = self.nodes.get_mut(&table_id) {
            store.immutable = true;
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn create_rel_table(
        &mut self,
        write: StorageWriteHandle,
        table_id: TableId,
        from: TableId,
        to: TableId,
        column_types: &[LogicalType],
        name: &str,
        multiplicity: RelMultiplicity,
    ) {
        let writer = write.writer_id();
        self.rels.insert(
            table_id,
            RelStore {
                immutable: false,
                num_columns: column_types.len(),
                from_table: from,
                to_table: to,
                columns: column_types
                    .iter()
                    .cloned()
                    .map(PropertyColumn::new)
                    .collect(),
                src: Vec::new(),
                dst: Vec::new(),
                count: 0,
                begin_ts: Vec::new(),
                end_ts: Vec::new(),
                row_memory: Vec::new(),
                updates: HashMap::new(),
                fwd_adj: Vec::new(),
                bwd_adj: Vec::new(),
                name: name.to_string(),
                multiplicity,
                stats: TableStats::with_types(column_types),
                stats_history: Vec::new(),
            },
        );
        self.push_undo(writer, UndoOp::UncreateTable { table: table_id });
    }

    pub fn mark_icebug_rel_table(&mut self, table_id: TableId) {
        if let Some(store) = self.rels.get_mut(&table_id) {
            store.immutable = true;
        }
    }

    pub fn drop_table(&mut self, write: StorageWriteHandle, table_id: TableId) {
        let writer = write.writer_id();
        // A table id lives in exactly one store. Move the dropped store into the undo
        // log so a transaction rollback restores its rows (there is no storage clone).
        if let Some(store) = self.nodes.remove(&table_id) {
            self.push_undo(
                writer,
                UndoOp::RestoreNodeTable {
                    table: table_id,
                    store,
                },
            );
        } else if let Some(store) = self.rels.remove(&table_id) {
            self.push_undo(
                writer,
                UndoOp::RestoreRelTable {
                    table: table_id,
                    store,
                },
            );
        }
    }

    pub fn add_column(
        &mut self,
        write: StorageWriteHandle,
        table_id: TableId,
        logical_type: LogicalType,
        default: Value,
    ) -> Result<()> {
        let writer = write.writer_id();
        let count = self
            .nodes
            .get(&table_id)
            .map(|store| store.count)
            .or_else(|| self.rels.get(&table_id).map(|store| store.count));
        let Some(count) = count else {
            return Ok(());
        };
        let mut column = PropertyColumn::new(logical_type.clone());
        for _ in 0..count {
            column.append(&default, &self.memory)?;
        }
        if let Some(store) = self.nodes.get_mut(&table_id) {
            store.columns.push(column);
            store.num_columns += 1;
            store.stats.push_column(&logical_type, &default, count);
        } else {
            let store = self
                .rels
                .get_mut(&table_id)
                .expect("table disappeared while adding a column");
            store.columns.push(column);
            store.num_columns += 1;
            store.stats.push_column(&logical_type, &default, count);
        }
        self.push_undo(writer, UndoOp::UnaddColumn { table: table_id });
        Ok(())
    }

    pub fn drop_column(&mut self, write: StorageWriteHandle, table_id: TableId, idx: usize) {
        let writer = write.writer_id();
        if let Some(s) = self.nodes.get_mut(&table_id) {
            let column = s.columns.remove(idx);
            s.num_columns -= 1;
            s.stats.remove_column(idx);
            let pk_column = s.pk_column;
            if s.pk_column > idx {
                s.pk_column -= 1;
            }
            self.push_undo(
                writer,
                UndoOp::RestoreNodeColumn {
                    table: table_id,
                    idx,
                    column,
                    pk_column,
                },
            );
        } else if let Some(s) = self.rels.get_mut(&table_id) {
            let column = s.columns.remove(idx);
            s.num_columns -= 1;
            s.stats.remove_column(idx);
            self.push_undo(
                writer,
                UndoOp::RestoreRelColumn {
                    table: table_id,
                    idx,
                    column,
                },
            );
        }
    }

    fn insert_node(
        &mut self,
        write: StorageWriteHandle,
        table_id: TableId,
        props: Vec<Value>,
    ) -> Result<InternalId> {
        let view = write.read().view();
        let writer = write.writer_id();
        let store = self
            .nodes
            .get_mut(&table_id)
            .expect("insert_node into unknown table");
        if store.immutable {
            return Err(Error::runtime("Cannot insert into icebug-disk node table."));
        }
        debug_assert_eq!(props.len(), store.num_columns);

        // Enforce primary-key uniqueness.
        let pk_val = &props[store.pk_column];
        if pk_val.is_null() {
            return Err(Error::runtime(
                "Found NULL, which violates the non-null constraint of the primary key column."
                    .to_string(),
            ));
        }
        let key = PkKey::from_value(pk_val).ok_or_else(|| {
            Error::runtime("Unsupported primary key type in this phase.".to_string())
        })?;
        // A NaN key is unindexed: duplicates are allowed and `= NaN` probes miss
        // (C++ hash-index semantics — "NaN should not be searchable").
        let pk_is_nan = matches!(pk_val, Value::Float(x) if x.is_nan())
            || matches!(pk_val, Value::Double(x) if x.is_nan());
        // The key is "taken" only if its current owner is still *live* from this
        // writer's view. A prior owner that the writer has already deleted (its
        // `end_ts` is this writer's own uncommitted delete) frees the key for
        // reclaim — so `delete (a {pk}) … create (b {pk})` in one transaction works
        // while a genuine duplicate is still rejected. (The deleted entry is kept in
        // the index — not removed at delete — so a concurrent reader at an older
        // view can still resolve the soon-to-be-replaced node by PK.)
        if !pk_is_nan {
            if let Some(&prev) = store.pk_index.get(&key) {
                let prev = prev as usize;
                if view.row_visible(store.begin_ts[prev], store.end_ts[prev])
                    || view.conflicts_with_write(store.begin_ts[prev])
                    || view.conflicts_with_write(store.end_ts[prev])
                {
                    return Err(Error::runtime(format!(
                        "Found duplicated primary key value {}, which violates the uniqueness \
                         constraint of the primary key column.",
                        pk_val.to_result_string()
                    )));
                }
            }
        }

        let reservation = self.memory.try_reserve(node_row_bytes(&key))?;
        prepare_column_append(&mut store.columns, &props, &self.memory)?;
        let offset = store.count;
        for (column, value) in store.columns.iter_mut().zip(props) {
            column.push(value);
        }
        let prev_owner = if pk_is_nan {
            None
        } else {
            store.pk_index.insert(key.clone(), offset)
        };
        store.begin_ts.push(writer);
        store.end_ts.push(TS_INF);
        store.row_memory.push(reservation);
        store.count += 1;
        self.push_undo(
            writer,
            UndoOp::UninsertNode {
                table: table_id,
                offset,
                pk: key,
                prev_owner,
            },
        );
        Ok(InternalId::new(table_id, offset))
    }

    fn insert_rel(
        &mut self,
        write: StorageWriteHandle,
        table_id: TableId,
        src: InternalId,
        dst: InternalId,
        props: Vec<Value>,
    ) -> Result<InternalId> {
        let view = write.read().view();
        let writer = write.writer_id();
        let store = self
            .rels
            .get_mut(&table_id)
            .expect("insert_rel into unknown table");
        if store.immutable {
            return Err(Error::runtime(
                "Cannot insert into icebug-disk relationship table.",
            ));
        }
        debug_assert_eq!(props.len(), store.num_columns);

        // Enforce the rel multiplicity constraint before inserting. A deleted rel is
        // pruned from the adjacencies, so a non-empty adjacency is a live edge. The
        // reported node is the one that would exceed its limit, with Kùzu's exact
        // direction wording ("fwd" = a src's outgoing, "bwd" = a dst's incoming).
        if store.multiplicity.src_single
            && adj_at(&store.fwd_adj, src, store.from_table)
                .iter()
                .any(|&o| view.row_visible(store.begin_ts[o as usize], store.end_ts[o as usize]))
        {
            return Err(Error::runtime(format!(
                "Node(nodeOffset: {}) has more than one neighbour in table {} in the fwd \
                 direction, which violates the rel multiplicity constraint.",
                src.offset.0, store.name
            )));
        }
        if store.multiplicity.dst_single
            && adj_at(&store.bwd_adj, dst, store.to_table)
                .iter()
                .any(|&o| view.row_visible(store.begin_ts[o as usize], store.end_ts[o as usize]))
        {
            return Err(Error::runtime(format!(
                "Node(nodeOffset: {}) has more than one neighbour in table {} in the bwd \
                 direction, which violates the rel multiplicity constraint.",
                dst.offset.0, store.name
            )));
        }

        let reservation = self.memory.try_reserve(rel_row_bytes())?;
        prepare_column_append(&mut store.columns, &props, &self.memory)?;
        let offset = store.count;
        for (column, value) in store.columns.iter_mut().zip(props) {
            column.push(value);
        }
        store.src.push(src);
        store.dst.push(dst);
        store.begin_ts.push(writer);
        store.end_ts.push(TS_INF);
        store.row_memory.push(reservation);
        adj_push(&mut store.fwd_adj, src, offset);
        adj_push(&mut store.bwd_adj, dst, offset);
        store.count += 1;
        self.push_undo(
            writer,
            UndoOp::UninsertRel {
                table: table_id,
                offset,
            },
        );
        Ok(InternalId::new(table_id, offset))
    }

    pub fn insert_node_batch(
        &mut self,
        write: StorageWriteHandle,
        table_id: TableId,
        rows: &DataChunk,
        continue_on_error: bool,
    ) -> Vec<Result<InternalId>> {
        let mut results = Vec::with_capacity(rows.size());
        for position in rows.sel.iter() {
            let props = rows
                .columns
                .iter()
                .map(|column| column.get_value(position))
                .collect();
            let result = self.insert_node(write, table_id, props);
            let failed = result.is_err();
            results.push(result);
            if failed && !continue_on_error {
                break;
            }
        }
        results
    }

    pub fn insert_rel_batch(
        &mut self,
        write: StorageWriteHandle,
        table_id: TableId,
        rows: &DataChunk,
        continue_on_error: bool,
    ) -> Vec<Result<InternalId>> {
        let mut results = Vec::with_capacity(rows.size());
        for position in rows.sel.iter() {
            let src = match rows.columns[0].get_value(position) {
                Value::InternalId(id) => id,
                value => {
                    results.push(Err(Error::runtime(format!(
                        "Relationship batch source is not an internal id: {value:?}."
                    ))));
                    if !continue_on_error {
                        break;
                    }
                    continue;
                }
            };
            let dst = match rows.columns[1].get_value(position) {
                Value::InternalId(id) => id,
                value => {
                    results.push(Err(Error::runtime(format!(
                        "Relationship batch destination is not an internal id: {value:?}."
                    ))));
                    if !continue_on_error {
                        break;
                    }
                    continue;
                }
            };
            let props = rows.columns[2..]
                .iter()
                .map(|column| column.get_value(position))
                .collect();
            let result = self.insert_rel(write, table_id, src, dst, props);
            let failed = result.is_err();
            results.push(result);
            if failed && !continue_on_error {
                break;
            }
        }
        results
    }

    pub fn node_count(&self, table_id: TableId) -> u64 {
        self.nodes.get(&table_id).map_or(0, |s| s.count)
    }

    pub fn rel_count(&self, table_id: TableId) -> u64 {
        self.rels.get(&table_id).map_or(0, |s| s.count)
    }

    pub fn rel_multiplicity(&self, table_id: TableId) -> RelMultiplicity {
        self.rels
            .get(&table_id)
            .map(|s| s.multiplicity)
            .unwrap_or_default()
    }

    pub fn scan_node_batch(
        &self,
        read: StorageReadHandle,
        table_id: TableId,
        projected_columns: &[usize],
        start_offset: u64,
        offset_count: usize,
    ) -> DataChunk {
        let Some(store) = self.nodes.get(&table_id) else {
            return DataChunk::new(&[]);
        };
        let mut types = Vec::with_capacity(projected_columns.len() + 1);
        types.push(LogicalType::InternalId);
        types.extend(
            projected_columns
                .iter()
                .map(|&column| store.columns[column].logical_type.clone()),
        );
        let mut output = DataChunk::new(&types);
        let end = start_offset
            .saturating_add(offset_count.min(VECTOR_CAPACITY) as u64)
            .min(store.count);
        let mut row = 0;
        for offset in start_offset..end {
            let physical = offset as usize;
            if !read
                .view()
                .row_visible(store.begin_ts[physical], store.end_ts[physical])
            {
                continue;
            }
            output.columns[0].set_internal_id(row, InternalId::new(table_id, offset));
            for (output_column, &property_column) in
                output.columns[1..].iter_mut().zip(projected_columns)
            {
                output_column.set_value_owned(
                    row,
                    self.node_property(read, table_id, offset, property_column),
                );
            }
            row += 1;
        }
        output.set_flat(row);
        output
    }

    pub fn scan_rel_batch(
        &self,
        read: StorageReadHandle,
        table_id: TableId,
        projected_columns: &[usize],
        start_offset: u64,
        offset_count: usize,
    ) -> DataChunk {
        let Some(store) = self.rels.get(&table_id) else {
            return DataChunk::new(&[]);
        };
        let mut types = Vec::with_capacity(projected_columns.len() + 3);
        types.extend([
            LogicalType::InternalId,
            LogicalType::InternalId,
            LogicalType::InternalId,
        ]);
        types.extend(
            projected_columns
                .iter()
                .map(|&column| store.columns[column].logical_type.clone()),
        );
        let mut output = DataChunk::new(&types);
        let end = start_offset
            .saturating_add(offset_count.min(VECTOR_CAPACITY) as u64)
            .min(store.count);
        let mut row = 0;
        for offset in start_offset..end {
            let physical = offset as usize;
            if !read
                .view()
                .row_visible(store.begin_ts[physical], store.end_ts[physical])
            {
                continue;
            }
            output.columns[0].set_internal_id(row, InternalId::new(table_id, offset));
            output.columns[1].set_internal_id(row, store.src[physical]);
            output.columns[2].set_internal_id(row, store.dst[physical]);
            for (output_column, &property_column) in
                output.columns[3..].iter_mut().zip(projected_columns)
            {
                output_column.set_value_owned(
                    row,
                    self.rel_property(read, table_id, offset, property_column),
                );
            }
            row += 1;
        }
        output.set_flat(row);
        output
    }

    /// Read one projected node value without allocating fixed-capacity vectors.
    ///
    /// This is the cold graph-value assembly adapter. Scan and extend pipelines use the
    /// typed batch operations below instead.
    pub fn node_projected_values(
        &self,
        read: StorageReadHandle,
        table_id: TableId,
        offset: u64,
        projected_columns: &[usize],
    ) -> Vec<Value> {
        projected_columns
            .iter()
            .map(|&column| self.node_property(read, table_id, offset, column))
            .collect()
    }

    /// Relationship counterpart of [`Self::node_projected_values`].
    pub fn rel_projected_values(
        &self,
        read: StorageReadHandle,
        table_id: TableId,
        offset: u64,
        projected_columns: &[usize],
    ) -> Vec<Value> {
        projected_columns
            .iter()
            .map(|&column| self.rel_property(read, table_id, offset, column))
            .collect()
    }

    /// Gather projected node properties for the supplied offsets into typed result batches.
    ///
    /// Output position `i` always corresponds to `offsets[i]`; unlike a scan, this operation
    /// does not filter rows because callers already obtained the ids from a snapshot-aware
    /// lookup or adjacency operation.
    pub fn node_properties_batch(
        &self,
        read: StorageReadHandle,
        table_id: TableId,
        offsets: &[u64],
        projected_columns: &[usize],
    ) -> Vec<DataChunk> {
        let Some(store) = self.nodes.get(&table_id) else {
            return Vec::new();
        };
        let types: Vec<LogicalType> = projected_columns
            .iter()
            .map(|&column| store.columns[column].logical_type.clone())
            .collect();
        offsets
            .chunks(VECTOR_CAPACITY)
            .map(|batch| {
                let mut output = DataChunk::new(&types);
                for (row, &offset) in batch.iter().enumerate() {
                    for (output_column, &property_column) in
                        output.columns.iter_mut().zip(projected_columns)
                    {
                        output_column.set_value_owned(
                            row,
                            self.node_property(read, table_id, offset, property_column),
                        );
                    }
                }
                output.set_flat(batch.len());
                output
            })
            .collect()
    }

    /// Relationship counterpart of [`Self::node_properties_batch`].
    pub fn rel_properties_batch(
        &self,
        read: StorageReadHandle,
        table_id: TableId,
        offsets: &[u64],
        projected_columns: &[usize],
    ) -> Vec<DataChunk> {
        let Some(store) = self.rels.get(&table_id) else {
            return Vec::new();
        };
        let types: Vec<LogicalType> = projected_columns
            .iter()
            .map(|&column| store.columns[column].logical_type.clone())
            .collect();
        offsets
            .chunks(VECTOR_CAPACITY)
            .map(|batch| {
                let mut output = DataChunk::new(&types);
                for (row, &offset) in batch.iter().enumerate() {
                    for (output_column, &property_column) in
                        output.columns.iter_mut().zip(projected_columns)
                    {
                        output_column.set_value_owned(
                            row,
                            self.rel_property(read, table_id, offset, property_column),
                        );
                    }
                }
                output.set_flat(batch.len());
                output
            })
            .collect()
    }

    /// Whether every physical row in `rel_table` is visible to this statement.
    /// Query operators cache this once per branch and can then omit the MVCC test
    /// from high-fan-out adjacency loops without weakening snapshot semantics.
    pub fn rel_rows_all_visible(&self, read: StorageReadHandle, rel_table: TableId) -> bool {
        let Some(store) = self.rels.get(&rel_table) else {
            return true;
        };
        let view = read.view();
        store
            .begin_ts
            .iter()
            .zip(&store.end_ts)
            .all(|(&begin, &end)| view.row_visible(begin, end))
    }

    /// Whether every physical row in `node_table` is visible to this statement.
    pub fn node_rows_all_visible(&self, read: StorageReadHandle, node_table: TableId) -> bool {
        let Some(store) = self.nodes.get(&node_table) else {
            return true;
        };
        let view = read.view();
        store
            .begin_ts
            .iter()
            .zip(&store.end_ts)
            .all(|(&begin, &end)| view.row_visible(begin, end))
    }

    /// Visit visible physical node offsets without constructing a [`DataChunk`].
    pub fn visit_node_offsets(
        &self,
        read: StorageReadHandle,
        node_table: TableId,
        visit: impl FnMut(u64) -> Result<()>,
    ) -> Result<()> {
        self.visit_node_offsets_impl(read, node_table, visit, false)
    }

    /// Visit physical node offsets after [`Self::node_rows_all_visible`] returned
    /// true for the same table and statement view.
    pub fn visit_node_offsets_all_visible(
        &self,
        read: StorageReadHandle,
        node_table: TableId,
        visit: impl FnMut(u64) -> Result<()>,
    ) -> Result<()> {
        self.visit_node_offsets_impl(read, node_table, visit, true)
    }

    fn visit_node_offsets_impl(
        &self,
        read: StorageReadHandle,
        node_table: TableId,
        mut visit: impl FnMut(u64) -> Result<()>,
        all_visible: bool,
    ) -> Result<()> {
        let Some(store) = self.nodes.get(&node_table) else {
            return Ok(());
        };
        let view = read.view();
        for offset in 0..store.count {
            let physical = offset as usize;
            if all_visible || view.row_visible(store.begin_ts[physical], store.end_ts[physical]) {
                visit(offset)?;
            }
        }
        Ok(())
    }

    /// Visit visible relationship rows as primitive
    /// `(relationship_offset, source_offset, destination_offset)` triples.
    pub fn visit_rel_endpoints(
        &self,
        read: StorageReadHandle,
        rel_table: TableId,
        visit: impl FnMut(u64, u64, u64) -> Result<()>,
    ) -> Result<()> {
        self.visit_rel_endpoints_impl(read, rel_table, visit, false)
    }

    /// Visit relationship endpoints after [`Self::rel_rows_all_visible`] returned
    /// true for the same table and statement view.
    pub fn visit_rel_endpoints_all_visible(
        &self,
        read: StorageReadHandle,
        rel_table: TableId,
        visit: impl FnMut(u64, u64, u64) -> Result<()>,
    ) -> Result<()> {
        self.visit_rel_endpoints_impl(read, rel_table, visit, true)
    }

    fn visit_rel_endpoints_impl(
        &self,
        read: StorageReadHandle,
        rel_table: TableId,
        mut visit: impl FnMut(u64, u64, u64) -> Result<()>,
        all_visible: bool,
    ) -> Result<()> {
        let Some(store) = self.rels.get(&rel_table) else {
            return Ok(());
        };
        let view = read.view();
        for offset in 0..store.count {
            let physical = offset as usize;
            if !all_visible && !view.row_visible(store.begin_ts[physical], store.end_ts[physical]) {
                continue;
            }
            visit(
                offset,
                store.src[physical].offset.0,
                store.dst[physical].offset.0,
            )?;
        }
        Ok(())
    }

    /// Visit visible adjacency entries as primitive
    /// `(relationship_offset, neighbor_offset)` pairs.
    pub fn visit_neighbors(
        &self,
        read: StorageReadHandle,
        rel_table: TableId,
        node: InternalId,
        direction: EdgeDir,
        visit: impl FnMut(u64, u64) -> Result<()>,
    ) -> Result<()> {
        self.visit_neighbors_impl(read, rel_table, node, direction, visit, false)
    }

    /// Visit adjacency entries after [`Self::rel_rows_all_visible`] returned true
    /// for the same table and statement view.
    pub fn visit_neighbors_all_visible(
        &self,
        read: StorageReadHandle,
        rel_table: TableId,
        node: InternalId,
        direction: EdgeDir,
        visit: impl FnMut(u64, u64) -> Result<()>,
    ) -> Result<()> {
        self.visit_neighbors_impl(read, rel_table, node, direction, visit, true)
    }

    /// Return the next visible adjacency entry and advance the physical
    /// adjacency `cursor`. A new scan starts with cursor zero.
    pub fn next_neighbor(
        &self,
        read: StorageReadHandle,
        rel_table: TableId,
        node: InternalId,
        direction: EdgeDir,
        cursor: &mut usize,
    ) -> Option<(u64, u64)> {
        self.next_neighbor_impl(read, rel_table, node, direction, cursor, false)
    }

    /// Return the next adjacency entry after [`Self::rel_rows_all_visible`]
    /// returned true for this table and statement view.
    pub fn next_neighbor_all_visible(
        &self,
        read: StorageReadHandle,
        rel_table: TableId,
        node: InternalId,
        direction: EdgeDir,
        cursor: &mut usize,
    ) -> Option<(u64, u64)> {
        self.next_neighbor_impl(read, rel_table, node, direction, cursor, true)
    }

    fn next_neighbor_impl(
        &self,
        read: StorageReadHandle,
        rel_table: TableId,
        node: InternalId,
        direction: EdgeDir,
        cursor: &mut usize,
        all_visible: bool,
    ) -> Option<(u64, u64)> {
        let store = self.rels.get(&rel_table)?;
        let (adjacency, keyed_table, forward) = match direction {
            EdgeDir::Fwd => (&store.fwd_adj, store.from_table, true),
            EdgeDir::Bwd => (&store.bwd_adj, store.to_table, false),
        };
        let offsets = adj_at(adjacency, node, keyed_table);
        let view = read.view();
        while let Some(&offset) = offsets.get(*cursor) {
            *cursor += 1;
            let physical = offset as usize;
            if !all_visible && !view.row_visible(store.begin_ts[physical], store.end_ts[physical]) {
                continue;
            }
            let neighbor = if forward {
                store.dst[physical]
            } else {
                store.src[physical]
            };
            return Some((offset, neighbor.offset.0));
        }
        None
    }

    fn visit_neighbors_impl(
        &self,
        read: StorageReadHandle,
        rel_table: TableId,
        node: InternalId,
        direction: EdgeDir,
        mut visit: impl FnMut(u64, u64) -> Result<()>,
        all_visible: bool,
    ) -> Result<()> {
        let Some(store) = self.rels.get(&rel_table) else {
            return Ok(());
        };
        let (adjacency, keyed_table, forward) = match direction {
            EdgeDir::Fwd => (&store.fwd_adj, store.from_table, true),
            EdgeDir::Bwd => (&store.bwd_adj, store.to_table, false),
        };
        let view = read.view();
        for &offset in adj_at(adjacency, node, keyed_table) {
            let physical = offset as usize;
            if !all_visible && !view.row_visible(store.begin_ts[physical], store.end_ts[physical]) {
                continue;
            }
            let neighbor = if forward {
                store.dst[physical]
            } else {
                store.src[physical]
            };
            visit(offset, neighbor.offset.0)?;
        }
        Ok(())
    }

    /// Count visible neighbors without materializing [`BatchNeighbor`] records.
    /// Valid only after [`Self::rel_rows_all_visible`] returned true for this table
    /// and statement view. `allow_neighbor_table` applies a polymorphic endpoint gate.
    pub fn extend_count_all_visible(
        &self,
        rel_table: TableId,
        node: InternalId,
        dir: ExtendDir,
        mut allow_neighbor_table: impl FnMut(TableId) -> bool,
    ) -> u64 {
        let Some(store) = self.rels.get(&rel_table) else {
            return 0;
        };
        match dir {
            ExtendDir::Forward => {
                if allow_neighbor_table(store.to_table) {
                    adj_at(&store.fwd_adj, node, store.from_table).len() as u64
                } else {
                    0
                }
            }
            ExtendDir::Backward => {
                if allow_neighbor_table(store.from_table) {
                    adj_at(&store.bwd_adj, node, store.to_table).len() as u64
                } else {
                    0
                }
            }
            ExtendDir::Both => {
                let forward = if allow_neighbor_table(store.to_table) {
                    adj_at(&store.fwd_adj, node, store.from_table).len() as u64
                } else {
                    0
                };
                let backward = if allow_neighbor_table(store.from_table) {
                    adj_at(&store.bwd_adj, node, store.to_table).len() as u64
                } else {
                    0
                };
                forward.saturating_add(backward)
            }
        }
    }

    pub fn extend_batch_into(
        &self,
        read: StorageReadHandle,
        rel_table: TableId,
        nodes: &[InternalId],
        dir: ExtendDir,
        out: &mut Vec<BatchNeighbor>,
    ) {
        self.extend_batch_impl(read, rel_table, nodes, dir, out, false);
    }

    /// Adjacency expansion after [`Self::rel_rows_all_visible`] returned true for
    /// the same read handle and relationship table.
    pub fn extend_batch_all_visible_into(
        &self,
        read: StorageReadHandle,
        rel_table: TableId,
        nodes: &[InternalId],
        dir: ExtendDir,
        out: &mut Vec<BatchNeighbor>,
    ) {
        self.extend_batch_impl(read, rel_table, nodes, dir, out, true);
    }

    fn extend_batch_impl(
        &self,
        read: StorageReadHandle,
        rel_table: TableId,
        nodes: &[InternalId],
        dir: ExtendDir,
        out: &mut Vec<BatchNeighbor>,
        all_visible: bool,
    ) {
        let Some(store) = self.rels.get(&rel_table) else {
            return;
        };
        let view = read.view();
        let mut extend_direction = |input_pos: usize,
                                    node: InternalId,
                                    adj: &[Vec<u64>],
                                    forward: bool,
                                    keyed: TableId| {
            if all_visible {
                for &offset in adj_at(adj, node, keyed) {
                    let physical = offset as usize;
                    out.push(BatchNeighbor {
                        input_pos,
                        nbr: if forward {
                            store.dst[physical]
                        } else {
                            store.src[physical]
                        },
                        rel: InternalId::new(rel_table, offset),
                    });
                }
            } else {
                for &offset in adj_at(adj, node, keyed) {
                    let physical = offset as usize;
                    if !view.row_visible(store.begin_ts[physical], store.end_ts[physical]) {
                        continue;
                    }
                    out.push(BatchNeighbor {
                        input_pos,
                        nbr: if forward {
                            store.dst[physical]
                        } else {
                            store.src[physical]
                        },
                        rel: InternalId::new(rel_table, offset),
                    });
                }
            }
        };
        for (input_pos, &node) in nodes.iter().enumerate() {
            match dir {
                ExtendDir::Forward => {
                    extend_direction(input_pos, node, &store.fwd_adj, true, store.from_table)
                }
                ExtendDir::Backward => {
                    extend_direction(input_pos, node, &store.bwd_adj, false, store.to_table)
                }
                ExtendDir::Both => {
                    extend_direction(input_pos, node, &store.fwd_adj, true, store.from_table);
                    extend_direction(input_pos, node, &store.bwd_adj, false, store.to_table);
                }
            }
        }
    }

    fn node_property(
        &self,
        read: StorageReadHandle,
        table_id: TableId,
        offset: u64,
        column_id: usize,
    ) -> Value {
        let Some(s) = self.nodes.get(&table_id) else {
            return Value::Null;
        };
        // A reader that has not yet observed a prior version's replacement reads that
        // prior value; otherwise the current in-place value.
        if let Some(chain) = s.updates.get(&(offset, column_id)) {
            for pv in chain {
                if !read.view().ts_visible(pv.replaced_at) {
                    return pv.value.clone();
                }
            }
        }
        s.columns[column_id].get(offset as usize)
    }

    pub fn find_node_by_pk(
        &self,
        read: StorageReadHandle,
        table_id: TableId,
        pk: &Value,
    ) -> Option<InternalId> {
        let store = self.nodes.get(&table_id)?;
        let key = PkKey::from_value(pk)?;
        let &offset = store.pk_index.get(&key)?;
        // MVCC: the mapped node must be visible at this read view. The index keeps
        // entries through uncommitted deletes, so a probe matches exactly what a
        // full scan + filter would observe — an own/committed delete hides the row,
        // while another connection's still-uncommitted delete does not.
        let o = offset as usize;
        read.view()
            .row_visible(store.begin_ts[o], store.end_ts[o])
            .then(|| InternalId::new(table_id, offset))
    }

    fn rel_property(
        &self,
        read: StorageReadHandle,
        rel_table: TableId,
        rel_offset: u64,
        column_id: usize,
    ) -> Value {
        let Some(s) = self.rels.get(&rel_table) else {
            return Value::Null;
        };
        if let Some(chain) = s.updates.get(&(rel_offset, column_id)) {
            for pv in chain {
                if !read.view().ts_visible(pv.replaced_at) {
                    return pv.value.clone();
                }
            }
        }
        s.columns[column_id].get(rel_offset as usize)
    }

    pub fn rel_endpoints(
        &self,
        _read: StorageReadHandle,
        rel_table: TableId,
        rel_offset: u64,
    ) -> (InternalId, InternalId) {
        let s = &self.rels[&rel_table];
        (s.src[rel_offset as usize], s.dst[rel_offset as usize])
    }

    fn set_node_property(
        &mut self,
        write: StorageWriteHandle,
        table_id: TableId,
        offset: u64,
        column_id: usize,
        value: Value,
    ) -> Result<()> {
        let view = write.read().view();
        let writer = write.writer_id();
        let store = self.nodes.get_mut(&table_id).expect("set on unknown table");
        if store.immutable {
            return Err(Error::runtime("Cannot update an icebug-disk node table."));
        }
        ensure_node_writable(store, offset, view)?;
        let prior_memory = self.memory.try_reserve(
            usize_bytes(std::mem::size_of::<PriorVersion>())
                .saturating_add(store.columns[column_id].value_heap_bytes(offset as usize)),
        )?;
        let before =
            store.columns[column_id].replace_accounted(offset as usize, value, &self.memory)?;
        store
            .updates
            .entry((offset, column_id))
            .or_default()
            .push(PriorVersion {
                value: before,
                replaced_at: writer,
                _memory: prior_memory,
            });
        self.push_undo(
            writer,
            UndoOp::RestoreNodeProp {
                table: table_id,
                offset,
                col: column_id,
            },
        );
        Ok(())
    }

    fn set_rel_property(
        &mut self,
        write: StorageWriteHandle,
        rel_table: TableId,
        rel_offset: u64,
        column_id: usize,
        value: Value,
    ) -> Result<()> {
        let view = write.read().view();
        let writer = write.writer_id();
        let store = self
            .rels
            .get_mut(&rel_table)
            .expect("set on unknown rel table");
        if store.immutable {
            return Err(Error::runtime(
                "Cannot update an icebug-disk relationship table.",
            ));
        }
        ensure_rel_writable(store, rel_offset, view)?;
        let prior_memory = self.memory.try_reserve(
            usize_bytes(std::mem::size_of::<PriorVersion>())
                .saturating_add(store.columns[column_id].value_heap_bytes(rel_offset as usize)),
        )?;
        let before =
            store.columns[column_id].replace_accounted(rel_offset as usize, value, &self.memory)?;
        store
            .updates
            .entry((rel_offset, column_id))
            .or_default()
            .push(PriorVersion {
                value: before,
                replaced_at: writer,
                _memory: prior_memory,
            });
        self.push_undo(
            writer,
            UndoOp::RestoreRelProp {
                table: rel_table,
                offset: rel_offset,
                col: column_id,
            },
        );
        Ok(())
    }

    /// Apply one typed batch of updates to a single node property column.
    ///
    /// `updates` contains `[INTERNAL_ID, property_type]`; its selection controls which
    /// physical positions are written.
    pub fn set_node_property_batch(
        &mut self,
        write: StorageWriteHandle,
        table_id: TableId,
        column_id: usize,
        updates: &DataChunk,
    ) -> Result<()> {
        for position in updates.sel.iter() {
            let Value::InternalId(id) = updates.columns[0].get_value(position) else {
                return Err(Error::runtime(
                    "Node property batch id is not an internal id.".to_string(),
                ));
            };
            if id.table_id != table_id {
                return Err(Error::runtime(
                    "Node property batch mixes table ids.".to_string(),
                ));
            }
            self.set_node_property(
                write,
                table_id,
                id.offset.0,
                column_id,
                updates.columns[1].get_value(position),
            )?;
        }
        Ok(())
    }

    /// Relationship counterpart of [`Self::set_node_property_batch`].
    pub fn set_rel_property_batch(
        &mut self,
        write: StorageWriteHandle,
        table_id: TableId,
        column_id: usize,
        updates: &DataChunk,
    ) -> Result<()> {
        for position in updates.sel.iter() {
            let Value::InternalId(id) = updates.columns[0].get_value(position) else {
                return Err(Error::runtime(
                    "Relationship property batch id is not an internal id.".to_string(),
                ));
            };
            if id.table_id != table_id {
                return Err(Error::runtime(
                    "Relationship property batch mixes table ids.".to_string(),
                ));
            }
            self.set_rel_property(
                write,
                table_id,
                id.offset.0,
                column_id,
                updates.columns[1].get_value(position),
            )?;
        }
        Ok(())
    }

    fn delete_node(
        &mut self,
        write: StorageWriteHandle,
        table_id: TableId,
        offset: u64,
    ) -> Result<()> {
        let view = write.read().view();
        let writer = write.writer_id();
        let store = self
            .nodes
            .get_mut(&table_id)
            .expect("delete on unknown table");
        if store.immutable {
            return Err(Error::runtime(
                "Cannot delete from an icebug-disk node table.",
            ));
        }
        if !node_delete_needs_tombstone(store, offset, view)? {
            return Ok(());
        }
        // Tombstone via `end_ts` only; the PK index entry stays (a reader at an
        // older view must still resolve this node by PK until the delete commits,
        // and `find_node_by_pk` filters by visibility). The key is reclaimed/
        // overwritten only by a later `insert_node` of the same PK.
        store.end_ts[offset as usize] = writer;
        self.push_undo(
            writer,
            UndoOp::UndeleteNode {
                table: table_id,
                offset,
            },
        );
        Ok(())
    }

    fn delete_rel(
        &mut self,
        write: StorageWriteHandle,
        rel_table: TableId,
        rel_offset: u64,
    ) -> Result<()> {
        let view = write.read().view();
        let writer = write.writer_id();
        {
            let store = self
                .rels
                .get_mut(&rel_table)
                .expect("delete on unknown rel table");
            if store.immutable {
                return Err(Error::runtime(
                    "Cannot delete from an icebug-disk relationship table.",
                ));
            }
            let o = rel_offset as usize;
            if store.end_ts[o] != TS_INF {
                if view.owns(store.end_ts[o]) {
                    return Ok(()); // already tombstoned by this writer/self-loop pass
                }
                if view.conflicts_with_write(store.end_ts[o]) {
                    return Err(delete_conflict());
                }
                return Ok(());
            }
            ensure_rel_writable(store, rel_offset, view)?;
            // Tombstone via `end_ts`; keep the rel in both adjacencies (`extend` and
            // the connected-edge queries filter it by visibility).
            store.end_ts[o] = writer;
        }
        self.push_undo(
            writer,
            UndoOp::UndeleteRel {
                table: rel_table,
                offset: rel_offset,
            },
        );
        Ok(())
    }

    /// Validate node-delete writer ownership without mutating storage.
    pub fn preflight_node_delete_batch(
        &self,
        write: StorageWriteHandle,
        ids: &DataChunk,
    ) -> Result<()> {
        let view = write.read().view();
        for position in ids.sel.iter() {
            let Value::InternalId(id) = ids.columns[0].get_value(position) else {
                return Err(Error::runtime(
                    "Node delete batch id is not an internal id.".to_string(),
                ));
            };
            let store = self
                .nodes
                .get(&id.table_id)
                .expect("delete on unknown table");
            if store.immutable {
                return Err(Error::runtime(
                    "Cannot delete from an icebug-disk node table.",
                ));
            }
            node_delete_needs_tombstone(store, id.offset.0, view)?;
        }
        Ok(())
    }

    /// Tombstone the selected node ids in one typed batch.
    pub fn delete_node_batch(&mut self, write: StorageWriteHandle, ids: &DataChunk) -> Result<()> {
        for position in ids.sel.iter() {
            let Value::InternalId(id) = ids.columns[0].get_value(position) else {
                return Err(Error::runtime(
                    "Node delete batch id is not an internal id.".to_string(),
                ));
            };
            self.delete_node(write, id.table_id, id.offset.0)?;
        }
        Ok(())
    }

    /// Tombstone the selected relationship ids in one typed batch.
    pub fn delete_rel_batch(&mut self, write: StorageWriteHandle, ids: &DataChunk) -> Result<()> {
        for position in ids.sel.iter() {
            let Value::InternalId(id) = ids.columns[0].get_value(position) else {
                return Err(Error::runtime(
                    "Relationship delete batch id is not an internal id.".to_string(),
                ));
            };
            self.delete_rel(write, id.table_id, id.offset.0)?;
        }
        Ok(())
    }

    pub fn node_is_deleted(&self, read: StorageReadHandle, table_id: TableId, offset: u64) -> bool {
        let Some(s) = self.nodes.get(&table_id) else {
            return false;
        };
        let o = offset as usize;
        match (s.begin_ts.get(o), s.end_ts.get(o)) {
            (Some(&b), Some(&e)) => !read.view().row_visible(b, e),
            _ => false,
        }
    }

    pub fn node_connected_edge(
        &self,
        read: StorageReadHandle,
        node: InternalId,
    ) -> Option<(TableId, EdgeDir)> {
        let view = read.view();
        let mut tables: Vec<TableId> = self.rels.keys().copied().collect();
        tables.sort();
        for t in tables {
            let store = &self.rels[&t];
            let any_visible = |adj: &[Vec<u64>], keyed: TableId| {
                adj_at(adj, node, keyed).iter().any(|&o| {
                    view.row_visible(store.begin_ts[o as usize], store.end_ts[o as usize])
                })
            };
            if any_visible(&store.fwd_adj, store.from_table) {
                return Some((t, EdgeDir::Fwd));
            }
            if any_visible(&store.bwd_adj, store.to_table) {
                return Some((t, EdgeDir::Bwd));
            }
        }
        None
    }

    pub fn node_connected_rels(
        &self,
        read: StorageReadHandle,
        node: InternalId,
    ) -> Vec<InternalId> {
        let view = read.view();
        let mut out = Vec::new();
        let mut tables: Vec<TableId> = self.rels.keys().copied().collect();
        tables.sort();
        for t in tables {
            let store = &self.rels[&t];
            for (adj, keyed) in [
                (&store.fwd_adj, store.from_table),
                (&store.bwd_adj, store.to_table),
            ] {
                out.extend(
                    adj_at(adj, node, keyed)
                        .iter()
                        .copied()
                        .filter(|&o| {
                            view.row_visible(store.begin_ts[o as usize], store.end_ts[o as usize])
                        })
                        .map(|o| InternalId::new(t, o)),
                );
            }
        }
        out
    }

    pub fn undo_mark(&self) -> usize {
        self.next_undo_seq
    }

    pub fn rollback_to(&mut self, write: StorageWriteHandle, mark: usize) {
        let writer_id = write.writer_id();
        let mut i = self.undo.len();
        while i > 0 {
            i -= 1;
            if self.undo[i].writer == writer_id && self.undo[i].seq >= mark {
                let op = self.undo.remove(i).op;
                self.apply_undo(op, writer_id);
            }
        }
    }

    pub fn commit_to(&mut self, write: StorageWriteHandle, mark: usize) {
        let writer_id = write.writer_id();
        let ts = self.commit_clock.next();
        let mut touched = HashSet::new();
        let mut pending = Vec::with_capacity(self.undo.len());
        std::mem::swap(&mut pending, &mut self.undo);
        for entry in pending {
            if entry.writer != writer_id || entry.seq < mark {
                self.undo.push(entry);
                continue;
            }
            match entry.op {
                UndoOp::UninsertNode { table, offset, .. } => {
                    touched.insert(table);
                    if let Some(store) = self.nodes.get_mut(&table) {
                        upgrade(Some(&mut store.begin_ts), offset, writer_id, ts);
                    }
                }
                UndoOp::UndeleteNode { table, offset } => {
                    touched.insert(table);
                    upgrade(
                        self.nodes.get_mut(&table).map(|store| &mut store.end_ts),
                        offset,
                        writer_id,
                        ts,
                    );
                }
                UndoOp::UninsertRel { table, offset } => {
                    touched.insert(table);
                    if let Some(store) = self.rels.get_mut(&table) {
                        upgrade(Some(&mut store.begin_ts), offset, writer_id, ts);
                    }
                }
                UndoOp::UndeleteRel { table, offset } => {
                    touched.insert(table);
                    upgrade(
                        self.rels.get_mut(&table).map(|store| &mut store.end_ts),
                        offset,
                        writer_id,
                        ts,
                    );
                }
                UndoOp::RestoreNodeProp { table, offset, col } => {
                    touched.insert(table);
                    if let Some(store) = self.nodes.get_mut(&table) {
                        upgrade_chain(store.updates.get_mut(&(offset, col)), writer_id, ts);
                    }
                }
                UndoOp::RestoreRelProp { table, offset, col } => {
                    touched.insert(table);
                    if let Some(store) = self.rels.get_mut(&table) {
                        upgrade_chain(store.updates.get_mut(&(offset, col)), writer_id, ts);
                    }
                }
                UndoOp::UncreateTable { table }
                | UndoOp::RestoreNodeTable { table, .. }
                | UndoOp::RestoreRelTable { table, .. }
                | UndoOp::UnaddColumn { table }
                | UndoOp::RestoreNodeColumn { table, .. }
                | UndoOp::RestoreRelColumn { table, .. } => {
                    touched.insert(table);
                }
            }
        }
        let view = ReadView::reader(ts);
        for table in touched {
            if let Some(store) = self.nodes.get_mut(&table) {
                let stats = store.calculate_stats(view);
                store.stats = stats.clone();
                store.stats_history.push((ts, stats));
            } else if let Some(store) = self.rels.get_mut(&table) {
                let stats = store.calculate_stats(view);
                store.stats = stats.clone();
                store.stats_history.push((ts, stats));
            }
        }
        if self.undo.is_empty() {
            self.undo = Vec::new();
        }
    }

    pub fn current_commit_ts(&self) -> Ts {
        self.commit_clock.current()
    }

    pub fn table_stats(&self, read: StorageReadHandle, table_id: TableId) -> Option<TableStats> {
        let view = read.view();
        if let Some(store) = self.nodes.get(&table_id) {
            if view.writer_id.is_some() {
                return Some(store.calculate_stats(view));
            }
            return Some(
                store
                    .stats_history
                    .iter()
                    .rev()
                    .find(|(timestamp, _)| *timestamp <= view.read_ts)
                    .map(|(_, stats)| stats.clone())
                    .unwrap_or_else(|| store.stats.empty_like()),
            );
        }
        self.rels.get(&table_id).map(|store| {
            if view.writer_id.is_some() {
                store.calculate_stats(view)
            } else {
                store
                    .stats_history
                    .iter()
                    .rev()
                    .find(|(timestamp, _)| *timestamp <= view.read_ts)
                    .map(|(_, stats)| stats.clone())
                    .unwrap_or_else(|| store.stats.empty_like())
            }
        })
    }
}

impl InMemStorage {
    /// Apply one inverse mutation during rollback.
    ///
    /// The facade prevents schema changes from interleaving with a normal rollback.
    /// This lower layer nevertheless tolerates a vanished store, shifted column, or
    /// missing offset so direct storage use and recovery never turn stale inverse
    /// metadata into a panic.
    fn apply_undo(&mut self, op: UndoOp, writer: Ts) {
        match op {
            // Undo an insert. If it is still the physical tail (the normal
            // single-writer case), remove it and release its reservation. A
            // multi-writer interleave can put a newer row after it; then retain
            // the allocated slot as an invisible tombstone.
            UndoOp::UninsertNode {
                table,
                offset,
                pk,
                prev_owner,
            } => {
                let Some(s) = self.nodes.get_mut(&table) else {
                    return;
                };
                let owned = s
                    .begin_ts
                    .get(offset as usize)
                    .is_some_and(|&begin| begin == writer);
                if owned && offset + 1 == s.count {
                    for column in &mut s.columns {
                        column.pop(&self.memory);
                    }
                    s.begin_ts.pop();
                    s.end_ts.pop();
                    s.row_memory.pop();
                    s.count -= 1;
                } else if owned {
                    s.begin_ts[offset as usize] = TS_INF;
                }
                // Restore the index entry to whoever held this key before this
                // insert claimed it (a reclaimed deleted node, or nothing).
                match prev_owner {
                    Some(prev) => {
                        s.pk_index.insert(pk, prev);
                    }
                    None => {
                        s.pk_index.remove(&pk);
                    }
                }
            }
            UndoOp::UninsertRel { table, offset, .. } => {
                let Some(s) = self.rels.get_mut(&table) else {
                    return;
                };
                let owned = s
                    .begin_ts
                    .get(offset as usize)
                    .is_some_and(|&begin| begin == writer);
                if owned && offset + 1 == s.count {
                    let src = s.src.pop().expect("tail relationship source");
                    let dst = s.dst.pop().expect("tail relationship destination");
                    for column in &mut s.columns {
                        column.pop(&self.memory);
                    }
                    s.begin_ts.pop();
                    s.end_ts.pop();
                    s.row_memory.pop();
                    s.count -= 1;
                    adj_remove(&mut s.fwd_adj, src, offset);
                    adj_remove(&mut s.bwd_adj, dst, offset);
                } else if owned {
                    s.begin_ts[offset as usize] = TS_INF;
                }
            }
            // Undo a SET: restore the in-place cell + drop the prior version we pushed.
            UndoOp::RestoreNodeProp { table, offset, col } => {
                if let Some(s) = self.nodes.get_mut(&table) {
                    if let Some(prior) = pop_prior(s.updates.get_mut(&(offset, col)), writer) {
                        let PriorVersion {
                            value,
                            _memory,
                            replaced_at: _,
                        } = prior;
                        drop(_memory);
                        if let Some(column) = s.columns.get_mut(col) {
                            column
                                .replace_accounted(offset as usize, value, &self.memory)
                                .expect(
                                    "restoring an accounted property value cannot exceed limit",
                                );
                        }
                    }
                }
            }
            UndoOp::RestoreRelProp { table, offset, col } => {
                if let Some(s) = self.rels.get_mut(&table) {
                    if let Some(prior) = pop_prior(s.updates.get_mut(&(offset, col)), writer) {
                        let PriorVersion {
                            value,
                            _memory,
                            replaced_at: _,
                        } = prior;
                        drop(_memory);
                        if let Some(column) = s.columns.get_mut(col) {
                            column
                                .replace_accounted(offset as usize, value, &self.memory)
                                .expect(
                                    "restoring an accounted property value cannot exceed limit",
                                );
                        }
                    }
                }
            }
            // Undo a delete: clear `end_ts` (live again). The PK index was never
            // touched by the delete, so nothing to re-index.
            UndoOp::UndeleteNode { table, offset } => {
                if let Some(e) = self
                    .nodes
                    .get_mut(&table)
                    .and_then(|s| s.end_ts.get_mut(offset as usize))
                {
                    if *e == writer {
                        *e = TS_INF;
                    }
                }
            }
            UndoOp::UndeleteRel { table, offset, .. } => {
                if let Some(e) = self
                    .rels
                    .get_mut(&table)
                    .and_then(|s| s.end_ts.get_mut(offset as usize))
                {
                    if *e == writer {
                        *e = TS_INF;
                    }
                }
            }
            // --- structural DDL: reverse a schema change ---
            UndoOp::UncreateTable { table } => {
                self.nodes.remove(&table);
                self.rels.remove(&table);
            }
            UndoOp::RestoreNodeTable { table, store } => {
                self.nodes.insert(table, store);
            }
            UndoOp::RestoreRelTable { table, store } => {
                self.rels.insert(table, store);
            }
            UndoOp::UnaddColumn { table } => {
                if let Some(s) = self.nodes.get_mut(&table) {
                    s.columns.pop();
                    s.num_columns -= 1;
                    s.stats.pop_column();
                } else if let Some(s) = self.rels.get_mut(&table) {
                    s.columns.pop();
                    s.num_columns -= 1;
                    s.stats.pop_column();
                }
            }
            UndoOp::RestoreNodeColumn {
                table,
                idx,
                column,
                pk_column,
            } => {
                if let Some(s) = self.nodes.get_mut(&table) {
                    s.stats
                        .restore_column(idx, &column.logical_type, &column.values());
                    s.columns.insert(idx, column);
                    s.num_columns += 1;
                    s.pk_column = pk_column;
                }
            }
            UndoOp::RestoreRelColumn { table, idx, column } => {
                if let Some(s) = self.rels.get_mut(&table) {
                    s.stats
                        .restore_column(idx, &column.logical_type, &column.values());
                    s.columns.insert(idx, column);
                    s.num_columns += 1;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use koko_common::START_TX_ID;

    fn test_write() -> StorageWriteHandle {
        StorageWriteHandle::new(ReadView::writer(START_TX_ID - 1, UNCOMMITTED), UNCOMMITTED)
    }

    fn test_read() -> StorageReadHandle {
        test_write().read()
    }

    fn extend(
        storage: &InMemStorage,
        rel_table: TableId,
        node: InternalId,
        dir: ExtendDir,
    ) -> Vec<BatchNeighbor> {
        let mut out = Vec::new();
        storage.extend_batch_into(
            test_read(),
            rel_table,
            std::slice::from_ref(&node),
            dir,
            &mut out,
        );
        out
    }

    #[test]
    fn node_insert_scan_pk() {
        let mut s = InMemStorage::new();
        let t = TableId(0);
        s.create_node_table(
            test_write(),
            t,
            &[LogicalType::String, LogicalType::Int64],
            0,
        ); // pk = column 0
        let a = s
            .insert_node(
                test_write(),
                t,
                vec![Value::String("Alice".into()), Value::Int64(35)],
            )
            .unwrap();
        assert_eq!(a, InternalId::new(t, 0));
        assert_eq!(s.node_count(t), 1);
        assert_eq!(s.node_property(test_read(), t, 0, 1), Value::Int64(35));
        // Duplicate primary key rejected.
        assert!(
            s.insert_node(
                test_write(),
                t,
                vec![Value::String("Alice".into()), Value::Int64(1)],
            )
            .is_err()
        );
    }

    #[test]
    fn rel_insert_and_extend() {
        let mut s = InMemStorage::new();
        let p = TableId(0);
        let k = TableId(1);
        s.create_node_table(test_write(), p, &[LogicalType::Int64], 0);
        s.create_rel_table(
            test_write(),
            k,
            p,
            p,
            &[LogicalType::Int64],
            "R",
            RelMultiplicity::default(),
        );
        let a = s
            .insert_node(test_write(), p, vec![Value::Int64(0)])
            .unwrap();
        let b = s
            .insert_node(test_write(), p, vec![Value::Int64(1)])
            .unwrap();
        let r = s
            .insert_rel(test_write(), k, a, b, vec![Value::Int64(2020)])
            .unwrap();

        let fwd = extend(&s, k, a, ExtendDir::Forward);
        assert_eq!(fwd.len(), 1);
        assert_eq!(fwd[0].nbr, b);
        assert_eq!(fwd[0].rel, r);
        assert_eq!(s.rel_property(test_read(), k, 0, 0), Value::Int64(2020));

        // Forward from b yields nothing; backward from b yields a.
        assert!(extend(&s, k, b, ExtendDir::Forward).is_empty());
        assert_eq!(extend(&s, k, b, ExtendDir::Backward)[0].nbr, a);
    }

    #[test]
    fn per_pair_dense_adjacency_keyed_by_table() {
        // Two per-pair stores of one logical rel group: (P,P) and (P,C). A node's
        // offset is a unique dense adjacency index only *within* its own table, so the
        // guard must reject a foreign-table node that shares an offset with a real src
        // (the unsoundness that reverted the first L3b; per-pair stores make it sound).
        let mut s = InMemStorage::new();
        let p = TableId(0);
        let c = TableId(1);
        let kpp = TableId(2); // FROM P TO P
        let kpc = TableId(3); // FROM P TO C
        s.create_node_table(test_write(), p, &[LogicalType::Int64], 0);
        s.create_node_table(test_write(), c, &[LogicalType::Int64], 0);
        s.create_rel_table(
            test_write(),
            kpp,
            p,
            p,
            &[],
            "R",
            RelMultiplicity::default(),
        );
        s.create_rel_table(
            test_write(),
            kpc,
            p,
            c,
            &[],
            "R",
            RelMultiplicity::default(),
        );

        let a = s
            .insert_node(test_write(), p, vec![Value::Int64(0)])
            .unwrap(); // P, offset 0
        let b = s
            .insert_node(test_write(), p, vec![Value::Int64(1)])
            .unwrap(); // P, offset 1
        let x = s
            .insert_node(test_write(), c, vec![Value::Int64(0)])
            .unwrap(); // C, offset 0 (== a's)
        assert_eq!(a.offset, x.offset); // same dense offset, different table

        s.insert_rel(test_write(), kpp, a, b, vec![]).unwrap(); // a -> b in (P,P)
        s.insert_rel(test_write(), kpc, a, x, vec![]).unwrap(); // a -> x in (P,C)

        // Each pair store holds only its own edge, and the rel's id carries the pair's
        // table id (distinct per pair).
        let pp = extend(&s, kpp, a, ExtendDir::Forward);
        assert_eq!(pp.len(), 1);
        assert_eq!(pp[0].nbr, b);
        assert_eq!(pp[0].rel.table_id, kpp);
        let pc = extend(&s, kpc, a, ExtendDir::Forward);
        assert_eq!(pc.len(), 1);
        assert_eq!(pc[0].nbr, x);
        assert_eq!(pc[0].rel.table_id, kpc);

        // The keyed-table guard: x (table C, offset 0) shares a's offset but is not a
        // FROM-P node, so it extends to nothing in the (P,P) store — never a's edge.
        assert!(extend(&s, kpp, x, ExtendDir::Forward).is_empty());
        // Backward over (P,C): x is the TO endpoint, b is not.
        assert_eq!(extend(&s, kpc, x, ExtendDir::Backward)[0].nbr, a);
        assert!(extend(&s, kpc, b, ExtendDir::Backward).is_empty());
    }

    #[test]
    fn narrow_graph_visitors_preserve_visibility_and_direction() {
        let mut storage = InMemStorage::new();
        let node_table = TableId(0);
        let rel_table = TableId(1);
        storage.create_node_table(test_write(), node_table, &[LogicalType::Int64], 0);
        storage.create_rel_table(
            test_write(),
            rel_table,
            node_table,
            node_table,
            &[],
            "R",
            RelMultiplicity::default(),
        );
        let a = storage
            .insert_node(test_write(), node_table, vec![Value::Int64(0)])
            .unwrap();
        let b = storage
            .insert_node(test_write(), node_table, vec![Value::Int64(1)])
            .unwrap();
        let c = storage
            .insert_node(test_write(), node_table, vec![Value::Int64(2)])
            .unwrap();
        storage
            .insert_rel(test_write(), rel_table, a, b, vec![])
            .unwrap();
        storage
            .insert_rel(test_write(), rel_table, a, c, vec![])
            .unwrap();

        assert!(storage.node_rows_all_visible(test_read(), node_table));
        assert!(storage.rel_rows_all_visible(test_read(), rel_table));

        let mut nodes = Vec::new();
        storage
            .visit_node_offsets_all_visible(test_read(), node_table, |offset| {
                nodes.push(offset);
                Ok(())
            })
            .unwrap();
        assert_eq!(nodes, [0, 1, 2]);

        let mut edges = Vec::new();
        storage
            .visit_rel_endpoints_all_visible(
                test_read(),
                rel_table,
                |rel_offset, src_offset, dst_offset| {
                    edges.push((rel_offset, src_offset, dst_offset));
                    Ok(())
                },
            )
            .unwrap();
        assert_eq!(edges, [(0, 0, 1), (1, 0, 2)]);

        let mut forward = Vec::new();
        storage
            .visit_neighbors_all_visible(
                test_read(),
                rel_table,
                a,
                EdgeDir::Fwd,
                |rel_offset, neighbor_offset| {
                    forward.push((rel_offset, neighbor_offset));
                    Ok(())
                },
            )
            .unwrap();
        assert_eq!(forward, [(0, 1), (1, 2)]);

        let mut backward = Vec::new();
        storage
            .visit_neighbors_all_visible(
                test_read(),
                rel_table,
                c,
                EdgeDir::Bwd,
                |rel_offset, neighbor_offset| {
                    backward.push((rel_offset, neighbor_offset));
                    Ok(())
                },
            )
            .unwrap();
        assert_eq!(backward, [(1, 0)]);

        storage.delete_rel(test_write(), rel_table, 1).unwrap();
        storage.delete_node(test_write(), node_table, 2).unwrap();
        assert!(!storage.node_rows_all_visible(test_read(), node_table));
        assert!(!storage.rel_rows_all_visible(test_read(), rel_table));

        nodes.clear();
        storage
            .visit_node_offsets(test_read(), node_table, |offset| {
                nodes.push(offset);
                Ok(())
            })
            .unwrap();
        assert_eq!(nodes, [0, 1]);

        edges.clear();
        storage
            .visit_rel_endpoints(
                test_read(),
                rel_table,
                |rel_offset, src_offset, dst_offset| {
                    edges.push((rel_offset, src_offset, dst_offset));
                    Ok(())
                },
            )
            .unwrap();
        assert_eq!(edges, [(0, 0, 1)]);

        forward.clear();
        storage
            .visit_neighbors(
                test_read(),
                rel_table,
                a,
                EdgeDir::Fwd,
                |rel_offset, neighbor_offset| {
                    forward.push((rel_offset, neighbor_offset));
                    Ok(())
                },
            )
            .unwrap();
        assert_eq!(forward, [(0, 1)]);
    }

    #[test]
    fn set_delete_and_undo() {
        let mut s = InMemStorage::new();
        let p = TableId(0);
        let k = TableId(1);
        s.create_node_table(test_write(), p, &[LogicalType::Int64], 0); // pk = col 0
        s.create_rel_table(test_write(), k, p, p, &[], "R", RelMultiplicity::default());
        let a = s
            .insert_node(test_write(), p, vec![Value::Int64(0)])
            .unwrap();
        let b = s
            .insert_node(test_write(), p, vec![Value::Int64(1)])
            .unwrap();
        s.insert_rel(test_write(), k, a, b, vec![]).unwrap();

        // SET a property in place, with undo.
        let mark = s.undo_mark();
        s.set_node_property(test_write(), p, 0, 0, Value::Int64(99))
            .unwrap();
        assert_eq!(s.node_property(test_read(), p, 0, 0), Value::Int64(99));
        s.rollback_to(test_write(), mark);
        assert_eq!(s.node_property(test_read(), p, 0, 0), Value::Int64(0));

        // A node with a connected edge can't be plain-deleted; DETACH lists its rels.
        assert_eq!(
            s.node_connected_edge(test_read(), a),
            Some((k, EdgeDir::Fwd))
        );
        assert_eq!(s.node_connected_rels(test_read(), a).len(), 1);

        // Delete the rel, then the node; both are hidden; offsets stay dense.
        let mark = s.undo_mark();
        s.delete_rel(test_write(), k, 0).unwrap();
        assert!(extend(&s, k, a, ExtendDir::Forward).is_empty());
        assert_eq!(s.node_connected_edge(test_read(), a), None);
        s.delete_node(test_write(), p, 0).unwrap();
        assert!(s.node_is_deleted(test_read(), p, 0));
        assert_eq!(s.find_node_by_pk(test_read(), p, &Value::Int64(0)), None);

        // Roll the delete back: node + rel + pk-index restored.
        s.rollback_to(test_write(), mark);
        assert!(!s.node_is_deleted(test_read(), p, 0));
        assert_eq!(s.find_node_by_pk(test_read(), p, &Value::Int64(0)), Some(a));
        assert_eq!(extend(&s, k, a, ExtendDir::Forward)[0].nbr, b);

        // Committing (discard) leaves mutations in place and unrollbackable.
        s.delete_node(test_write(), p, 1).unwrap();
        s.commit_to(test_write(), 0);
        assert!(s.node_is_deleted(test_read(), p, 1));
    }

    /// Statistics are folded in at commit: a table's `num_tuples`, per-column
    /// distinct estimate, min/max, and null count reflect committed rows.
    #[test]
    fn table_stats_folds_committed_inserts() {
        let mut s = InMemStorage::new();
        let t = TableId(0);
        s.create_node_table(
            test_write(),
            t,
            &[LogicalType::Int64, LogicalType::Int64],
            0,
        ); // col 0 = pk id, col 1 = age
        let mark = s.undo_mark();
        s.insert_node(test_write(), t, vec![Value::Int64(1), Value::Int64(35)])
            .unwrap();
        s.insert_node(test_write(), t, vec![Value::Int64(2), Value::Int64(30)])
            .unwrap();
        s.insert_node(test_write(), t, vec![Value::Int64(3), Value::Int64(35)])
            .unwrap();
        // The writer's own uncommitted writes are visible in its statistics snapshot.
        assert_eq!(s.table_stats(test_read(), t).unwrap().num_tuples(), 3);
        s.commit_to(test_write(), mark);
        let st = s.table_stats(test_read(), t).unwrap();
        assert_eq!(st.num_tuples(), 3);
        // age (col 1): 2 distinct (30, 35), min 30, max 35, no nulls.
        let age = st.column(1).unwrap();
        assert_eq!(age.num_distinct(), 2);
        assert_eq!(age.min(), Some(&Value::Int64(30)));
        assert_eq!(age.max(), Some(&Value::Int64(35)));
        assert_eq!(age.null_count(), 0);
        // pk (col 0): 3 distinct.
        assert_eq!(st.column(0).unwrap().num_distinct(), 3);
    }

    /// A rolled-back insert is never folded — stats only fold at commit.
    #[test]
    fn table_stats_rollback_leaves_no_trace() {
        let mut s = InMemStorage::new();
        let t = TableId(0);
        s.create_node_table(test_write(), t, &[LogicalType::Int64], 0);
        let mark = s.undo_mark();
        s.insert_node(test_write(), t, vec![Value::Int64(1)])
            .unwrap();
        s.rollback_to(test_write(), mark);
        assert_eq!(s.table_stats(test_read(), t).unwrap().num_tuples(), 0);
    }

    /// Cardinality is rebuilt for the selected snapshot after a committed delete.
    #[test]
    fn table_stats_reflects_committed_delete() {
        let mut s = InMemStorage::new();
        let t = TableId(0);
        s.create_node_table(test_write(), t, &[LogicalType::Int64], 0);
        let mark = s.undo_mark();
        s.insert_node(test_write(), t, vec![Value::Int64(1)])
            .unwrap();
        s.insert_node(test_write(), t, vec![Value::Int64(2)])
            .unwrap();
        s.commit_to(test_write(), mark);
        assert_eq!(s.table_stats(test_read(), t).unwrap().num_tuples(), 2);
        let mark = s.undo_mark();
        s.delete_node(test_write(), t, 0).unwrap();
        s.commit_to(test_write(), mark);
        assert_eq!(s.table_stats(test_read(), t).unwrap().num_tuples(), 1);
    }

    /// Null counts are tracked, and `ALTER … ADD` keeps the per-column stats aligned.
    #[test]
    fn table_stats_nulls_and_added_column() {
        let mut s = InMemStorage::new();
        let t = TableId(0);
        s.create_node_table(
            test_write(),
            t,
            &[LogicalType::Int64, LogicalType::String],
            0,
        );
        let mark = s.undo_mark();
        s.insert_node(test_write(), t, vec![Value::Int64(1), Value::Null])
            .unwrap();
        s.insert_node(
            test_write(),
            t,
            vec![Value::Int64(2), Value::String("x".into())],
        )
        .unwrap();
        s.commit_to(test_write(), mark);
        let st = s.table_stats(test_read(), t).unwrap();
        assert_eq!(st.column(1).unwrap().null_count(), 1);
        // ALTER ADD a constant-default column: stats gain an aligned column.
        s.add_column(test_write(), t, LogicalType::Int64, Value::Int64(7))
            .unwrap();
        let st = s.table_stats(test_read(), t).unwrap();
        assert_eq!(st.num_columns(), 3);
        assert_eq!(st.column(2).unwrap().num_distinct(), 1);
        assert_eq!(st.column(2).unwrap().min(), Some(&Value::Int64(7)));
    }
}
