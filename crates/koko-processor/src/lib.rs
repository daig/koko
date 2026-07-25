//! `koko-processor` — pull-based (Volcano) typed-batch execution with
//! factorization, morsel parallelism, and columnar result production.
//!
//! Executes a [`QueryPlan`] (one [`PartPlan`] per `WITH`-delimited part) against
//! the typed [`InMemStorage`] engine, then applies each part's projection (scalar/aggregate),
//! `DISTINCT`, `ORDER BY`, `SKIP`, and `LIMIT` — or, for write queries, performs
//! the `CREATE`/`SET`/`DELETE`/`MERGE` mutations. Parts run in sequence, each
//! seeded by the previous part's projected rows.
//!
//! Each [`PlanOp`] is compiled into a stateful pull operator ([`Exec`]); the sink
//! drives `while let Some(chunk) = root.next_chunk(ctx)? { … }`, and every operator
//! readies at most one [`DataChunk`] (≤[`VECTOR_CAPACITY`] rows) per call. Stateless
//! pipelines stream and `LIMIT`/`EXISTS` terminate early; joins, aggregation, ordering,
//! recursive enumeration, and writes are explicit pipeline breakers. Recursive paths
//! materialize per source (mirroring the C++ `getNextTuplesInternal` model;
//! `docs/cpp-reference/03-exec-model.md` §4). Execution is flat with multiplicity
//! factorization; eligible stateless scan spines run morsel-parallel and stateful shapes fall
//! back to serial execution. Writes
//! are pipeline breakers: the read pipeline is drained (releasing `&storage`) before
//! the clause mutates with `&mut storage`.

use koko_binder::{
    BoundCreate, BoundDelete, BoundExpr, BoundProjection, BoundQuery, BoundRegularQuery, BoundSet,
    BoundSetTarget, BoundTableFunc, CsvLoadOptions, OrderKey, PathSemantic, ProjItem,
    RecursiveMode, SequenceFn, SubqueryKind, TableFuncRuntime, VarId, table_func_rows,
};
use koko_catalog::{Catalog, ColumnDefault, IcebugTableSource, RelTable};
use koko_common::{
    DataChunk, Error, ExtendDir, InternalId, LogicalType, MemoryReservation, MemoryTracker,
    NodeValue, RecursiveRelValue, RelValue, Result, Selection, TableId, VECTOR_CAPACITY, Value,
    file_resolver::FileFormat, value_payload_bytes,
};
use koko_expr::{AccessorKind, AggSpec, ColumnResolver, CompiledExpr, compile, compile_collect};
use koko_function::{
    AggOp, AggState, ValueKey, cast_value, cypher_cmp, eval_scalar, eval_scalar_func_with_context,
    oracle_hash::RandomState, order_cmp,
};
use koko_planner::{
    Extend, ExtendTarget, IndexScan, InputSlot, JoinKind, MergePlan, PartPlan, PathRel, PlanOp,
    ProjectPath, QueryPlan, RegularPlan, RowLayout, ScanNode, ScanTable, UnwindTarget, UpdateOp,
    VarColKind, VarLengthExtend,
};
use koko_storage::{
    BatchNeighbor, InMemStorage, SharedStorage, StorageReadHandle, StorageWriteHandle,
};
use std::cmp::Ordering;
use std::collections::{HashMap, HashSet, VecDeque};
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
use std::time::Instant;

/// Cooperative cancellation/deadline state checked at every pull boundary.
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
            .is_some_and(|epoch| epoch.load(AtomicOrdering::Acquire) != self.captured_epoch)
            || self
                .deadline
                .is_some_and(|deadline| Instant::now() >= deadline)
        {
            return Err(Error::interrupt());
        }
        Ok(())
    }
}

/// Statement-lifetime accounting for operator-owned temporary allocations.
///
/// Charges are monotone for statement-lifetime structures and parallel morsels, preventing one
/// worker from under-reporting allocations released on another. A fully consumed serial correlated
/// subplan restores its pre-execution checkpoint because consecutive instances are never live
/// simultaneously.
pub struct QueryMemory {
    tracker: MemoryTracker,
    reservation: Mutex<MemoryReservation>,
    /// Statement-view visibility decisions shared by every operator instance and
    /// parallel morsel. Correlated subplans rebuild operators per outer row; keeping
    /// this cache here prevents each rebuild from rescanning a whole relationship table.
    rel_visibility: Mutex<HashMap<TableId, bool>>,
}

impl QueryMemory {
    pub fn new(tracker: &MemoryTracker) -> Result<Self> {
        Ok(Self {
            tracker: tracker.clone(),
            reservation: Mutex::new(tracker.try_reserve(0)?),
            rel_visibility: Mutex::new(HashMap::new()),
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

    fn temporary_reservation(&self, bytes: u64) -> Result<MemoryReservation> {
        self.tracker.try_reserve(bytes)
    }

    pub fn bytes(&self) -> u64 {
        self.reservation
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .bytes()
    }

    fn release_to(&self, bytes: u64) {
        let mut reservation = self
            .reservation
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        debug_assert!(bytes <= reservation.bytes());
        reservation
            .resize(bytes)
            .expect("shrinking a query-memory reservation cannot fail");
    }

    fn rel_rows_all_visible(
        &self,
        storage: &InMemStorage,
        read: StorageReadHandle,
        table: TableId,
    ) -> bool {
        let mut cache = self
            .rel_visibility
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *cache
            .entry(table)
            .or_insert_with(|| storage.rel_rows_all_visible(read, table))
    }

    fn clear_rel_visibility(&self) {
        self.rel_visibility
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clear();
    }
}
enum PinnedIcebugSource {
    Node {
        path: PathBuf,
        file: File,
        num_rows: u64,
    },
    RelCsr {
        indices_path: PathBuf,
        indices_file: File,
        indptr_path: PathBuf,
        indptr_file: File,
        target_column: String,
        num_rows: u64,
        num_bound_nodes: u64,
    },
    RelFlat {
        path: PathBuf,
        file: File,
        source_column: String,
        target_column: String,
        num_rows: u64,
    },
}

/// Query-lifetime handles for local file-backed tables. Capturing opens every
/// selected source before execution, so unlink/rename cannot switch files under
/// an already-running statement.
#[derive(Default)]
pub struct QuerySourceState {
    tables: HashMap<TableId, PinnedIcebugSource>,
}

impl QuerySourceState {
    pub fn capture(
        catalog: &Catalog,
        control: QueryControl<'_>,
        memory: &QueryMemory,
    ) -> Result<Self> {
        let mut tables = HashMap::new();
        let table_ids: Vec<_> = catalog
            .node_table_ids()
            .into_iter()
            .chain(catalog.rel_table_ids())
            .collect();
        for table_id in table_ids {
            let Some(source) = catalog
                .icebug_table(table_id)
                .and_then(|table| table.source.as_ref())
            else {
                continue;
            };
            let pinned = match source {
                IcebugTableSource::Node { path, num_rows } => PinnedIcebugSource::Node {
                    file: open_source(path)?,
                    path: path.clone(),
                    num_rows: *num_rows,
                },
                IcebugTableSource::RelCsr {
                    indices_path,
                    indptr_path,
                    target_column,
                    num_rows,
                    num_bound_nodes,
                } => PinnedIcebugSource::RelCsr {
                    indices_file: open_source(indices_path)?,
                    indptr_file: open_source(indptr_path)?,
                    indices_path: indices_path.clone(),
                    indptr_path: indptr_path.clone(),
                    target_column: target_column.clone(),
                    num_rows: *num_rows,
                    num_bound_nodes: *num_bound_nodes,
                },
                IcebugTableSource::RelFlat {
                    path,
                    source_column,
                    target_column,
                    num_rows,
                } => PinnedIcebugSource::RelFlat {
                    file: open_source(path)?,
                    path: path.clone(),
                    source_column: source_column.clone(),
                    target_column: target_column.clone(),
                    num_rows: *num_rows,
                },
            };
            tables.insert(table_id, pinned);
        }
        let state = Self { tables };
        state.validate(catalog, control, memory)?;
        Ok(state)
    }

    pub fn is_external(&self, table: TableId) -> bool {
        self.tables.contains_key(&table)
    }

    pub fn num_rows(&self, table: TableId) -> Option<u64> {
        self.tables.get(&table).map(|source| match source {
            PinnedIcebugSource::Node { num_rows, .. }
            | PinnedIcebugSource::RelCsr { num_rows, .. }
            | PinnedIcebugSource::RelFlat { num_rows, .. } => *num_rows,
        })
    }

    fn open_node_reader(
        &self,
        table: TableId,
        projected_columns: &[usize],
        start: u64,
        end: u64,
        catalog: &Catalog,
    ) -> Result<ExternalNodeReader> {
        let Some(PinnedIcebugSource::Node {
            path,
            file,
            num_rows,
        }) = self.tables.get(&table)
        else {
            return Err(Error::runtime(
                "Icebug-disk source kind does not match its node catalog entry.",
            ));
        };
        if end > *num_rows || start > end {
            return Err(Error::runtime(
                "Icebug-disk node scan range exceeds its declared row count.",
            ));
        }
        let node = catalog
            .node_table(table)
            .ok_or_else(|| Error::runtime("Icebug-disk node table is missing from the catalog."))?;
        let expected: Vec<_> = node
            .columns
            .iter()
            .map(|column| (column.name.clone(), column.ty.clone()))
            .collect();
        let projection: Vec<_> = projected_columns
            .iter()
            .map(|column| {
                node.columns
                    .get(*column)
                    .map(|column| column.name.clone())
                    .ok_or_else(|| {
                        Error::runtime(
                            "Icebug-disk node projection references a missing catalog column.",
                        )
                    })
            })
            .collect::<Result<_>>()?;
        let property_count = projection.len();
        let decode_projection: Vec<_> = if projection.is_empty() {
            node.columns
                .first()
                .map(|column| vec![column.name.clone()])
                .ok_or_else(|| Error::runtime("Icebug-disk node table has no physical columns."))?
        } else {
            projection
        };
        let reader = koko_loader::parquet::ParquetReader::open_projected_file_range(
            path,
            file.try_clone()?,
            &decode_projection,
            start,
            end - start,
        )?;
        koko_loader::icebug::validate_version(reader.file_metadata(), path)?;
        koko_loader::icebug::validate_schema(&reader.file_metadata().schema, &expected, path)?;
        if reader.file_metadata().num_rows != *num_rows {
            return Err(Error::runtime(format!(
                "Icebug-disk node file {} row count changed after table creation.",
                path.display()
            )));
        }
        Ok(ExternalNodeReader {
            table,
            next_offset: start,
            end,
            property_count,
            reader,
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn extend_batch_into(
        &self,
        rel_table: TableId,
        nodes: &[InternalId],
        dir: ExtendDir,
        out: &mut Vec<BatchNeighbor>,
        catalog: &Catalog,
        control: QueryControl<'_>,
        memory: &QueryMemory,
    ) -> Result<bool> {
        let Some(source) = self.tables.get(&rel_table) else {
            return Ok(false);
        };
        let rel = catalog.rel_table(rel_table).ok_or_else(|| {
            Error::runtime("Icebug-disk relationship table is missing from the catalog.")
        })?;
        let (from_table, to_table) = rel
            .pairs
            .first()
            .copied()
            .ok_or_else(|| Error::runtime("Icebug-disk relationship has no endpoint pair."))?;
        for (input_pos, &node) in nodes.iter().enumerate() {
            control.check()?;
            match dir {
                ExtendDir::Forward => {
                    if node.table_id == from_table {
                        external_edges_for_node(
                            source,
                            node.offset.0,
                            true,
                            input_pos,
                            rel_table,
                            from_table,
                            to_table,
                            out,
                            control,
                            memory,
                        )?;
                    }
                }
                ExtendDir::Backward => {
                    if node.table_id == to_table {
                        external_edges_for_node(
                            source,
                            node.offset.0,
                            false,
                            input_pos,
                            rel_table,
                            from_table,
                            to_table,
                            out,
                            control,
                            memory,
                        )?;
                    }
                }
                ExtendDir::Both => {
                    if node.table_id == from_table {
                        external_edges_for_node(
                            source,
                            node.offset.0,
                            true,
                            input_pos,
                            rel_table,
                            from_table,
                            to_table,
                            out,
                            control,
                            memory,
                        )?;
                    }
                    if node.table_id == to_table {
                        external_edges_for_node(
                            source,
                            node.offset.0,
                            false,
                            input_pos,
                            rel_table,
                            from_table,
                            to_table,
                            out,
                            control,
                            memory,
                        )?;
                    }
                }
            }
        }
        let bytes = u64::try_from(out.capacity())
            .unwrap_or(u64::MAX)
            .saturating_mul(std::mem::size_of::<BatchNeighbor>() as u64);
        let _neighbors_memory = memory.temporary_reservation(bytes)?;
        Ok(true)
    }

    fn projected_rows(
        &self,
        table: TableId,
        offsets: &[u64],
        columns: &[usize],
        catalog: &Catalog,
        control: QueryControl<'_>,
        memory: &QueryMemory,
    ) -> Result<Option<Vec<DataChunk>>> {
        let Some(source) = self.tables.get(&table) else {
            return Ok(None);
        };
        let (path, file, num_rows, names) = match source {
            PinnedIcebugSource::Node {
                path,
                file,
                num_rows,
            } => {
                let table = catalog.node_table(table).ok_or_else(|| {
                    Error::runtime("Icebug-disk node table is missing from the catalog.")
                })?;
                let names = columns
                    .iter()
                    .map(|column| {
                        table
                            .columns
                            .get(*column)
                            .map(|column| column.name.clone())
                            .ok_or_else(|| {
                                Error::runtime(
                                    "Icebug-disk node property references a missing column.",
                                )
                            })
                    })
                    .collect::<Result<Vec<_>>>()?;
                (path, file, *num_rows, names)
            }
            PinnedIcebugSource::RelCsr {
                indices_path,
                indices_file,
                num_rows,
                ..
            } => {
                let table = catalog.rel_table(table).ok_or_else(|| {
                    Error::runtime("Icebug-disk relationship table is missing from the catalog.")
                })?;
                let names = columns
                    .iter()
                    .map(|column| {
                        table
                            .columns
                            .get(*column)
                            .map(|column| column.name.clone())
                            .ok_or_else(|| {
                                Error::runtime(
                                    "Icebug-disk relationship property references a missing column.",
                                )
                            })
                    })
                    .collect::<Result<Vec<_>>>()?;
                (indices_path, indices_file, *num_rows, names)
            }
            PinnedIcebugSource::RelFlat {
                path,
                file,
                num_rows,
                ..
            } => {
                let table = catalog.rel_table(table).ok_or_else(|| {
                    Error::runtime("Icebug-disk relationship table is missing from the catalog.")
                })?;
                let names = columns
                    .iter()
                    .map(|column| {
                        table
                            .columns
                            .get(*column)
                            .map(|column| column.name.clone())
                            .ok_or_else(|| {
                                Error::runtime(
                                    "Icebug-disk relationship property references a missing column.",
                                )
                            })
                    })
                    .collect::<Result<Vec<_>>>()?;
                (path, file, *num_rows, names)
            }
        };
        if offsets.iter().any(|offset| *offset >= num_rows) {
            return Err(Error::runtime(
                "Icebug-disk property offset exceeds its declared row count.",
            ));
        }
        if columns.is_empty() {
            let batches = offsets
                .chunks(VECTOR_CAPACITY)
                .map(|offsets| {
                    let mut chunk = DataChunk::new(&[]);
                    chunk.set_flat(offsets.len());
                    chunk
                })
                .collect();
            return Ok(Some(batches));
        }
        let mut batches: Vec<DataChunk> = Vec::new();
        let mut accumulator: Option<ChunkAccum> = None;
        let mut rows = 0usize;
        let mut first = 0usize;
        while first < offsets.len() {
            control.check()?;
            let mut last = first + 1;
            while last < offsets.len() && offsets[last] == offsets[last - 1].saturating_add(1) {
                last += 1;
            }
            let mut reader = koko_loader::parquet::ParquetReader::open_projected_file_range(
                path,
                file.try_clone()?,
                &names,
                offsets[first],
                (last - first) as u64,
            )?;
            let run_start = rows;
            while let Some(chunk) = reader.next_chunk()? {
                control.check()?;
                let _batch_memory = memory.temporary_reservation(chunk.allocated_bytes())?;
                let types: Vec<_> = chunk
                    .columns
                    .iter()
                    .map(|column| column.logical_type.clone())
                    .collect();
                let all_columns: Vec<_> = (0..chunk.columns.len()).collect();
                for position in chunk.sel.iter() {
                    if accumulator.as_ref().is_some_and(ChunkAccum::is_full) {
                        let output = accumulator
                            .take()
                            .expect("full projected accumulator")
                            .into_chunk();
                        batches.push(output);
                    }
                    let accumulator = accumulator.get_or_insert_with(|| ChunkAccum::new(&types));
                    accumulator.push_chunk_row_with_mult(&chunk, position, &all_columns, 1);
                    rows += 1;
                }
            }
            if rows - run_start != last - first {
                return Err(Error::runtime(
                    "Icebug-disk property range ended before its declared row count.",
                ));
            }
            first = last;
        }
        if let Some(output) = accumulator.and_then(ChunkAccum::take) {
            batches.push(output);
        }
        Ok(Some(batches))
    }

    fn projected_values(
        &self,
        table: TableId,
        offset: u64,
        columns: &[usize],
        catalog: &Catalog,
        control: QueryControl<'_>,
        memory: &QueryMemory,
    ) -> Result<Option<Vec<Value>>> {
        let Some(batches) =
            self.projected_rows(table, &[offset], columns, catalog, control, memory)?
        else {
            return Ok(None);
        };
        Ok(Some(
            columns
                .iter()
                .enumerate()
                .map(|(column, _)| gathered_property(&batches, 0, column))
                .collect(),
        ))
    }

    fn rel_endpoints(
        &self,
        table: TableId,
        offset: u64,
        catalog: &Catalog,
        control: QueryControl<'_>,
        memory: &QueryMemory,
    ) -> Result<Option<(InternalId, InternalId)>> {
        let Some(source) = self.tables.get(&table) else {
            return Ok(None);
        };
        let rel = catalog.rel_table(table).ok_or_else(|| {
            Error::runtime("Icebug-disk relationship table is missing from the catalog.")
        })?;
        let (from_table, to_table) = rel
            .pairs
            .first()
            .copied()
            .ok_or_else(|| Error::runtime("Icebug-disk relationship has no endpoint pair."))?;
        let endpoints = match source {
            PinnedIcebugSource::Node { .. } => {
                return Err(Error::runtime(
                    "Icebug-disk source kind does not match its relationship catalog entry.",
                ));
            }
            PinnedIcebugSource::RelCsr {
                indices_path,
                indices_file,
                indptr_path,
                indptr_file,
                target_column,
                num_rows,
                num_bound_nodes,
            } => {
                if offset >= *num_rows {
                    return Err(Error::runtime(
                        "Icebug-disk relationship offset exceeds its row count.",
                    ));
                }
                let mut source_offset = None;
                for candidate in 0..*num_bound_nodes {
                    let (start, end) =
                        csr_bounds(indptr_path, indptr_file, candidate, control, memory)?;
                    if start <= offset && offset < end {
                        source_offset = Some(candidate);
                        break;
                    }
                }
                let source_offset = source_offset.ok_or_else(|| {
                    Error::runtime("Icebug-disk CSR does not own a relationship row.")
                })?;
                let mut target = None;
                scan_csr_edges(
                    indices_path,
                    indices_file,
                    target_column,
                    offset,
                    offset + 1,
                    |_, value| target = Some(value),
                    control,
                    memory,
                )?;
                (
                    InternalId::new(from_table, source_offset),
                    InternalId::new(
                        to_table,
                        target.expect("validated one-row CSR relationship range"),
                    ),
                )
            }
            PinnedIcebugSource::RelFlat {
                path,
                file,
                source_column,
                target_column,
                num_rows,
            } => {
                if offset >= *num_rows {
                    return Err(Error::runtime(
                        "Icebug-disk relationship offset exceeds its row count.",
                    ));
                }
                let projection = [source_column.clone(), target_column.clone()];
                let mut reader = koko_loader::parquet::ParquetReader::open_projected_file_range(
                    path,
                    file.try_clone()?,
                    &projection,
                    offset,
                    1,
                )?;
                let chunk = reader
                    .next_chunk()?
                    .ok_or_else(|| Error::runtime("Icebug-disk relationship row is missing."))?;
                let _batch_memory = memory.temporary_reservation(chunk.allocated_bytes())?;
                (
                    InternalId::new(
                        from_table,
                        parquet_offset(chunk.columns[0].get_value(0), path)?,
                    ),
                    InternalId::new(
                        to_table,
                        parquet_offset(chunk.columns[1].get_value(0), path)?,
                    ),
                )
            }
        };
        Ok(Some(endpoints))
    }
    fn validate(
        &self,
        catalog: &Catalog,
        control: QueryControl<'_>,
        memory: &QueryMemory,
    ) -> Result<()> {
        for (&table_id, source) in &self.tables {
            control.check()?;
            match source {
                PinnedIcebugSource::Node {
                    path,
                    file,
                    num_rows,
                } => {
                    let table = catalog.node_table(table_id).ok_or_else(|| {
                        Error::runtime("Icebug-disk node table is missing from the catalog.")
                    })?;
                    let expected: Vec<_> = table
                        .columns
                        .iter()
                        .map(|column| (column.name.clone(), column.ty.clone()))
                        .collect();
                    let validation_projection = expected
                        .first()
                        .map(|(name, _)| vec![name.clone()])
                        .ok_or_else(|| {
                            Error::runtime("Icebug-disk node table has no physical columns.")
                        })?;
                    let reader = koko_loader::parquet::ParquetReader::open_projected_file(
                        path,
                        file.try_clone()?,
                        &validation_projection,
                    )?;
                    koko_loader::icebug::validate_version(reader.file_metadata(), path)?;
                    koko_loader::icebug::validate_schema(
                        &reader.file_metadata().schema,
                        &expected,
                        path,
                    )?;
                    validate_row_count(reader.file_metadata().num_rows, *num_rows, path)?;
                }
                PinnedIcebugSource::RelCsr {
                    indices_path,
                    indices_file,
                    indptr_path,
                    indptr_file,
                    target_column,
                    num_rows,
                    num_bound_nodes,
                } => {
                    let table = catalog.rel_table(table_id).ok_or_else(|| {
                        Error::runtime(
                            "Icebug-disk relationship table is missing from the catalog.",
                        )
                    })?;
                    let (_, to_table) = table.pairs.first().copied().ok_or_else(|| {
                        Error::runtime("Icebug-disk relationship has no endpoint pair.")
                    })?;
                    validate_csr_source(
                        table,
                        indices_path,
                        indices_file,
                        indptr_path,
                        indptr_file,
                        target_column,
                        *num_rows,
                        *num_bound_nodes,
                        external_node_count(catalog, to_table)?,
                        control,
                        memory,
                    )?;
                }
                PinnedIcebugSource::RelFlat {
                    path,
                    file,
                    source_column,
                    target_column,
                    num_rows,
                } => {
                    let table = catalog.rel_table(table_id).ok_or_else(|| {
                        Error::runtime(
                            "Icebug-disk relationship table is missing from the catalog.",
                        )
                    })?;
                    let (from_table, to_table) = table.pairs.first().copied().ok_or_else(|| {
                        Error::runtime("Icebug-disk relationship has no endpoint pair.")
                    })?;
                    validate_flat_source(
                        table,
                        path,
                        file,
                        source_column,
                        target_column,
                        *num_rows,
                        external_node_count(catalog, from_table)?,
                        external_node_count(catalog, to_table)?,
                        control,
                        memory,
                    )?;
                }
            }
        }
        Ok(())
    }
}

fn open_source(path: &Path) -> Result<File> {
    File::open(path)
        .map_err(|error| Error::runtime(format!("Cannot open {}: {error}", path.display())))
}

fn external_node_count(catalog: &Catalog, table: TableId) -> Result<u64> {
    catalog
        .icebug_table(table)
        .and_then(|entry| entry.source.as_ref())
        .map(IcebugTableSource::num_rows)
        .ok_or_else(|| {
            Error::runtime("Icebug-disk relationship endpoint has no pinned local node descriptor.")
        })
}

fn validate_row_count(actual: u64, expected: u64, path: &Path) -> Result<()> {
    if actual != expected {
        return Err(Error::runtime(format!(
            "Icebug-disk file {} row count changed after table creation.",
            path.display()
        )));
    }
    Ok(())
}

fn parquet_offset(value: Value, path: &Path) -> Result<u64> {
    let value = value.as_u128().ok_or_else(|| {
        Error::runtime(format!(
            "Icebug-disk endpoint offset in {} is not an unsigned integer.",
            path.display()
        ))
    })?;
    u64::try_from(value).map_err(|_| {
        Error::runtime(format!(
            "Icebug-disk endpoint offset in {} exceeds UINT64.",
            path.display()
        ))
    })
}

fn validate_rel_properties(
    table: &RelTable,
    fields: &[koko_loader::parquet::ParquetField],
    path: &Path,
) -> Result<()> {
    let expected: Vec<_> = table
        .columns
        .iter()
        .map(|column| (column.name.clone(), column.ty.clone()))
        .collect();
    koko_loader::icebug::validate_schema(
        &koko_loader::parquet::ParquetSchema::new(fields.to_vec())?,
        &expected,
        path,
    )
}

#[allow(clippy::too_many_arguments)]
fn validate_csr_source(
    table: &RelTable,
    indices_path: &Path,
    indices_file: &File,
    indptr_path: &Path,
    indptr_file: &File,
    target_column: &str,
    num_rows: u64,
    num_bound_nodes: u64,
    target_count: u64,
    control: QueryControl<'_>,
    memory: &QueryMemory,
) -> Result<()> {
    let projection = [target_column.to_string()];
    let mut indices = koko_loader::parquet::ParquetReader::open_projected_file(
        indices_path,
        indices_file.try_clone()?,
        &projection,
    )?;
    koko_loader::icebug::validate_version(indices.file_metadata(), indices_path)?;
    validate_row_count(indices.file_metadata().num_rows, num_rows, indices_path)?;
    let fields = &indices.file_metadata().schema.fields;
    if fields.len() != table.columns.len() + 1
        || !fields[0].name.eq_ignore_ascii_case(target_column)
        || !matches!(fields[0].logical_type, LogicalType::Int(_))
    {
        return Err(Error::runtime(format!(
            "Icebug-disk indices file {} has an invalid endpoint column.",
            indices_path.display()
        )));
    }
    validate_rel_properties(table, &fields[1..], indices_path)?;
    let mut seen = 0u64;
    while let Some(chunk) = indices.next_chunk()? {
        control.check()?;
        let _batch_memory = memory.temporary_reservation(chunk.allocated_bytes())?;
        for position in chunk.sel.iter() {
            if parquet_offset(chunk.columns[0].get_value(position), indices_path)? >= target_count {
                return Err(Error::runtime(format!(
                    "Icebug-disk relationship endpoint in {} is out of range.",
                    indices_path.display()
                )));
            }
            seen += 1;
        }
    }
    if seen != num_rows {
        return Err(Error::runtime(format!(
            "Icebug-disk indices file {} ended before its declared row count.",
            indices_path.display()
        )));
    }

    let mut indptr = koko_loader::parquet::ParquetReader::open_projected_file(
        indptr_path,
        indptr_file.try_clone()?,
        &[],
    )?;
    koko_loader::icebug::validate_version(indptr.file_metadata(), indptr_path)?;
    validate_row_count(
        indptr.file_metadata().num_rows,
        num_bound_nodes.saturating_add(1),
        indptr_path,
    )?;
    if indptr.file_metadata().schema.fields.len() != 1
        || !matches!(
            indptr.file_metadata().schema.fields[0].logical_type,
            LogicalType::Int(_)
        )
    {
        return Err(Error::runtime(format!(
            "Icebug-disk indptr file {} must contain one integer column.",
            indptr_path.display()
        )));
    }
    let mut seen = 0u64;
    let mut previous = 0u64;
    while let Some(chunk) = indptr.next_chunk()? {
        control.check()?;
        let _batch_memory = memory.temporary_reservation(chunk.allocated_bytes())?;
        for position in chunk.sel.iter() {
            let offset = parquet_offset(chunk.columns[0].get_value(position), indptr_path)?;
            if (seen == 0 && offset != 0) || (seen > 0 && offset < previous) {
                return Err(Error::runtime(format!(
                    "Icebug-disk indptr file {} is not monotone from zero.",
                    indptr_path.display()
                )));
            }
            previous = offset;
            seen += 1;
        }
    }
    if seen != num_bound_nodes.saturating_add(1) || previous != num_rows {
        return Err(Error::runtime(format!(
            "Icebug-disk CSR files for {} disagree on relationship count.",
            table.name
        )));
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn validate_flat_source(
    table: &RelTable,
    path: &Path,
    file: &File,
    source_column: &str,
    target_column: &str,
    num_rows: u64,
    source_count: u64,
    target_count: u64,
    control: QueryControl<'_>,
    memory: &QueryMemory,
) -> Result<()> {
    let projection = [source_column.to_string(), target_column.to_string()];
    let mut reader = koko_loader::parquet::ParquetReader::open_projected_file(
        path,
        file.try_clone()?,
        &projection,
    )?;
    koko_loader::icebug::validate_version(reader.file_metadata(), path)?;
    validate_row_count(reader.file_metadata().num_rows, num_rows, path)?;
    let fields = &reader.file_metadata().schema.fields;
    if fields.len() != table.columns.len() + 2
        || !fields[0].name.eq_ignore_ascii_case(source_column)
        || !fields[1].name.eq_ignore_ascii_case(target_column)
        || !matches!(fields[0].logical_type, LogicalType::Int(_))
        || !matches!(fields[1].logical_type, LogicalType::Int(_))
    {
        return Err(Error::runtime(format!(
            "Icebug-disk flat relationship file {} has invalid endpoint columns.",
            path.display()
        )));
    }
    validate_rel_properties(table, &fields[2..], path)?;
    let mut seen = 0u64;
    while let Some(chunk) = reader.next_chunk()? {
        control.check()?;
        let _batch_memory = memory.temporary_reservation(chunk.allocated_bytes())?;
        for position in chunk.sel.iter() {
            if parquet_offset(chunk.columns[0].get_value(position), path)? >= source_count
                || parquet_offset(chunk.columns[1].get_value(position), path)? >= target_count
            {
                return Err(Error::runtime(format!(
                    "Icebug-disk relationship endpoint in {} is out of range.",
                    path.display()
                )));
            }
            seen += 1;
        }
    }
    if seen != num_rows {
        return Err(Error::runtime(format!(
            "Icebug-disk relationship file {} ended before its declared row count.",
            path.display()
        )));
    }
    Ok(())
}

fn csr_bounds(
    path: &Path,
    file: &File,
    source_offset: u64,
    control: QueryControl<'_>,
    memory: &QueryMemory,
) -> Result<(u64, u64)> {
    let mut reader = koko_loader::parquet::ParquetReader::open_projected_file_range(
        path,
        file.try_clone()?,
        &[],
        source_offset,
        2,
    )?;
    let mut bounds = [0u64; 2];
    let mut count = 0usize;
    while let Some(chunk) = reader.next_chunk()? {
        control.check()?;
        let _batch_memory = memory.temporary_reservation(chunk.allocated_bytes())?;
        for position in chunk.sel.iter() {
            if count >= bounds.len() {
                return Err(Error::runtime(
                    "Icebug-disk CSR indptr range returned too many offsets.",
                ));
            }
            bounds[count] = parquet_offset(chunk.columns[0].get_value(position), path)?;
            count += 1;
        }
    }
    if count != 2 {
        return Err(Error::runtime(
            "Icebug-disk CSR indptr range ended before two offsets.",
        ));
    }
    Ok((bounds[0], bounds[1]))
}

#[allow(clippy::too_many_arguments)]
fn scan_csr_edges(
    path: &Path,
    file: &File,
    target_column: &str,
    start: u64,
    end: u64,
    mut visit: impl FnMut(u64, u64),
    control: QueryControl<'_>,
    memory: &QueryMemory,
) -> Result<()> {
    let projection = [target_column.to_string()];
    let mut reader = koko_loader::parquet::ParquetReader::open_projected_file_range(
        path,
        file.try_clone()?,
        &projection,
        start,
        end.saturating_sub(start),
    )?;
    let mut physical = start;
    while let Some(chunk) = reader.next_chunk()? {
        control.check()?;
        let _batch_memory = memory.temporary_reservation(chunk.allocated_bytes())?;
        for position in chunk.sel.iter() {
            visit(
                physical,
                parquet_offset(chunk.columns[0].get_value(position), path)?,
            );
            physical += 1;
        }
    }
    if physical != end {
        return Err(Error::runtime(
            "Icebug-disk relationship range ended before its declared row count.",
        ));
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn external_edges_for_node(
    source: &PinnedIcebugSource,
    bound_offset: u64,
    forward: bool,
    input_pos: usize,
    rel_table: TableId,
    from_table: TableId,
    to_table: TableId,
    out: &mut Vec<BatchNeighbor>,
    control: QueryControl<'_>,
    memory: &QueryMemory,
) -> Result<()> {
    match source {
        PinnedIcebugSource::Node { .. } => Err(Error::runtime(
            "Icebug-disk source kind does not match its relationship catalog entry.",
        )),
        PinnedIcebugSource::RelCsr {
            indices_path,
            indices_file,
            indptr_path,
            indptr_file,
            target_column,
            num_bound_nodes,
            ..
        } => {
            if forward {
                let (start, end) =
                    csr_bounds(indptr_path, indptr_file, bound_offset, control, memory)?;
                scan_csr_edges(
                    indices_path,
                    indices_file,
                    target_column,
                    start,
                    end,
                    |physical, target| {
                        out.push(BatchNeighbor {
                            input_pos,
                            nbr: InternalId::new(to_table, target),
                            rel: InternalId::new(rel_table, physical),
                        });
                    },
                    control,
                    memory,
                )?;
            } else {
                for source_offset in 0..*num_bound_nodes {
                    control.check()?;
                    let (start, end) =
                        csr_bounds(indptr_path, indptr_file, source_offset, control, memory)?;
                    scan_csr_edges(
                        indices_path,
                        indices_file,
                        target_column,
                        start,
                        end,
                        |physical, target| {
                            if target == bound_offset {
                                out.push(BatchNeighbor {
                                    input_pos,
                                    nbr: InternalId::new(from_table, source_offset),
                                    rel: InternalId::new(rel_table, physical),
                                });
                            }
                        },
                        control,
                        memory,
                    )?;
                }
            }
            Ok(())
        }
        PinnedIcebugSource::RelFlat {
            path,
            file,
            source_column,
            target_column,
            num_rows,
        } => {
            let projection = [source_column.clone(), target_column.clone()];
            let mut reader = koko_loader::parquet::ParquetReader::open_projected_file(
                path,
                file.try_clone()?,
                &projection,
            )?;
            let mut physical = 0u64;
            while let Some(chunk) = reader.next_chunk()? {
                control.check()?;
                let _batch_memory = memory.temporary_reservation(chunk.allocated_bytes())?;
                for position in chunk.sel.iter() {
                    let src = parquet_offset(chunk.columns[0].get_value(position), path)?;
                    let dst = parquet_offset(chunk.columns[1].get_value(position), path)?;
                    let matches = if forward {
                        src == bound_offset
                    } else {
                        dst == bound_offset
                    };
                    if matches {
                        out.push(BatchNeighbor {
                            input_pos,
                            nbr: if forward {
                                InternalId::new(to_table, dst)
                            } else {
                                InternalId::new(from_table, src)
                            },
                            rel: InternalId::new(rel_table, physical),
                        });
                    }
                    physical += 1;
                }
            }
            if physical != *num_rows {
                return Err(Error::runtime(
                    "Icebug-disk relationship file ended before its declared row count.",
                ));
            }
            Ok(())
        }
    }
}

struct ExternalNodeReader {
    table: TableId,
    next_offset: u64,
    end: u64,
    property_count: usize,
    reader: koko_loader::parquet::ParquetReader,
}

impl ExternalNodeReader {
    fn next_chunk(
        &mut self,
        scan: &ScanNode,
        scan_table: &ScanTable,
        ctx: &Ctx<'_>,
    ) -> Result<Option<DataChunk>> {
        let Some(batch) = self.reader.next_chunk()? else {
            if self.next_offset != self.end {
                return Err(Error::runtime(
                    "Icebug-disk node file ended before its declared row count.",
                ));
            }
            return Ok(None);
        };
        ctx.execution.control.check()?;
        let _batch_memory = ctx
            .execution
            .memory
            .temporary_reservation(batch.allocated_bytes())?;
        let size = batch.size();
        let size_u64 = u64::try_from(size).map_err(|_| Error::buffer_manager())?;
        if self.next_offset.saturating_add(size_u64) > self.end {
            return Err(Error::runtime(
                "Icebug-disk node file exceeds its declared row count.",
            ));
        }
        let mut output = DataChunk::new(&ctx.layout.col_types);
        for position in 0..size {
            output.columns[scan.id_col].set_value(
                position,
                &Value::InternalId(InternalId::new(
                    self.table,
                    self.next_offset + position as u64,
                )),
            );
        }
        self.next_offset += size_u64;
        for (property, source) in scan_table
            .prop_cols
            .iter()
            .zip(batch.columns.into_iter().take(self.property_count))
        {
            let target_type = &ctx.layout.col_types[property.col_index];
            if &source.logical_type == target_type {
                output.columns[property.col_index] = source;
            } else {
                for row in 0..size {
                    output.columns[property.col_index]
                        .set_value_owned(row, promote_prop(source.get_value(row), target_type));
                }
            }
        }
        output.set_flat(size);
        Ok(Some(output))
    }
}

/// Immutable statement execution context supplied by the connection layer.
/// It carries every non-catalog input that can affect query behavior.
#[derive(Clone, Copy)]
pub struct ExecutionContext<'a> {
    pub table_functions: &'a dyn TableFuncRuntime,
    pub random: &'a RandomState,
    pub worker_count: usize,
    pub warnings: &'a koko_common::warnings::WarningSink,
    pub storage_read: StorageReadHandle,
    pub storage_write: Option<StorageWriteHandle>,
    pub control: QueryControl<'a>,
    pub memory: &'a QueryMemory,
    pub sources: &'a QuerySourceState,
}

/// The materialized typed result of a query.
#[derive(Debug, Clone, Default)]
pub struct ExecResult {
    pub column_names: Vec<String>,
    pub column_types: Vec<LogicalType>,
    pub batches: Vec<DataChunk>,
}

impl ExecResult {
    pub fn num_rows(&self) -> usize {
        self.batches.iter().map(DataChunk::size).sum()
    }

    pub fn into_rows(self) -> Vec<Vec<Value>> {
        let mut rows = Vec::with_capacity(self.num_rows());
        for batch in self.batches {
            for position in batch.sel.iter() {
                rows.push(
                    batch
                        .columns
                        .iter()
                        .map(|column| column.get_value(position))
                        .collect(),
                );
            }
        }
        rows
    }

    pub fn map_values_mut(&mut self, mut map: impl FnMut(&mut Value)) {
        for batch in &mut self.batches {
            let positions: Vec<_> = batch.sel.iter().collect();
            for position in positions {
                for column in &mut batch.columns {
                    let mut value = column.get_value(position);
                    map(&mut value);
                    column.set_value_owned(position, value);
                }
            }
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct OutputPosition {
    batch: usize,
    physical: usize,
    ordinal: usize,
}

/// Columnar final-result builder. Projection emits directly into typed chunks;
/// only ORDER BY keys and DISTINCT hashes use row-shaped temporary metadata.
struct OutputBuffer {
    column_types: Vec<LogicalType>,
    batches: Vec<DataChunk>,
    order_keys: Option<Vec<Vec<Value>>>,
    num_rows: usize,
}

impl OutputBuffer {
    fn new(column_types: Vec<LogicalType>, has_order: bool) -> Self {
        Self {
            column_types,
            batches: Vec::new(),
            order_keys: has_order.then(Vec::new),
            num_rows: 0,
        }
    }

    fn from_exec(result: ExecResult) -> Self {
        let num_rows = result.num_rows();
        Self {
            column_types: result.column_types,
            batches: result.batches,
            order_keys: None,
            num_rows,
        }
    }

    fn len(&self) -> usize {
        self.num_rows
    }

    /// Append one projected row and return newly retained heap bytes.
    fn push(&mut self, values: Vec<Value>, order_keys: Vec<Value>) -> u64 {
        debug_assert_eq!(values.len(), self.column_types.len());
        let mut allocated = values.iter().map(value_payload_bytes).sum::<u64>();
        if self
            .batches
            .last()
            .is_none_or(|batch| batch.size() == VECTOR_CAPACITY)
        {
            let batch = DataChunk::new(&self.column_types);
            allocated = allocated.saturating_add(batch.allocated_bytes());
            self.batches.push(batch);
        }
        let batch = self.batches.last_mut().expect("created above");
        let position = batch.size();
        for (column, value) in batch.columns.iter_mut().zip(values) {
            column.set_value_owned(position, value);
        }
        batch.set_flat(position + 1);
        if let Some(keys) = &mut self.order_keys {
            allocated = allocated
                .saturating_add(std::mem::size_of::<Vec<Value>>() as u64)
                .saturating_add((order_keys.capacity() * std::mem::size_of::<Value>()) as u64)
                .saturating_add(order_keys.iter().map(value_payload_bytes).sum::<u64>());
            keys.push(order_keys);
        } else {
            debug_assert!(order_keys.is_empty());
        }
        self.num_rows += 1;
        allocated
    }

    fn append(&mut self, mut other: Self, memory: &QueryMemory) -> Result<()> {
        if self.column_types != other.column_types {
            // UNION may bind a NULL-only operand as `ANY` while the result type
            // comes from another operand. Repack that operand into the result's
            // exact vectors without constructing a complete row result.
            let positions = other.positions();
            let mut normalized = Self::new(self.column_types.clone(), other.order_keys.is_some());
            for position in positions {
                let values = (0..other.column_types.len())
                    .map(|column| other.value_at(position, column))
                    .collect();
                let order_keys = other
                    .order_keys
                    .as_ref()
                    .map_or_else(Vec::new, |keys| keys[position.ordinal].clone());
                memory.charge(normalized.push(values, order_keys))?;
            }
            other = normalized;
        }
        self.batches.append(&mut other.batches);
        match (&mut self.order_keys, other.order_keys) {
            (Some(keys), Some(mut other_keys)) => keys.append(&mut other_keys),
            (None, None) => {}
            _ => unreachable!("all projection blocks use the same ORDER BY shape"),
        }
        self.num_rows += other.num_rows;
        Ok(())
    }

    fn positions(&self) -> Vec<OutputPosition> {
        let mut positions = Vec::with_capacity(self.num_rows);
        let mut ordinal = 0;
        for (batch_index, batch) in self.batches.iter().enumerate() {
            for physical in batch.sel.iter() {
                positions.push(OutputPosition {
                    batch: batch_index,
                    physical,
                    ordinal,
                });
                ordinal += 1;
            }
        }
        positions
    }

    fn value_at(&self, position: OutputPosition, column: usize) -> Value {
        self.batches[position.batch].columns[column].get_value(position.physical)
    }

    fn compact(&self, positions: &[OutputPosition]) -> Vec<DataChunk> {
        let mut batches = Vec::with_capacity(positions.len().div_ceil(VECTOR_CAPACITY));
        for selected in positions.chunks(VECTOR_CAPACITY) {
            let mut batch = DataChunk::new(&self.column_types);
            for (column_index, output) in batch.columns.iter_mut().enumerate() {
                for (output_position, source) in selected.iter().copied().enumerate() {
                    output.set_value_owned(output_position, self.value_at(source, column_index));
                }
            }
            batch.set_flat(selected.len());
            batches.push(batch);
        }
        batches
    }

    fn finish(
        self,
        column_names: Vec<String>,
        distinct: bool,
        order_ascending: &[bool],
        skip: usize,
        limit: Option<usize>,
        memory: &QueryMemory,
    ) -> Result<ExecResult> {
        let position_bytes = (self.num_rows * std::mem::size_of::<OutputPosition>()) as u64;
        let distinct_bytes = if distinct {
            (self.num_rows
                * (std::mem::size_of::<Vec<ValueKey>>()
                    + self.column_types.len() * std::mem::size_of::<ValueKey>()
                    + 2 * std::mem::size_of::<usize>())) as u64
        } else {
            0
        };
        memory.charge(position_bytes.saturating_add(distinct_bytes))?;
        let mut positions = self.positions();
        if distinct {
            let mut seen = HashSet::new();
            positions.retain(|position| {
                let key = (0..self.column_types.len())
                    .map(|column| ValueKey::from_value(&self.value_at(*position, column)))
                    .collect::<Vec<_>>();
                seen.insert(key)
            });
        }
        if !order_ascending.is_empty() {
            let order_keys = self
                .order_keys
                .as_ref()
                .expect("ORDER BY projection records sort keys");
            positions.sort_by(|left, right| {
                for (index, ascending) in order_ascending.iter().copied().enumerate() {
                    let ordering = order_cmp(
                        &order_keys[left.ordinal][index],
                        &order_keys[right.ordinal][index],
                    );
                    let ordering = if ascending {
                        ordering
                    } else {
                        ordering.reverse()
                    };
                    if ordering != Ordering::Equal {
                        return ordering;
                    }
                }
                Ordering::Equal
            });
        }
        let positions = positions
            .into_iter()
            .skip(skip)
            .take(limit.unwrap_or(usize::MAX))
            .collect::<Vec<_>>();
        let identity = positions.len() == self.num_rows
            && positions
                .iter()
                .enumerate()
                .all(|(ordinal, position)| position.ordinal == ordinal);
        let batches = if identity {
            self.batches
        } else {
            let batches = self.compact(&positions);
            memory.charge(batches.iter().map(DataChunk::allocated_bytes).sum())?;
            batches
        };
        Ok(ExecResult {
            column_names,
            column_types: self.column_types,
            batches,
        })
    }
}

/// Execute a `UNION`/`UNION ALL` query by concatenating typed operand batches.
/// Plain `UNION` deduplicates through columnar row references without creating a
/// second complete row representation.
pub fn execute_regular(
    rq: &BoundRegularQuery,
    plan: &RegularPlan,
    catalog: &Catalog,
    storage: &mut InMemStorage,
    execution: &ExecutionContext<'_>,
) -> Result<ExecResult> {
    let mut operands = rq.operands.iter().zip(&plan.operands);
    let (first_query, first_plan) = operands.next().expect("a query has at least one operand");
    let first = execute(first_query, first_plan, catalog, storage, execution)?;
    let column_names = first.column_names.clone();
    let mut output = OutputBuffer::from_exec(first);
    for (query, operand_plan) in operands {
        output.append(
            OutputBuffer::from_exec(execute(query, operand_plan, catalog, storage, execution)?),
            execution.memory,
        )?;
    }
    output.finish(column_names, plan.distinct, &[], 0, None, execution.memory)
}

/// Execute against shared storage with one lock per pipeline phase.
///
/// Read-only parts take shared guards, so independent statements overlap. A
/// write part drains under a shared guard, releases it, mutates under an
/// exclusive guard, then reacquires a shared guard for result materialization.
pub fn execute_regular_synchronized(
    rq: &BoundRegularQuery,
    plan: &RegularPlan,
    catalog: &Catalog,
    storage: &SharedStorage,
    execution: &ExecutionContext<'_>,
) -> Result<ExecResult> {
    let mut operands = rq.operands.iter().zip(&plan.operands);
    let (first_query, first_plan) = operands.next().expect("a query has at least one operand");
    let first = execute_synchronized(first_query, first_plan, catalog, storage, execution)?;
    let column_names = first.column_names.clone();
    let mut output = OutputBuffer::from_exec(first);
    for (query, operand_plan) in operands {
        output.append(
            OutputBuffer::from_exec(execute_synchronized(
                query,
                operand_plan,
                catalog,
                storage,
                execution,
            )?),
            execution.memory,
        )?;
    }
    output.finish(column_names, plan.distinct, &[], 0, None, execution.memory)
}

fn execute_synchronized(
    query: &BoundQuery,
    plan: &QueryPlan,
    catalog: &Catalog,
    storage: &SharedStorage,
    execution: &ExecutionContext<'_>,
) -> Result<ExecResult> {
    debug_assert_eq!(query.parts.len(), plan.parts.len());
    let mut input = Vec::new();
    for (index, (part, part_plan)) in query.parts.iter().zip(&plan.parts).enumerate() {
        // A prior query part or UNION operand may have mutated storage. Visibility
        // decisions are stable only while this part holds its storage guard.
        execution.memory.clear_rel_visibility();
        let is_last = index + 1 == query.parts.len();
        let layout = &part_plan.layout;
        if part_plan.update_ops.is_empty() {
            let guard = storage.read();
            let context = Ctx::new(catalog, &guard, layout, execution);
            if is_last {
                return match &part.projection {
                    Some(projection) => read_part_results(projection, part_plan, &context, &input),
                    None => Ok(ExecResult::default()),
                };
            }
            let projection = part
                .projection
                .as_ref()
                .expect("a non-terminal part must have a WITH projection");
            let result = read_part_results(projection, part_plan, &context, &input)?;
            input = materialize_carried(&result, &plan.parts[index + 1]);
            continue;
        }

        let mut chunks = {
            let guard = storage.read();
            let context = Ctx::new(catalog, &guard, layout, execution);
            let mut root = build_exec(&part_plan.root, &context, &input)?;
            drain_all(&mut root, &context)?
        };
        {
            let mut guard = storage.write();
            for update in &part_plan.update_ops {
                chunks = run_update(update, layout, chunks, catalog, &mut guard, execution)?;
            }
        }
        let guard = storage.read();
        let context = Ctx::new(catalog, &guard, layout, execution);
        let mut root = Exec::Buffered { chunks, idx: 0 };
        if is_last {
            return match &part.projection {
                Some(projection) => produce_results(projection, &mut root, &context),
                None => Ok(ExecResult::default()),
            };
        }
        let projection = part
            .projection
            .as_ref()
            .expect("a non-terminal part must have a WITH projection");
        let result = produce_results(projection, &mut root, &context)?;
        input = materialize_carried(&result, &plan.parts[index + 1]);
    }
    unreachable!("terminal part returns")
}

/// Execute a bound query against the catalog and storage.
///
/// Parts run in sequence: each part's reading pipeline is seeded with the
/// previous part's projected rows (its carried scalar scope), executed, then
/// projected — for a `WITH` part the projection feeds the next part; for the
/// terminal part it is the result (or a `CREATE` runs).
pub fn execute(
    query: &BoundQuery,
    plan: &QueryPlan,
    catalog: &Catalog,
    storage: &mut InMemStorage,
    execution: &ExecutionContext<'_>,
) -> Result<ExecResult> {
    debug_assert_eq!(query.parts.len(), plan.parts.len());
    // Rows carried in from the previous part, materialized into the current
    // part's input layout (empty for the first part).
    let mut input: Vec<DataChunk> = Vec::new();

    for (idx, (part, part_plan)) in query.parts.iter().zip(&plan.parts).enumerate() {
        execution.memory.clear_rel_visibility();
        let is_last = idx + 1 == query.parts.len();
        let layout = &part_plan.layout;

        // A read part (no updating clauses) is produced straight from the plan —
        // morsel-parallel when the gate passes, else the serial streamed pull (so a
        // terminal `LIMIT`/`EXISTS` can terminate early and intermediates never fully
        // materialize). A write part is a pipeline breaker handled separately below.
        // Building/pulling the `Exec` tree borrows storage only through the `ctx`
        // threaded into `next_chunk`, so a read holds no lasting `&mut` borrow.
        if part_plan.update_ops.is_empty() {
            let ctx = Ctx::new(catalog, &*storage, layout, execution);
            if is_last {
                return match &part.projection {
                    Some(projection) => read_part_results(projection, part_plan, &ctx, &input),
                    // A terminal read part with no projection is empty (`---- ok`).
                    None => Ok(ExecResult::default()),
                };
            }
            // A `WITH` part: project, then carry forward into the next part's layout.
            let projection = part
                .projection
                .as_ref()
                .expect("a non-terminal part must have a WITH projection");
            let result = read_part_results(projection, part_plan, &ctx, &input)?;
            input = materialize_carried(&result, &plan.parts[idx + 1]);
            continue;
        }

        // Writes are a pipeline breaker: drain the read pipeline (releasing the
        // immutable borrow), apply the updating clauses, then produce from the
        // post-mutation chunks (writes never parallelize). `SET` also updates the
        // live chunk so a following projection reflects the new values, a `DELETE`d
        // row stays so `DELETE … RETURN` sees its values, and `MERGE` replaces the
        // rows with its matched/created bindings.
        let mut chunks = {
            let ctx = Ctx::new(catalog, &*storage, layout, execution);
            let mut root = build_exec(&part_plan.root, &ctx, &input)?;
            drain_all(&mut root, &ctx)?
        };
        for op in &part_plan.update_ops {
            chunks = run_update(op, layout, chunks, catalog, storage, execution)?;
        }
        let mut root = Exec::Buffered { chunks, idx: 0 };

        // Reads are done; this immutable borrow only feeds result production.
        let ctx = Ctx::new(catalog, &*storage, layout, execution);

        if is_last {
            return match &part.projection {
                Some(projection) => produce_results(projection, &mut root, &ctx),
                // A write-only terminal part returns an empty (`---- ok`) result.
                None => Ok(ExecResult::default()),
            };
        }

        // A `WITH` write part: project, then carry the projected rows forward.
        let projection = part
            .projection
            .as_ref()
            .expect("a non-terminal part must have a WITH projection");
        let result = produce_results(projection, &mut root, &ctx)?;
        input = materialize_carried(&result, &plan.parts[idx + 1]);
    }

    // A query always has at least one part, the terminal one, which returns above.
    unreachable!("terminal part returns")
}

/// Build the next part's input chunks from a `WITH` part's projected rows. Each
/// projected value is unpacked per its [`InputSlot`]: a scalar into one column, a
/// carried node exploded into its id + property columns. The rest of the row is
/// left NULL (filled by the next part's scans/extends).
fn materialize_carried(result: &ExecResult, next: &PartPlan) -> Vec<DataChunk> {
    let mut builder = ChunkBuilder::new(&next.layout.col_types);
    let width = next.layout.width();
    for batch in &result.batches {
        for position in batch.sel.iter() {
            let mut full = vec![Value::Null; width];
            for (index, slot) in next.inputs.iter().enumerate() {
                let value = batch.columns[index].get_value(position);
                match slot {
                    InputSlot::Scalar { col } => full[*col] = value,
                    InputSlot::Node {
                        id_col,
                        prop_tables,
                    } => explode_node(&value, *id_col, prop_tables, &mut full),
                }
            }
            builder.push_row(&full);
        }
    }
    builder.finish()
}

/// Unpack a carried `Value::Node` back into a binding: its internal id into
/// `id_col`, then each property through the [`ScanTable`] mapping for the node's
/// runtime table. A NULL leaves the binding columns NULL.
fn explode_node(v: &Value, id_col: usize, prop_tables: &[ScanTable], full: &mut [Value]) {
    if let Value::Node(node) = v {
        full[id_col] = Value::InternalId(node.id);
        if let Some(table) = prop_tables
            .iter()
            .find(|table| table.table == node.id.table_id)
        {
            for pc in &table.prop_cols {
                if let Some((_, val)) = node.props.get(pc.column_id as usize) {
                    full[pc.col_index] = val.clone();
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Plan execution → Vec<DataChunk>
// ---------------------------------------------------------------------------

/// Adapts a [`RowLayout`] to the `koko-expr` column resolver.
struct LayoutResolver<'a>(&'a RowLayout);

impl ColumnResolver for LayoutResolver<'_> {
    fn column(&self, var: VarId, prop: Option<&str>) -> Result<usize> {
        self.0
            .column(var, prop)
            .ok_or_else(|| Error::binder("internal: unresolved column reference".to_string()))
    }
    fn subquery_column(&self, id: usize) -> Result<usize> {
        self.0
            .subquery_column(id)
            .ok_or_else(|| Error::binder("internal: unresolved subquery reference".to_string()))
    }
    fn sequence_column(&self, id: usize) -> Result<usize> {
        self.0
            .sequence_column(id)
            .ok_or_else(|| Error::binder("internal: unresolved sequence reference".to_string()))
    }
    fn table_names(&self) -> HashMap<TableId, String> {
        self.0.table_names.clone()
    }
    fn value_column(&self, var: VarId) -> Option<usize> {
        self.0.try_var(var).and_then(|vc| vc.value_col)
    }
}

/// Accumulates full-width rows into [`DataChunk`]s of at most [`VECTOR_CAPACITY`].
struct ChunkBuilder<'a> {
    col_types: &'a [LogicalType],
    chunks: Vec<DataChunk>,
    current: DataChunk,
    len: usize,
}

impl<'a> ChunkBuilder<'a> {
    fn new(col_types: &'a [LogicalType]) -> Self {
        Self {
            col_types,
            chunks: Vec::new(),
            current: DataChunk::new(col_types),
            len: 0,
        }
    }

    fn push_row(&mut self, row: &[Value]) {
        for (c, v) in row.iter().enumerate() {
            self.current.columns[c].set_value(self.len, v);
        }
        self.len += 1;
        if self.len == VECTOR_CAPACITY {
            self.flush();
        }
    }

    fn flush(&mut self) {
        if self.len > 0 {
            self.current.set_flat(self.len);
            let full = std::mem::replace(&mut self.current, DataChunk::new(self.col_types));
            self.chunks.push(full);
            self.len = 0;
        }
    }

    fn finish(mut self) -> Vec<DataChunk> {
        self.flush();
        self.chunks
    }
}

// ---------------------------------------------------------------------------
// Streaming (pull) execution engine
// ---------------------------------------------------------------------------

/// The read-only context threaded through the pull pipeline.
#[derive(Clone, Copy)]
struct Ctx<'a> {
    catalog: &'a Catalog,
    storage: &'a InMemStorage,
    layout: &'a RowLayout,
    execution: &'a ExecutionContext<'a>,
}

impl<'a> Ctx<'a> {
    fn new(
        catalog: &'a Catalog,
        storage: &'a InMemStorage,
        layout: &'a RowLayout,
        execution: &'a ExecutionContext<'a>,
    ) -> Self {
        Self {
            catalog,
            storage,
            layout,
            execution,
        }
    }
}
impl Ctx<'_> {
    #[inline]
    fn read(self) -> StorageReadHandle {
        self.execution.storage_read
    }
}

/// Accumulates rows into **one** [`DataChunk`] of at most [`VECTOR_CAPACITY`], the
/// single-chunk currency of streaming operators (unlike [`ChunkBuilder`], which
/// eagerly builds *all* chunks for the breaker/write path). The operator owns the
/// resumable cursor; this just fills the next chunk to yield.
struct ChunkAccum {
    chunk: DataChunk,
    len: usize,
    /// Per-row factorization multiplicity, allocated lazily on the first non-unit
    /// value (back-filling prior rows with 1) — so the common all-1 path pays
    /// nothing. The factorizing extend (P3 step 6) is the only producer of a non-1.
    mult: Option<Vec<u64>>,
}

impl ChunkAccum {
    fn new(col_types: &[LogicalType]) -> Self {
        Self {
            chunk: DataChunk::new(col_types),
            len: 0,
            mult: None,
        }
    }

    fn push_row(&mut self, row: &[Value]) {
        self.push_row_with_mult(row, 1);
    }

    /// Append a row that stands for `m` logical tuples (its factorization
    /// multiplicity). `m == 1` is the ordinary path; a non-unit `m` materializes the
    /// multiplicity vector (once) so the chunk carries it.
    fn push_row_with_mult(&mut self, row: &[Value], m: u64) {
        for (c, v) in row.iter().enumerate() {
            self.chunk.columns[c].set_value(self.len, v);
        }
        self.finish_row_with_mult(m);
    }

    /// Append selected columns from one input row without materializing them as
    /// [`Value`]s. The planner supplies the columns that remain live above the
    /// operator; every other output column stays NULL.
    fn push_chunk_row_with_mult(
        &mut self,
        chunk: &DataChunk,
        pos: usize,
        columns: &[usize],
        m: u64,
    ) {
        let output_pos = self.len;
        for &column in columns {
            self.chunk.columns[column].copy_value_from(output_pos, &chunk.columns[column], pos);
        }
        self.finish_row_with_mult(m);
    }

    fn finish_row_with_mult(&mut self, m: u64) {
        if m != 1 {
            let len = self.len;
            self.mult.get_or_insert_with(|| vec![1u64; len]).push(m);
        } else if let Some(values) = &mut self.mult {
            values.push(1);
        }
        self.len += 1;
    }

    fn is_full(&self) -> bool {
        self.len >= VECTOR_CAPACITY
    }

    /// Finalize the accumulated rows into a flat chunk (attaching the multiplicity
    /// vector only when some row carried a non-unit multiplicity).
    fn into_chunk(mut self) -> DataChunk {
        self.chunk.set_flat(self.len);
        if let Some(m) = self.mult {
            self.chunk.mult = Some(m.into_boxed_slice());
        }
        self.chunk
    }

    /// The accumulated chunk, or `None` if no rows were pushed.
    fn take(self) -> Option<DataChunk> {
        if self.len == 0 {
            None
        } else {
            Some(self.into_chunk())
        }
    }
}

/// Per-operator state for the row-expansion streaming pattern (see
/// [`stream_expand`]): the current input chunk + position cursor, and buffered output.
#[derive(Default)]
struct ExpandState {
    cur: Option<DataChunk>,
    positions: Vec<usize>,
    pi: usize,
    pending: Vec<Vec<Value>>,
    pending_row_memory: Option<MemoryReservation>,
    pend_i: usize,
    /// Multiplicity parallel to `pending`; batched expanders can mix source rows.
    pending_mults: Vec<u64>,
    /// Columnar output waiting to be pulled by the parent extend operator.
    pending_chunks: VecDeque<DataChunk>,
    /// Tracks queued chunks plus the most recently yielded chunk until the parent pulls again.
    pending_chunk_memory: Option<MemoryReservation>,
    yielded_chunk_bytes: u64,
    /// Reusable batched-adjacency scratch.
    neighbors: Vec<BatchNeighbor>,
    tagged_neighbors: Vec<(usize, usize, BatchNeighbor)>,
    nodes: Vec<InternalId>,
    node_positions: Vec<usize>,
    /// Per-relationship-branch MVCC fast-path decision for this statement view.
    all_visible: Vec<Option<bool>>,
}

impl ExpandState {
    fn release_yielded_chunk(&mut self) {
        if self.yielded_chunk_bytes == 0 {
            return;
        }
        let reservation = self
            .pending_chunk_memory
            .as_mut()
            .expect("yielded extend chunk has a memory reservation");
        debug_assert!(self.yielded_chunk_bytes <= reservation.bytes());
        reservation
            .resize(reservation.bytes() - self.yielded_chunk_bytes)
            .expect("shrinking an extend-chunk reservation cannot fail");
        self.yielded_chunk_bytes = 0;
    }

    fn release_pending_rows(&mut self) {
        if let Some(reservation) = &mut self.pending_row_memory {
            reservation
                .resize(0)
                .expect("shrinking a pending-row reservation cannot fail");
        }
    }

    fn charge_pending_rows(&mut self, memory: &QueryMemory, bytes: u64) -> Result<()> {
        if bytes == 0 {
            return Ok(());
        }
        if self.pending_row_memory.is_none() {
            self.pending_row_memory = Some(memory.temporary_reservation(0)?);
        }
        let reservation = self
            .pending_row_memory
            .as_mut()
            .expect("pending expand rows have a memory reservation");
        let total = reservation
            .bytes()
            .checked_add(bytes)
            .ok_or_else(Error::buffer_manager)?;
        reservation.resize(total)
    }

    fn retain_yielded_chunk(&mut self, memory: &QueryMemory, chunk: &DataChunk) -> Result<()> {
        debug_assert_eq!(self.yielded_chunk_bytes, 0);
        let bytes = chunk.allocated_bytes();
        if self.pending_chunk_memory.is_none() {
            self.pending_chunk_memory = Some(memory.temporary_reservation(0)?);
        }
        let reservation = self
            .pending_chunk_memory
            .as_mut()
            .expect("yielded expand chunk has a memory reservation");
        let total = reservation
            .bytes()
            .checked_add(bytes)
            .ok_or_else(Error::buffer_manager)?;
        reservation.resize(total)?;
        self.yielded_chunk_bytes = bytes;
        Ok(())
    }

    fn push_pending_chunk(&mut self, memory: &QueryMemory, chunk: DataChunk) -> Result<()> {
        let bytes = chunk.allocated_bytes();
        if self.pending_chunk_memory.is_none() {
            self.pending_chunk_memory = Some(memory.temporary_reservation(0)?);
        }
        let reservation = self
            .pending_chunk_memory
            .as_mut()
            .expect("pending extend chunk has a memory reservation");
        let total = reservation
            .bytes()
            .checked_add(bytes)
            .ok_or_else(Error::buffer_manager)?;
        reservation.resize(total)?;
        self.pending_chunks.push_back(chunk);
        Ok(())
    }

    fn take_pending_chunk(&mut self) -> Option<DataChunk> {
        let chunk = self.pending_chunks.pop_front()?;
        self.yielded_chunk_bytes = chunk.allocated_bytes();
        Some(chunk)
    }
}

enum ColumnarLoadReader {
    Parquet(koko_loader::parquet::ParquetReader),
    Npy(koko_loader::npy::NpyBatchReader),
}

impl Iterator for ColumnarLoadReader {
    type Item = Result<DataChunk>;

    fn next(&mut self) -> Option<Self::Item> {
        match self {
            Self::Parquet(reader) => reader.next(),
            Self::Npy(reader) => reader.next(),
        }
    }
}

/// A pull operator: each [`PlanOp`] compiles (via [`build_exec`]) into one of
/// these, and `next_chunk` readies the next ≤[`VECTOR_CAPACITY`]-row chunk or
/// `None` at end of stream. Correlated operators store their sub-pattern as a
/// `&PlanOp` and (re)build a seeded sub-pipeline per input row.
enum Exec<'a> {
    /// One empty row, then exhausted (the source for match-free queries).
    SingleRow { done: bool },
    /// Replay the carried input chunks (the previous part's projected rows, or a
    /// correlated operator's per-row seed), one at a time.
    InputScan { chunks: &'a [DataChunk], idx: usize },
    /// Drain already-materialized chunks (the post-mutation rows of a write part).
    Buffered { chunks: Vec<DataChunk>, idx: usize },
    /// Scan node tables: a resumable `(table_idx, offset)` cursor yields at most one
    /// [`VECTOR_CAPACITY`]-sized chunk of live nodes per call, advancing across tables.
    /// `end` bounds the current table's offsets (`u64::MAX` = its full count);
    /// `single_table` stops after `table_idx` instead of advancing; together they
    /// realize one parallel scan morsel: a table's `[offset, end)` slice.
    ScanNode {
        scan: &'a ScanNode,
        table_idx: usize,
        offset: u64,
        end: u64,
        single_table: bool,
        projected_columns: Vec<Vec<usize>>,
        external_reader: Option<ExternalNodeReader>,
    },
    /// A constant primary-key point lookup: probe the in-memory PK index once and
    /// yield the matching node's row (or nothing).
    IndexScan { scan: &'a IndexScan, done: bool },
    /// A correlated primary-key point lookup: for each input row, evaluate
    /// `scan.pk_value`, probe the PK index, and preserve the input row on a hit.
    IndexLookup {
        input: Box<Exec<'a>>,
        scan: &'a IndexScan,
        key: CompiledExpr,
        st: ExpandState,
    },
    /// A catalog table-function leaf source: its rows are computed once (lazily)
    /// then yielded in chunks.
    ScanTableFunc {
        func: BoundTableFunc,
        arg: Option<&'a str>,
        cols: &'a [usize],
        rows: Option<Vec<Vec<Value>>>,
        idx: usize,
    },
    /// A CSV `LOAD FROM` leaf source: streams the file in chunks. The reader is
    /// opened lazily on the first pull (then `header`/`skip` consumed); cell `i` is
    /// parsed against `ctx.layout.col_types[cols[i]]`.
    LoadScan {
        paths: &'a [String],
        file_idx: usize,
        cols: &'a [usize],
        col_names: &'a [String],
        options: &'a CsvLoadOptions,
        format: FileFormat,
        bare: bool,
        reader: Option<csv::Reader<std::fs::File>>,
        dialect: Option<koko_common::csv_dialect::Dialect>,
        columnar_reader: Option<ColumnarLoadReader>,
        done: bool,
    },
    Filter {
        input: Box<Exec<'a>>,
        predicate: CompiledExpr,
    },
    Extend {
        input: Box<Exec<'a>>,
        extend: &'a Extend,
        st: ExpandState,
    },
    VarExtend {
        input: Box<Exec<'a>>,
        ve: &'a VarLengthExtend,
        filter: Option<CompiledFilter<'a>>,
        st: ExpandState,
    },
    ProjectPath {
        input: Box<Exec<'a>>,
        pp: &'a ProjectPath,
        st: ExpandState,
    },
    Unwind {
        input: Box<Exec<'a>>,
        list: CompiledExpr,
        target: &'a UnwindTarget,
        st: ExpandState,
    },
    /// Buffer the right (build) side fully, then stream the left, emitting the
    /// product per left row.
    CrossProduct {
        left: Box<Exec<'a>>,
        right: Box<Exec<'a>>,
        left_width: usize,
        right_width: usize,
        right_buf: Option<Vec<DataChunk>>,
        st: ExpandState,
    },
    /// Hash join (P3 step 7): build a hash table over the (drained) build side keyed
    /// by its key expressions, then stream the probe side and emit each probe row ×
    /// its matching build rows. A NULL key never matches (Cypher `=` semantics).
    HashJoin {
        probe: Box<Exec<'a>>,
        build: Box<Exec<'a>>,
        /// `(start, len)` of each side's columns in the layout (the build side is the
        /// cost-chosen smaller input, so it may be either the left or right input).
        probe_cols: (usize, usize),
        build_cols: (usize, usize),
        probe_keys: Vec<CompiledExpr>,
        build_keys: Vec<CompiledExpr>,
        /// Built lazily on the first pull: build-key → the matching build rows'
        /// build-side columns (`[build_start..build_start + build_len)`).
        table: Option<HashMap<JoinKey, Vec<Vec<Value>>>>,
        /// Inner (the cross-product rewrite) vs the decorrelated Left/Mark forms.
        kind: JoinKind,
        st: ExpandState,
    },
    /// Left join: per input row, drain the seeded sub-pipeline; emit its matches,
    /// or one NULL-extended row if it has none.
    Optional {
        input: Box<Exec<'a>>,
        pattern: &'a PlanOp,
        new_cols: &'a [usize],
        st: ExpandState,
    },
    /// `EXISTS {}` / `COUNT {}`: per input row, run the seeded sub-pipeline and
    /// write the boolean/count into `result_col` (EXISTS short-circuits).
    Subquery {
        input: Box<Exec<'a>>,
        pattern: &'a PlanOp,
        result_col: usize,
        kind: SubqueryKind,
        st: ExpandState,
    },
    SequenceCall {
        input: Box<Exec<'a>>,
        func: SequenceFn,
        name: &'a str,
        result_col: usize,
        st: ExpandState,
    },
    /// Fill materialized node/rel value columns from id columns (audit V12).
    MaterializeValues {
        input: Box<Exec<'a>>,
        items: &'a [koko_planner::MaterializeItem],
    },
}

impl<'a> Exec<'a> {
    /// Pull the next chunk of ≤[`VECTOR_CAPACITY`] rows, or `None` when exhausted.
    fn next_chunk(&mut self, ctx: &Ctx<'a>) -> Result<Option<DataChunk>> {
        ctx.execution.control.check()?;
        match self {
            Exec::SingleRow { done } => {
                if *done {
                    Ok(None)
                } else {
                    *done = true;
                    let mut chunk = DataChunk::new(&ctx.layout.col_types);
                    chunk.set_flat(1);
                    Ok(Some(chunk))
                }
            }
            Exec::InputScan { chunks, idx } => {
                if *idx < chunks.len() {
                    let c = chunks[*idx].clone();
                    *idx += 1;
                    Ok(Some(c))
                } else {
                    Ok(None)
                }
            }
            Exec::Buffered { chunks, idx } => {
                if *idx < chunks.len() {
                    // Move the chunk out (it is never revisited) to avoid a clone.
                    let c = std::mem::replace(&mut chunks[*idx], DataChunk::new(&[]));
                    *idx += 1;
                    Ok(Some(c))
                } else {
                    Ok(None)
                }
            }
            Exec::ScanNode {
                scan,
                table_idx,
                offset,
                end,
                single_table,
                projected_columns,
                external_reader,
            } => loop {
                if *table_idx >= scan.tables.len() {
                    return Ok(None);
                }
                let scan_table = &scan.tables[*table_idx];
                let external_bound = ctx.execution.sources.num_rows(scan_table.table);
                let bound = external_bound
                    .unwrap_or_else(|| ctx.storage.node_count(scan_table.table))
                    .min(*end);
                if external_bound.is_some() && *offset < bound {
                    if external_reader.is_none() {
                        *external_reader = Some(ctx.execution.sources.open_node_reader(
                            scan_table.table,
                            &projected_columns[*table_idx],
                            *offset,
                            bound,
                            ctx.catalog,
                        )?);
                    }
                    let reader = external_reader
                        .as_mut()
                        .expect("external node reader was initialized");
                    if let Some(output) = reader.next_chunk(scan, scan_table, ctx)? {
                        *offset = reader.next_offset;
                        return Ok(Some(output));
                    }
                    *offset = bound;
                }
                if *offset >= bound {
                    if *single_table {
                        return Ok(None);
                    }
                    *table_idx += 1;
                    *offset = 0;
                    *external_reader = None;
                    continue;
                }
                let offset_count = (bound - *offset).min(VECTOR_CAPACITY as u64) as usize;
                let batch = ctx.storage.scan_node_batch(
                    ctx.read(),
                    scan_table.table,
                    &projected_columns[*table_idx],
                    *offset,
                    offset_count,
                );
                *offset += offset_count as u64;
                let size = batch.size();
                if size == 0 {
                    continue;
                }
                let mut output = DataChunk::new(&ctx.layout.col_types);
                let mut source_columns = batch.columns.into_iter();
                output.columns[scan.id_col] =
                    source_columns.next().expect("node batch includes its id");
                for (property, source) in scan_table.prop_cols.iter().zip(source_columns) {
                    let target_type = &ctx.layout.col_types[property.col_index];
                    if &source.logical_type == target_type {
                        output.columns[property.col_index] = source;
                    } else {
                        for row in 0..size {
                            output.columns[property.col_index].set_value_owned(
                                row,
                                promote_prop(source.get_value(row), target_type),
                            );
                        }
                    }
                }
                output.set_flat(size);
                return Ok(Some(output));
            },
            Exec::IndexScan { scan, done } => {
                if *done {
                    return Ok(None);
                }
                *done = true;
                let mut accum = ChunkAccum::new(&ctx.layout.col_types);
                // Fold the constant key once and probe the in-memory PK index.
                let key = eval_constant(&scan.pk_value)?;
                let mut rows = Vec::new();
                expand_index_lookup_row(scan, &key, ctx, None, &mut rows);
                for row in rows {
                    accum.push_row(&row);
                }
                Ok(accum.take())
            }
            Exec::IndexLookup {
                input,
                scan,
                key,
                st,
            } => {
                let key = &*key;
                stream_expand(st, input, ctx, |chunk, pos, out| {
                    let value = key.eval(chunk, pos, ctx.execution.random)?;
                    expand_index_lookup_row(scan, &value, ctx, Some((chunk, pos)), out);
                    Ok(())
                })
            }
            Exec::ScanTableFunc {
                func,
                arg,
                cols,
                rows,
                idx,
            } => {
                if rows.is_none() {
                    // Computed once (byte-exact with the standalone `CALL`
                    // short-circuit), then yielded in chunks.
                    *rows = Some(table_func_rows(
                        ctx.catalog,
                        *func,
                        *arg,
                        ctx.execution.table_functions,
                    )?);
                }
                let all = rows.as_ref().expect("rows computed above");
                let width = ctx.layout.width();
                let mut accum = ChunkAccum::new(&ctx.layout.col_types);
                while *idx < all.len() {
                    let values = &all[*idx];
                    *idx += 1;
                    debug_assert_eq!(values.len(), cols.len(), "row width matches scan schema");
                    let mut row = vec![Value::Null; width];
                    for (&col, value) in cols.iter().zip(values) {
                        row[col] = value.clone();
                    }
                    accum.push_row(&row);
                    if accum.is_full() {
                        return Ok(Some(accum.into_chunk()));
                    }
                }
                Ok(accum.take())
            }
            Exec::LoadScan {
                paths,
                file_idx,
                cols,
                col_names,
                options,
                bare,
                format,
                reader,
                dialect,
                columnar_reader,
                done,
            } => {
                if *format != FileFormat::Csv {
                    return next_columnar_load_chunk(
                        ColumnarLoadSpec {
                            paths,
                            cols,
                            format: *format,
                            options,
                            ctx: *ctx,
                        },
                        file_idx,
                        columnar_reader,
                        done,
                    );
                }
                if *done {
                    return Ok(None);
                }
                let mut accum = ChunkAccum::new(&ctx.layout.col_types);
                // Stream the files in order: open+position the current file's
                // reader on demand, and when it exhausts advance to the next.
                loop {
                    if reader.is_none() {
                        if *file_idx >= paths.len() {
                            *done = true;
                            return Ok(accum.take());
                        }
                        let path = paths[*file_idx].as_str();
                        let (mut rdr, resolved_dialect) =
                            open_csv_reader(path, options, cols.len())?;
                        *dialect = Some(resolved_dialect);
                        let mut rec = csv::StringRecord::new();
                        let mut skipped = 0usize;
                        // C++ order (audit W5): the header row is consumed FIRST,
                        // then `skip=N` skips N *data* rows.
                        match options.header {
                            Some(true) => {
                                read_csv(&mut rdr, &mut rec, path, options, resolved_dialect, ctx)?; // discard header
                            }
                            Some(false) => {}
                            // Auto-detect: peek row 1. It is a header (skip it) iff
                            // its fields match the declared names OR it does not
                            // parse as the declared types; otherwise it is the first
                            // data row — emitted unless a `skip=N` consumes it.
                            None => {
                                if read_csv(
                                    &mut rdr,
                                    &mut rec,
                                    path,
                                    options,
                                    resolved_dialect,
                                    ctx,
                                )? {
                                    let col_types: Vec<_> = cols
                                        .iter()
                                        .map(|&c| ctx.layout.col_types[c].clone())
                                        .collect();
                                    let is_header = koko_common::csv_dialect::looks_like_header(
                                        &rec, col_names, &col_types,
                                    );
                                    if !is_header {
                                        if options.skip > 0 {
                                            skipped = 1;
                                        } else {
                                            let r = push_csv_row(
                                                &rec, cols, *bare, ctx, &mut accum, path, options,
                                            );
                                            if let Err(e) = r {
                                                if !options.ignore_errors
                                                    || matches!(e, Error::Parser(_))
                                                {
                                                    return Err(e);
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        while skipped < options.skip {
                            if !read_csv(&mut rdr, &mut rec, path, options, resolved_dialect, ctx)?
                            {
                                break;
                            }
                            skipped += 1;
                        }
                        *reader = Some(rdr);
                    }
                    let path = paths[*file_idx].as_str();
                    let rdr = reader.as_mut().expect("reader opened above");
                    let mut rec = csv::StringRecord::new();
                    loop {
                        ctx.execution.control.check()?;
                        if accum.is_full() {
                            return Ok(Some(accum.into_chunk()));
                        }
                        if !read_csv(
                            rdr,
                            &mut rec,
                            path,
                            options,
                            dialect.expect("CSV dialect resolved with reader"),
                            ctx,
                        )? {
                            // This file is exhausted; advance to the next.
                            *reader = None;
                            *file_idx += 1;
                            break;
                        }
                        if let Err(e) =
                            push_csv_row(&rec, cols, *bare, ctx, &mut accum, path, options)
                        {
                            // IGNORE_ERRORS: a malformed row is skipped, not fatal
                            // — except Parser-class errors, which C++ always raises.
                            if !options.ignore_errors || matches!(e, Error::Parser(_)) {
                                return Err(e);
                            }
                        }
                    }
                }
            }
            Exec::Filter { input, predicate } => {
                // Pull child chunks, narrowing each selection; skip wholly-filtered
                // chunks so the consumer only sees rows.
                loop {
                    match input.next_chunk(ctx)? {
                        None => return Ok(None),
                        Some(mut chunk) => {
                            let mut kept = Vec::with_capacity(chunk.sel.len());
                            for pos in chunk.sel.iter() {
                                if predicate.eval_predicate(&chunk, pos, ctx.execution.random)? {
                                    kept.push(pos);
                                }
                            }
                            if kept.is_empty() {
                                continue;
                            }
                            chunk.sel = Selection::Filtered(kept);
                            return Ok(Some(chunk));
                        }
                    }
                }
            }
            Exec::Extend { input, extend, st } => {
                let extend = *extend;
                if extend.factorize {
                    // Factorized (P3 step 6): collapse the fan-out into a multiplicity.
                    factorized_extend(st, input, extend, ctx)
                } else {
                    stream_extend_batch(st, input, extend, ctx)
                }
            }
            Exec::VarExtend {
                input,
                ve,
                filter,
                st,
            } => {
                let ve = *ve;
                let filter = filter.as_ref();
                if ve.factorize {
                    factorized_var_extend(input, ve, filter, ctx)
                } else {
                    stream_expand(st, input, ctx, |chunk, pos, out| {
                        expand_var_extend_row(ve, filter, ctx, chunk, pos, out)
                    })
                }
            }
            Exec::ProjectPath { input, pp, st } => {
                let pp = *pp;
                stream_expand(st, input, ctx, |chunk, pos, out| {
                    expand_project_path_row(pp, ctx, chunk, pos, out)
                })
            }
            Exec::Unwind {
                input,
                list,
                target,
                st,
            } => {
                let list = &*list;
                stream_expand(st, input, ctx, |chunk, pos, out| {
                    expand_unwind_row(list, target, ctx, chunk, pos, out)
                })
            }
            Exec::CrossProduct {
                left,
                right,
                left_width,
                right_width,
                right_buf,
                st,
            } => {
                if right_buf.is_none() {
                    *right_buf = Some(drain_all(right, ctx)?);
                }
                let right_buf = right_buf.as_ref().expect("buffered above");
                let left_width = *left_width;
                let right_width = *right_width;
                stream_expand(st, left, ctx, |chunk, pos, out| {
                    expand_cross_row(right_buf, left_width, right_width, ctx, chunk, pos, out)
                })
            }
            Exec::HashJoin {
                probe,
                build,
                probe_cols,
                build_cols,
                probe_keys,
                build_keys,
                table,
                kind,
                st,
            } => {
                let (ps, pl) = *probe_cols;
                let (bs, bl) = *build_cols;
                let width = ctx.layout.width();
                // Build phase (once): drain the build side and hash it by its key,
                // storing each row's build-side columns. NULL-keyed rows are dropped
                // (a NULL key never matches, by Cypher `=` semantics).
                if table.is_none() {
                    let mut t: HashMap<JoinKey, Vec<Vec<Value>>> = HashMap::new();
                    for chunk in drain_all(build, ctx)? {
                        for pos in chunk.sel.iter() {
                            let Some(key) =
                                eval_join_key(build_keys, &chunk, pos, ctx.execution.random)?
                            else {
                                continue;
                            };
                            let row: Vec<Value> = (bs..bs + bl)
                                .map(|c| chunk.columns[c].get_value(pos))
                                .collect();
                            let retained_bytes = key
                                .retained_bytes()
                                .saturating_add(
                                    (row.capacity() * std::mem::size_of::<Value>()) as u64,
                                )
                                .saturating_add(row.iter().map(value_payload_bytes).sum::<u64>())
                                .saturating_add(
                                    (std::mem::size_of::<JoinKey>()
                                        + std::mem::size_of::<Vec<Value>>()
                                        + 2 * std::mem::size_of::<usize>())
                                        as u64,
                                );
                            ctx.execution.memory.charge(retained_bytes)?;
                            t.entry(key).or_default().push(row);
                        }
                    }
                    *table = Some(t);
                }
                let table = table.as_ref().expect("built above");
                // `out` row carrying the probe-side columns (the build columns are
                // filled per match, or left NULL).
                let probe_row = |chunk: &DataChunk, pos: usize| {
                    let mut row = vec![Value::Null; width];
                    for i in 0..pl {
                        row[ps + i] = chunk.columns[ps + i].get_value(pos);
                    }
                    row
                };
                // Probe phase: stream the probe side. Each side fills its own global
                // column range, so the output reconstructs the full layout regardless
                // of which input was hashed.
                match kind {
                    // Emit `probe × each build match` (NULL probe key ⇒ no output).
                    JoinKind::Inner => stream_expand(st, probe, ctx, |chunk, pos, out| {
                        let Some(key) =
                            eval_join_key(probe_keys, chunk, pos, ctx.execution.random)?
                        else {
                            return Ok(());
                        };
                        if let Some(matches) = table.get(&key) {
                            for (match_index, brow) in matches.iter().enumerate() {
                                if match_index % VECTOR_CAPACITY == 0 {
                                    ctx.execution.control.check()?;
                                }
                                let mut row = probe_row(chunk, pos);
                                for (i, c) in (bs..bs + bl).enumerate() {
                                    row[c] = brow[i].clone();
                                }
                                out.push(row);
                            }
                        }
                        Ok(())
                    }),
                    // Left outer: emit matches, or one probe row with the build
                    // columns left NULL when there is none (decorrelated OPTIONAL).
                    JoinKind::Left => stream_expand(st, probe, ctx, |chunk, pos, out| {
                        let key = eval_join_key(probe_keys, chunk, pos, ctx.execution.random)?;
                        match key.as_ref().and_then(|k| table.get(k)) {
                            Some(matches) if !matches.is_empty() => {
                                for (match_index, brow) in matches.iter().enumerate() {
                                    if match_index % VECTOR_CAPACITY == 0 {
                                        ctx.execution.control.check()?;
                                    }
                                    let mut row = probe_row(chunk, pos);
                                    for (i, c) in (bs..bs + bl).enumerate() {
                                        row[c] = brow[i].clone();
                                    }
                                    out.push(row);
                                }
                            }
                            _ => out.push(probe_row(chunk, pos)),
                        }
                        Ok(())
                    }),
                    // Mark: exactly one output row per probe row. Annotate the
                    // probe chunk in place instead of expanding every row through a
                    // temporary `Vec<Value>` and rebuilding an identical chunk.
                    JoinKind::Mark {
                        mark_col,
                        kind: sqk,
                    } => {
                        let Some(mut chunk) = probe.next_chunk(ctx)? else {
                            return Ok(None);
                        };
                        for pos in chunk.sel.iter() {
                            let key = eval_join_key(probe_keys, &chunk, pos, ctx.execution.random)?;
                            let n = key
                                .as_ref()
                                .and_then(|key| table.get(key))
                                .map_or(0, Vec::len);
                            let value = match sqk {
                                SubqueryKind::Exists => Value::Bool(n > 0),
                                SubqueryKind::Count => Value::Int64(n as i64),
                            };
                            chunk.columns[*mark_col].set_value_owned(pos, value);
                        }
                        Ok(Some(chunk))
                    }
                }
            }
            Exec::Optional {
                input,
                pattern,
                new_cols,
                st,
            } => {
                let pattern = *pattern;
                let new_cols = *new_cols;
                stream_expand(st, input, ctx, |chunk, pos, out| {
                    expand_optional_row(pattern, new_cols, ctx, chunk, pos, out)
                })
            }
            Exec::Subquery {
                input,
                pattern,
                result_col,
                kind,
                st,
            } => {
                let pattern = *pattern;
                let result_col = *result_col;
                let kind = *kind;
                stream_expand(st, input, ctx, |chunk, pos, out| {
                    expand_subquery_row(pattern, result_col, kind, ctx, chunk, pos, out)
                })
            }
            Exec::SequenceCall {
                input,
                func,
                name,
                result_col,
                st,
            } => {
                let func = *func;
                let name = *name;
                let result_col = *result_col;
                stream_expand(st, input, ctx, |chunk, pos, out| {
                    expand_sequence_row(func, name, result_col, ctx, chunk, pos, out)
                })
            }
            Exec::MaterializeValues { input, items } => {
                // 1:1 pass-through: assemble each selected row's node/rel value
                // from its internal id into the value column (audit V12 seam).
                match input.next_chunk(ctx)? {
                    None => Ok(None),
                    Some(mut chunk) => {
                        let positions: Vec<usize> = chunk.sel.iter().collect();
                        for it in *items {
                            for &pos in &positions {
                                let v = match chunk.columns[it.id_col].get_value(pos) {
                                    Value::InternalId(id) if id.table_id.0 != u64::MAX => {
                                        let entity = EntityRead::from_ctx(ctx);
                                        if it.is_node {
                                            assemble_node_opt(id, entity)?
                                                .map(|node| Value::Node(Box::new(node)))
                                                .unwrap_or(Value::Null)
                                        } else {
                                            Value::Rel(Box::new(assemble_rel_value(id, entity)?))
                                        }
                                    }
                                    // Already a value (or NULL/unbound) — carry it.
                                    other @ (Value::Node(_) | Value::Rel(_)) => other,
                                    _ => Value::Null,
                                };
                                chunk.columns[it.value_col].set_value(pos, &v);
                            }
                        }
                        Ok(Some(chunk))
                    }
                }
            }
        }
    }
}

/// Drive the row-expansion streaming pattern shared by all 1-child operators that
/// map each input row to zero or more output rows.
fn stream_expand<'a>(
    st: &mut ExpandState,
    child: &mut Exec<'a>,
    ctx: &Ctx<'a>,
    mut expand: impl FnMut(&DataChunk, usize, &mut Vec<Vec<Value>>) -> Result<()>,
) -> Result<Option<DataChunk>> {
    st.release_yielded_chunk();
    let mut accum = ChunkAccum::new(&ctx.layout.col_types);
    loop {
        while st.pend_i < st.pending.len() {
            accum.push_row_with_mult(&st.pending[st.pend_i], st.pending_mults[st.pend_i]);
            st.pend_i += 1;
            if accum.is_full() {
                let output = accum.into_chunk();
                st.retain_yielded_chunk(ctx.execution.memory, &output)?;
                return Ok(Some(output));
            }
        }
        st.release_pending_rows();
        st.pending.clear();
        st.pending_mults.clear();
        st.pend_i = 0;

        loop {
            if st.cur.is_none() {
                match child.next_chunk(ctx)? {
                    Some(chunk) => {
                        st.positions = chunk.sel.iter().collect();
                        st.pi = 0;
                        st.cur = Some(chunk);
                    }
                    None => {
                        let output = accum.take();
                        if let Some(chunk) = &output {
                            st.retain_yielded_chunk(ctx.execution.memory, chunk)?;
                        }
                        return Ok(output);
                    }
                }
            }
            if st.pi >= st.positions.len() {
                st.cur = None;
                continue;
            }
            let pos = st.positions[st.pi];
            st.pi += 1;
            let cur = st.cur.as_ref().expect("current input chunk");
            let multiplicity = cur.multiplicity(pos);
            let pending_before = st.pending.len();
            expand(cur, pos, &mut st.pending)?;
            let pending_bytes = st.pending[pending_before..]
                .iter()
                .map(|row| {
                    (row.capacity() * std::mem::size_of::<Value>()) as u64
                        + row.iter().map(value_payload_bytes).sum::<u64>()
                })
                .sum();
            st.charge_pending_rows(ctx.execution.memory, pending_bytes)?;
            st.pending_mults.resize(st.pending.len(), multiplicity);
            break;
        }
    }
}

struct ExtendPropertyCache {
    rel: Vec<DataChunk>,
    node_locations: Option<Vec<Option<(usize, usize)>>>,
    nodes: Vec<Vec<DataChunk>>,
}

fn gathered_property(batches: &[DataChunk], index: usize, column: usize) -> Value {
    batches[index / VECTOR_CAPACITY].columns[column].get_value(index % VECTOR_CAPACITY)
}

fn take_gathered_property(batches: &mut [DataChunk], index: usize, column: usize) -> Value {
    batches[index / VECTOR_CAPACITY].columns[column].take_value(index % VECTOR_CAPACITY)
}

/// Batched adjacency extension. One storage dispatch handles all non-null endpoints
/// in the input chunk for each relationship-table branch.
fn stream_extend_batch<'a>(
    st: &mut ExpandState,
    child: &mut Exec<'a>,
    extend: &Extend,
    ctx: &Ctx<'a>,
) -> Result<Option<DataChunk>> {
    st.release_yielded_chunk();
    if let Some(chunk) = st.take_pending_chunk() {
        return Ok(Some(chunk));
    }
    loop {
        let chunk = match child.next_chunk(ctx)? {
            Some(chunk) => chunk,
            None => return Ok(None),
        };
        st.nodes.clear();
        st.node_positions.clear();
        for pos in chunk.sel.iter() {
            if let Value::InternalId(node) = chunk.columns[extend.from_id_col].get_value(pos) {
                st.nodes.push(node);
                st.node_positions.push(pos);
            }
        }
        if st.nodes.is_empty() {
            continue;
        }

        st.tagged_neighbors.clear();
        let mut property_caches = Vec::with_capacity(extend.branches.len());
        if st.all_visible.len() < extend.branches.len() {
            st.all_visible.resize(extend.branches.len(), None);
        }
        for (branch_index, branch) in extend.branches.iter().enumerate() {
            ctx.execution.control.check()?;
            st.neighbors.clear();
            let all_visible = match st.all_visible[branch_index] {
                Some(all_visible) => all_visible,
                None => {
                    let all_visible = ctx.execution.memory.rel_rows_all_visible(
                        ctx.storage,
                        ctx.read(),
                        branch.rel_table,
                    );
                    st.all_visible[branch_index] = Some(all_visible);
                    all_visible
                }
            };
            let external = ctx.execution.sources.extend_batch_into(
                branch.rel_table,
                &st.nodes,
                extend.dir,
                &mut st.neighbors,
                ctx.catalog,
                ctx.execution.control,
                ctx.execution.memory,
            )?;
            if !external {
                if all_visible {
                    ctx.storage.extend_batch_all_visible_into(
                        ctx.read(),
                        branch.rel_table,
                        &st.nodes,
                        extend.dir,
                        &mut st.neighbors,
                    );
                } else {
                    ctx.storage.extend_batch_into(
                        ctx.read(),
                        branch.rel_table,
                        &st.nodes,
                        extend.dir,
                        &mut st.neighbors,
                    );
                }
            }
            let rel = if branch.rel_prop_cols.is_empty() {
                Vec::new()
            } else {
                let rel_offsets: Vec<u64> = st
                    .neighbors
                    .iter()
                    .map(|neighbor| neighbor.rel.offset.0)
                    .collect();
                let rel_columns: Vec<usize> = branch
                    .rel_prop_cols
                    .iter()
                    .map(|property| property.column_id as usize)
                    .collect();
                if external {
                    ctx.execution
                        .sources
                        .projected_rows(
                            branch.rel_table,
                            &rel_offsets,
                            &rel_columns,
                            ctx.catalog,
                            ctx.execution.control,
                            ctx.execution.memory,
                        )?
                        .expect("external relationship source is pinned")
                } else {
                    ctx.storage.rel_properties_batch(
                        ctx.read(),
                        branch.rel_table,
                        &rel_offsets,
                        &rel_columns,
                    )
                }
            };
            let (node_locations, nodes) = match &extend.target {
                ExtendTarget::New { to_tables, .. }
                    if to_tables.iter().any(|table| !table.prop_cols.is_empty()) =>
                {
                    let mut locations = vec![None; st.neighbors.len()];
                    let mut batches = Vec::with_capacity(to_tables.len());
                    for (table_index, table) in to_tables.iter().enumerate() {
                        let mut offsets = Vec::new();
                        for (neighbor_index, neighbor) in st.neighbors.iter().enumerate() {
                            if neighbor.nbr.table_id == table.table {
                                locations[neighbor_index] = Some((table_index, offsets.len()));
                                offsets.push(neighbor.nbr.offset.0);
                            }
                        }
                        let columns: Vec<usize> = table
                            .prop_cols
                            .iter()
                            .map(|property| property.column_id as usize)
                            .collect();
                        batches.push(
                            if let Some(rows) = ctx.execution.sources.projected_rows(
                                table.table,
                                &offsets,
                                &columns,
                                ctx.catalog,
                                ctx.execution.control,
                                ctx.execution.memory,
                            )? {
                                rows
                            } else {
                                ctx.storage.node_properties_batch(
                                    ctx.read(),
                                    table.table,
                                    &offsets,
                                    &columns,
                                )
                            },
                        );
                    }
                    (Some(locations), batches)
                }
                ExtendTarget::New { .. } | ExtendTarget::Existing { .. } => (None, Vec::new()),
            };
            st.tagged_neighbors.extend(
                st.neighbors
                    .iter()
                    .copied()
                    .enumerate()
                    .map(|(neighbor_index, neighbor)| (branch_index, neighbor_index, neighbor)),
            );
            property_caches.push(ExtendPropertyCache {
                rel,
                node_locations,
                nodes,
            });
        }
        // A single branch is already row-major. Multiple relationship-table
        // branches need a stable merge back to scalar branch ordering.
        if extend.branches.len() > 1 {
            st.tagged_neighbors
                .sort_by_key(|(_, _, neighbor)| neighbor.input_pos);
        }

        let mut output: Option<ChunkAccum> = None;
        for index in 0..st.tagged_neighbors.len() {
            if index % VECTOR_CAPACITY == 0 {
                ctx.execution.control.check()?;
            }
            let (branch_index, neighbor_index, neighbor) = st.tagged_neighbors[index];
            let branch = &extend.branches[branch_index];
            let property_cache = &mut property_caches[branch_index];
            let pos = st.node_positions[neighbor.input_pos];
            if let ExtendTarget::Existing { filter_col } = &extend.target {
                match chunk.columns[*filter_col].get_value(pos) {
                    Value::InternalId(expected) if expected == neighbor.nbr => {}
                    _ => continue,
                }
            }

            let accum = output.get_or_insert_with(|| ChunkAccum::new(&ctx.layout.col_types));
            let output_pos = accum.len;
            for &column in &extend.carry_cols {
                accum.chunk.columns[column].copy_value_from(
                    output_pos,
                    &chunk.columns[column],
                    pos,
                );
            }
            accum.chunk.columns[extend.rel_id_col].set_internal_id(output_pos, neighbor.rel);
            for (property_index, property) in branch.rel_prop_cols.iter().enumerate() {
                accum.chunk.columns[property.col_index].set_value_owned(
                    output_pos,
                    promote_prop(
                        take_gathered_property(
                            &mut property_cache.rel,
                            neighbor_index,
                            property_index,
                        ),
                        &ctx.layout.col_types[property.col_index],
                    ),
                );
            }
            if let ExtendTarget::New {
                to_id_col,
                to_tables,
            } = &extend.target
            {
                let location = match property_cache.node_locations.as_ref() {
                    Some(locations) => locations[neighbor_index],
                    None => to_tables
                        .iter()
                        .position(|table| table.table == neighbor.nbr.table_id)
                        .map(|table_index| (table_index, 0)),
                };
                let Some((table_index, property_index)) = location else {
                    continue;
                };
                let table = &to_tables[table_index];
                accum.chunk.columns[*to_id_col].set_internal_id(output_pos, neighbor.nbr);
                for (column_index, property) in table.prop_cols.iter().enumerate() {
                    accum.chunk.columns[property.col_index].set_value_owned(
                        output_pos,
                        promote_prop(
                            take_gathered_property(
                                &mut property_cache.nodes[table_index],
                                property_index,
                                column_index,
                            ),
                            &ctx.layout.col_types[property.col_index],
                        ),
                    );
                }
            }
            accum.finish_row_with_mult(chunk.multiplicity(pos));
            if accum.is_full() {
                let finished = output
                    .take()
                    .expect("full extend output exists")
                    .into_chunk();
                st.push_pending_chunk(ctx.execution.memory, finished)?;
            }
        }
        if let Some(output) = output.and_then(ChunkAccum::take) {
            st.push_pending_chunk(ctx.execution.memory, output)?;
        }
        if let Some(output) = st.take_pending_chunk() {
            return Ok(Some(output));
        }
    }
}

/// Drive `root` to exhaustion, collecting all its chunks (a pipeline breaker —
/// used by write parts, `CrossProduct`'s build side, and `MERGE`'s per-row match).
fn drain_all<'a>(root: &mut Exec<'a>, ctx: &Ctx<'a>) -> Result<Vec<DataChunk>> {
    let mut chunks = Vec::new();
    while let Some(c) = root.next_chunk(ctx)? {
        ctx.execution.memory.charge(c.allocated_bytes())?;
        chunks.push(c);
    }
    Ok(chunks)
}

/// Count the rows `root` produces. With `stop_at_first`, return as soon as one is
/// found (the `EXISTS {}` short-circuit); otherwise drain fully (`COUNT {}`).
fn drain_count<'a>(root: &mut Exec<'a>, ctx: &Ctx<'a>, stop_at_first: bool) -> Result<i64> {
    let mut count = 0i64;
    while let Some(c) = root.next_chunk(ctx)? {
        count += c.size() as i64;
        if stop_at_first && count > 0 {
            break;
        }
    }
    Ok(count)
}

/// Read a full-width binding row from a chunk position.
fn row_at(chunk: &DataChunk, pos: usize, width: usize) -> Vec<Value> {
    (0..width)
        .map(|c| chunk.columns[c].get_value(pos))
        .collect()
}

/// Build a one-row seed chunk from a full-width row (the per-row input to a
/// correlated sub-pipeline).
fn seed_chunk(row: &[Value], col_types: &[LogicalType]) -> DataChunk {
    let mut seed = DataChunk::new(col_types);
    for (c, v) in row.iter().enumerate() {
        seed.columns[c].set_value(0, v);
    }
    seed.set_flat(1);
    seed
}

/// Compact hash-join key. The overwhelmingly common graph join is one internal
/// node id; represent it directly rather than allocating a one-element vector and
/// evaluating an accessor expression for every build/probe row.
#[derive(PartialEq, Eq, Hash)]
enum JoinKey {
    InternalId(InternalId),
    InternalIds(InternalId, InternalId),
    One(ValueKey),
    Many(Vec<ValueKey>),
}

impl JoinKey {
    fn retained_bytes(&self) -> u64 {
        match self {
            JoinKey::InternalId(_) | JoinKey::InternalIds(..) => 0,
            JoinKey::One(value) => value.heap_bytes(),
            JoinKey::Many(values) => {
                (values.capacity() * std::mem::size_of::<ValueKey>()) as u64
                    + values.iter().map(ValueKey::heap_bytes).sum::<u64>()
            }
        }
    }
}

fn compiled_internal_id_column(expr: &CompiledExpr) -> Option<usize> {
    match expr {
        CompiledExpr::Column(column) => Some(*column),
        CompiledExpr::Accessor {
            kind: AccessorKind::Id,
            arg,
            ..
        } => match arg.as_ref() {
            CompiledExpr::Column(column) => Some(*column),
            _ => None,
        },
        _ => None,
    }
}

/// Evaluate a hash join's key expressions at one chunk position, or `None` if
/// any component is NULL. `ValueKey` preserves cross-numeric Cypher equality.
fn eval_join_key(
    keys: &[CompiledExpr],
    chunk: &DataChunk,
    pos: usize,
    random: &RandomState,
) -> Result<Option<JoinKey>> {
    let pair_columns = match keys {
        [left, right] => compiled_internal_id_column(left).zip(compiled_internal_id_column(right)),
        _ => None,
    };
    if let Some((left_column, right_column)) = pair_columns {
        match (
            chunk.columns[left_column].get_value(pos),
            chunk.columns[right_column].get_value(pos),
        ) {
            (Value::InternalId(left), Value::InternalId(right)) => {
                return Ok(Some(JoinKey::InternalIds(left, right)));
            }
            (Value::Null, _) | (_, Value::Null) => return Ok(None),
            _ => {}
        }
    }
    if let Some(column) = (keys.len() == 1)
        .then(|| compiled_internal_id_column(&keys[0]))
        .flatten()
    {
        match chunk.columns[column].get_value(pos) {
            Value::InternalId(id) => return Ok(Some(JoinKey::InternalId(id))),
            Value::Null => return Ok(None),
            _ => {}
        }
    }
    if let [expr] = keys {
        let value = expr.eval(chunk, pos, random)?;
        return Ok((!value.is_null()).then(|| JoinKey::One(ValueKey::from_value(&value))));
    }
    let mut key = Vec::with_capacity(keys.len());
    for expr in keys {
        let value = expr.eval(chunk, pos, random)?;
        if value.is_null() {
            return Ok(None);
        }
        key.push(ValueKey::from_value(&value));
    }
    Ok(Some(JoinKey::Many(key)))
}

// --- CSV `LOAD FROM` reader helpers ---

/// Open a streaming CSV reader for `LOAD FROM`: resolve the dialect (honoring any
/// pinned options, else auto-detecting against the known column `arity`) and open
/// a BOM-skipping reader. Header detection is handled by the caller.
fn open_csv_reader(
    path: &str,
    options: &CsvLoadOptions,
    arity: usize,
) -> Result<(
    csv::Reader<std::fs::File>,
    koko_common::csv_dialect::Dialect,
)> {
    let mut dialect = koko_common::csv_dialect::resolve_dialect(
        std::path::Path::new(path),
        options.delimiter,
        options.quote,
        options.escape,
        options.auto_detect,
        Some(arity),
    )?;
    if arity == 1
        && options.quote.is_none()
        && std::fs::read(path).is_ok_and(|bytes| {
            bytes
                .split(|&byte| byte == b'\n')
                .skip(1)
                .any(|line| line.first() == Some(&b'"'))
        })
    {
        dialect.quote = Some(b'"');
    }
    if !options.ignore_errors {
        koko_common::csv_dialect::validate_file_structure(std::path::Path::new(path), options)?;
    }
    let reader = koko_common::csv_dialect::open_reader(std::path::Path::new(path), &dialect)?;
    Ok((reader, dialect))
}

fn is_compressed_csv(path: &str) -> bool {
    std::path::Path::new(path)
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            extension.eq_ignore_ascii_case("gz") || extension.eq_ignore_ascii_case("gzip")
        })
}

/// Read the next record, mapping an I/O/parse error to our `Error` and enforcing
/// option-sensitive multiline quoting semantics.
fn read_csv(
    rdr: &mut csv::Reader<std::fs::File>,
    rec: &mut csv::StringRecord,
    path: &str,
    options: &CsvLoadOptions,
    dialect: koko_common::csv_dialect::Dialect,
    ctx: &Ctx,
) -> Result<bool> {
    let compressed = is_compressed_csv(path);
    let serial_options;
    let options = if compressed && options.parallel {
        serial_options = CsvLoadOptions {
            parallel: false,
            ..options.clone()
        };
        &serial_options
    } else {
        options
    };
    let source = std::path::Path::new(path);
    loop {
        let has_record =
            koko_common::csv_dialect::read_prevalidated_record(rdr, rec, source, options)?;
        if !has_record {
            return Ok(false);
        }
        let warning = koko_common::csv_dialect::invalid_record_parts_with_dialect(
            source, rec, options, dialect,
        )
        .or_else(|| koko_common::csv_dialect::quoted_newline_parts(source, rec, options));
        let Some((message, line, mut fragment)) = warning else {
            return Ok(true);
        };
        if compressed {
            fragment.clear();
        }
        if !options.ignore_errors {
            return Err(Error::copy(format!(
                "Error in file {} on line {line}: {message} Line/record containing the error: \
                 '{fragment}'",
                source.display()
            )));
        }
        ctx.execution.warnings.push(
            message,
            source.to_string_lossy().into_owned(),
            line,
            fragment,
        );
        rec.clear();
    }
}

/// Parse one CSV record into a binding row and append it to `accum`: cell `i` is
/// parsed against the layout type of `cols[i]` and lands in that layout column.
fn push_csv_row(
    rec: &csv::StringRecord,
    cols: &[usize],
    bare: bool,
    ctx: &Ctx,
    accum: &mut ChunkAccum,
    path: &str,
    options: &CsvLoadOptions,
) -> Result<()> {
    let p = std::path::Path::new(path);
    // Physical 1-based line of the record start, for the C++-style row-error
    // wrapper (`Copy exception: Error in file ... on line N: ...`).
    let line_no = rec.position().map(|pos| pos.line() as usize).unwrap_or(0);
    let dialect = || {
        koko_common::csv_dialect::resolve_dialect(
            p,
            options.delimiter,
            options.quote,
            options.escape,
            options.auto_detect,
            Some(cols.len()),
        )
        .unwrap_or_default()
    };
    // Ragged rows: more fields than columns (beyond one allowed trailing
    // delimiter) is "got more."; fewer reports the exact count (C++ driver.cpp).
    let expected = cols.len();
    let mut len = rec.len();
    if len == expected + 1 && rec.get(len - 1) == Some("") {
        len = expected;
    }
    if len != expected {
        let reported = if bare && expected == 1 { 0 } else { expected };
        let inner = if len > expected {
            format!("expected {reported} values per row, but got more.")
        } else {
            format!("expected {reported} values per row, but got {len}.")
        };
        if options.ignore_errors {
            ctx.execution.warnings.push(
                inner.clone(),
                p.to_string_lossy().into_owned(),
                line_no as u64,
                if is_compressed_csv(path) {
                    String::new()
                } else {
                    koko_common::csv_dialect::physical_line_text(p, line_no as u64)
                },
            );
        }
        return Err(koko_common::csv_dialect::wrap_row_error(
            p,
            line_no,
            &inner,
            None,
            &dialect(),
        ));
    }
    let mut row = vec![Value::Null; ctx.layout.width()];
    for (i, &col) in cols.iter().enumerate() {
        let ty = &ctx.layout.col_types[col];
        // Every column casts through the shared CSV-cell path: a STRING column
        // nulls only on a null_strings match (a literal `null` or `'abc'` cell
        // stays verbatim text); other types carry the C++ cast wording,
        // wrapped with the file/line/record context.
        let mut value = {
            let raw = rec.get(i).unwrap_or("");
            let normalized =
                koko_common::csv_dialect::normalize_unbraced_list(raw, ty, options.list_unbraced);
            koko_function::parse_csv_cell(&normalized, ty, &options.null_strings).map_err(|e| {
                // Under IGNORE_ERRORS the skip site records the INNER error
                // text (the C++ show_warnings shape). Parser-class errors
                // stay fatal and record nothing.
                if options.ignore_errors && !matches!(e, Error::Parser(_)) {
                    ctx.execution.warnings.push(
                        e.to_string(),
                        p.to_string_lossy().into_owned(),
                        line_no as u64,
                        koko_common::csv_dialect::physical_line_text(p, line_no as u64),
                    );
                }
                koko_common::csv_dialect::wrap_row_error(
                    p,
                    line_no,
                    &e.to_string(),
                    Some(i),
                    &dialect(),
                )
            })?
        };
        if bare && matches!(ty, LogicalType::String) {
            if let Value::String(s) = &value {
                if let Some(normalized) = koko_common::literal::normalize_list_literal_text(s) {
                    value = Value::String(normalized);
                }
            }
        }
        row[col] = value;
    }
    accum.push_row(&row);
    Ok(())
}

struct ColumnarLoadSpec<'a, 'ctx> {
    paths: &'a [String],
    cols: &'a [usize],
    format: FileFormat,
    options: &'a CsvLoadOptions,
    ctx: Ctx<'ctx>,
}

fn next_columnar_load_chunk(
    spec: ColumnarLoadSpec<'_, '_>,
    file_idx: &mut usize,
    reader: &mut Option<ColumnarLoadReader>,
    done: &mut bool,
) -> Result<Option<DataChunk>> {
    let ColumnarLoadSpec {
        paths,
        cols,
        format,
        options,
        ctx,
    } = spec;
    if *done {
        return Ok(None);
    }
    loop {
        if reader.is_none() {
            *reader = Some(match format {
                FileFormat::Parquet => {
                    if *file_idx >= paths.len() {
                        *done = true;
                        return Ok(None);
                    }
                    ColumnarLoadReader::Parquet(koko_loader::parquet::ParquetReader::open(
                        &paths[*file_idx],
                    )?)
                }
                FileFormat::Npy => {
                    let npy_paths: Vec<std::path::PathBuf> =
                        paths.iter().map(std::path::PathBuf::from).collect();
                    let metadata = koko_loader::npy::preflight_npy(&npy_paths, None)?;
                    ColumnarLoadReader::Npy(koko_loader::npy::NpyBatchReader::from_metadata(
                        metadata,
                    )?)
                }
                FileFormat::Csv => unreachable!("CSV has a dedicated reader"),
            });
        }
        match reader.as_mut().expect("reader initialized").next() {
            Some(Ok(source)) => {
                if source.columns.len() != cols.len() {
                    return Err(Error::binder(format!(
                        "Number of columns mismatch. Expected {} but got {}.",
                        cols.len(),
                        source.columns.len()
                    )));
                }
                let mut output = DataChunk::new(&ctx.layout.col_types);
                let mut output_row = 0usize;
                let mut converted = Vec::with_capacity(cols.len());
                for (source_row, physical) in source.sel.iter().enumerate() {
                    converted.clear();
                    let conversion = source.columns.iter().zip(cols).try_for_each(
                        |(source_column, &target_column)| {
                            converted.push(cast_value(
                                &source_column.get_value(physical),
                                &ctx.layout.col_types[target_column],
                            )?);
                            Ok::<(), Error>(())
                        },
                    );
                    if let Err(error) = conversion {
                        if !options.ignore_errors {
                            return Err(error);
                        }
                        ctx.execution.warnings.push(
                            error.to_string(),
                            paths[(*file_idx).min(paths.len() - 1)].clone(),
                            (source_row + 1) as u64,
                            String::new(),
                        );
                        continue;
                    }
                    for ((&target_column, value), _) in cols
                        .iter()
                        .zip(converted.drain(..))
                        .zip(std::iter::repeat(()))
                    {
                        output.columns[target_column].set_value_owned(output_row, value);
                    }
                    output_row += 1;
                }
                if output_row > 0 {
                    output.set_flat(output_row);
                    return Ok(Some(output));
                }
            }
            Some(Err(error)) => return Err(error),
            None => {
                *reader = None;
                match format {
                    FileFormat::Parquet => *file_idx += 1,
                    FileFormat::Npy => {
                        *done = true;
                        return Ok(None);
                    }
                    FileFormat::Csv => unreachable!("CSV has a dedicated reader"),
                }
            }
        }
    }
}

/// Compile a [`PlanOp`] tree into a streaming [`Exec`] tree. Children are built
/// recursively and owned by their parent; correlated sub-patterns are kept as
/// `&PlanOp` (rebuilt per input row, seeded with that row). `input` is the carried
/// scope replayed by an `InputScan` leaf.
fn build_exec<'a>(op: &'a PlanOp, ctx: &Ctx<'a>, input: &'a [DataChunk]) -> Result<Exec<'a>> {
    build_exec_morsel(op, ctx, input, None)
}

/// A single scan morsel: scan only `table_idx`, offsets `[start, end)`. The
/// parallel driver (P3 step 9) builds one bounded pipeline per morsel; serial
/// builds pass `None` (full multi-table scan).
type MorselBound = (usize, u64, u64);

/// Compile a [`PlanOp`] tree into a streaming [`Exec`] tree, optionally bounding the
/// driving spine's `ScanNode` to one morsel. `morsel` flows down the linear spine
/// (`Filter`/`Extend`/…`/`input` children) to the single leaf `ScanNode`; branch and
/// correlated children always get `None` (they are never on a parallelizable spine —
/// see `parallel_scan_source`).
fn build_exec_morsel<'a>(
    op: &'a PlanOp,
    ctx: &Ctx<'a>,
    input: &'a [DataChunk],
    morsel: Option<MorselBound>,
) -> Result<Exec<'a>> {
    let resolver = LayoutResolver(ctx.layout);
    Ok(match op {
        PlanOp::SingleRow => Exec::SingleRow { done: false },
        // The carried rows already sit in this part's layout, so replay them.
        PlanOp::InputScan => Exec::InputScan {
            chunks: input,
            idx: 0,
        },
        PlanOp::ScanNode(scan) => {
            let projected_columns = || {
                scan.tables
                    .iter()
                    .map(|table| {
                        table
                            .prop_cols
                            .iter()
                            .map(|property| property.column_id as usize)
                            .collect()
                    })
                    .collect()
            };
            match morsel {
                // One morsel: a single table's `[start, end)` slice, no advancing.
                Some((table_idx, offset, end)) => Exec::ScanNode {
                    scan,
                    table_idx,
                    offset,
                    end,
                    single_table: true,
                    projected_columns: projected_columns(),
                    external_reader: None,
                },
                // Full scan: all candidate tables, each in full.
                None => Exec::ScanNode {
                    scan,
                    table_idx: 0,
                    offset: 0,
                    end: u64::MAX,
                    single_table: false,
                    projected_columns: projected_columns(),
                    external_reader: None,
                },
            }
        }
        PlanOp::IndexScan(scan) => {
            if let Some(child) = &scan.input {
                Exec::IndexLookup {
                    input: Box::new(build_exec(child, ctx, input)?),
                    scan,
                    key: compile(&scan.pk_value, &resolver)?,
                    st: ExpandState::default(),
                }
            } else {
                Exec::IndexScan { scan, done: false }
            }
        }
        PlanOp::ScanTableFunc { func, arg, cols } => Exec::ScanTableFunc {
            func: *func,
            arg: arg.as_deref(),
            cols,
            rows: None,
            idx: 0,
        },
        PlanOp::LoadScan {
            cols,
            col_names,
            path: _,
            paths,
            format,
            options,
            bare,
        } => Exec::LoadScan {
            paths,
            file_idx: 0,
            cols,
            col_names,
            options,
            bare: *bare,
            reader: None,
            dialect: None,
            format: *format,
            columnar_reader: None,
            done: false,
        },
        PlanOp::Filter {
            input: child,
            predicate,
        } => Exec::Filter {
            input: Box::new(build_exec_morsel(child, ctx, input, morsel)?),
            predicate: compile(predicate, &resolver)?,
        },
        PlanOp::Extend(extend) => Exec::Extend {
            input: Box::new(build_exec_morsel(&extend.input, ctx, input, morsel)?),
            extend,
            st: ExpandState::default(),
        },
        PlanOp::VarLengthExtend(ve) => Exec::VarExtend {
            input: Box::new(build_exec_morsel(&ve.input, ctx, input, morsel)?),
            ve,
            filter: build_recursive_filter(ve, &resolver)?,
            st: ExpandState::default(),
        },
        PlanOp::ProjectPath(pp) => Exec::ProjectPath {
            input: Box::new(build_exec_morsel(&pp.input, ctx, input, morsel)?),
            pp,
            st: ExpandState::default(),
        },
        PlanOp::Unwind {
            input: child,
            list,
            target,
        } => Exec::Unwind {
            input: Box::new(build_exec_morsel(child, ctx, input, morsel)?),
            list: compile(list, &resolver)?,
            target,
            st: ExpandState::default(),
        },
        // Branch / correlated children are never on a parallelizable spine, so they
        // always build a full (`None`) scan.
        PlanOp::CrossProduct {
            left,
            left_width,
            right,
            right_width,
        } => Exec::CrossProduct {
            left: Box::new(build_exec(left, ctx, input)?),
            right: Box::new(build_exec(right, ctx, input)?),
            left_width: *left_width,
            right_width: *right_width,
            right_buf: None,
            st: ExpandState::default(),
        },
        PlanOp::HashJoin {
            probe,
            build,
            probe_cols,
            build_cols,
            keys,
            kind,
        } => Exec::HashJoin {
            probe: Box::new(build_exec(probe, ctx, input)?),
            build: Box::new(build_exec(build, ctx, input)?),
            probe_cols: *probe_cols,
            build_cols: *build_cols,
            probe_keys: keys
                .iter()
                .map(|(pe, _)| compile(pe, &resolver))
                .collect::<Result<_>>()?,
            build_keys: keys
                .iter()
                .map(|(_, be)| compile(be, &resolver))
                .collect::<Result<_>>()?,
            table: None,
            kind: kind.clone(),
            st: ExpandState::default(),
        },
        PlanOp::Optional {
            input: child,
            pattern,
            new_cols,
        } => Exec::Optional {
            input: Box::new(build_exec(child, ctx, input)?),
            pattern,
            new_cols,
            st: ExpandState::default(),
        },
        PlanOp::Subquery {
            input: child,
            pattern,
            result_col,
            kind,
        } => Exec::Subquery {
            input: Box::new(build_exec(child, ctx, input)?),
            pattern,
            result_col: *result_col,
            kind: *kind,
            st: ExpandState::default(),
        },
        PlanOp::SequenceCall {
            input: child,
            func,
            name,
            result_col,
        } => Exec::SequenceCall {
            input: Box::new(build_exec(child, ctx, input)?),
            func: *func,
            name,
            result_col: *result_col,
            st: ExpandState::default(),
        },
        PlanOp::MaterializeValues {
            input: child,
            items,
        } => Exec::MaterializeValues {
            input: Box::new(build_exec(child, ctx, input)?),
            items,
        },
    })
}

/// Compile a recursive rel's per-step `(r, n | WHERE …)` filter (a relationship
/// gate + an intermediate-node gate) for a [`VarLengthExtend`].
fn build_recursive_filter<'a>(
    ve: &'a VarLengthExtend,
    resolver: &LayoutResolver,
) -> Result<Option<CompiledFilter<'a>>> {
    Ok(match &ve.filter {
        None => None,
        Some(f) => Some(CompiledFilter {
            rel_param: &f.rel_param,
            node_param: &f.node_param,
            rel_pred: f
                .rel_pred
                .as_ref()
                .map(|p| compile(p, resolver))
                .transpose()?,
            node_pred: f
                .node_pred
                .as_ref()
                .map(|p| compile(p, resolver))
                .transpose()?,
        }),
    })
}

// --- per-row expanders (one input row → zero or more output rows) ---

thread_local! {
    /// Reusable output for the factorized one-source use of the batched adjacency API.
    /// Ordinary expand submits a full input chunk; this path preserves the collapsed-count
    /// optimization without retaining the removed scalar storage call.
    static NBR_BUF: std::cell::RefCell<Vec<BatchNeighbor>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// Factorized extend (P3 step 6): the optimizer proved this extend's introduced
/// columns (the rel, and the new node for a `New` target) are never read — only
/// counted — so instead of fanning out one row per neighbor, count the valid
/// neighbors and fold that into the input row's multiplicity. A row with no neighbor
/// is dropped (inner-join semantics, identical to the fan-out producing zero rows).
/// One input chunk is consumed per call (1:1, so the output never exceeds a chunk).
fn factorized_extend<'a>(
    st: &mut ExpandState,
    input: &mut Exec<'a>,
    extend: &Extend,
    ctx: &Ctx<'a>,
) -> Result<Option<DataChunk>> {
    loop {
        let Some(chunk) = input.next_chunk(ctx)? else {
            return Ok(None);
        };
        let mut accum = ChunkAccum::new(&ctx.layout.col_types);
        for pos in chunk.sel.iter() {
            let count = count_extend_row(st, extend, ctx, &chunk, pos)?;
            if count == 0 {
                continue;
            }
            // The introduced (rel / new-node) columns stay NULL — they are unread.
            accum.push_chunk_row_with_mult(
                &chunk,
                pos,
                &extend.carry_cols,
                chunk.multiplicity(pos).saturating_mul(count),
            );
        }
        // The whole input chunk may collapse to zero matches; pull the next then.
        if let Some(c) = accum.take() {
            return Ok(Some(c));
        }
    }
}

/// Count the valid extensions of one input row — the factorized analog of
/// [`expand_extend_row`], applying the same to-table / bound-endpoint filtering but
/// counting neighbors instead of materializing rows.
fn count_extend_row(
    st: &mut ExpandState,
    extend: &Extend,
    ctx: &Ctx,
    chunk: &DataChunk,
    pos: usize,
) -> Result<u64> {
    let from_id = match chunk.columns[extend.from_id_col].get_value(pos) {
        Value::InternalId(id) => id,
        _ => return Ok(0), // null endpoint cannot extend
    };
    let want = match &extend.target {
        ExtendTarget::Existing { filter_col } => match chunk.columns[*filter_col].get_value(pos) {
            Value::InternalId(id) => Some(id),
            _ => return Ok(0),
        },
        ExtendTarget::New { .. } => None,
    };
    let mut neighbors = NBR_BUF.with(|b| std::mem::take(&mut *b.borrow_mut()));
    if st.all_visible.len() < extend.branches.len() {
        st.all_visible.resize(extend.branches.len(), None);
    }
    let mut count = 0u64;
    for (branch_index, branch) in extend.branches.iter().enumerate() {
        ctx.execution.control.check()?;
        neighbors.clear();
        let all_visible = match st.all_visible[branch_index] {
            Some(all_visible) => all_visible,
            None => {
                let all_visible = ctx.execution.memory.rel_rows_all_visible(
                    ctx.storage,
                    ctx.read(),
                    branch.rel_table,
                );
                st.all_visible[branch_index] = Some(all_visible);
                all_visible
            }
        };
        let external = ctx.execution.sources.extend_batch_into(
            branch.rel_table,
            std::slice::from_ref(&from_id),
            extend.dir,
            &mut neighbors,
            ctx.catalog,
            ctx.execution.control,
            ctx.execution.memory,
        )?;
        if !external {
            let new_target = all_visible.then_some(&extend.target).and_then(|target| {
                if let ExtendTarget::New { to_tables, .. } = target {
                    Some(to_tables)
                } else {
                    None
                }
            });
            if let Some(to_tables) = new_target {
                count = count.saturating_add(ctx.storage.extend_count_all_visible(
                    branch.rel_table,
                    from_id,
                    extend.dir,
                    |table| to_tables.iter().any(|candidate| candidate.table == table),
                ));
                continue;
            }
            if all_visible {
                ctx.storage.extend_batch_all_visible_into(
                    ctx.read(),
                    branch.rel_table,
                    std::slice::from_ref(&from_id),
                    extend.dir,
                    &mut neighbors,
                );
            } else {
                ctx.storage.extend_batch_into(
                    ctx.read(),
                    branch.rel_table,
                    std::slice::from_ref(&from_id),
                    extend.dir,
                    &mut neighbors,
                );
            }
        }
        for (neighbor_index, n) in neighbors.iter().enumerate() {
            if neighbor_index % VECTOR_CAPACITY == 0 {
                ctx.execution.control.check()?;
            }
            let ok = match &extend.target {
                ExtendTarget::Existing { .. } => want == Some(n.nbr),
                ExtendTarget::New { to_tables, .. } => {
                    to_tables.iter().any(|st| st.table == n.nbr.table_id)
                }
            };
            if ok {
                count += 1;
            }
        }
    }
    NBR_BUF.with(|b| *b.borrow_mut() = neighbors);
    Ok(count)
}

/// Factorized variable-length extend (P3 step 6): count the valid paths from the
/// start node and fold that into the row's multiplicity, instead of materializing one
/// row per path. The companion of [`factorized_extend`] for recursive rels.
fn factorized_var_extend<'a>(
    input: &mut Exec<'a>,
    ve: &VarLengthExtend,
    filter: Option<&CompiledFilter>,
    ctx: &Ctx<'a>,
) -> Result<Option<DataChunk>> {
    let width = ctx.layout.width();
    loop {
        let Some(chunk) = input.next_chunk(ctx)? else {
            return Ok(None);
        };
        let mut accum = ChunkAccum::new(&ctx.layout.col_types);
        for pos in chunk.sel.iter() {
            let count = count_var_extend_row(ve, filter, ctx, &chunk, pos)?;
            if count == 0 {
                continue;
            }
            let row = row_at(&chunk, pos, width);
            accum.push_row_with_mult(&row, chunk.multiplicity(pos).saturating_mul(count));
        }
        if let Some(c) = accum.take() {
            return Ok(Some(c));
        }
    }
}

/// Count the valid variable-length paths from one input row — the factorized analog
/// of [`expand_var_extend_row`], applying the same end-node filtering.
fn count_var_extend_row(
    ve: &VarLengthExtend,
    filter: Option<&CompiledFilter>,
    ctx: &Ctx,
    chunk: &DataChunk,
    pos: usize,
) -> Result<u64> {
    let from_id = match chunk.columns[ve.from_id_col].get_value(pos) {
        Value::InternalId(id) => id,
        _ => return Ok(0),
    };
    let want_end = match &ve.target {
        ExtendTarget::Existing { filter_col } => match chunk.columns[*filter_col].get_value(pos) {
            Value::InternalId(id) => Some(id),
            _ => return Ok(0),
        },
        ExtendTarget::New { .. } => None,
    };
    let mut count = 0u64;
    for path in enumerate_paths(
        ve,
        ctx.catalog,
        ctx.storage,
        ctx.execution.sources,
        ctx.read(),
        filter,
        ctx.execution.random,
        ctx.execution.memory,
        ctx.execution.control,
        from_id,
    )? {
        let ok = match &ve.target {
            ExtendTarget::New { to_tables, .. } => {
                to_tables.iter().any(|st| st.table == path.end.table_id)
            }
            ExtendTarget::Existing { .. } => want_end == Some(path.end),
        };
        if ok {
            count += 1;
        }
    }
    Ok(count)
}

/// Enumerate the variable-length paths from the row's start node (per-source, into
/// `out`), emitting one row per path. The per-source enumeration is bounded by the
/// graph and the depth/semantic; streaming yields these in chunks (the old
/// `MAX_PATHS_PER_SOURCE` truncation cap is gone).
fn expand_var_extend_row(
    ve: &VarLengthExtend,
    filter: Option<&CompiledFilter>,
    ctx: &Ctx,
    chunk: &DataChunk,
    pos: usize,
    out: &mut Vec<Vec<Value>>,
) -> Result<()> {
    let width = ctx.layout.width();
    let input_cols = ve.rel_value_col; // input populates columns [0..rel_value_col)
    let from_id = match chunk.columns[ve.from_id_col].get_value(pos) {
        Value::InternalId(id) => id,
        _ => return Ok(()), // null start cannot extend
    };
    // For an extend onto an already-bound node, the required end node.
    let want_end = match &ve.target {
        ExtendTarget::Existing { filter_col } => match chunk.columns[*filter_col].get_value(pos) {
            Value::InternalId(id) => Some(id),
            _ => return Ok(()),
        },
        ExtendTarget::New { .. } => None,
    };

    for path in enumerate_paths(
        ve,
        ctx.catalog,
        ctx.storage,
        ctx.execution.sources,
        ctx.read(),
        filter,
        ctx.execution.random,
        ctx.execution.memory,
        ctx.execution.control,
        from_id,
    )? {
        // The end must satisfy the to-node's allowed tables (New) or equal the
        // already-bound endpoint (Existing).
        match &ve.target {
            ExtendTarget::New { to_tables, .. } => {
                if !to_tables.iter().any(|st| st.table == path.end.table_id) {
                    continue;
                }
            }
            ExtendTarget::Existing { .. } => {
                if want_end != Some(path.end) {
                    continue;
                }
            }
        }

        let mut row = vec![Value::Null; width];
        for (c, slot) in row.iter_mut().enumerate().take(input_cols) {
            *slot = chunk.columns[c].get_value(pos);
        }
        // Assemble the recursive-rel value (intermediate nodes + rels), applying
        // any lambda projection to the intermediate values.
        if ve.build_value {
            let (node_proj, rel_proj) = match &ve.filter {
                Some(f) => (f.node_proj.as_ref(), f.rel_proj.as_ref()),
                None => (None, None),
            };
            let entity = EntityRead::from_ctx(ctx);
            let nodes = path
                .node_ids
                .iter()
                .map(|&id| {
                    let mut node = assemble_node_value(id, entity)?;
                    node.props = project_props(node.props, node_proj);
                    Ok(node)
                })
                .collect::<Result<_>>()?;
            let rels = path
                .rel_ids
                .iter()
                .map(|&id| {
                    let mut rel = assemble_rel_value(id, entity)?;
                    rel.props = project_props(rel.props, rel_proj);
                    Ok(rel)
                })
                .collect::<Result<_>>()?;
            row[ve.rel_value_col] = Value::RecursiveRel(Box::new(RecursiveRelValue {
                nodes,
                rels,
                degenerate: false,
                cost: path.cost,
                null_nodes: 0,
            }));
        }
        // Bind the end node (its id + properties from its actual table).
        if let ExtendTarget::New {
            to_id_col,
            to_tables,
        } = &ve.target
        {
            let st = to_tables
                .iter()
                .find(|st| st.table == path.end.table_id)
                .expect("end table checked above");
            row[*to_id_col] = Value::InternalId(path.end);
            let columns: Vec<usize> = st
                .prop_cols
                .iter()
                .map(|property| property.column_id as usize)
                .collect();
            let properties = if let Some(values) = ctx.execution.sources.projected_values(
                path.end.table_id,
                path.end.offset.0,
                &columns,
                ctx.catalog,
                ctx.execution.control,
                ctx.execution.memory,
            )? {
                values
            } else {
                ctx.storage.node_projected_values(
                    ctx.read(),
                    path.end.table_id,
                    path.end.offset.0,
                    &columns,
                )
            };
            for (property, value) in st.prop_cols.iter().zip(properties) {
                row[property.col_index] =
                    promote_prop(value, &ctx.layout.col_types[property.col_index]);
            }
        }
        out.push(row);
    }
    Ok(())
}

/// Assemble the named path's value into its column for this row.
fn expand_project_path_row(
    pp: &ProjectPath,
    ctx: &Ctx,
    chunk: &DataChunk,
    pos: usize,
    out: &mut Vec<Vec<Value>>,
) -> Result<()> {
    let width = ctx.layout.width();
    let mut row = row_at(chunk, pos, width);
    row[pp.path_col] = assemble_path(pp, ctx.layout, chunk, pos, EntityRead::from_ctx(ctx))?;
    out.push(row);
    Ok(())
}

/// `UNWIND list AS var`: emit one row per list element (a NULL/non-list yields none).
fn expand_unwind_row(
    list: &CompiledExpr,
    target: &UnwindTarget,
    ctx: &Ctx,
    chunk: &DataChunk,
    pos: usize,
    out: &mut Vec<Vec<Value>>,
) -> Result<()> {
    let width = ctx.layout.width();
    let items = match list.eval(chunk, pos, ctx.execution.random)? {
        Value::List(items) => items,
        _ => return Ok(()),
    };
    for (index, item) in items.into_iter().enumerate() {
        if index % VECTOR_CAPACITY == 0 {
            ctx.execution.control.check()?;
        }
        let mut row = row_at(chunk, pos, width);
        match target {
            UnwindTarget::Scalar { col } => row[*col] = item,
            UnwindTarget::Node {
                id_col,
                prop_tables,
            } => explode_node(&item, *id_col, prop_tables, &mut row),
        }
        out.push(row);
    }
    Ok(())
}

fn expand_index_lookup_row(
    scan: &IndexScan,
    key: &Value,
    ctx: &Ctx,
    input: Option<(&DataChunk, usize)>,
    out: &mut Vec<Vec<Value>>,
) {
    let Some(id) = ctx.storage.find_node_by_pk(ctx.read(), scan.table, key) else {
        return;
    };
    let off = id.offset.0;
    // Re-check visibility so the probe matches what a full scan + filter would
    // observe under MVCC (the PK index tracks the writer's latest state, not a
    // per-statement read view).
    if ctx.storage.node_is_deleted(ctx.read(), scan.table, off) {
        return;
    }
    let width = ctx.layout.width();
    let mut row = if let Some((chunk, pos)) = input {
        row_at(chunk, pos, width)
    } else {
        vec![Value::Null; width]
    };
    row[scan.id_col] = Value::InternalId(InternalId::new(scan.table, off));
    let columns: Vec<usize> = scan
        .prop_cols
        .iter()
        .map(|property| property.column_id as usize)
        .collect();
    let properties = ctx
        .storage
        .node_projected_values(ctx.read(), scan.table, off, &columns);
    for (property, value) in scan.prop_cols.iter().zip(properties) {
        row[property.col_index] = promote_prop(value, &ctx.layout.col_types[property.col_index]);
    }
    out.push(row);
}

/// Cross product: emit `left_row × every buffered right row`.
fn expand_cross_row(
    right_buf: &[DataChunk],
    left_width: usize,
    right_width: usize,
    ctx: &Ctx,
    chunk: &DataChunk,
    pos: usize,
    out: &mut Vec<Vec<Value>>,
) -> Result<()> {
    let width = ctx.layout.width();
    for rc in right_buf {
        ctx.execution.control.check()?;
        for rpos in rc.sel.iter() {
            let mut row = vec![Value::Null; width];
            for (c, slot) in row.iter_mut().enumerate().take(left_width) {
                *slot = chunk.columns[c].get_value(pos);
            }
            for (c, slot) in row
                .iter_mut()
                .enumerate()
                .skip(left_width)
                .take(right_width)
            {
                *slot = rc.columns[c].get_value(rpos);
            }
            out.push(row);
        }
    }
    Ok(())
}

/// `OPTIONAL MATCH`: drain the seeded sub-pipeline; emit its matches, or one
/// NULL-extended row if it produced none.
fn expand_optional_row(
    pattern: &PlanOp,
    new_cols: &[usize],
    ctx: &Ctx,
    chunk: &DataChunk,
    pos: usize,
    out: &mut Vec<Vec<Value>>,
) -> Result<()> {
    let width = ctx.layout.width();
    let row = row_at(chunk, pos, width);
    let seed = seed_chunk(&row, &ctx.layout.col_types);
    let mut sub = build_exec(pattern, ctx, std::slice::from_ref(&seed))?;
    let mut any = false;
    while let Some(mc) = sub.next_chunk(ctx)? {
        for mpos in mc.sel.iter() {
            any = true;
            out.push(row_at(&mc, mpos, width));
        }
    }
    if !any {
        let mut nrow = row;
        for &nc in new_cols {
            nrow[nc] = Value::Null;
        }
        fill_optional_empty_paths(pattern, ctx, &mut nrow)?;
        out.push(nrow);
    }
    Ok(())
}

fn fill_optional_empty_paths(pattern: &PlanOp, ctx: &Ctx, row: &mut [Value]) -> Result<()> {
    match pattern {
        PlanOp::ProjectPath(pp) => {
            fill_optional_empty_paths(&pp.input, ctx, row)?;
            row[pp.path_col] =
                assemble_empty_optional_path(pp, ctx.layout, row, EntityRead::from_ctx(ctx))?;
        }
        PlanOp::Filter { input, .. }
        | PlanOp::Unwind { input, .. }
        | PlanOp::Subquery { input, .. }
        | PlanOp::SequenceCall { input, .. }
        | PlanOp::MaterializeValues { input, .. } => {
            fill_optional_empty_paths(input, ctx, row)?;
        }
        PlanOp::Extend(e) => fill_optional_empty_paths(&e.input, ctx, row)?,
        PlanOp::VarLengthExtend(ve) => fill_optional_empty_paths(&ve.input, ctx, row)?,
        PlanOp::CrossProduct { left, right, .. } => {
            fill_optional_empty_paths(left, ctx, row)?;
            fill_optional_empty_paths(right, ctx, row)?;
        }
        PlanOp::HashJoin { probe, build, .. } => {
            fill_optional_empty_paths(probe, ctx, row)?;
            fill_optional_empty_paths(build, ctx, row)?;
        }
        PlanOp::Optional { input, pattern, .. } => {
            fill_optional_empty_paths(input, ctx, row)?;
            fill_optional_empty_paths(pattern, ctx, row)?;
        }
        PlanOp::IndexScan(idx) => {
            if let Some(input) = &idx.input {
                fill_optional_empty_paths(input, ctx, row)?;
            }
        }
        PlanOp::SingleRow
        | PlanOp::InputScan
        | PlanOp::ScanTableFunc { .. }
        | PlanOp::LoadScan { .. }
        | PlanOp::ScanNode(_) => {}
    }
    Ok(())
}

fn read_var_id_from_row(var: VarId, layout: &RowLayout, row: &[Value]) -> InternalId {
    let id_col = layout.var(var).id_col;
    match row.get(id_col) {
        Some(Value::InternalId(id)) => *id,
        _ => InternalId::new(TableId(u64::MAX), u64::MAX),
    }
}

fn assemble_empty_optional_path(
    pp: &ProjectPath,
    layout: &RowLayout,
    row: &[Value],
    entity: EntityRead<'_>,
) -> Result<Value> {
    let mut nodes = Vec::new();
    if let Some(head) = assemble_node_opt(read_var_id_from_row(pp.head, layout, row), entity)? {
        nodes.push(head);
    }
    // A PURELY-recursive pattern (`(a)-[*]->(b)`) leaves just the head — the
    // `*` matched nothing, so no intermediate/end slots (`[{A}]`). A pattern
    // with any FIXED single hop instead allocates a per-segment node slot:
    //  - a NEW (unmatched) end → a NULL slot (`(a)-->(b)-[*]->(c)` → `[{A}, , ]`);
    //  - a BOUND end of a single hop → included (`(a)-->(x)`, x=C → `[{A}, {C}]`).
    let has_fixed = pp
        .segments
        .iter()
        .any(|s| matches!(s.rel, PathRel::Single { .. }));
    let mut null_nodes = 0usize;
    if has_fixed {
        for seg in &pp.segments {
            let end_id = read_var_id_from_row(seg.to_node, layout, row);
            if end_id.table_id.0 == u64::MAX {
                null_nodes += 1;
            } else if matches!(seg.rel, PathRel::Single { .. }) {
                if let Some(node) = assemble_node_opt(end_id, entity)? {
                    nodes.push(node);
                }
            }
        }
    }
    Ok(Value::RecursiveRel(Box::new(RecursiveRelValue {
        nodes,
        rels: Vec::new(),
        // length() returns NULL on this unmatched-OPTIONAL leftover (audit V13).
        degenerate: true,
        cost: None,
        null_nodes,
    })))
}

/// `EXISTS {}` / `COUNT {}`: run the seeded sub-pipeline, write the result column,
/// emit one row. EXISTS short-circuits at the first match.
fn expand_subquery_row(
    pattern: &PlanOp,
    result_col: usize,
    kind: SubqueryKind,
    ctx: &Ctx,
    chunk: &DataChunk,
    pos: usize,
    out: &mut Vec<Vec<Value>>,
) -> Result<()> {
    let width = ctx.layout.width();
    let mut row = row_at(chunk, pos, width);
    let seed = seed_chunk(&row, &ctx.layout.col_types);
    // A correlated subplan is rebuilt and fully consumed for this one outer row.
    // Release its temporary accounting before evaluating the next row; retaining
    // every sequential subplan's high-water mark would turn bounded execution into
    // a false OOM on large datasets.
    let memory_before = ctx.execution.memory.bytes();
    let result = (|| {
        let mut sub = build_exec(pattern, ctx, std::slice::from_ref(&seed))?;
        drain_count(&mut sub, ctx, matches!(kind, SubqueryKind::Exists))
    })();
    ctx.execution.memory.release_to(memory_before);
    let count = result?;
    row[result_col] = match kind {
        SubqueryKind::Exists => Value::Bool(count > 0),
        SubqueryKind::Count => Value::Int64(count),
    };
    out.push(row);
    Ok(())
}

/// `nextval`/`currval`: advance/read the named sequence into the result column.
fn expand_sequence_row(
    func: SequenceFn,
    name: &str,
    result_col: usize,
    ctx: &Ctx,
    chunk: &DataChunk,
    pos: usize,
    out: &mut Vec<Vec<Value>>,
) -> Result<()> {
    let width = ctx.layout.width();
    let mut row = row_at(chunk, pos, width);
    let v = match func {
        SequenceFn::NextVal => ctx.catalog.sequence_next_val(name)?,
        SequenceFn::CurrVal => ctx.catalog.sequence_curr_val(name)?,
    };
    row[result_col] = Value::Int64(v);
    out.push(row);
    Ok(())
}

/// One enumerated path: its end node, the intermediate node ids (endpoints
/// excluded), and the relationship ids in order. The id vecs are populated only
/// when the rel value is needed (`build_value`); otherwise just `end` is used.
struct EnumPath {
    end: InternalId,
    node_ids: Vec<InternalId>,
    rel_ids: Vec<InternalId>,
    /// Accumulated edge weight for a (ALL) WSHORTEST path; `None` otherwise.
    cost: Option<f64>,
}

/// A compiled per-step recursive filter (the bound `(r, n | WHERE …)` split into
/// a relationship gate and an intermediate-node gate).
struct CompiledFilter<'a> {
    rel_param: &'a str,
    node_param: &'a str,
    rel_pred: Option<CompiledExpr>,
    node_pred: Option<CompiledExpr>,
}

impl CompiledFilter<'_> {
    /// Evaluate one of the predicates against the current `(rel, node)` binding.
    fn check(pred: &Option<CompiledExpr>, binds: &[(String, Value)], random: &RandomState) -> bool {
        match pred {
            None => true,
            Some(ce) => {
                let dummy = DataChunk::new(&[]);
                matches!(
                    ce.eval_with_bindings(&dummy, 0, binds, random),
                    Ok(v) if v.as_bool() == Some(true)
                )
            }
        }
    }
}

/// Enumerate paths from `start`: DFS for the `All` mode, BFS for shortest modes.
fn enum_path_bytes(path: &EnumPath) -> u64 {
    (std::mem::size_of::<EnumPath>()
        + path.node_ids.capacity() * std::mem::size_of::<InternalId>()
        + path.rel_ids.capacity() * std::mem::size_of::<InternalId>()) as u64
}

#[allow(clippy::too_many_arguments)]
fn extend_dispatch(
    sources: &QuerySourceState,
    catalog: &Catalog,
    storage: &InMemStorage,
    read: StorageReadHandle,
    rel_table: TableId,
    nodes: &[InternalId],
    dir: ExtendDir,
    out: &mut Vec<BatchNeighbor>,
    control: QueryControl<'_>,
    memory: &QueryMemory,
) -> Result<()> {
    if !sources.extend_batch_into(rel_table, nodes, dir, out, catalog, control, memory)? {
        storage.extend_batch_into(read, rel_table, nodes, dir, out);
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn enumerate_paths(
    ve: &VarLengthExtend,
    catalog: &Catalog,
    storage: &InMemStorage,
    sources: &QuerySourceState,
    read: StorageReadHandle,
    filter: Option<&CompiledFilter>,
    random: &RandomState,
    memory: &QueryMemory,
    control: QueryControl<'_>,
    start: InternalId,
) -> Result<Vec<EnumPath>> {
    match ve.mode {
        RecursiveMode::All => {
            let frontier_capacity = ve.upper as usize;
            memory.charge(
                (frontier_capacity
                    .saturating_mul(2)
                    .saturating_mul(std::mem::size_of::<InternalId>())) as u64,
            )?;
            let mut out = Vec::new();
            if ve.lower == 0 {
                let path = EnumPath {
                    end: start,
                    node_ids: Vec::new(),
                    rel_ids: Vec::new(),
                    cost: None,
                };
                memory.charge(enum_path_bytes(&path))?;
                out.push(path);
            }
            let mut inter = Vec::with_capacity(frontier_capacity);
            let mut rels = Vec::with_capacity(frontier_capacity);
            dfs_all(
                ve, catalog, storage, sources, read, filter, random, memory, control, start,
                &mut inter, &mut rels, &mut out,
            )?;
            Ok(out)
        }
        RecursiveMode::Shortest | RecursiveMode::AllShortest => enumerate_shortest(
            ve, catalog, storage, sources, read, filter, random, memory, control, start,
        ),
        RecursiveMode::WShortest | RecursiveMode::AllWShortest => enumerate_wshortest(
            ve, catalog, storage, sources, read, filter, random, memory, control, start,
        ),
    }
}

/// Assemble the `(rel_param, rel_value)` + `(node_param, node_value)` bindings for
/// one edge, for evaluating the per-step filter predicates.
fn edge_binds(
    filter: &CompiledFilter,
    rel_id: InternalId,
    nbr_id: InternalId,
    entity: EntityRead<'_>,
) -> Result<Vec<(String, Value)>> {
    Ok(vec![
        (
            filter.rel_param.to_string(),
            Value::Rel(Box::new(assemble_rel_value(rel_id, entity)?)),
        ),
        (
            filter.node_param.to_string(),
            Value::Node(Box::new(assemble_node_value(nbr_id, entity)?)),
        ),
    ])
}

/// Depth-first enumeration of every walk (subject to the path semantic + filter)
/// within `[lower, upper]` from `current`. `inter` holds the intermediate nodes
/// visited so far (n1..n_depth); `rels` the relationships taken.
#[allow(clippy::too_many_arguments)]
fn dfs_all(
    ve: &VarLengthExtend,
    catalog: &Catalog,
    storage: &InMemStorage,
    sources: &QuerySourceState,
    read: StorageReadHandle,
    filter: Option<&CompiledFilter>,
    random: &RandomState,
    memory: &QueryMemory,
    control: QueryControl<'_>,
    current: InternalId,
    inter: &mut Vec<InternalId>,
    rels: &mut Vec<InternalId>,
    out: &mut Vec<EnumPath>,
) -> Result<()> {
    control.check()?;
    let depth = rels.len() as u32;
    if depth >= ve.upper {
        return Ok(());
    }
    let mut neighbors = Vec::new();
    for &rt in &ve.rel_tables {
        neighbors.clear();
        extend_dispatch(
            sources,
            catalog,
            storage,
            read,
            rt,
            std::slice::from_ref(&current),
            ve.dir,
            &mut neighbors,
            control,
            memory,
        )?;
        for (neighbor_index, n) in neighbors.iter().enumerate() {
            if neighbor_index % VECTOR_CAPACITY == 0 {
                control.check()?;
            }
            match ve.semantic {
                // No repeated relationship.
                PathSemantic::Trail if rels.contains(&n.rel) => continue,
                _ => {}
            }
            // Per-step filter: the relationship gate blocks the edge entirely; the
            // node gate only blocks using `n.nbr` as an intermediate (recursion).
            let mut recurse_ok = true;
            if let Some(f) = filter {
                let binds = edge_binds(
                    f,
                    n.rel,
                    n.nbr,
                    EntityRead {
                        catalog,
                        storage,
                        read,
                        sources,
                        memory,
                        control,
                    },
                )?;
                if !CompiledFilter::check(&f.rel_pred, &binds, random) {
                    continue;
                }
                recurse_ok = CompiledFilter::check(&f.node_pred, &binds, random);
            }
            rels.push(n.rel);
            if rels.len() as u32 >= ve.lower {
                let path = EnumPath {
                    end: n.nbr,
                    node_ids: if ve.build_value {
                        inter.clone()
                    } else {
                        Vec::new()
                    },
                    rel_ids: if ve.build_value {
                        rels.clone()
                    } else {
                        Vec::new()
                    },
                    cost: None,
                };
                memory.charge(enum_path_bytes(&path))?;
                out.push(path);
            }
            // ACYCLIC constrains only the INTERMEDIATES to be pairwise
            // distinct: any step may still END a path (oracle: a 2-cycle from
            // 0 yields 0→1, 0→1→0, and 0→1→0→1 — but not length 4, whose
            // recursion would pass through the repeated intermediate 1).
            let acyclic_repeat = ve.semantic == PathSemantic::Acyclic && inter.contains(&n.nbr);
            if recurse_ok && !acyclic_repeat {
                inter.push(n.nbr);
                dfs_all(
                    ve, catalog, storage, sources, read, filter, random, memory, control, n.nbr,
                    inter, rels, out,
                )?;
                inter.pop();
            }
            rels.pop();
        }
    }
    Ok(())
}

/// BFS shortest-path enumeration: compute the minimal distance to each node (and
/// its shortest-path predecessors), then reconstruct one path (`Shortest`) or all
/// (`AllShortest`) per reachable end whose distance is in `[lower, upper]`.
#[allow(clippy::too_many_arguments)]
fn enumerate_shortest(
    ve: &VarLengthExtend,
    catalog: &Catalog,
    storage: &InMemStorage,
    sources: &QuerySourceState,
    read: StorageReadHandle,
    filter: Option<&CompiledFilter>,
    random: &RandomState,
    memory: &QueryMemory,
    control: QueryControl<'_>,
    start: InternalId,
) -> Result<Vec<EnumPath>> {
    memory.charge(
        (std::mem::size_of::<InternalId>()
            + std::mem::size_of::<u32>()
            + 3 * std::mem::size_of::<usize>()) as u64,
    )?;
    let mut dist: HashMap<InternalId, u32> = HashMap::new();
    // node → shortest-path predecessor edges `(prev_node, rel)`.
    let mut preds: HashMap<InternalId, Vec<(InternalId, InternalId)>> = HashMap::new();
    dist.insert(start, 0);
    let mut frontier = vec![start];
    let mut d = 0;
    while d < ve.upper && !frontier.is_empty() {
        control.check()?;
        let mut next = Vec::new();
        for &cur in &frontier {
            control.check()?;
            let mut neighbors = Vec::new();
            for &rt in &ve.rel_tables {
                neighbors.clear();
                extend_dispatch(
                    sources,
                    catalog,
                    storage,
                    read,
                    rt,
                    std::slice::from_ref(&cur),
                    ve.dir,
                    &mut neighbors,
                    control,
                    memory,
                )?;
                for (neighbor_index, n) in neighbors.iter().enumerate() {
                    if neighbor_index % VECTOR_CAPACITY == 0 {
                        control.check()?;
                    }
                    // Per-step filter: the relationship gate blocks the edge; the
                    // node gate blocks expanding *through* `n.nbr` (using it as an
                    // intermediate) but still allows it as a destination.
                    let mut expand_ok = true;
                    if let Some(f) = filter {
                        let binds = edge_binds(
                            f,
                            n.rel,
                            n.nbr,
                            EntityRead {
                                catalog,
                                storage,
                                read,
                                sources,
                                memory,
                                control,
                            },
                        )?;
                        if !CompiledFilter::check(&f.rel_pred, &binds, random) {
                            continue;
                        }
                        expand_ok = CompiledFilter::check(&f.node_pred, &binds, random);
                    }
                    match dist.get(&n.nbr) {
                        None => {
                            memory.charge(
                                (4 * std::mem::size_of::<InternalId>()
                                    + std::mem::size_of::<u32>()
                                    + std::mem::size_of::<Vec<(InternalId, InternalId)>>()
                                    + 4 * std::mem::size_of::<usize>())
                                    as u64,
                            )?;
                            dist.insert(n.nbr, d + 1);
                            preds.entry(n.nbr).or_default().push((cur, n.rel));
                            if expand_ok {
                                next.push(n.nbr);
                            }
                        }
                        // Another equally-short predecessor (incl. parallel edges).
                        Some(&dn) if dn == d + 1 => {
                            memory.charge((2 * std::mem::size_of::<InternalId>()) as u64)?;
                            preds.entry(n.nbr).or_default().push((cur, n.rel));
                        }
                        _ => {}
                    }
                }
            }
        }
        frontier = next;
        d += 1;
    }

    let mut out = Vec::new();
    if ve.lower == 0 {
        let path = EnumPath {
            end: start,
            node_ids: Vec::new(),
            rel_ids: Vec::new(),
            cost: None,
        };
        memory.charge(enum_path_bytes(&path))?;
        out.push(path);
    }
    for (&end, &de) in &dist {
        control.check()?;
        if de == 0 || de < ve.lower || de > ve.upper {
            continue;
        }
        match ve.mode {
            RecursiveMode::Shortest => {
                let mut nodes = Vec::new();
                let mut rels = Vec::new();
                if ve.build_value {
                    reconstruct_one(end, start, &preds, &mut nodes, &mut rels);
                }
                let path = EnumPath {
                    end,
                    node_ids: nodes,
                    rel_ids: rels,
                    cost: None,
                };
                memory.charge(enum_path_bytes(&path))?;
                out.push(path);
            }
            RecursiveMode::AllShortest => {
                let mut nodes = Vec::new();
                let mut rels = Vec::new();
                reconstruct_all(
                    end,
                    end,
                    start,
                    &preds,
                    ve.build_value,
                    memory,
                    control,
                    &mut nodes,
                    &mut rels,
                    &mut out,
                )?;
            }
            RecursiveMode::All | RecursiveMode::WShortest | RecursiveMode::AllWShortest => {
                unreachable!()
            }
        }
    }
    Ok(out)
}

/// Weighted shortest path (Dijkstra): minimize the summed edge weight (a rel
/// property) rather than the hop count. Tracks the min cost + hop count to each
/// node and the predecessor edges achieving it, then reconstructs one path
/// (`WShortest`) or all (`AllWShortest`) per reachable end within the hop
/// bounds, tagging each with its total cost (`cost(e)`). A negative weight is a
/// runtime error (Dijkstra assumes non-negative edges).
#[allow(clippy::too_many_arguments)]
fn enumerate_wshortest(
    ve: &VarLengthExtend,
    catalog: &Catalog,
    storage: &InMemStorage,
    sources: &QuerySourceState,
    read: StorageReadHandle,
    filter: Option<&CompiledFilter>,
    random: &RandomState,
    memory: &QueryMemory,
    control: QueryControl<'_>,
    start: InternalId,
) -> Result<Vec<EnumPath>> {
    memory.charge(
        (2 * std::mem::size_of::<InternalId>()
            + std::mem::size_of::<f64>()
            + std::mem::size_of::<u32>()
            + 4 * std::mem::size_of::<usize>()) as u64,
    )?;
    // The weight column NAME (resolved to a per-table column id at each edge).
    let weight_col = ve.weight.as_deref().unwrap_or("");
    // Min cost + hop count to each node; predecessors `(prev, rel)` at that cost.
    let mut dist: HashMap<InternalId, f64> = HashMap::new();
    let mut hops: HashMap<InternalId, u32> = HashMap::new();
    let mut preds: HashMap<InternalId, Vec<(InternalId, InternalId)>> = HashMap::new();
    dist.insert(start, 0.0);
    hops.insert(start, 0);
    // A simple label-correcting queue (test graphs are small): pop the current
    // minimum-cost unsettled node each round. `settled` fixes a node's cost.
    let mut settled: HashSet<InternalId> = HashSet::new();
    loop {
        control.check()?;
        // The unsettled node of least tentative cost.
        let Some((&cur, &cur_cost)) = dist
            .iter()
            .filter(|(n, _)| !settled.contains(*n))
            // Ties settle by the smaller node id (deterministic path choice —
            // C++ prefers the lower-offset predecessor: A→B→D over A→C→D).
            .min_by(|a, b| a.1.total_cmp(b.1).then_with(|| a.0.cmp(b.0)))
        else {
            break;
        };
        memory.charge(
            (std::mem::size_of::<InternalId>() + 2 * std::mem::size_of::<usize>()) as u64,
        )?;
        settled.insert(cur);
        let cur_hops = hops[&cur];
        if cur_hops >= ve.upper {
            continue;
        }
        let mut neighbors = Vec::new();
        for &rt in &ve.rel_tables {
            neighbors.clear();
            extend_dispatch(
                sources,
                catalog,
                storage,
                read,
                rt,
                std::slice::from_ref(&cur),
                ve.dir,
                &mut neighbors,
                control,
                memory,
            )?;
            let weight_column = catalog.rel_table(rt).and_then(|table| {
                table
                    .columns
                    .iter()
                    .find(|column| column.name.eq_ignore_ascii_case(weight_col))
                    .map(|column| column.column_id.0 as usize)
            });
            let weight_offsets: Vec<u64> = neighbors
                .iter()
                .map(|neighbor| neighbor.rel.offset.0)
                .collect();
            let weight_columns: Vec<usize> = weight_column.into_iter().collect();
            let weight_batches = if let Some(batches) = sources.projected_rows(
                rt,
                &weight_offsets,
                &weight_columns,
                catalog,
                control,
                memory,
            )? {
                batches
            } else {
                storage.rel_properties_batch(read, rt, &weight_offsets, &weight_columns)
            };
            memory.charge(
                ((weight_offsets.capacity() * std::mem::size_of::<u64>())
                    + (weight_columns.capacity() * std::mem::size_of::<usize>()))
                    as u64
                    + weight_batches
                        .iter()
                        .map(DataChunk::allocated_bytes)
                        .sum::<u64>(),
            )?;
            for (neighbor_index, n) in neighbors.iter().enumerate() {
                if neighbor_index % VECTOR_CAPACITY == 0 {
                    control.check()?;
                }
                // Per-step filter: rel gate blocks the edge; node gate blocks
                // using the neighbor as an intermediate (still a valid dest).
                let mut expand_ok = true;
                if let Some(f) = filter {
                    let binds = edge_binds(
                        f,
                        n.rel,
                        n.nbr,
                        EntityRead {
                            catalog,
                            storage,
                            read,
                            sources,
                            memory,
                            control,
                        },
                    )?;
                    if !CompiledFilter::check(&f.rel_pred, &binds, random) {
                        continue;
                    }
                    expand_ok = CompiledFilter::check(&f.node_pred, &binds, random);
                }
                let w = if weight_column.is_some() {
                    gathered_property(&weight_batches, neighbor_index, 0)
                        .as_f64()
                        .unwrap_or(0.0)
                } else {
                    0.0
                };
                if w < 0.0 {
                    // The error names the returned shape: WEIGHTED_SP_PATHS when
                    // the path value is assembled (`RETURN p`), else
                    // WEIGHTED_SP_DESTINATIONS (`RETURN cost(e)`, endpoints).
                    // ALL WSHORTEST always tracks paths; plain WSHORTEST names
                    // PATHS iff a named path uses this rel, else DESTINATIONS.
                    let kind = match ve.mode {
                        RecursiveMode::AllWShortest => "ALL_WEIGHTED_SP_PATHS",
                        _ if ve.in_named_path => "WEIGHTED_SP_PATHS",
                        _ => "WEIGHTED_SP_DESTINATIONS",
                    };
                    return Err(Error::runtime(format!(
                        "Found negative weight {}. This is not a supported weight for {kind}",
                        format_weight(w)
                    )));
                }
                let ncost = cur_cost + w;
                match dist.get(&n.nbr) {
                    Some(&dn) if ncost > dn + f64::EPSILON => {}
                    Some(&dn) if (ncost - dn).abs() <= f64::EPSILON => {
                        memory.charge((2 * std::mem::size_of::<InternalId>()) as u64)?;
                        // Equal-cost alternative predecessor (AllWShortest).
                        preds.entry(n.nbr).or_default().push((cur, n.rel));
                    }
                    _ => {
                        memory.charge(
                            (4 * std::mem::size_of::<InternalId>()
                                + std::mem::size_of::<f64>()
                                + std::mem::size_of::<u32>()
                                + std::mem::size_of::<Vec<(InternalId, InternalId)>>()
                                + 6 * std::mem::size_of::<usize>())
                                as u64,
                        )?;
                        // Strictly better (or first) path to the neighbor.
                        dist.insert(n.nbr, ncost);
                        hops.insert(n.nbr, cur_hops + 1);
                        preds.insert(n.nbr, vec![(cur, n.rel)]);
                        settled.remove(&n.nbr);
                        let _ = expand_ok; // node gate handled at emit below
                    }
                }
            }
        }
    }

    let mut out = Vec::new();
    if ve.lower == 0 {
        let path = EnumPath {
            end: start,
            node_ids: Vec::new(),
            rel_ids: Vec::new(),
            cost: Some(0.0),
        };
        memory.charge(enum_path_bytes(&path))?;
        out.push(path);
    }
    for (&end, &de) in &dist {
        control.check()?;
        let he = hops.get(&end).copied().unwrap_or(0);
        if end == start || he < ve.lower || he > ve.upper {
            continue;
        }
        match ve.mode {
            RecursiveMode::WShortest => {
                let mut nodes = Vec::new();
                let mut rels = Vec::new();
                if ve.build_value {
                    reconstruct_one(end, start, &preds, &mut nodes, &mut rels);
                }
                let path = EnumPath {
                    end,
                    node_ids: nodes,
                    rel_ids: rels,
                    cost: Some(de),
                };
                memory.charge(enum_path_bytes(&path))?;
                out.push(path);
            }
            RecursiveMode::AllWShortest => {
                let mut before = Vec::new();
                let mut nodes = Vec::new();
                let mut rels = Vec::new();
                reconstruct_all(
                    end,
                    end,
                    start,
                    &preds,
                    ve.build_value,
                    memory,
                    control,
                    &mut nodes,
                    &mut rels,
                    &mut before,
                )?;
                for mut p in before {
                    p.cost = Some(de);
                    out.push(p);
                }
            }
            _ => unreachable!("enumerate_wshortest on non-weighted mode"),
        }
    }
    Ok(out)
}

/// Render a weight for the negative-weight error the C++ way (an integer
/// weight prints without a decimal, e.g. `-1`).
fn format_weight(w: f64) -> String {
    if w.fract() == 0.0 {
        format!("{}", w as i64)
    } else {
        format!("{w}")
    }
}

/// Reconstruct one shortest path to `end` (following the first predecessor),
/// filling intermediate node ids and rel ids in forward order.
fn reconstruct_one(
    end: InternalId,
    start: InternalId,
    preds: &HashMap<InternalId, Vec<(InternalId, InternalId)>>,
    nodes: &mut Vec<InternalId>,
    rels: &mut Vec<InternalId>,
) {
    let mut cur = end;
    while let Some(ps) = preds.get(&cur) {
        let (prev, rel) = ps[0];
        rels.push(rel);
        cur = prev;
        if cur != start {
            nodes.push(cur);
        }
    }
    rels.reverse();
    nodes.reverse();
}

/// Reconstruct *all* shortest paths to `end` (DFS over the predecessor multimap),
/// pushing one [`EnumPath`] per path. `nodes`/`rels` are the in-progress reverse
/// accumulators.
#[allow(clippy::too_many_arguments)]
fn reconstruct_all(
    cur: InternalId,
    end: InternalId,
    start: InternalId,
    preds: &HashMap<InternalId, Vec<(InternalId, InternalId)>>,
    build_value: bool,
    memory: &QueryMemory,
    control: QueryControl<'_>,
    nodes: &mut Vec<InternalId>,
    rels: &mut Vec<InternalId>,
    out: &mut Vec<EnumPath>,
) -> Result<()> {
    control.check()?;
    if cur == start {
        let (node_ids, rel_ids) = if build_value {
            let mut n = nodes.clone();
            n.reverse();
            let mut r = rels.clone();
            r.reverse();
            (n, r)
        } else {
            (Vec::new(), Vec::new())
        };
        let path = EnumPath {
            end,
            node_ids,
            rel_ids,
            cost: None,
        };
        memory.charge(enum_path_bytes(&path))?;
        out.push(path);
        return Ok(());
    }
    let Some(ps) = preds.get(&cur) else {
        return Ok(());
    };
    for &(prev, rel) in ps {
        rels.push(rel);
        let is_inter = prev != start;
        if is_inter {
            nodes.push(prev);
        }
        reconstruct_all(
            prev,
            end,
            start,
            preds,
            build_value,
            memory,
            control,
            nodes,
            rels,
            out,
        )?;
        if is_inter {
            nodes.pop();
        }
        rels.pop();
    }
    Ok(())
}

/// Build the path value for one row, or `Null` if any required endpoint is absent
/// (e.g. an unmatched `OPTIONAL MATCH`).
fn assemble_path(
    pp: &ProjectPath,
    layout: &RowLayout,
    chunk: &DataChunk,
    pos: usize,
    entity: EntityRead<'_>,
) -> Result<Value> {
    let Some(head) = assemble_node_opt(read_var_id(pp.head, layout, chunk, pos), entity)? else {
        return Ok(Value::Null);
    };
    let mut nodes = vec![head];
    let mut rels = Vec::new();
    for seg in &pp.segments {
        let end_id = read_var_id(seg.to_node, layout, chunk, pos);
        match &seg.rel {
            PathRel::Recursive { value_col } => {
                match chunk.columns[*value_col].get_value(pos) {
                    Value::RecursiveRel(rr) => {
                        // A zero-length segment contributes neither rels nor an end.
                        if !rr.rels.is_empty() {
                            nodes.extend(rr.nodes.iter().cloned());
                            let Some(end) = assemble_node_opt(end_id, entity)? else {
                                return Ok(Value::Null);
                            };
                            nodes.push(end);
                            rels.extend(rr.rels.iter().cloned());
                        }
                    }
                    _ => return Ok(Value::Null),
                }
            }
            PathRel::Single { rel } => {
                let rel_id = read_var_id(*rel, layout, chunk, pos);
                if rel_id.table_id.0 == u64::MAX {
                    return Ok(Value::Null);
                }
                rels.push(assemble_rel_value(rel_id, entity)?);
                let Some(end) = assemble_node_opt(end_id, entity)? else {
                    return Ok(Value::Null);
                };
                nodes.push(end);
            }
        }
    }
    Ok(Value::RecursiveRel(Box::new(RecursiveRelValue {
        nodes,
        rels,
        degenerate: false,
        cost: None,
        null_nodes: 0,
    })))
}

// ---------------------------------------------------------------------------
// Morsel-driven parallel execution (P3 step 9)
// ---------------------------------------------------------------------------

/// Below this many candidate rows a scan runs serially — the per-query thread spawn
/// + merge would dominate a small scan.
const PARALLEL_MIN_ROWS: u64 = 4 * VECTOR_CAPACITY as u64;

/// One unit of parallel scan work: a single table's `[start, end)` offset slice.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Morsel {
    table_idx: usize,
    start: u64,
    end: u64,
}

/// Slice each candidate table's `[0, count)` offset space into `morsel_rows`-sized
/// morsels, in scan order (table 0 first). An empty table contributes none.
fn slice_morsels(counts: &[u64], morsel_rows: u64) -> Vec<Morsel> {
    let morsel_rows = morsel_rows.max(1);
    let mut morsels = Vec::new();
    for (table_idx, &count) in counts.iter().enumerate() {
        let mut start = 0;
        while start < count {
            let end = (start + morsel_rows).min(count);
            morsels.push(Morsel {
                table_idx,
                start,
                end,
            });
            start = end;
        }
    }
    morsels
}

/// Hands a driving `ScanNode`'s precomputed morsels to workers via a lock-free atomic
/// cursor. Morsels are in scan order (table 0, then table 1, …, each sliced by
/// offset), so a morsel's index *is* its slice's position in a serial scan — merging
/// partials in morsel-index order reproduces serial order exactly.
fn scan_node_count(storage: &InMemStorage, sources: &QuerySourceState, table: TableId) -> u64 {
    sources
        .num_rows(table)
        .unwrap_or_else(|| storage.node_count(table))
}

struct ScanDispatcher {
    morsels: Vec<Morsel>,
    next: std::sync::atomic::AtomicUsize,
}

impl ScanDispatcher {
    /// Slice each candidate table's `[0, node_count)` into `morsel_rows`-sized
    /// morsels (tombstoned offsets inside a slice are skipped at scan time; an empty
    /// table contributes none).
    fn new(
        scan: &ScanNode,
        storage: &InMemStorage,
        sources: &QuerySourceState,
        morsel_rows: u64,
    ) -> Self {
        let counts: Vec<u64> = scan
            .tables
            .iter()
            .map(|st| scan_node_count(storage, sources, st.table))
            .collect();
        Self {
            morsels: slice_morsels(&counts, morsel_rows),
            next: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    /// The next `(morsel_index, morsel)`, or `None` when exhausted.
    fn next_morsel(&self) -> Option<(usize, Morsel)> {
        let i = self.next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.morsels.get(i).map(|m| (i, *m))
    }
}

/// Target ~`4 × threads` morsels (good load balance), each ≥ `min_morsel` rows.
/// `min_morsel` is normally one vector wide; for a small scan feeding a heavy
/// fan-out (P3 step 10b L4) it is lowered so the scan still splits across workers
/// (a 1.7K-row scan under a 2K floor would be one morsel = no parallelism).
fn morsel_rows_for(
    scan: &ScanNode,
    storage: &InMemStorage,
    sources: &QuerySourceState,
    threads: usize,
    min_morsel: u64,
) -> u64 {
    let total: u64 = scan
        .tables
        .iter()
        .map(|table| scan_node_count(storage, sources, table.table))
        .sum();
    let target = (threads as u64 * 4).max(1);
    (total / target).max(min_morsel.max(1))
}

/// Run `f` over every morsel across `threads` scoped workers, returning each result
/// tagged with its morsel index (sorted ascending). On error, returns the
/// **lowest-morsel-index** error — the one a serial run would hit first — so error
/// messages stay byte-identical too. (Workers pull morsel indices monotonically, so a
/// worker's first error is its lowest; a global flag stops fetching new morsels once
/// any worker fails, but in-flight lower-index morsels still finish and are compared.)
fn run_morsels<T, F>(threads: usize, dispatcher: &ScanDispatcher, f: F) -> Result<Vec<(usize, T)>>
where
    F: Fn(Morsel) -> Result<T> + Sync,
    T: Send,
{
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, Ordering};

    let results: Mutex<Vec<(usize, T)>> = Mutex::new(Vec::new());
    let err: Mutex<Option<(usize, Error)>> = Mutex::new(None);
    let failed = AtomicBool::new(false);

    std::thread::scope(|s| {
        for _ in 0..threads {
            s.spawn(|| {
                let mut local: Vec<(usize, T)> = Vec::new();
                while !failed.load(Ordering::Relaxed) {
                    let Some((mi, m)) = dispatcher.next_morsel() else {
                        break;
                    };
                    match f(m) {
                        Ok(t) => local.push((mi, t)),
                        Err(e) => {
                            let mut g = err.lock().unwrap();
                            if g.as_ref().is_none_or(|(j, _)| mi < *j) {
                                *g = Some((mi, e));
                            }
                            failed.store(true, Ordering::Relaxed);
                            break;
                        }
                    }
                }
                results.lock().unwrap().extend(local);
            });
        }
    });

    if let Some((_, e)) = err.into_inner().unwrap() {
        return Err(e);
    }
    let mut out = results.into_inner().unwrap();
    out.sort_by_key(|(mi, _)| *mi);
    Ok(out)
}

/// The driving `ScanNode` to morsel-parallelize this read part over, or `None` to run
/// serially. See `docs/PARALLELISM_PLAN.md` for the full gate.
fn parallel_scan_source<'a>(
    root: &'a PlanOp,
    projection: &BoundProjection,
    ctx: &Ctx<'_>,
) -> Option<&'a ScanNode> {
    if ctx.execution.worker_count <= 1 || !projection_parallel_safe(projection) {
        return None;
    }
    // A LIMIT with no aggregate/order/distinct already stops early when serial; don't
    // over-scan it in parallel.
    if !projection.has_aggregates()
        && projection.order_by.is_empty()
        && !projection.distinct
        && projection.limit.is_some()
    {
        return None;
    }
    let scan = spine_scan(root)?;
    let rows: u64 = scan
        .tables
        .iter()
        .map(|table| scan_node_count(ctx.storage, ctx.execution.sources, table.table))
        .sum();
    // Parallelize when the driving scan is large (the step-9 gate) OR when a smaller
    // scan feeds a heavy fan-out whose *effective* work clears the bar (P3 step 10b
    // L4 — parallelism over the intermediate, not just the scan). The fan-out case
    // still needs enough scan rows to hand every worker a morsel, else the split
    // can't use the cores.
    if rows >= PARALLEL_MIN_ROWS {
        return Some(scan);
    }
    let steps = spine_fanout_steps(root);
    let effective = rows as f64 * ASSUMED_FANOUT.powi(steps as i32);
    let enough_to_split = (ctx.execution.worker_count as u64) * 2;
    (steps >= 1 && rows >= enough_to_split && effective >= PARALLEL_MIN_ROWS as f64).then_some(scan)
}

/// The leaf `ScanNode` of a **linear stateless spine** (only `Filter`/`Extend`/
/// `VarLengthExtend`/`ProjectPath`/`Unwind` above it), or `None` if `root` branches,
/// is stateful/correlated, or its leaf is not a `ScanNode`. These ops carry no
/// cross-row state, so a per-morsel pipeline is correct with no redundant work.
fn spine_scan(op: &PlanOp) -> Option<&ScanNode> {
    match op {
        PlanOp::ScanNode(scan) => Some(scan),
        PlanOp::Filter { input, .. } | PlanOp::Unwind { input, .. } => spine_scan(input),
        PlanOp::Extend(e) => spine_scan(&e.input),
        PlanOp::VarLengthExtend(ve) => spine_scan(&ve.input),
        PlanOp::ProjectPath(pp) => spine_scan(&pp.input),
        _ => None,
    }
}

/// The number of fan-out steps (`Extend`/`VarLengthExtend`/`Unwind`) on the linear
/// spine — a proxy for how much work each driving-scan row generates. A small scan
/// with a heavy fan-out (lsqb q6: 1.7K Person, three extends) does enormous work that
/// the per-row morsel split parallelizes even though the scan is below the row gate.
fn spine_fanout_steps(op: &PlanOp) -> usize {
    match op {
        PlanOp::Extend(e) => 1 + spine_fanout_steps(&e.input),
        PlanOp::VarLengthExtend(ve) => 1 + spine_fanout_steps(&ve.input),
        PlanOp::Unwind { input, .. } => 1 + spine_fanout_steps(input),
        PlanOp::Filter { input, .. } => spine_fanout_steps(input),
        PlanOp::ProjectPath(pp) => spine_fanout_steps(&pp.input),
        _ => 0,
    }
}

/// Assumed per-extend fan-out for the effective-work estimate (deliberately modest;
/// it only decides whether a small-scan/heavy-fan-out query is worth parallelizing).
const ASSUMED_FANOUT: f64 = 3.0;

/// Whether every aggregate in the projection merges **bit-identically** across
/// morsels: no `DISTINCT` (cross-morsel dedup is order-sensitive) and no `SUM`/`AVG`
/// over a floating-point argument (f64 addition is non-associative, so a partitioned
/// float sum would diverge from the serial left-to-right sum). Integer `SUM`/`AVG`
/// (exact `i128`), `COUNT`, `MIN`/`MAX`, `COLLECT` are all safe.
fn projection_parallel_safe(projection: &BoundProjection) -> bool {
    projection.items.iter().all(|item| match item {
        ProjItem::Var { .. } => true,
        ProjItem::Scalar { expr, .. } => aggs_parallel_safe(expr),
    })
}

fn is_float_type(ty: &LogicalType) -> bool {
    matches!(
        ty,
        LogicalType::Double | LogicalType::Float | LogicalType::Decimal(..)
    )
}

fn aggs_parallel_safe(expr: &BoundExpr) -> bool {
    match expr {
        BoundExpr::Aggregate {
            op, distinct, arg, ..
        } => {
            if *distinct {
                return false;
            }
            if matches!(op, AggOp::Sum | AggOp::Avg)
                && arg.as_ref().is_some_and(|a| is_float_type(&a.ty()))
            {
                return false;
            }
            arg.as_ref().is_none_or(|a| aggs_parallel_safe(a))
        }
        BoundExpr::Scalar { args, .. } | BoundExpr::Call { args, .. } => {
            args.iter().all(aggs_parallel_safe)
        }
        BoundExpr::List { elems, .. } => elems.iter().all(aggs_parallel_safe),
        BoundExpr::Cast { expr, .. } | BoundExpr::ValueProperty { value: expr, .. } => {
            aggs_parallel_safe(expr)
        }
        BoundExpr::Struct { fields, .. } => fields.iter().all(|(_, v)| aggs_parallel_safe(v)),
        BoundExpr::ListLambda { list, body, .. } => {
            aggs_parallel_safe(list) && aggs_parallel_safe(body)
        }
        BoundExpr::Case {
            operand,
            branches,
            else_,
            ..
        } => {
            operand.as_ref().is_none_or(|o| aggs_parallel_safe(o))
                && branches
                    .iter()
                    .all(|(c, r)| aggs_parallel_safe(c) && aggs_parallel_safe(r))
                && else_.as_ref().is_none_or(|e| aggs_parallel_safe(e))
        }
        // No aggregate inside these (Literal/Property/NodeRef/ScalarVar/LambdaVar/
        // Subquery/SequenceCall).
        _ => true,
    }
}

/// Produce a read part's results: morsel-parallel when the gate passes, else the
/// serial streamed pull. (Write parts are a serial breaker handled in `execute`.)
fn read_part_results<'a>(
    projection: &BoundProjection,
    part_plan: &'a PartPlan,
    ctx: &Ctx<'a>,
    input: &'a [DataChunk],
) -> Result<ExecResult> {
    if let Some(scan) = parallel_scan_source(&part_plan.root, projection, ctx) {
        return parallel_results(projection, &part_plan.root, scan, ctx, input);
    }
    let mut root = build_exec(&part_plan.root, ctx, input)?;
    produce_results(projection, &mut root, ctx)
}

/// Output column names for a projection (shared by the serial + parallel sinks).
fn projection_column_names(projection: &BoundProjection) -> Vec<String> {
    projection
        .items
        .iter()
        .map(|i| match i {
            ProjItem::Scalar { name, .. } | ProjItem::Var { name, .. } => name.clone(),
        })
        .collect()
}

fn projection_column_types(projection: &BoundProjection, layout: &RowLayout) -> Vec<LogicalType> {
    projection
        .items
        .iter()
        .map(|item| match item {
            ProjItem::Scalar { expr, .. } => expr.ty(),
            ProjItem::Var { var, .. } => {
                let columns = layout.var(*var);
                match columns.kind {
                    VarColKind::Node {
                        table: Some(table), ..
                    } => LogicalType::Node(table),
                    VarColKind::Rel {
                        table: Some(table), ..
                    } => LogicalType::Rel(table),
                    VarColKind::Node { table: None, .. } | VarColKind::Rel { table: None, .. } => {
                        LogicalType::Any
                    }
                    VarColKind::Scalar => layout.col_types[columns.id_col].clone(),
                }
            }
        })
        .collect()
}

/// Morsel-parallel result production: partition the driving scan, run a bounded
/// pipeline per morsel, merge typed output buffers in serial scan order, then
/// apply DISTINCT/ORDER BY/SKIP/LIMIT through columnar row references.
fn parallel_results<'a>(
    projection: &BoundProjection,
    root: &'a PlanOp,
    scan: &ScanNode,
    ctx: &Ctx<'a>,
    input: &'a [DataChunk],
) -> Result<ExecResult> {
    let threads = ctx.execution.worker_count.max(1);
    // A small scan only got here via the heavy-fan-out path, so let it split below the
    // one-vector floor; a large scan keeps the vector-wide morsel.
    let scan_rows: u64 = scan
        .tables
        .iter()
        .map(|table| scan_node_count(ctx.storage, ctx.execution.sources, table.table))
        .sum();
    let min_morsel = if scan_rows >= PARALLEL_MIN_ROWS {
        VECTOR_CAPACITY as u64
    } else {
        1
    };
    let morsel_rows = morsel_rows_for(
        scan,
        ctx.storage,
        ctx.execution.sources,
        threads,
        min_morsel,
    );
    let dispatcher = ScanDispatcher::new(scan, ctx.storage, ctx.execution.sources, morsel_rows);

    let output = if projection.has_aggregates() {
        let plan = AggPlan::build(projection, ctx.layout)?;
        let partials = run_morsels(threads, &dispatcher, |morsel| {
            let mut exec = build_exec_morsel(
                root,
                ctx,
                input,
                Some((morsel.table_idx, morsel.start, morsel.end)),
            )?;
            accumulate_groups(&plan, &mut exec, ctx)
        })?;
        let mut global = AggPartial {
            groups: HashMap::new(),
            order: Vec::new(),
        };
        for (_, partial) in partials {
            merge_partial(&mut global, partial);
        }
        emit_groups(&plan, global, projection, ctx)?
    } else {
        let blocks = run_morsels(threads, &dispatcher, |morsel| {
            let mut exec = build_exec_morsel(
                root,
                ctx,
                input,
                Some((morsel.table_idx, morsel.start, morsel.end)),
            )?;
            project(projection, &mut exec, ctx, None)
        })?;
        let mut output = OutputBuffer::new(
            projection_column_types(projection, ctx.layout),
            !projection.order_by.is_empty(),
        );
        for (_, block) in blocks {
            output.append(block, ctx.execution.memory)?;
        }
        output
    };
    finish_projection(
        output,
        projection_column_names(projection),
        projection,
        ctx.execution.memory,
    )
}

// ---------------------------------------------------------------------------
// Result production (projection / aggregation)
// ---------------------------------------------------------------------------

fn produce_results<'a>(
    projection: &BoundProjection,
    root: &mut Exec<'a>,
    ctx: &Ctx<'a>,
) -> Result<ExecResult> {
    let column_names = projection_column_names(projection);
    let output = if projection.has_aggregates() {
        // Aggregation/grouping is a pipeline breaker: drain fully.
        aggregate(projection, root, ctx)?
    } else {
        // Without `ORDER BY`/`DISTINCT` a `LIMIT` lets us stop pulling once
        // `SKIP + LIMIT` rows are collected (the streaming early-termination win);
        // sort/dedup need every row, so they drain fully.
        let early = if projection.order_by.is_empty() && !projection.distinct {
            match fold_skip_limit(projection.limit.as_ref())? {
                Some(limit) => Some(
                    fold_skip_limit(projection.skip.as_ref())?.unwrap_or(0) as usize
                        + limit as usize,
                ),
                None => None,
            }
        } else {
            None
        };
        project(projection, root, ctx, early)?
    };
    finish_projection(output, column_names, projection, ctx.execution.memory)
}

fn finish_projection(
    output: OutputBuffer,
    column_names: Vec<String>,
    projection: &BoundProjection,
    memory: &QueryMemory,
) -> Result<ExecResult> {
    let order_ascending = projection
        .order_by
        .iter()
        .map(|(_, ascending)| *ascending)
        .collect::<Vec<_>>();
    let skip = fold_skip_limit(projection.skip.as_ref())?.unwrap_or(0) as usize;
    let limit = fold_skip_limit(projection.limit.as_ref())?.map(|value| value as usize);
    output.finish(
        column_names,
        projection.distinct,
        &order_ascending,
        skip,
        limit,
        memory,
    )
}

/// Compile each projection item for the non-aggregate path.
enum ItemExec {
    Var(VarId),
    Scalar(CompiledExpr),
}

fn project<'a>(
    projection: &BoundProjection,
    root: &mut Exec<'a>,
    ctx: &Ctx<'a>,
    early_target: Option<usize>,
) -> Result<OutputBuffer> {
    let layout = ctx.layout;
    let resolver = LayoutResolver(layout);
    let items: Vec<ItemExec> = projection
        .items
        .iter()
        .map(|item| match item {
            ProjItem::Var { var, .. } => Ok(ItemExec::Var(*var)),
            ProjItem::Scalar { expr, .. } => Ok(ItemExec::Scalar(compile(expr, &resolver)?)),
        })
        .collect::<Result<_>>()?;
    let deep_types = output_deep_types(projection);

    // ORDER BY keys: an output-column reference, an input expression, or a
    // post-projection expression over already-produced output columns.
    enum OrderExec {
        Output(usize),
        Expr(CompiledExpr),
        Post(BoundExpr),
    }
    let orders: Vec<(OrderExec, bool)> = projection
        .order_by
        .iter()
        .map(|(key, ascending)| {
            let execution = match key {
                OrderKey::Output(index) => OrderExec::Output(*index),
                OrderKey::Expr(expr) => OrderExec::Expr(compile(expr, &resolver)?),
                OrderKey::PostProjection(expr) => OrderExec::Post(expr.clone()),
            };
            Ok((execution, *ascending))
        })
        .collect::<Result<_>>()?;

    let mut output = OutputBuffer::new(
        projection_column_types(projection, layout),
        !orders.is_empty(),
    );
    'pull: while let Some(chunk) = root.next_chunk(ctx)? {
        for position in chunk.sel.iter() {
            let mut values = Vec::with_capacity(items.len());
            for item in &items {
                values.push(match item {
                    ItemExec::Var(var) => {
                        assemble_var(*var, layout, &chunk, position, EntityRead::from_ctx(ctx))?
                    }
                    ItemExec::Scalar(expr) => expr.eval(&chunk, position, ctx.execution.random)?,
                });
            }
            let order_keys = orders
                .iter()
                .map(|(execution, _)| match execution {
                    OrderExec::Output(index) => Ok(values[*index].clone()),
                    OrderExec::Expr(expr) => expr.eval(&chunk, position, ctx.execution.random),
                    OrderExec::Post(expr) => eval_output_expr(expr, &values, ctx.execution.random),
                })
                .collect::<Result<_>>()?;
            deep_materialize_values(&mut values, &deep_types, ctx)?;
            ctx.execution
                .memory
                .charge(output.push(values, order_keys))?;
            // Early-termination: stop once `SKIP + LIMIT` rows are collected.
            if early_target.is_some_and(|target| output.len() >= target) {
                break 'pull;
            }
        }
    }
    Ok(output)
}

/// Classification of a projection item for the aggregate path.
enum GroupItem {
    /// A grouping key that is a whole node/rel variable (keyed by id).
    Var(VarId),
    /// A grouping key that is a scalar expression.
    Scalar(CompiledExpr),
    /// An aggregate output expression (its `Agg` leaves index into `aggs`).
    Agg(CompiledExpr),
}

struct GroupData {
    key_values: Vec<Value>,
    states: Vec<AggState>,
}

/// The compiled shape of an aggregating projection, reused by the serial path and by
/// every parallel morsel: which output items are grouping keys vs aggregates, the
/// lifted aggregate specs, and the grouping-key item indices. Built once; shared by
/// `&` across morsel workers (`CompiledExpr` is `Sync` — its only interior mutability
/// is a per-thread lambda stack).
struct AggPlan {
    item_execs: Vec<GroupItem>,
    aggs: Vec<AggSpec>,
    /// Indices (into `item_execs`) of the grouping-key items.
    group_keys: Vec<usize>,
}

impl AggPlan {
    fn build(projection: &BoundProjection, layout: &RowLayout) -> Result<AggPlan> {
        let resolver = LayoutResolver(layout);
        let mut aggs: Vec<AggSpec> = Vec::new();
        let mut group_keys: Vec<usize> = Vec::new();
        let mut item_execs: Vec<GroupItem> = Vec::with_capacity(projection.items.len());
        for (idx, item) in projection.items.iter().enumerate() {
            match item {
                ProjItem::Var { var, .. } => {
                    item_execs.push(GroupItem::Var(*var));
                    group_keys.push(idx);
                }
                ProjItem::Scalar { expr, .. } => {
                    if expr.contains_aggregate() {
                        let ce = compile_collect(expr, &resolver, &mut aggs)?;
                        item_execs.push(GroupItem::Agg(ce));
                    } else {
                        item_execs.push(GroupItem::Scalar(compile(expr, &resolver)?));
                        group_keys.push(idx);
                    }
                }
            }
        }
        Ok(AggPlan {
            item_execs,
            aggs,
            group_keys,
        })
    }

    fn new_states(&self) -> Vec<AggState> {
        self.aggs
            .iter()
            .map(|s| AggState::new(s.op, s.distinct))
            .collect()
    }
}

/// One pipeline's partial aggregate: its groups plus the first-seen key order.
struct AggPartial {
    groups: HashMap<Vec<ValueKey>, GroupData>,
    order: Vec<Vec<ValueKey>>,
}

/// Global (no grouping-key) aggregate. There is exactly one state vector, so avoid
/// constructing and hashing an empty `Vec<ValueKey>` for every input row. The common
/// analytical `count(*)` shape further reduces each chunk to its multiplicity sum.
fn accumulate_global_group<'a>(
    plan: &AggPlan,
    root: &mut Exec<'a>,
    ctx: &Ctx<'a>,
) -> Result<AggPartial> {
    let mut states = plan.new_states();
    let count_star = matches!(
        plan.aggs.as_slice(),
        [spec] if spec.op == AggOp::Count && !spec.distinct && spec.arg.is_none()
    );
    if count_star {
        let mut count = 0u64;
        while let Some(chunk) = root.next_chunk(ctx)? {
            for pos in chunk.sel.iter() {
                count = count.saturating_add(chunk.multiplicity(pos));
            }
        }
        states[0].update_n(&Value::Null, count);
    } else {
        while let Some(chunk) = root.next_chunk(ctx)? {
            for pos in chunk.sel.iter() {
                let multiplicity = chunk.multiplicity(pos);
                for (index, spec) in plan.aggs.iter().enumerate() {
                    let value = match &spec.arg {
                        Some(expr) => expr.eval(&chunk, pos, ctx.execution.random)?,
                        None => Value::Null,
                    };
                    ctx.execution
                        .memory
                        .charge(states[index].reservation_bytes_for_update(&value, multiplicity))?;
                    states[index].update_n(&value, multiplicity);
                }
            }
        }
    }
    ctx.execution.memory.charge(
        (states.capacity() * std::mem::size_of::<AggState>()
            + std::mem::size_of::<GroupData>()
            + 2 * std::mem::size_of::<Vec<ValueKey>>()) as u64,
    )?;
    let key = Vec::new();
    let mut groups = HashMap::with_capacity(1);
    groups.insert(
        key.clone(),
        GroupData {
            key_values: Vec::new(),
            states,
        },
    );
    Ok(AggPartial {
        groups,
        order: vec![key],
    })
}

/// Accumulate everything `root` produces into a fresh partial under `plan` — the
/// per-pipeline aggregate loop, shared by the serial sink and each parallel morsel.
fn accumulate_groups<'a>(plan: &AggPlan, root: &mut Exec<'a>, ctx: &Ctx<'a>) -> Result<AggPartial> {
    if plan.group_keys.is_empty() {
        return accumulate_global_group(plan, root, ctx);
    }
    let layout = ctx.layout;
    let mut groups: HashMap<Vec<ValueKey>, GroupData> = HashMap::new();
    let mut order: Vec<Vec<ValueKey>> = Vec::new(); // first-seen order

    while let Some(chunk) = root.next_chunk(ctx)? {
        for pos in chunk.sel.iter() {
            // Factorization (P3 step 6): a row may stand for `m` logical tuples when a
            // collapsible fan-out suffix was folded into a count rather than
            // materialized; the aggregate folds each value with that weight.
            let m = chunk.multiplicity(pos);
            // Compute the group key from the grouping items, in item order.
            let mut key = Vec::with_capacity(plan.group_keys.len());
            let mut key_values = Vec::with_capacity(plan.group_keys.len());
            for &gi in &plan.group_keys {
                let v = match &plan.item_execs[gi] {
                    GroupItem::Var(var) => {
                        Value::InternalId(read_var_id(*var, layout, &chunk, pos))
                    }
                    GroupItem::Scalar(ce) => ce.eval(&chunk, pos, ctx.execution.random)?,
                    GroupItem::Agg(_) => unreachable!(),
                };
                key.push(ValueKey::from_value(&v));
                key_values.push(v);
            }
            let entry = match groups.entry(key) {
                std::collections::hash_map::Entry::Occupied(entry) => entry.into_mut(),
                std::collections::hash_map::Entry::Vacant(entry) => {
                    let states = plan.new_states();
                    let key_bytes = (entry.key().capacity() * std::mem::size_of::<ValueKey>())
                        as u64
                        + entry.key().iter().map(ValueKey::heap_bytes).sum::<u64>();
                    let key_value_bytes = (key_values.capacity() * std::mem::size_of::<Value>())
                        as u64
                        + key_values.iter().map(value_payload_bytes).sum::<u64>();
                    let state_bytes = (states.capacity() * std::mem::size_of::<AggState>()) as u64;
                    let retained_bytes = key_bytes
                        .saturating_mul(2)
                        .saturating_add(key_value_bytes)
                        .saturating_add(state_bytes)
                        .saturating_add(
                            (std::mem::size_of::<GroupData>()
                                + std::mem::size_of::<Vec<ValueKey>>()
                                + 2 * std::mem::size_of::<usize>())
                                as u64,
                        );
                    ctx.execution.memory.charge(retained_bytes)?;
                    order.push(entry.key().clone());
                    entry.insert(GroupData { key_values, states })
                }
            };
            for (i, spec) in plan.aggs.iter().enumerate() {
                let v = match &spec.arg {
                    Some(ce) => ce.eval(&chunk, pos, ctx.execution.random)?,
                    None => Value::Null, // count(*)
                };
                ctx.execution
                    .memory
                    .charge(entry.states[i].reservation_bytes_for_update(&v, m))?;
                entry.states[i].update_n(&v, m);
            }
        }
    }
    Ok(AggPartial { groups, order })
}

/// Fold a morsel's partial into the global accumulator, preserving **first-seen key
/// order**: because the caller merges partials in morsel-index (scan) order, a key's
/// global position is where a serial scan would first encounter it — so the emitted
/// group order is byte-identical to serial. Same-key accumulators combine via
/// [`AggState::merge`].
fn merge_partial(global: &mut AggPartial, partial: AggPartial) {
    let AggPartial { mut groups, order } = partial;
    for key in order {
        let pdata = groups.remove(&key).expect("ordered key is present");
        match global.groups.get_mut(&key) {
            Some(g) => {
                for (gs, ps) in g.states.iter_mut().zip(pdata.states) {
                    gs.merge(ps);
                }
            }
            None => {
                global.order.push(key.clone());
                global.groups.insert(key, pdata);
            }
        }
    }
}

/// Emit one typed output row per group (the shared tail of the serial and
/// parallel aggregate). Consumes the accumulated partial.
fn emit_groups<'a>(
    plan: &AggPlan,
    partial: AggPartial,
    projection: &BoundProjection,
    ctx: &Ctx<'a>,
) -> Result<OutputBuffer> {
    let AggPartial {
        mut groups,
        mut order,
    } = partial;
    let layout = ctx.layout;

    // A global aggregate over zero rows still emits exactly one row.
    if order.is_empty() && plan.group_keys.is_empty() {
        order.push(Vec::new());
        groups.insert(
            Vec::new(),
            GroupData {
                key_values: Vec::new(),
                states: plan.new_states(),
            },
        );
    }

    let dummy = DataChunk::new(&[]);
    let mut output = OutputBuffer::new(
        projection_column_types(projection, layout),
        !projection.order_by.is_empty(),
    );
    let deep_types = output_deep_types(projection);
    for key in &order {
        let data = groups.remove(key).unwrap();
        let agg_values: Vec<Value> = data
            .states
            .into_iter()
            .map(|s| s.finalize())
            .collect::<Result<_>>()?;
        let mut key_iter = data.key_values.into_iter();
        let mut values = Vec::with_capacity(plan.item_execs.len());
        for item in &plan.item_execs {
            values.push(match item {
                GroupItem::Var(var) => {
                    // Reconstruct the node/rel from its grouped id.
                    let id = match key_iter.next() {
                        Some(Value::InternalId(id)) => id,
                        other => {
                            return Err(Error::runtime(format!(
                                "internal: group key for variable was not an id ({other:?})"
                            )));
                        }
                    };
                    assemble_id(*var, id, layout, EntityRead::from_ctx(ctx))?
                }
                GroupItem::Scalar(_) => key_iter.next().unwrap(),
                GroupItem::Agg(ce) => {
                    ce.eval_with_aggs(&dummy, 0, &agg_values, ctx.execution.random)?
                }
            });
        }
        // ORDER BY for the aggregate path references output columns only.
        let order_keys = order_keys_from_output(projection, &values, ctx.execution.random)?;
        deep_materialize_values(&mut values, &deep_types, ctx)?;
        ctx.execution
            .memory
            .charge(output.push(values, order_keys))?;
    }
    Ok(output)
}

/// Serial aggregate (a pipeline breaker): accumulate the whole pipeline, then emit.
fn aggregate<'a>(
    projection: &BoundProjection,
    root: &mut Exec<'a>,
    ctx: &Ctx<'a>,
) -> Result<OutputBuffer> {
    let plan = AggPlan::build(projection, ctx.layout)?;
    let partial = accumulate_groups(&plan, root, ctx)?;
    emit_groups(&plan, partial, projection, ctx)
}

/// Materialize a stored property into layout column type `ty`: a promoted
/// polymorphic column (heterogeneous multi-label property, e.g. INT64+DOUBLE →
/// DOUBLE) casts each table's raw value up; homogeneous columns pass through.
fn promote_prop(v: Value, ty: &LogicalType) -> Value {
    if v.is_null() || matches!(ty, LogicalType::Any) || v.logical_type() == *ty {
        return v;
    }
    let raw = v.clone();
    cast_value(&v, ty).unwrap_or(raw)
}

fn eval_output_expr(expr: &BoundExpr, values: &[Value], random: &RandomState) -> Result<Value> {
    match expr {
        BoundExpr::Literal(v) => Ok(v.clone()),
        BoundExpr::Parameter { name, .. } => Err(Error::binder(format!(
            "symbolic parameter ${name} cannot be evaluated during execution"
        ))),
        BoundExpr::Column { col, .. } => values
            .get(*col)
            .cloned()
            .ok_or_else(|| Error::runtime(format!("ORDER BY output column {col} is out of range"))),
        BoundExpr::ValueProperty { value, prop, .. } => {
            let base = eval_output_expr(value, values, random)?;
            eval_scalar_func_with_context(
                "struct_extract",
                &[base, Value::String(prop.clone())],
                random,
            )
        }
        BoundExpr::Scalar { op, args, .. } => {
            let vals = args
                .iter()
                .map(|a| eval_output_expr(a, values, random))
                .collect::<Result<Vec<_>>>()?;
            eval_scalar(*op, &vals)
        }
        BoundExpr::Cast { expr, target } => {
            let v = eval_output_expr(expr, values, random)?;
            // C++ resolves STRUCT→STRUCT casts against the DECLARED source
            // type: a field-name mismatch reports the static shapes even when
            // a field's value is NULL (cast_value only sees value-level types).
            if let (LogicalType::Struct(sf), LogicalType::Struct(tf)) = (&expr.ty(), target) {
                let names_match = sf.len() == tf.len()
                    && sf
                        .iter()
                        .zip(tf)
                        .all(|((sn, _), (tn, _))| sn.eq_ignore_ascii_case(tn));
                if !names_match {
                    return Err(Error::conversion(format!(
                        "Unsupported casting function from {} to {}.",
                        expr.ty(),
                        target
                    )));
                }
            }
            cast_value(&v, target)
        }
        BoundExpr::Call { name, args, .. } if name == "typeof" && args.len() == 1 => {
            let _ = eval_output_expr(&args[0], values, random)?;
            Ok(Value::String(koko_function::scalarfn::typeof_type_name(
                &args[0].ty(),
            )))
        }
        // `union_value` is constructed here (not in the scalar evaluator): the active
        // member's *name* lives in the bound UNION type, not in the payload value, so
        // the tagged value is built by pairing the bound type with the evaluated arg.
        BoundExpr::Call { name, args, ty } if name == "union_value" && args.len() == 1 => {
            let payload = eval_output_expr(&args[0], values, random)?;
            match ty {
                LogicalType::Union(variants) => Ok(Value::Union {
                    variants: variants.clone(),
                    tag: 0,
                    value: Box::new(payload),
                }),
                _ => Ok(payload),
            }
        }
        BoundExpr::Call { name, args, .. } => {
            let vals = args
                .iter()
                .map(|a| eval_output_expr(a, values, random))
                .collect::<Result<Vec<_>>>()?;
            eval_scalar_func_with_context(name, &vals, random)
        }
        BoundExpr::Udf { function, args, .. } => {
            let values = args
                .iter()
                .map(|argument| eval_output_expr(argument, values, random))
                .collect::<Result<Vec<_>>>()?;
            function.invoke(&values)
        }
        BoundExpr::List { elems, .. } => elems
            .iter()
            .map(|e| eval_output_expr(e, values, random))
            .collect::<Result<Vec<_>>>()
            .map(Value::List),
        BoundExpr::Struct { fields, .. } => fields
            .iter()
            .map(|(k, e)| Ok((k.clone(), eval_output_expr(e, values, random)?)))
            .collect::<Result<Vec<_>>>()
            .map(Value::Struct),
        BoundExpr::Case {
            operand,
            branches,
            else_,
            ..
        } => {
            let operand_val = operand
                .as_ref()
                .map(|o| eval_output_expr(o, values, random))
                .transpose()?;
            for (cond, res) in branches {
                let cv = eval_output_expr(cond, values, random)?;
                let matched = match &operand_val {
                    // C++ rule: a NULL when-value matches ANY operand; a NULL
                    // operand matches nothing else.
                    Some(ov) => match (ov.is_null(), cv.is_null()) {
                        (_, true) => true,
                        (true, false) => false,
                        (false, false) => cypher_cmp(ov, &cv) == Some(std::cmp::Ordering::Equal),
                    },
                    None => cv == Value::Bool(true),
                };
                if matched {
                    return eval_output_expr(res, values, random);
                }
            }
            else_
                .as_ref()
                .map(|e| eval_output_expr(e, values, random))
                .unwrap_or(Ok(Value::Null))
        }
        BoundExpr::Property { .. }
        | BoundExpr::NodeRef { .. }
        | BoundExpr::ScalarVar { .. }
        | BoundExpr::Aggregate { .. }
        | BoundExpr::ListLambda { .. }
        | BoundExpr::LambdaVar { .. }
        | BoundExpr::Subquery { .. }
        | BoundExpr::SequenceCall { .. } => Err(Error::binder(
            "ORDER BY expression cannot be evaluated over projected output".to_string(),
        )),
    }
}

fn order_keys_from_output(
    projection: &BoundProjection,
    values: &[Value],
    random: &RandomState,
) -> Result<Vec<Value>> {
    projection
        .order_by
        .iter()
        .map(|(k, _)| match k {
            OrderKey::Output(i) => Ok(values[*i].clone()),
            OrderKey::PostProjection(e) => eval_output_expr(e, values, random),
            OrderKey::Expr(_) => Err(Error::not_implemented(
                "ORDER BY an expression over aggregated results is not supported in this phase"
                    .to_string(),
            )),
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Node/rel value assembly
// ---------------------------------------------------------------------------

fn read_var_id(var: VarId, layout: &RowLayout, chunk: &DataChunk, pos: usize) -> InternalId {
    let id_col = layout.var(var).id_col;
    match chunk.columns[id_col].get_value(pos) {
        Value::InternalId(id) => id,
        _ => InternalId::new(TableId(u64::MAX), u64::MAX),
    }
}

#[derive(Clone, Copy)]
struct EntityRead<'a> {
    catalog: &'a Catalog,
    storage: &'a InMemStorage,
    read: StorageReadHandle,
    sources: &'a QuerySourceState,
    memory: &'a QueryMemory,
    control: QueryControl<'a>,
}

impl<'a> EntityRead<'a> {
    fn from_ctx(ctx: &Ctx<'a>) -> Self {
        Self {
            catalog: ctx.catalog,
            storage: ctx.storage,
            read: ctx.read(),
            sources: ctx.execution.sources,
            memory: ctx.execution.memory,
            control: ctx.execution.control,
        }
    }
}

fn assemble_var(
    var: VarId,
    layout: &RowLayout,
    chunk: &DataChunk,
    pos: usize,
    entity: EntityRead<'_>,
) -> Result<Value> {
    let id = read_var_id(var, layout, chunk, pos);
    assemble_id(var, id, layout, entity)
}

/// Assemble a whole node/rel value from its internal id, reading properties
/// straight from storage (so it works in both the projection and aggregate
/// paths without depending on which columns the chunk carries).
fn assemble_id(
    var: VarId,
    id: InternalId,
    layout: &RowLayout,
    entity: EntityRead<'_>,
) -> Result<Value> {
    if id.table_id.0 == u64::MAX {
        return Ok(Value::Null);
    }
    match &layout.var(var).kind {
        // For a polymorphic node/rel the actual table comes from the runtime id,
        // so the label and property set are those of the matched table.
        koko_planner::VarColKind::Node { .. } => {
            Ok(Value::Node(Box::new(assemble_node_value(id, entity)?)))
        }
        koko_planner::VarColKind::Rel { .. } => {
            Ok(Value::Rel(Box::new(assemble_rel_value(id, entity)?)))
        }
        // Scalar variables are projected as plain column reads, never here.
        koko_planner::VarColKind::Scalar => unreachable!("scalar var is not a node/rel"),
    }
}

/// Restrict a property list to the projected names (case-insensitive). `None`
/// keeps all properties; `Some(names)` keeps only those (the recursive-lambda
/// projection over intermediate node/rel values).
fn project_props(props: Vec<(String, Value)>, proj: Option<&Vec<String>>) -> Vec<(String, Value)> {
    match proj {
        None => props,
        Some(keep) => props
            .into_iter()
            .filter(|(name, _)| keep.iter().any(|k| k.eq_ignore_ascii_case(name)))
            .collect(),
    }
}

/// Build a whole [`NodeValue`] from its id (label + all properties of its actual
/// table), reading cells straight from storage.
fn assemble_node_value(id: InternalId, entity: EntityRead<'_>) -> Result<NodeValue> {
    let table = id.table_id;
    let node_table = entity
        .catalog
        .node_table(table)
        .ok_or_else(|| Error::runtime("Node id references a table missing from the catalog."))?;
    let columns: Vec<usize> = node_table
        .columns
        .iter()
        .map(|column| column.column_id.0 as usize)
        .collect();
    let values = if let Some(values) = entity.sources.projected_values(
        table,
        id.offset.0,
        &columns,
        entity.catalog,
        entity.control,
        entity.memory,
    )? {
        values
    } else {
        entity
            .storage
            .node_projected_values(entity.read, table, id.offset.0, &columns)
    };
    let props = node_table
        .columns
        .iter()
        .zip(values)
        .map(|(column, value)| (column.name.clone(), value))
        .collect();
    Ok(NodeValue {
        id,
        label: node_table.name.clone(),
        props,
    })
}

/// Build a whole [`RelValue`] from its id (endpoints + label + all properties).
fn assemble_rel_value(id: InternalId, entity: EntityRead<'_>) -> Result<RelValue> {
    let table = id.table_id;
    let rel_table = entity.catalog.rel_table(table).ok_or_else(|| {
        Error::runtime("Relationship id references a table missing from the catalog.")
    })?;
    let (src, dst) = if let Some(endpoints) = entity.sources.rel_endpoints(
        table,
        id.offset.0,
        entity.catalog,
        entity.control,
        entity.memory,
    )? {
        endpoints
    } else {
        entity
            .storage
            .rel_endpoints(entity.read, table, id.offset.0)
    };
    let columns: Vec<usize> = rel_table
        .columns
        .iter()
        .map(|column| column.column_id.0 as usize)
        .collect();
    let values = if let Some(values) = entity.sources.projected_values(
        table,
        id.offset.0,
        &columns,
        entity.catalog,
        entity.control,
        entity.memory,
    )? {
        values
    } else {
        entity
            .storage
            .rel_projected_values(entity.read, table, id.offset.0, &columns)
    };
    let props = rel_table
        .columns
        .iter()
        .zip(values)
        .map(|(column, value)| (column.name.clone(), value))
        .collect();
    Ok(RelValue {
        src,
        dst,
        id,
        label: rel_table.name.clone(),
        props,
        src_node: assemble_node_opt(src, entity)?.map(Box::new),
        dst_node: assemble_node_opt(dst, entity)?.map(Box::new),
    })
}

/// Assemble a node value from its id, or `None` for the sentinel "absent" id (an
/// unmatched optional endpoint).
fn assemble_node_opt(id: InternalId, entity: EntityRead<'_>) -> Result<Option<NodeValue>> {
    if id.table_id.0 == u64::MAX {
        Ok(None)
    } else {
        assemble_node_value(id, entity).map(Some)
    }
}

/// True if `ty` carries a NODE or REL anywhere — i.e. a value of this type may
/// hold a bare `InternalId` that must be inflated to a full node/rel value when
/// it escapes inside a container. `InternalId` (an `id()` result) and
/// `RecursiveRel` (paths, already materialized eagerly) deliberately do not count.
fn type_contains_graph(ty: &LogicalType) -> bool {
    match ty {
        LogicalType::Node(_) | LogicalType::Rel(_) => true,
        LogicalType::List(inner) | LogicalType::Array(inner, _) => type_contains_graph(inner),
        LogicalType::Map(k, v) => type_contains_graph(k) || type_contains_graph(v),
        LogicalType::Struct(fields) | LogicalType::Union(fields) => {
            fields.iter().any(|(_, t)| type_contains_graph(t))
        }
        _ => false,
    }
}

/// Inflate a bare node/rel `InternalId` nested inside `v` into a full
/// `Value::Node`/`Value::Rel`, guided by the value's static type. Used only at the
/// projection-output boundary and only for graph-typed items, so the per-row
/// pipeline keeps carrying cheap bare ids — a node/rel is materialized only when it
/// actually escapes inside a `collect`/list/map/struct. Idempotent (an already
/// assembled `Value::Node` passes through); `Null` (incl. an absent optional
/// endpoint, which reads as `Null`) passes through.
fn deep_materialize(v: Value, ty: &LogicalType, entity: EntityRead<'_>) -> Result<Value> {
    let value = match ty {
        LogicalType::Node(_) => match v {
            Value::InternalId(id) if id.table_id.0 != u64::MAX => {
                Value::Node(Box::new(assemble_node_value(id, entity)?))
            }
            Value::InternalId(_) => Value::Null,
            other => other,
        },
        LogicalType::Rel(_) => match v {
            Value::InternalId(id) if id.table_id.0 != u64::MAX => {
                Value::Rel(Box::new(assemble_rel_value(id, entity)?))
            }
            Value::InternalId(_) => Value::Null,
            other => other,
        },
        LogicalType::List(inner) | LogicalType::Array(inner, _) => match v {
            Value::List(items) => Value::List(
                items
                    .into_iter()
                    .map(|item| deep_materialize(item, inner, entity))
                    .collect::<Result<_>>()?,
            ),
            other => other,
        },
        LogicalType::Struct(fields) => match v {
            Value::Struct(values) => Value::Struct(
                values
                    .into_iter()
                    .enumerate()
                    .map(|(index, (name, value))| {
                        let value = match fields.get(index) {
                            Some((_, field_type)) => deep_materialize(value, field_type, entity)?,
                            None => value,
                        };
                        Ok((name, value))
                    })
                    .collect::<Result<_>>()?,
            ),
            other => other,
        },
        LogicalType::Map(key_type, value_type) => match v {
            Value::Map(pairs) => Value::Map(
                pairs
                    .into_iter()
                    .map(|(key, value)| {
                        Ok((
                            deep_materialize(key, key_type, entity)?,
                            deep_materialize(value, value_type, entity)?,
                        ))
                    })
                    .collect::<Result<_>>()?,
            ),
            other => other,
        },
        _ => v,
    };
    Ok(value)
}

/// Static graph-containing result types that require deep materialization before
/// values leave the execution context.
fn output_deep_types(projection: &BoundProjection) -> Vec<Option<LogicalType>> {
    projection
        .items
        .iter()
        .map(|item| match item {
            ProjItem::Scalar { expr, .. } => {
                let ty = expr.ty();
                type_contains_graph(&ty).then_some(ty)
            }
            ProjItem::Var { .. } => None,
        })
        .collect()
}

fn deep_materialize_values(
    values: &mut [Value],
    item_types: &[Option<LogicalType>],
    ctx: &Ctx<'_>,
) -> Result<()> {
    let entity = EntityRead::from_ctx(ctx);
    for (value, ty) in values.iter_mut().zip(item_types) {
        if let Some(ty) = ty {
            let owned = std::mem::replace(value, Value::Null);
            *value = deep_materialize(owned, ty, entity)?;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Writes (CREATE / SET / DELETE)
// ---------------------------------------------------------------------------

/// Apply one updating clause to the matched rows, mutating storage. Most clauses
/// edit the rows in place and return them; `MERGE` returns a fresh set of rows
/// (its matched/created bindings).
fn run_update(
    op: &UpdateOp,
    layout: &RowLayout,
    mut chunks: Vec<DataChunk>,
    catalog: &Catalog,
    storage: &mut InMemStorage,
    execution: &ExecutionContext<'_>,
) -> Result<Vec<DataChunk>> {
    let read = execution.storage_read;
    let write = execution
        .storage_write
        .expect("write operator requires a storage write handle");
    match op {
        UpdateOp::Create(c) => run_creates(
            c,
            layout,
            &mut chunks,
            catalog,
            storage,
            read,
            write,
            execution.control,
            execution.random,
        )?,
        UpdateOp::Set(s) => run_set(
            s,
            layout,
            &mut chunks,
            catalog,
            storage,
            write,
            execution.control,
            execution.random,
        )?,
        UpdateOp::Delete(d) => run_delete(
            d,
            layout,
            &chunks,
            catalog,
            storage,
            read,
            write,
            execution.control,
        )?,
        UpdateOp::Merge(m) => {
            return run_merge(m, layout, &chunks, catalog, storage, execution);
        }
    }
    Ok(chunks)
}

/// Execute a `MERGE` per input row: run the seeded match against live storage; on
/// a hit, apply `ON MATCH SET` and emit each match; on a miss, create the pattern,
/// apply `ON CREATE SET`, and emit the created row. Returns the resulting rows.
fn run_merge(
    merge: &MergePlan,
    layout: &RowLayout,
    chunks: &[DataChunk],
    catalog: &Catalog,
    storage: &mut InMemStorage,
    execution: &ExecutionContext<'_>,
) -> Result<Vec<DataChunk>> {
    let read = execution.storage_read;
    let write = execution
        .storage_write
        .expect("MERGE requires a storage write handle");
    let width = layout.width();
    let mut builder = ChunkBuilder::new(&layout.col_types);
    let resolver = LayoutResolver(layout);

    // The MERGE key, mirroring Kùzu's `getColumnDataExprs` minus literals: the
    // *non-literal* inline property values, evaluated from each input row. A node
    // or rel created earlier in this same statement is identified by key even when
    // the full graph-match predicate no longer matches it — because an
    // ON CREATE / ON MATCH SET mutated a matched inline property (e.g.
    // `MERGE (a:school {name:x, id:x}) ON CREATE SET a.id = …`: the next same-`x`
    // row's `id = x` predicate misses, but the key still finds the created node).
    // (Already-bound node ids are part of Kùzu's key too; the corpus's bound
    // endpoints are constant per statement, so omitting them doesn't change dedup.)
    let key_exprs: Vec<CompiledExpr> = merge
        .create
        .nodes
        .iter()
        .flat_map(|n| n.props.iter())
        .chain(merge.create.rels.iter().flat_map(|r| r.props.iter()))
        .filter(|(_, e)| !matches!(e, BoundExpr::Literal(_)))
        .map(|(_, e)| compile(e, &resolver))
        .collect::<Result<_>>()?;
    // key → the pattern's created node/rel ids, to reconstruct an ON MATCH on a
    // key hit. Scoped to this MERGE statement (Kùzu's per-operator hash table).
    let mut created_keys: HashMap<Vec<String>, Vec<(VarId, InternalId)>> = HashMap::new();
    // Output dedup for Kùzu's `suppressDuplicateCreatedOutput` (see `MergePlan`): a
    // merge key already emitted this statement collapses to no further row.
    let mut emitted_keys: HashSet<Vec<String>> = HashSet::new();
    // Even when suppression is gated off (non-key payload carried), C++'s
    // factorized output collapses duplicate (input row, key) pairs (audit W9):
    // `MATCH (n:Q) UNWIND [1,1] AS i MERGE (p:P {id:i}) RETURN n.qid` emits one
    // row per n. Empirically bounded against the oracle: the collapse applies
    // only when the merge key has non-literal exprs AND there is no ON CREATE /
    // ON MATCH SET (a literal-only key emits per row — `UNWIND [5,5,5] MERGE
    // (p:P {id:9})` is 3 rows; SET clauses keep every row too). Side effects
    // still run per input row; only the OUTPUT dedups.
    let mut emitted_rows: HashSet<(Vec<String>, Vec<String>)> = HashSet::new();

    for chunk in chunks {
        execution.control.check()?;
        let positions: Vec<usize> = chunk.sel.iter().collect();
        for pos in positions {
            // Seed the per-row match with this row's bindings.
            let mut seed = DataChunk::new(&layout.col_types);
            for c in 0..width {
                seed.columns[c].set_value(0, &chunk.columns[c].get_value(pos));
            }
            seed.set_flat(1);

            // The MERGE key (computed up front — it drives both the output dedup and
            // the created-key reconstruction below): the non-literal inline property
            // values plus the already-bound endpoints' ids (so the same inline values
            // over different endpoints stay distinct).
            let mut key: Vec<String> = key_exprs
                .iter()
                .map(|ce| Ok(ce.eval(&seed, 0, execution.random)?.to_result_string()))
                .collect::<Result<_>>()?;
            for &v in &merge.key_node_vars {
                if let Some(vc) = layout.try_var(v) {
                    key.push(seed.columns[vc.id_col].get_value(0).to_result_string());
                }
            }

            // `suppressDuplicateCreatedOutput`: a duplicate merge key already emitted
            // this statement produces no further row (no match, no create) — so
            // `UNWIND [1, 1] AS i MERGE (a:A {stuff: i})` collapses to one row + one
            // node. Gated (in the planner) to node-only MERGEs with no SET clause and
            // no carried non-key payload, matching Kùzu.
            if merge.suppress_dup && !emitted_keys.insert(key.clone()) {
                continue;
            }
            // The input row's identity for the W9 output dedup (pre-fill: the
            // merge pattern's own columns are still uniformly unset here).
            let output_dedup = !key_exprs.is_empty()
                && merge.on_create.items.is_empty()
                && merge.on_match.items.is_empty();
            let emit_fresh = !output_dedup || {
                let row_id: Vec<String> = (0..width)
                    .map(|c| seed.columns[c].get_value(0).to_result_string())
                    .collect();
                emitted_rows.insert((row_id, key.clone()))
            };

            // Match against live storage (it reflects prior rows' creates). A
            // The statement-local key table is probed FIRST, like Kùzu's MERGE
            // hash table: a row whose key matches a node/rel created earlier in
            // this same statement applies ON MATCH to that entry only — never a
            // live re-match (which could also bind pre-existing duplicates the
            // statement did not create; corpus merge_tinysnb pins this order).
            if let Some(ids) = created_keys.get(&key).cloned() {
                for (var, id) in ids {
                    if catalog.node_table(id.table_id).is_some() {
                        fill_created_node(var, id, layout, &mut seed, 0, catalog, &*storage, read);
                    } else {
                        fill_created_rel(var, id, layout, &mut seed, 0, catalog, &*storage, read);
                    }
                }
                let mut hit_chunk = vec![seed];
                run_set(
                    &merge.on_match,
                    layout,
                    &mut hit_chunk,
                    catalog,
                    storage,
                    write,
                    execution.control,
                    execution.random,
                )?;
                if emit_fresh {
                    push_all_rows(&hit_chunk, width, &mut builder);
                }
                continue;
            }

            // Match against live storage (it reflects prior rows' creates). A
            // scoped immutable reborrow streams the seeded match to completion and
            // is released before the create/set mutations below take `&mut storage`.
            let matched = {
                let ctx = Ctx::new(catalog, &*storage, layout, execution);
                let mut m = build_exec(&merge.match_pattern, &ctx, std::slice::from_ref(&seed))?;
                drain_all(&mut m, &ctx)?
            };
            // A MERGE binds EVERY existing match (audit W1, oracle-verified): with
            // two matching rels, ON MATCH SET updates both and both rows are
            // emitted — result cardinality and final DB state follow C++.
            if matched.iter().any(|c| c.sel.iter().next().is_some()) {
                let mut matched = matched;
                run_set(
                    &merge.on_match,
                    layout,
                    &mut matched,
                    catalog,
                    storage,
                    write,
                    execution.control,
                    execution.random,
                )?;
                if emit_fresh {
                    push_all_rows(&matched, width, &mut builder);
                }
            } else {
                let mut created = vec![seed];
                run_creates(
                    &merge.create,
                    layout,
                    &mut created,
                    catalog,
                    storage,
                    read,
                    write,
                    execution.control,
                    execution.random,
                )?;
                run_set(
                    &merge.on_create,
                    layout,
                    &mut created,
                    catalog,
                    storage,
                    write,
                    execution.control,
                    execution.random,
                )?;
                created_keys.insert(key, collect_created_ids(&merge.create, layout, &created[0]));
                if emit_fresh {
                    push_all_rows(&created, width, &mut builder);
                }
            }
        }
    }
    Ok(builder.finish())
}

/// The internal ids of a `BoundCreate`'s freshly-created nodes/rels, read back from
/// the chunk their `fill_created_*` populated — recorded under the MERGE key so a
/// later same-key row can reconstruct them for ON MATCH.
fn collect_created_ids(
    create: &BoundCreate,
    layout: &RowLayout,
    chunk: &DataChunk,
) -> Vec<(VarId, InternalId)> {
    let mut ids = Vec::new();
    let mut record = |var: VarId| {
        if let Some(vc) = layout.try_var(var) {
            if let Value::InternalId(id) = chunk.columns[vc.id_col].get_value(0) {
                ids.push((var, id));
            }
        }
    };
    for n in &create.nodes {
        record(n.var);
    }
    for r in &create.rels {
        if let Some(var) = r.var {
            record(var);
        }
    }
    ids
}

/// Copy every selected row of `chunks` into `builder`.
fn push_all_rows(chunks: &[DataChunk], width: usize, builder: &mut ChunkBuilder) {
    for chunk in chunks {
        for pos in chunk.sel.iter() {
            let row: Vec<Value> = (0..width)
                .map(|c| chunk.columns[c].get_value(pos))
                .collect();
            builder.push_row(&row);
        }
    }
}

/// One coerced property update waiting to enter a homogeneous typed batch.
struct PropertyUpdate {
    id: InternalId,
    is_node: bool,
    column_id: usize,
    ty: LogicalType,
    value: Value,
}

struct PendingPropertyBatch {
    is_node: bool,
    table: TableId,
    column_id: usize,
    ty: LogicalType,
    chunk: DataChunk,
    len: usize,
}

fn flush_property_batch(
    pending: &mut Option<PendingPropertyBatch>,
    storage: &mut InMemStorage,
    write: StorageWriteHandle,
) -> Result<()> {
    let Some(batch) = pending.take() else {
        return Ok(());
    };
    if batch.is_node {
        storage.set_node_property_batch(write, batch.table, batch.column_id, &batch.chunk)
    } else {
        storage.set_rel_property_batch(write, batch.table, batch.column_id, &batch.chunk)
    }
}

fn push_property_update(
    pending: &mut Option<PendingPropertyBatch>,
    update: PropertyUpdate,
    storage: &mut InMemStorage,
    write: StorageWriteHandle,
) -> Result<()> {
    let compatible = pending.as_ref().is_some_and(|batch| {
        batch.is_node == update.is_node
            && batch.table == update.id.table_id
            && batch.column_id == update.column_id
            && batch.ty == update.ty
            && batch.len < VECTOR_CAPACITY
    });
    if !compatible {
        flush_property_batch(pending, storage, write)?;
        *pending = Some(PendingPropertyBatch {
            is_node: update.is_node,
            table: update.id.table_id,
            column_id: update.column_id,
            ty: update.ty.clone(),
            chunk: DataChunk::new(&[LogicalType::InternalId, update.ty]),
            len: 0,
        });
    }
    let batch = pending.as_mut().expect("property batch created");
    batch.chunk.columns[0].set_internal_id(batch.len, update.id);
    batch.chunk.columns[1].set_value_owned(batch.len, update.value);
    batch.len += 1;
    batch.chunk.set_flat(batch.len);
    if batch.len == VECTOR_CAPACITY {
        flush_property_batch(pending, storage, write)?;
    }
    Ok(())
}

/// Execute `SET` in statement order, coalescing adjacent updates to the same physical
/// property column while immediately updating live chunk cells for later expressions.
#[allow(clippy::too_many_arguments)]
fn run_set(
    set: &BoundSet,
    layout: &RowLayout,
    chunks: &mut [DataChunk],
    catalog: &Catalog,
    storage: &mut InMemStorage,
    write: StorageWriteHandle,
    control: QueryControl<'_>,
    random: &RandomState,
) -> Result<()> {
    let resolver = LayoutResolver(layout);
    let compiled: Vec<CompiledExpr> = set
        .items
        .iter()
        .map(|it| compile(&it.value, &resolver))
        .collect::<Result<_>>()?;
    let mut pending = None;

    for chunk in chunks.iter_mut() {
        control.check()?;
        let positions: Vec<usize> = chunk.sel.iter().collect();
        for pos in positions {
            for (item, ce) in set.items.iter().zip(&compiled) {
                let value = ce.eval(chunk, pos, random)?;
                match &item.target {
                    BoundSetTarget::Property { var, prop } => {
                        if let Some(update) =
                            prepare_one_property(*var, prop, &value, layout, chunk, pos, catalog)
                        {
                            push_property_update(&mut pending, update, storage, write)?;
                        }
                    }
                    BoundSetTarget::DynamicProperty { var, prop } => {
                        if let Some(update) = prepare_dynamic_property(
                            *var, prop, &value, layout, chunk, pos, catalog,
                        )? {
                            push_property_update(&mut pending, update, storage, write)?;
                        }
                    }
                    BoundSetTarget::Var { var } => {
                        for update in
                            prepare_whole_value(*var, &value, layout, chunk, pos, catalog)?
                        {
                            push_property_update(&mut pending, update, storage, write)?;
                        }
                    }
                }
            }
        }
    }
    flush_property_batch(&mut pending, storage, write)
}

/// Prepare one property update and mirror its coerced value into the live chunk.
#[allow(clippy::too_many_arguments)]
fn prepare_one_property(
    var: VarId,
    prop: &str,
    value: &Value,
    layout: &RowLayout,
    chunk: &mut DataChunk,
    pos: usize,
    catalog: &Catalog,
) -> Option<PropertyUpdate> {
    let id = read_var_id(var, layout, chunk, pos);
    if id.table_id.0 == u64::MAX {
        return None;
    }
    let is_node = matches!(layout.var(var).kind, VarColKind::Node { .. });
    let column = if is_node {
        catalog
            .node_table(id.table_id)
            .and_then(|table| table.column(prop))
    } else {
        catalog
            .rel_table(id.table_id)
            .and_then(|table| table.column(prop))
    };
    let Some(column) = column else {
        if let Some(chunk_column) = layout.column(var, Some(prop)) {
            chunk.columns[chunk_column].set_value(pos, &Value::Null);
        }
        return None;
    };
    let coerced = cast_value(value, &column.ty).unwrap_or(Value::Null);
    if let Some(chunk_column) = layout.column(var, Some(prop)) {
        chunk.columns[chunk_column].set_value(pos, &coerced);
    }
    Some(PropertyUpdate {
        id,
        is_node,
        column_id: column.column_id.0 as usize,
        ty: column.ty.clone(),
        value: coerced,
    })
}

/// Update one key inside an ANY graph's ordered JSON object and mirror the complete object into
/// the live hidden `data` column.
#[allow(clippy::too_many_arguments)]
fn prepare_dynamic_property(
    var: VarId,
    prop: &str,
    value: &Value,
    layout: &RowLayout,
    chunk: &mut DataChunk,
    pos: usize,
    catalog: &Catalog,
) -> Result<Option<PropertyUpdate>> {
    let id = read_var_id(var, layout, chunk, pos);
    if id.table_id.0 == u64::MAX {
        return Ok(None);
    }
    let is_node = matches!(layout.var(var).kind, VarColKind::Node { .. });
    if !(catalog.is_any_node_table(id.table_id) || catalog.is_any_rel_table(id.table_id)) {
        return Ok(None);
    }
    let table_column = if is_node {
        catalog
            .node_table(id.table_id)
            .and_then(|table| table.column("data"))
    } else {
        catalog
            .rel_table(id.table_id)
            .and_then(|table| table.column("data"))
    };
    let Some(table_column) = table_column else {
        return Ok(None);
    };
    let Some(chunk_column) = layout.column(var, Some("data")) else {
        return Ok(None);
    };
    let mut fields = match chunk.columns[chunk_column].get_value(pos) {
        Value::Json(koko_common::JsonValue::Object(fields)) => fields,
        _ => Vec::new(),
    };
    if value.is_null() {
        fields.retain(|(name, _)| name != prop);
    } else {
        let json = koko_common::JsonValue::from_value(value)?;
        if let Some((_, existing)) = fields.iter_mut().find(|(name, _)| name == prop) {
            *existing = json;
        } else {
            fields.push((prop.to_string(), json));
        }
    }
    fields.sort_by(|left, right| left.0.cmp(&right.0));
    let value = Value::Json(koko_common::JsonValue::Object(fields));
    chunk.columns[chunk_column].set_value(pos, &value);
    Ok(Some(PropertyUpdate {
        id,
        is_node,
        column_id: table_column.column_id.0 as usize,
        ty: LogicalType::Json,
        value,
    }))
}

/// Prepare whole-value assignments in catalog order. Unlisted properties and node primary
/// keys are preserved, matching the scalar `SET n = {k: v}` contract.
#[allow(clippy::too_many_arguments)]
fn prepare_whole_value(
    var: VarId,
    value: &Value,
    layout: &RowLayout,
    chunk: &mut DataChunk,
    pos: usize,
    catalog: &Catalog,
) -> Result<Vec<PropertyUpdate>> {
    let id = read_var_id(var, layout, chunk, pos);
    if id.table_id.0 == u64::MAX {
        return Ok(Vec::new());
    }
    let is_node = matches!(layout.var(var).kind, VarColKind::Node { .. });
    let new_props: Vec<(String, Value)> = match value {
        Value::Struct(fields) => fields.clone(),
        Value::Map(entries) => entries
            .iter()
            .filter_map(|(key, value)| key.as_str().map(|name| (name.to_string(), value.clone())))
            .collect(),
        Value::Node(node) => node.props.clone(),
        Value::Rel(rel) => rel.props.clone(),
        _ => return Ok(Vec::new()),
    };
    if catalog.is_any_node_table(id.table_id) || catalog.is_any_rel_table(id.table_id) {
        let mut updates = Vec::new();
        for (name, value) in new_props {
            if let Some(update) =
                prepare_dynamic_property(var, &name, &value, layout, chunk, pos, catalog)?
            {
                updates.push(update);
            }
        }
        return Ok(updates);
    }
    let columns: Vec<(String, usize, LogicalType, bool)> = if is_node {
        let table = catalog.node_table(id.table_id).expect("bound node table");
        table
            .columns
            .iter()
            .enumerate()
            .map(|(index, column)| {
                (
                    column.name.clone(),
                    column.column_id.0 as usize,
                    column.ty.clone(),
                    index == table.primary_key,
                )
            })
            .collect()
    } else {
        catalog
            .rel_table(id.table_id)
            .expect("bound relationship table")
            .columns
            .iter()
            .map(|column| {
                (
                    column.name.clone(),
                    column.column_id.0 as usize,
                    column.ty.clone(),
                    false,
                )
            })
            .collect()
    };
    let mut updates = Vec::new();
    for (name, column_id, ty, is_primary_key) in columns {
        if is_primary_key {
            continue;
        }
        let Some(provided) = new_props
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(&name))
            .map(|(_, value)| value)
        else {
            continue;
        };
        let value = cast_value(provided, &ty).unwrap_or(Value::Null);
        if let Some(chunk_column) = layout.column(var, Some(&name)) {
            chunk.columns[chunk_column].set_value(pos, &value);
        }
        updates.push(PropertyUpdate {
            id,
            is_node,
            column_id,
            ty,
            value,
        });
    }
    Ok(updates)
}

/// Execute a `[DETACH] DELETE`: submit selected typed id batches, relationships first.
#[allow(clippy::too_many_arguments)]
fn run_delete(
    del: &BoundDelete,
    layout: &RowLayout,
    chunks: &[DataChunk],
    catalog: &Catalog,
    storage: &mut InMemStorage,
    read: StorageReadHandle,
    write: StorageWriteHandle,
    control: QueryControl<'_>,
) -> Result<()> {
    for &var in &del.vars {
        control.check()?;
        if matches!(layout.var(var).kind, VarColKind::Rel { .. }) {
            let ids = bound_ids(var, layout, chunks);
            for batch in id_batches(&ids) {
                storage.delete_rel_batch(write, &batch)?;
            }
        }
    }
    for &var in &del.vars {
        control.check()?;
        if !matches!(layout.var(var).kind, VarColKind::Node { .. }) {
            continue;
        }
        let ids = bound_ids(var, layout, chunks);
        let batches = id_batches(&ids);
        if del.detach {
            let connected: Vec<InternalId> = ids
                .iter()
                .flat_map(|&id| storage.node_connected_rels(read, id))
                .collect();
            for batch in id_batches(&connected) {
                storage.delete_rel_batch(write, &batch)?;
            }
        } else {
            for batch in &batches {
                storage.preflight_node_delete_batch(write, batch)?;
            }
            for id in &ids {
                if let Some((rel_table, dir)) = storage.node_connected_edge(read, *id) {
                    let rel_name = catalog.rel_table(rel_table).map_or("", |table| &table.name);
                    return Err(Error::runtime(format!(
                        "Node(nodeOffset: {}) has connected edges in table {} in the {} direction, \
                         which cannot be deleted. Please delete the edges first or try DETACH DELETE.",
                        id.offset.0,
                        rel_name,
                        dir.name()
                    )));
                }
            }
        }
        for batch in &batches {
            storage.delete_node_batch(write, batch)?;
        }
    }
    Ok(())
}

fn bound_ids(var: VarId, layout: &RowLayout, chunks: &[DataChunk]) -> Vec<InternalId> {
    let mut ids = Vec::new();
    for chunk in chunks {
        for pos in chunk.sel.iter() {
            let id = read_var_id(var, layout, chunk, pos);
            if id.table_id.0 != u64::MAX {
                ids.push(id);
            }
        }
    }
    ids
}

fn id_batches(ids: &[InternalId]) -> Vec<DataChunk> {
    ids.chunks(VECTOR_CAPACITY)
        .map(|ids| {
            let mut chunk = DataChunk::new(&[LogicalType::InternalId]);
            for (position, &id) in ids.iter().enumerate() {
                chunk.columns[0].set_internal_id(position, id);
            }
            chunk.set_flat(ids.len());
            chunk
        })
        .collect()
}

/// Fold a constant (column-free) bound expression to a `Value` — used by the exec
/// layer to resolve a column `DEFAULT` at CREATE/ALTER time. A reference to a
/// column / subquery / sequence is rejected (a `DEFAULT` must be constant;
/// `nextval` defaults are classified separately and applied per row).
/// Fold a bound `SKIP`/`LIMIT` count and validate it — C++ defers the value
/// check to execution: anything but a non-negative integer is the *runtime*
/// error, after the binder already ensured the expression is constant.
fn fold_skip_limit(e: Option<&BoundExpr>) -> Result<Option<i64>> {
    let Some(e) = e else { return Ok(None) };
    let v = eval_constant(e)?;
    match v.as_i64() {
        Some(n) if n >= 0 => Ok(Some(n)),
        _ => Err(Error::runtime(
            "The number of rows to skip/limit must be a non-negative integer.".to_string(),
        )),
    }
}

pub fn eval_constant(expr: &BoundExpr) -> Result<Value> {
    struct NoCols;
    impl ColumnResolver for NoCols {
        fn column(&self, _: VarId, _: Option<&str>) -> Result<usize> {
            Err(Error::binder(
                "a DEFAULT value must be constant (it cannot reference columns)".to_string(),
            ))
        }
        fn subquery_column(&self, _: usize) -> Result<usize> {
            Err(Error::binder(
                "a DEFAULT value cannot contain a subquery".to_string(),
            ))
        }
        fn sequence_column(&self, _: usize) -> Result<usize> {
            Err(Error::binder(
                "a DEFAULT value cannot contain a nested sequence call".to_string(),
            ))
        }
        fn table_names(&self) -> HashMap<TableId, String> {
            HashMap::new()
        }
    }
    let compiled = compile(expr, &NoCols)?;
    // A constant never touches the chunk; an empty 1-position chunk suffices.
    let chunk = DataChunk::new(&[]);
    compiled.eval(&chunk, 0, &RandomState::default())
}

/// The catalog defaults for the columns a CREATE pattern does not supply (skipping
/// `None` — those stay NULL). A `SERIAL` column carries a `NextVal` default (its
/// implicit sequence), so it is applied here like any other `nextval` default.
fn omitted_defaults(
    catalog: &Catalog,
    table: TableId,
    num_columns: usize,
    props: &[(usize, BoundExpr)],
) -> Vec<(usize, ColumnDefault)> {
    (0..num_columns)
        .filter(|&c| !props.iter().any(|(pc, _)| *pc == c))
        .filter_map(|c| match catalog.column_default(table, c) {
            ColumnDefault::None => None,
            d => Some((c, d)),
        })
        .collect()
}

/// Write each precomputed column default into `values` for one inserted row.
/// `NextVal` advances its sequence once (per row); `Const` reuses its folded value.
fn apply_defaults(
    defaults: &[(usize, ColumnDefault)],
    values: &mut [Value],
    catalog: &Catalog,
) -> Result<()> {
    for (col, def) in defaults {
        values[*col] = match def {
            ColumnDefault::Const(v) => v.clone(),
            ColumnDefault::NextVal(s) => Value::Int64(catalog.sequence_next_val(s)?),
            ColumnDefault::None => continue,
        };
    }
    Ok(())
}

fn insert_node_row_batch(
    storage: &mut InMemStorage,
    write: StorageWriteHandle,
    catalog: &Catalog,
    table: TableId,
    values: &[Value],
) -> Result<InternalId> {
    let types: Vec<LogicalType> = catalog
        .node_table(table)
        .expect("bound node table")
        .columns
        .iter()
        .map(|column| column.ty.clone())
        .collect();
    let mut batch = DataChunk::new(&types);
    for (column, value) in batch.columns.iter_mut().zip(values) {
        column.set_value(0, value);
    }
    batch.set_flat(1);
    storage
        .insert_node_batch(write, table, &batch, false)
        .into_iter()
        .next()
        .expect("one node batch row")
}

fn insert_rel_row_batch(
    storage: &mut InMemStorage,
    write: StorageWriteHandle,
    catalog: &Catalog,
    table: TableId,
    src: InternalId,
    dst: InternalId,
    values: &[Value],
) -> Result<InternalId> {
    let mut types = vec![LogicalType::InternalId, LogicalType::InternalId];
    types.extend(
        catalog
            .rel_table(table)
            .expect("bound relationship table")
            .columns
            .iter()
            .map(|column| column.ty.clone()),
    );
    let mut batch = DataChunk::new(&types);
    batch.columns[0].set_internal_id(0, src);
    batch.columns[1].set_internal_id(0, dst);
    for (column, value) in batch.columns[2..].iter_mut().zip(values) {
        column.set_value(0, value);
    }
    batch.set_flat(1);
    storage
        .insert_rel_batch(write, table, &batch, false)
        .into_iter()
        .next()
        .expect("one relationship batch row")
}

fn coerce_stored_value(value: Value, target: &LogicalType) -> Result<Value> {
    if matches!(target, LogicalType::Json) && !matches!(value, Value::Json(_)) {
        return koko_common::JsonValue::from_value(&value).map(Value::Json);
    }
    Ok(value)
}

#[allow(clippy::too_many_arguments)]
fn run_creates(
    create: &BoundCreate,
    layout: &RowLayout,
    chunks: &mut [DataChunk],
    catalog: &Catalog,
    storage: &mut InMemStorage,
    read: StorageReadHandle,
    write: StorageWriteHandle,
    control: QueryControl<'_>,
    random: &RandomState,
) -> Result<()> {
    let resolver = LayoutResolver(layout);

    // Compile property expressions once.
    struct NodePlan {
        var: VarId,
        table: TableId,
        num_columns: usize,
        props: Vec<(usize, CompiledExpr)>,
        defaults: Vec<(usize, ColumnDefault)>,
    }
    let node_plans: Vec<NodePlan> = create
        .nodes
        .iter()
        .map(|n| {
            Ok(NodePlan {
                var: n.var,
                table: n.table,
                num_columns: n.num_columns,
                props: n
                    .props
                    .iter()
                    .map(|(c, e)| Ok((*c, compile(e, &resolver)?)))
                    .collect::<Result<_>>()?,
                defaults: omitted_defaults(catalog, n.table, n.num_columns, &n.props),
            })
        })
        .collect::<Result<_>>()?;
    struct RelPlan {
        table: TableId,
        src: VarId,
        dst: VarId,
        num_columns: usize,
        props: Vec<(usize, CompiledExpr)>,
        defaults: Vec<(usize, ColumnDefault)>,
        var: Option<VarId>,
    }
    let rel_plans: Vec<RelPlan> = create
        .rels
        .iter()
        .map(|r| {
            Ok(RelPlan {
                table: r.table,
                src: r.src,
                dst: r.dst,
                num_columns: r.num_columns,
                props: r
                    .props
                    .iter()
                    .map(|(c, e)| Ok((*c, compile(e, &resolver)?)))
                    .collect::<Result<_>>()?,
                defaults: omitted_defaults(catalog, r.table, r.num_columns, &r.props),
                var: r.var,
            })
        })
        .collect::<Result<_>>()?;

    // The dominant bulk-write shape (`UNWIND … CREATE (n)`) has one node plan and no
    // relationships. Evaluate one input chunk, then publish it through the typed batch API.
    if node_plans.len() == 1 && rel_plans.is_empty() {
        let plan = &node_plans[0];
        let types: Vec<LogicalType> = catalog
            .node_table(plan.table)
            .expect("bound node table")
            .columns
            .iter()
            .map(|column| column.ty.clone())
            .collect();
        for chunk in chunks {
            control.check()?;
            let positions: Vec<usize> = chunk.sel.iter().collect();
            let mut batch = DataChunk::new(&types);
            for (batch_position, &position) in positions.iter().enumerate() {
                let mut values = vec![Value::Null; plan.num_columns];
                for (column, expression) in &plan.props {
                    values[*column] = coerce_stored_value(
                        expression.eval(chunk, position, random)?,
                        &types[*column],
                    )?;
                }
                apply_defaults(&plan.defaults, &mut values, catalog)?;
                for (output, value) in batch.columns.iter_mut().zip(&values) {
                    output.set_value(batch_position, value);
                }
            }
            batch.set_flat(positions.len());
            let results = storage.insert_node_batch(write, plan.table, &batch, false);
            for (&position, result) in positions.iter().zip(results) {
                let id = result?;
                fill_created_node(
                    plan.var, id, layout, chunk, position, catalog, &*storage, read,
                );
            }
        }
        return Ok(());
    }

    struct RelBatchRow {
        position: usize,
        src: InternalId,
        dst: InternalId,
        values: Vec<Value>,
    }

    struct RelBatchGroup {
        table: TableId,
        rows: Vec<RelBatchRow>,
    }

    // Relationship-only CREATE has no freshly-created endpoint dependency, so rows can be
    // grouped by concrete per-pair storage table and inserted as typed batches.
    if node_plans.is_empty() && rel_plans.len() == 1 {
        let plan = &rel_plans[0];
        let created = HashMap::new();
        for chunk in chunks {
            control.check()?;
            let positions: Vec<usize> = chunk.sel.iter().collect();
            let mut groups: Vec<RelBatchGroup> = Vec::new();
            for position in positions {
                let (Some(src), Some(dst)) = (
                    resolve_endpoint(plan.src, &created, layout, chunk, position)?,
                    resolve_endpoint(plan.dst, &created, layout, chunk, position)?,
                ) else {
                    if let Some(var) = plan.var {
                        if let Some(columns) = layout.try_var(var) {
                            chunk.columns[columns.id_col].set_value(position, &Value::Null);
                        }
                    }
                    continue;
                };
                let Some(member) = catalog.rel_member_for(plan.table, src.table_id, dst.table_id)
                else {
                    let name = catalog
                        .rel_table(plan.table)
                        .map_or("", |table| table.name.as_str());
                    return Err(Error::runtime(format!(
                        "Nodes are not connected through relationship table {name}."
                    )));
                };
                let mut values = vec![Value::Null; plan.num_columns];
                for (column, expression) in &plan.props {
                    let ty = &catalog
                        .rel_table(plan.table)
                        .expect("bound relationship table")
                        .columns[*column]
                        .ty;
                    values[*column] =
                        coerce_stored_value(expression.eval(chunk, position, random)?, ty)?;
                }
                apply_defaults(&plan.defaults, &mut values, catalog)?;
                let entries = match groups.iter_mut().find(|group| group.table == member) {
                    Some(group) => &mut group.rows,
                    None => {
                        groups.push(RelBatchGroup {
                            table: member,
                            rows: Vec::new(),
                        });
                        &mut groups.last_mut().expect("group inserted").rows
                    }
                };
                entries.push(RelBatchRow {
                    position,
                    src,
                    dst,
                    values,
                });
            }
            for group in groups {
                let table = group.table;
                let entries = group.rows;
                let mut types = vec![LogicalType::InternalId, LogicalType::InternalId];
                types.extend(
                    catalog
                        .rel_table(table)
                        .expect("bound relationship table")
                        .columns
                        .iter()
                        .map(|column| column.ty.clone()),
                );
                let mut batch = DataChunk::new(&types);
                for (batch_position, entry) in entries.iter().enumerate() {
                    batch.columns[0].set_internal_id(batch_position, entry.src);
                    batch.columns[1].set_internal_id(batch_position, entry.dst);
                    for (column, value) in batch.columns[2..].iter_mut().zip(&entry.values) {
                        column.set_value(batch_position, value);
                    }
                }
                batch.set_flat(entries.len());
                let results = storage.insert_rel_batch(write, table, &batch, false);
                for (entry, result) in entries.into_iter().zip(results) {
                    let id = result?;
                    if let Some(var) = plan.var {
                        fill_created_rel(
                            var,
                            id,
                            layout,
                            chunk,
                            entry.position,
                            catalog,
                            &*storage,
                            read,
                        );
                    }
                }
            }
        }
        return Ok(());
    }

    for chunk in chunks {
        control.check()?;
        let positions: Vec<usize> = chunk.sel.iter().collect();
        for pos in positions {
            let mut created: HashMap<VarId, InternalId> = HashMap::new();

            for np in &node_plans {
                let mut values = vec![Value::Null; np.num_columns];
                for (col, ce) in &np.props {
                    let ty = &catalog
                        .node_table(np.table)
                        .expect("bound node table")
                        .columns[*col]
                        .ty;
                    values[*col] = coerce_stored_value(ce.eval(chunk, pos, random)?, ty)?;
                }
                apply_defaults(&np.defaults, &mut values, catalog)?;
                let id = insert_node_row_batch(storage, write, catalog, np.table, &values)?;
                created.insert(np.var, id);
                // Write the created node back into the chunk so a following
                // `WITH`/`RETURN` can carry/project it.
                fill_created_node(np.var, id, layout, chunk, pos, catalog, &*storage, read);
            }

            for rp in &rel_plans {
                // An endpoint can be NULL when it came from an OPTIONAL MATCH that
                // didn't match; then the relationship is simply not created (and its
                // variable is NULL), matching Kùzu. We must explicitly NULL the var's
                // id column on skip — otherwise it keeps the zero-initialised
                // `InternalId`, which `id(e)` would read as `0:0`.
                let (Some(src), Some(dst)) = (
                    resolve_endpoint(rp.src, &created, layout, chunk, pos)?,
                    resolve_endpoint(rp.dst, &created, layout, chunk, pos)?,
                ) else {
                    if let Some(var) = rp.var {
                        if let Some(vc) = layout.try_var(var) {
                            chunk.columns[vc.id_col].set_value(pos, &Value::Null);
                        }
                    }
                    continue;
                };
                // Route the edge to its pair's per-pair store (a multi-pair rel group has
                // one store per FROM-TO pair); single-pair resolves to the primary id.
                let Some(member) = catalog.rel_member_for(rp.table, src.table_id, dst.table_id)
                else {
                    let rel_name = catalog.rel_table(rp.table).map_or("", |t| t.name.as_str());
                    return Err(Error::runtime(format!(
                        "Nodes are not connected through relationship table {rel_name}."
                    )));
                };
                let mut values = vec![Value::Null; rp.num_columns];
                for (col, ce) in &rp.props {
                    let ty = &catalog
                        .rel_table(rp.table)
                        .expect("bound relationship table")
                        .columns[*col]
                        .ty;
                    values[*col] = coerce_stored_value(ce.eval(chunk, pos, random)?, ty)?;
                }
                apply_defaults(&rp.defaults, &mut values, catalog)?;
                let id = insert_rel_row_batch(storage, write, catalog, member, src, dst, &values)?;
                // A MERGE'd rel is projectable — write it back into the chunk.
                if let Some(var) = rp.var {
                    fill_created_rel(var, id, layout, chunk, pos, catalog, &*storage, read);
                }
            }
        }
    }
    Ok(())
}

/// Write a just-created relationship's id + properties into its layout columns for
/// the current row (a no-op when the var has no columns).
#[allow(clippy::too_many_arguments)]
fn fill_created_rel(
    var: VarId,
    id: InternalId,
    layout: &RowLayout,
    chunk: &mut DataChunk,
    pos: usize,
    catalog: &Catalog,
    storage: &InMemStorage,
    read: StorageReadHandle,
) {
    let Some(vc) = layout.try_var(var) else {
        return;
    };
    chunk.columns[vc.id_col].set_value(pos, &Value::InternalId(id));
    let Some(rt) = catalog.rel_table(id.table_id) else {
        return;
    };
    let projected: Vec<(usize, usize)> = vc
        .props
        .iter()
        .filter_map(|property| {
            rt.column(&property.name)
                .map(|column| (property.col_index, column.column_id.0 as usize))
        })
        .collect();
    let columns: Vec<usize> = projected.iter().map(|(_, column)| *column).collect();
    let properties = storage.rel_projected_values(read, id.table_id, id.offset.0, &columns);
    for (&(chunk_column, _), value) in projected.iter().zip(properties) {
        chunk.columns[chunk_column].set_value(pos, &value);
    }
}

/// Write a just-created node's id + properties into its layout columns for the
/// current row (a no-op when the var has no columns, i.e. it is never read).
#[allow(clippy::too_many_arguments)]
fn fill_created_node(
    var: VarId,
    id: InternalId,
    layout: &RowLayout,
    chunk: &mut DataChunk,
    pos: usize,
    catalog: &Catalog,
    storage: &InMemStorage,
    read: StorageReadHandle,
) {
    let Some(vc) = layout.try_var(var) else {
        return;
    };
    chunk.columns[vc.id_col].set_value(pos, &Value::InternalId(id));
    let Some(nt) = catalog.node_table(id.table_id) else {
        return;
    };
    let projected: Vec<(usize, usize)> = vc
        .props
        .iter()
        .filter_map(|property| {
            nt.column(&property.name)
                .map(|column| (property.col_index, column.column_id.0 as usize))
        })
        .collect();
    let columns: Vec<usize> = projected.iter().map(|(_, column)| *column).collect();
    let properties = storage.node_projected_values(read, id.table_id, id.offset.0, &columns);
    for (&(chunk_column, _), value) in projected.iter().zip(properties) {
        chunk.columns[chunk_column].set_value(pos, &value);
    }
}

/// Resolve a CREATE relationship endpoint to an id: a just-created node, or a
/// matched variable read from the binding row.
fn resolve_endpoint(
    var: VarId,
    created: &HashMap<VarId, InternalId>,
    layout: &RowLayout,
    chunk: &DataChunk,
    pos: usize,
) -> Result<Option<InternalId>> {
    if let Some(id) = created.get(&var) {
        return Ok(Some(*id));
    }
    let col = layout
        .column(var, None)
        .ok_or_else(|| Error::runtime("internal: CREATE endpoint is unbound".to_string()))?;
    // `Null` => the endpoint didn't match (an OPTIONAL MATCH): signal "skip".
    match chunk.columns[col].get_value(pos) {
        Value::InternalId(id) => Ok(Some(id)),
        _ => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use koko_storage::InMemStorage;

    /// A minimal context over an empty catalog/storage with a one-`INT64`-column
    /// layout — enough to drive the streaming harness without the binder/planner.
    fn int_layout() -> RowLayout {
        let mut layout = RowLayout::default();
        layout.col_types = vec![LogicalType::Int64];
        layout
    }

    struct EmptyTableRuntime;

    impl TableFuncRuntime for EmptyTableRuntime {
        fn current_setting(&self, _: &str) -> Value {
            Value::Null
        }

        fn warning_rows(&self) -> Vec<Vec<Value>> {
            Vec::new()
        }

        fn clear_warnings(&self) {}

        fn macro_rows(&self) -> Vec<Vec<Value>> {
            Vec::new()
        }

        fn memory_usage(&self) -> koko_common::MemoryUsage {
            koko_common::MemoryUsage {
                current: 0,
                peak: 0,
                limit: None,
            }
        }
    }

    macro_rules! test_execution {
        ($name:ident) => {
            let table_runtime = EmptyTableRuntime;
            let random = RandomState::default();
            let warning_registry = koko_common::warnings::WarningRegistry::default();
            let warning_sink = warning_registry.sink(0, u64::MAX);
            let memory_tracker = MemoryTracker::default();
            let query_memory = QueryMemory::new(&memory_tracker).unwrap();
            let sources = QuerySourceState::default();
            let $name = ExecutionContext {
                table_functions: &table_runtime,
                random: &random,
                worker_count: 1,
                warnings: &warning_sink,
                storage_read: StorageReadHandle::new(koko_common::ReadView::reader(0)),
                storage_write: None,
                control: QueryControl::default(),
                memory: &query_memory,
                sources: &sources,
            };
        };
    }
    macro_rules! test_execution_with_memory {
        ($name:ident, $tracker:ident, $memory:ident, $limit:expr, $control:expr) => {
            let table_runtime = EmptyTableRuntime;
            let random = RandomState::default();
            let warning_registry = koko_common::warnings::WarningRegistry::default();
            let warning_sink = warning_registry.sink(0, u64::MAX);
            let $tracker = MemoryTracker::new($limit);
            let $memory = QueryMemory::new(&$tracker).unwrap();
            let sources = QuerySourceState::default();
            let $name = ExecutionContext {
                table_functions: &table_runtime,
                random: &random,
                worker_count: 1,
                warnings: &warning_sink,
                storage_read: StorageReadHandle::new(koko_common::ReadView::reader(0)),
                storage_write: None,
                control: $control,
                memory: &$memory,
                sources: &sources,
            };
        };
    }

    /// A `Buffered` source of `n` one-column rows, packed into `chunk` -sized chunks
    /// (an odd size, deliberately not a multiple of `VECTOR_CAPACITY`).
    fn buffered_source<'a>(n: i64, chunk: usize) -> Exec<'a> {
        let types = [LogicalType::Int64];
        let mut chunks = Vec::new();
        let mut id = 0i64;
        while id < n {
            let take = ((n - id) as usize).min(chunk);
            let mut c = DataChunk::new(&types);
            for i in 0..take {
                c.columns[0].set_value(i, &Value::Int64(id));
                id += 1;
            }
            c.set_flat(take);
            chunks.push(c);
        }
        Exec::Buffered { chunks, idx: 0 }
    }

    /// The streaming harness re-packs a high-fanout source into
    /// `VECTOR_CAPACITY`-bounded chunks (not one giant materialized chunk),
    /// resuming across calls — the core streaming win, and the property the old
    /// eager engine lacked.
    #[test]
    fn stream_expand_rechunks_to_vector_capacity() {
        let catalog = Catalog::new();
        let storage = InMemStorage::new();
        let layout = int_layout();
        test_execution!(execution);
        let ctx = Ctx::new(&catalog, &storage, &layout, &execution);

        const N: i64 = (VECTOR_CAPACITY * 2 + 1) as i64;
        let mut src = buffered_source(N, VECTOR_CAPACITY / 3);
        let mut st = ExpandState::default();
        let mut sizes = Vec::new();
        loop {
            // Identity expander: one output row per input row.
            let out = stream_expand(&mut st, &mut src, &ctx, |chunk, pos, out| {
                out.push(vec![chunk.columns[0].get_value(pos)]);
                Ok(())
            })
            .unwrap();
            match out {
                Some(c) => sizes.push(c.size()),
                None => break,
            }
        }

        assert_eq!(
            sizes.iter().sum::<usize>(),
            N as usize,
            "every row preserved"
        );
        assert!(sizes.len() >= 3, "yielded multiple chunks, got {sizes:?}");
        assert!(
            sizes.iter().all(|&s| s <= VECTOR_CAPACITY),
            "no chunk exceeds the cap"
        );
        assert_eq!(sizes[0], VECTOR_CAPACITY, "non-final chunks are full");
    }

    /// A single high-fanout input row whose expansion exceeds one chunk is yielded
    /// across multiple chunks (the resumable-pending path), not buffered whole.
    #[test]
    fn stream_expand_resumes_across_a_single_fanout_row() {
        let catalog = Catalog::new();
        let storage = InMemStorage::new();
        let layout = int_layout();
        test_execution!(execution);
        let ctx = Ctx::new(&catalog, &storage, &layout, &execution);

        // One input row that fans out across three output chunks.
        const N: i64 = (VECTOR_CAPACITY * 2 + 1) as i64;
        let mut src = buffered_source(1, 1);
        let mut st = ExpandState::default();
        let mut total = 0usize;
        let mut count = 0usize;
        loop {
            let out = stream_expand(&mut st, &mut src, &ctx, |_chunk, _pos, out| {
                for i in 0..N {
                    out.push(vec![Value::Int64(i)]);
                }
                Ok(())
            })
            .unwrap();
            match out {
                Some(c) => {
                    assert!(c.size() <= VECTOR_CAPACITY);
                    total += c.size();
                    count += 1;
                }
                None => break,
            }
        }
        assert_eq!(total, N as usize);
        assert!(count >= 3, "fanout spanned multiple chunks, got {count}");
    }

    /// `drain_count(stop_at_first)` (the `EXISTS {}` short-circuit) returns at the
    /// first match without draining the rest of the source.
    #[test]
    fn drain_count_short_circuits_for_exists() {
        let catalog = Catalog::new();
        let storage = InMemStorage::new();
        let layout = int_layout();
        test_execution!(execution);
        let ctx = Ctx::new(&catalog, &storage, &layout, &execution);

        let mut src = buffered_source(3, 1); // three single-row chunks
        let n = drain_count(&mut src, &ctx, true).unwrap();
        assert_eq!(n, 1, "stopped at the first match");
        assert!(
            matches!(src, Exec::Buffered { idx, .. } if idx == 1),
            "did not consume the remaining chunks"
        );

        // Without the flag, it drains everything.
        let mut src = buffered_source(3, 1);
        assert_eq!(drain_count(&mut src, &ctx, false).unwrap(), 3);
    }

    // --- P3 step 9: morsel-driven parallelism ---

    fn m(table_idx: usize, start: u64, end: u64) -> Morsel {
        Morsel {
            table_idx,
            start,
            end,
        }
    }

    /// `slice_morsels` cuts each table's offset space into contiguous, scan-ordered
    /// slices — so concatenating their results reproduces a serial scan.
    #[test]
    fn slice_morsels_partitions_in_scan_order() {
        // One table, exact multiple.
        assert_eq!(slice_morsels(&[6], 3), vec![m(0, 0, 3), m(0, 3, 6)]);
        // Ragged last slice.
        assert_eq!(
            slice_morsels(&[7], 3),
            vec![m(0, 0, 3), m(0, 3, 6), m(0, 6, 7)]
        );
        // Multi-table (polymorphic scan): table 0 fully, then table 1 — scan order.
        assert_eq!(
            slice_morsels(&[4, 0, 3], 2),
            vec![m(0, 0, 2), m(0, 2, 4), m(2, 0, 2), m(2, 2, 3)],
            "empty table contributes no morsels; tables stay in order"
        );
        // A morsel wider than the table is one slice; an empty table is none.
        assert_eq!(slice_morsels(&[5], 100), vec![m(0, 0, 5)]);
        assert!(slice_morsels(&[0], 4).is_empty());
        // The morsels' offsets exactly tile [0, count) with no gaps or overlaps.
        let ms = slice_morsels(&[10], 4);
        assert_eq!(ms.first().unwrap().start, 0);
        assert_eq!(ms.last().unwrap().end, 10);
        for w in ms.windows(2) {
            assert_eq!(w[0].end, w[1].start);
        }
    }

    fn scan_node() -> ScanNode {
        ScanNode {
            var: VarId(0),
            id_col: 0,
            tables: vec![],
        }
    }

    /// `spine_scan` finds the driving scan only under a linear stateless spine, and
    /// bails on a branch/stateful op — the parallelism gate.
    #[test]
    fn spine_scan_accepts_linear_spine_rejects_branches() {
        // Bare scan.
        assert!(spine_scan(&PlanOp::ScanNode(scan_node())).is_some());
        // Filter over a scan (a stateless spine op).
        let filtered = PlanOp::Filter {
            input: Box::new(PlanOp::ScanNode(scan_node())),
            predicate: BoundExpr::Literal(Value::Bool(true)),
        };
        assert!(spine_scan(&filtered).is_some());
        // A leaf that is not a node scan: not parallelizable here.
        assert!(spine_scan(&PlanOp::SingleRow).is_none());
        // A branch (cross product) clears the linear-spine requirement.
        let cross = PlanOp::CrossProduct {
            left: Box::new(PlanOp::ScanNode(scan_node())),
            left_width: 1,
            right: Box::new(PlanOp::ScanNode(scan_node())),
            right_width: 1,
        };
        assert!(spine_scan(&cross).is_none());
    }

    #[test]
    fn spine_fanout_steps_counts_fanout_ops() {
        // A bare scan does no fan-out.
        assert_eq!(spine_fanout_steps(&PlanOp::ScanNode(scan_node())), 0);
        // `Unwind` is a fan-out step; a `Filter` between is transparent (P3 step 10b
        // L4 — the count gates parallelizing a small scan with heavy fan-out).
        let unwound = PlanOp::Unwind {
            input: Box::new(PlanOp::Filter {
                input: Box::new(PlanOp::ScanNode(scan_node())),
                predicate: BoundExpr::Literal(Value::Bool(true)),
            }),
            list: BoundExpr::Literal(Value::Null),
            target: UnwindTarget::Scalar { col: 0 },
        };
        assert_eq!(spine_fanout_steps(&unwound), 1);
    }

    fn agg(op: AggOp, distinct: bool, arg_ty: LogicalType) -> BoundExpr {
        BoundExpr::Aggregate {
            op,
            distinct,
            arg: Some(Box::new(BoundExpr::Literal(match arg_ty {
                LogicalType::Double => Value::Double(0.0),
                LogicalType::Int64 => Value::Int64(0),
                _ => Value::Null,
            }))),
            ty: LogicalType::Int64,
        }
    }

    /// Only accumulators that merge bit-identically across morsels are parallel-safe:
    /// integer count/sum/avg/min/max are; a float SUM/AVG (non-associative f64) and any
    /// DISTINCT aggregate are not.
    #[test]
    fn aggs_parallel_safe_gate() {
        assert!(aggs_parallel_safe(&BoundExpr::Literal(Value::Int64(1))));
        assert!(aggs_parallel_safe(&agg(
            AggOp::Count,
            false,
            LogicalType::Int64
        )));
        assert!(aggs_parallel_safe(&agg(
            AggOp::Sum,
            false,
            LogicalType::Int64
        )));
        assert!(aggs_parallel_safe(&agg(
            AggOp::Avg,
            false,
            LogicalType::Int64
        )));
        assert!(aggs_parallel_safe(&agg(
            AggOp::Min,
            false,
            LogicalType::Double
        )));
        // Float SUM/AVG: non-associative accumulation -> serial only.
        assert!(!aggs_parallel_safe(&agg(
            AggOp::Sum,
            false,
            LogicalType::Double
        )));
        assert!(!aggs_parallel_safe(&agg(
            AggOp::Avg,
            false,
            LogicalType::Double
        )));
        // DISTINCT: cross-morsel dedup is order-sensitive -> serial only.
        assert!(!aggs_parallel_safe(&agg(
            AggOp::Count,
            true,
            LogicalType::Int64
        )));
        // Nested in a scalar expression (sum(x) + 1): the inner agg still decides.
        let nested = BoundExpr::Scalar {
            op: koko_function::ScalarOp::Add,
            args: vec![
                agg(AggOp::Sum, false, LogicalType::Double),
                BoundExpr::Literal(Value::Int64(1)),
            ],
            ty: LogicalType::Double,
        };
        assert!(!aggs_parallel_safe(&nested));
    }
    #[test]
    fn operator_memory_limit_rejects_hash_join_build() {
        let layout = int_layout();
        let batch_bytes = DataChunk::new(&layout.col_types).allocated_bytes();
        test_execution_with_memory!(
            execution,
            tracker,
            query_memory,
            Some(batch_bytes + 4096),
            QueryControl::default()
        );
        let catalog = Catalog::new();
        let storage = InMemStorage::new();
        let ctx = Ctx::new(&catalog, &storage, &layout, &execution);
        let resolver = LayoutResolver(&layout);
        let key_expr = BoundExpr::Column {
            col: 0,
            ty: LogicalType::Int64,
        };
        let mut join = Exec::HashJoin {
            probe: Box::new(buffered_source(1, 1)),
            build: Box::new(buffered_source(256, 256)),
            probe_cols: (0, 1),
            build_cols: (0, 1),
            probe_keys: vec![compile(&key_expr, &resolver).unwrap()],
            build_keys: vec![compile(&key_expr, &resolver).unwrap()],
            table: None,
            kind: JoinKind::Inner,
            st: ExpandState::default(),
        };

        let error = join.next_chunk(&ctx).unwrap_err();
        assert!(matches!(error, Error::BufferManager));
        assert_eq!(
            error.to_string(),
            "Buffer manager exception: Unable to allocate memory! The buffer pool is full and no memory could be freed!"
        );
        assert!(tracker.usage().peak > batch_bytes);
        drop(query_memory);
        assert_eq!(tracker.usage().current, 0);
    }

    #[test]
    fn operator_memory_limit_rejects_collect_aggregate() {
        let layout = int_layout();
        test_execution_with_memory!(
            execution,
            tracker,
            query_memory,
            Some(4096),
            QueryControl::default()
        );
        let catalog = Catalog::new();
        let storage = InMemStorage::new();
        let ctx = Ctx::new(&catalog, &storage, &layout, &execution);
        let resolver = LayoutResolver(&layout);
        let plan = AggPlan {
            item_execs: Vec::new(),
            aggs: vec![AggSpec {
                op: AggOp::Collect,
                distinct: false,
                arg: Some(
                    compile(
                        &BoundExpr::Literal(Value::String("x".repeat(256))),
                        &resolver,
                    )
                    .unwrap(),
                ),
            }],
            group_keys: Vec::new(),
        };
        let mut input = buffered_source(128, 128);

        let error = match accumulate_groups(&plan, &mut input, &ctx) {
            Err(error) => error,
            Ok(_) => panic!("collect aggregation unexpectedly fit under the memory limit"),
        };
        assert!(matches!(error, Error::BufferManager));
        assert_eq!(
            error.to_string(),
            "Buffer manager exception: Unable to allocate memory! The buffer pool is full and no memory could be freed!"
        );
        assert!(tracker.usage().peak > 0);
        drop(query_memory);
        assert_eq!(tracker.usage().current, 0);
    }

    #[test]
    fn operator_memory_limit_rejects_distinct_metadata() {
        let types = vec![LogicalType::Int64];
        let batch_bytes = DataChunk::new(&types).allocated_bytes();
        let tracker = MemoryTracker::new(Some(batch_bytes + 1024));
        let memory = QueryMemory::new(&tracker).unwrap();
        let mut output = OutputBuffer::new(types, false);
        for value in 0..100 {
            memory
                .charge(output.push(vec![Value::Int64(value)], Vec::new()))
                .unwrap();
        }

        let error = output
            .finish(vec!["value".to_string()], true, &[], 0, None, &memory)
            .unwrap_err();
        assert!(matches!(error, Error::BufferManager));
        assert_eq!(
            error.to_string(),
            "Buffer manager exception: Unable to allocate memory! The buffer pool is full and no memory could be freed!"
        );
        drop(memory);
        assert_eq!(tracker.usage().current, 0);
    }

    #[test]
    fn pull_boundary_honors_shared_cancellation_and_deadline() {
        let epoch = AtomicU64::new(0);
        let control = QueryControl::new(&epoch, 0, None);
        epoch.store(1, AtomicOrdering::Release);
        let interrupted = control.check().unwrap_err();
        assert!(matches!(interrupted, Error::Interrupt));
        assert_eq!(interrupted.to_string(), "Interrupted.");

        let expired = Instant::now()
            .checked_sub(std::time::Duration::from_secs(1))
            .expect("one second before now is representable");
        let timeout = QueryControl::new(&epoch, 1, Some(expired))
            .check()
            .unwrap_err();
        assert!(matches!(timeout, Error::Interrupt));
        assert_eq!(timeout.to_string(), "Interrupted.");
    }
}
