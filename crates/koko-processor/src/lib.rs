//! `koko-processor` — pull-based (Volcano) typed-batch execution with
//! factorization, morsel parallelism, and columnar result production.
//!
//! Executes a [`QueryPlan`] (one [`PartPlan`] per `WITH`-delimited part) against
//! the typed [`InMemStorage`] engine, then applies each part's projection (scalar/aggregate),
//! `DISTINCT`, `ORDER BY`, `SKIP`, and `LIMIT` — or, for write queries, performs
//! the `CREATE`/`SET`/`DELETE`/`MERGE` mutations. Parts run in sequence, each
//! seeded by the previous part's projected rows.
//!
//! Each [`PlanOp`] is compiled into a stateful pull operator (`Exec`); the sink
//! drives `while let Some(chunk) = root.next_chunk(ctx)? { … }`, and every operator
//! readies at most one [`DataChunk`] (≤[`VECTOR_CAPACITY`] rows) per call. Stateless
//! pipelines stream and `LIMIT`/`EXISTS` terminate early; joins, aggregation, ordering,
//! recursive enumeration, and writes are explicit pipeline breakers. Recursive paths
//! materialize one source at a time before the next source is traversed. Execution is
//! flat with multiplicity factorization. Eligible stateless scan spines run
//! morsel-parallel; stateful shapes fall back to serial execution. Writes
//! are pipeline breakers: the read pipeline is drained (releasing `&storage`) before
//! the clause mutates with `&mut storage`.

mod build;
mod context;
mod entity;
mod operator;
mod parallel;
mod result;
pub mod table_function;
mod write;

use build::*;
use context::*;
use entity::*;
use operator::*;
use parallel::*;
use result::*;
use write::*;

pub use context::{ExecutionContext, QueryMemory};
pub use result::ExecResult;

use koko_catalog::{Catalog, ColumnDefault};
use koko_common::{
    ColumnData, DataChunk, Error, ExtendDir, InternalId, LogicalType, MemoryReservation,
    MemoryTracker, NodeValue, QueryControl, RecursiveRelValue, RelValue, Result, Selection,
    TableId, VECTOR_CAPACITY, Value, ValueVector, value_payload_bytes,
};
use koko_expr::{
    AccessorKind, AggSpec, ColumnResolver, CompiledExpr, EvalState, compile, compile_collect,
    eval_constant,
};
use koko_function::{
    AggOp, AggState, BuiltinScalar, ValueKey, cast_value, cypher_cmp, eval_scalar,
    eval_scalar_func_with_context, oracle_hash::RandomState, order_cmp,
};
use koko_ir::bound::{
    BoundCreate, BoundDelete, BoundExpr, BoundProjection, BoundQuery, BoundRegularQuery, BoundSet,
    BoundSetTarget, BoundTableFunc, LambdaVarId, OrderKey, PathSemantic, ProjItem, RecursiveMode,
    SequenceFn, SubqueryKind, VarId,
};
use koko_ir::plan::{
    Extend, ExtendTarget, IndexScan, InputSlot, JoinKind, MaterializeItem, MergePlan, PartPlan,
    PathRel, PlanOp, ProjectPath, QueryPlan, RegularPlan, RowLayout, ScanNode, ScanTable,
    UnwindTarget, UpdateOp, VarColKind, VarLengthExtend,
};
use koko_loader::{
    icebug::{IcebugNodeScan, IcebugQuerySources},
    scan::LoadScan as SourceLoadScan,
};
use koko_storage::{
    BatchNeighbor, InMemStorage, SharedStorage, StorageReadHandle, StorageWriteHandle,
};
use std::cmp::Ordering;
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Mutex;
use table_function::{TableFunctionRuntime, produce_table_function_rows};

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
    let visibility = ReadVisibilityCache::default();
    let (column_names, column_types) = rq.result_columns().iter().cloned().unzip();
    let mut output = OutputBuffer::new(column_types, false);
    for (query, operand_plan) in rq.operands.iter().zip(&plan.operands) {
        output.append(
            OutputBuffer::from_exec(execute_with_visibility(
                query,
                operand_plan,
                catalog,
                storage,
                execution,
                &visibility,
            )?),
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
    let visibility = ReadVisibilityCache::default();
    let (column_names, column_types) = rq.result_columns().iter().cloned().unzip();
    let mut output = OutputBuffer::new(column_types, false);
    for (query, operand_plan) in rq.operands.iter().zip(&plan.operands) {
        output.append(
            OutputBuffer::from_exec(execute_synchronized(
                query,
                operand_plan,
                catalog,
                storage,
                execution,
                &visibility,
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
    visibility: &ReadVisibilityCache,
) -> Result<ExecResult> {
    debug_assert_eq!(query.parts.len(), plan.parts.len());
    let mut input = Vec::new();
    for (index, (part, part_plan)) in query.parts.iter().zip(&plan.parts).enumerate() {
        // A prior query part or UNION operand may have mutated storage. Visibility
        // decisions are stable only while this part holds its storage guard.
        visibility.clear();
        let is_last = index + 1 == query.parts.len();
        let layout = &part_plan.layout;
        if part_plan.update_ops.is_empty() {
            let guard = storage.read();
            let context = OperatorContext::new(catalog, &guard, layout, execution, visibility);
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
            let context = OperatorContext::new(catalog, &guard, layout, execution, visibility);
            let mut root = build_exec(&part_plan.root, &context, &input)?;
            let mut eval = EvalState::new();
            drain_all(&mut root, &context, &mut eval)?
        };
        {
            let mut guard = storage.write();
            for update in &part_plan.update_ops {
                chunks = WriteExecutor::new(catalog, &mut guard, execution, visibility)
                    .apply(update, layout, chunks)?;
            }
        }
        let guard = storage.read();
        let context = OperatorContext::new(catalog, &guard, layout, execution, visibility);
        let mut root = Exec::Buffered(BufferedState { chunks, idx: 0 });
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

/// Execute one bound query with an execution-local relationship visibility cache.
pub fn execute(
    query: &BoundQuery,
    plan: &QueryPlan,
    catalog: &Catalog,
    storage: &mut InMemStorage,
    execution: &ExecutionContext<'_>,
) -> Result<ExecResult> {
    let visibility = ReadVisibilityCache::default();
    execute_with_visibility(query, plan, catalog, storage, execution, &visibility)
}

/// Execute a bound query against the catalog and storage.
///
/// Parts run in sequence: each part's reading pipeline is seeded with the
/// previous part's projected rows (its carried scalar scope), executed, then
/// projected — for a `WITH` part the projection feeds the next part; for the
/// terminal part it is the result (or a `CREATE` runs).
fn execute_with_visibility(
    query: &BoundQuery,
    plan: &QueryPlan,
    catalog: &Catalog,
    storage: &mut InMemStorage,
    execution: &ExecutionContext<'_>,
    visibility: &ReadVisibilityCache,
) -> Result<ExecResult> {
    debug_assert_eq!(query.parts.len(), plan.parts.len());
    // Rows carried in from the previous part, materialized into the current
    // part's input layout (empty for the first part).
    let mut input: Vec<DataChunk> = Vec::new();

    for (idx, (part, part_plan)) in query.parts.iter().zip(&plan.parts).enumerate() {
        visibility.clear();
        let is_last = idx + 1 == query.parts.len();
        let layout = &part_plan.layout;

        // A read part (no updating clauses) is produced straight from the plan —
        // morsel-parallel when the gate passes, else the serial streamed pull (so a
        // terminal `LIMIT`/`EXISTS` can terminate early and intermediates never fully
        // materialize). A write part is a pipeline breaker handled separately below.
        // Building/pulling the `Exec` tree borrows storage only through the `ctx`
        // threaded into `next_chunk`, so a read holds no lasting `&mut` borrow.
        if part_plan.update_ops.is_empty() {
            let ctx = OperatorContext::new(catalog, &*storage, layout, execution, visibility);
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
            let ctx = OperatorContext::new(catalog, &*storage, layout, execution, visibility);
            let mut root = build_exec(&part_plan.root, &ctx, &input)?;
            let mut eval = EvalState::new();
            drain_all(&mut root, &ctx, &mut eval)?
        };
        for op in &part_plan.update_ops {
            chunks = WriteExecutor::new(catalog, storage, execution, visibility)
                .apply(op, layout, chunks)?;
        }
        let mut root = Exec::Buffered(BufferedState { chunks, idx: 0 });

        // Reads are done; this immutable borrow only feeds result production.
        let ctx = OperatorContext::new(catalog, &*storage, layout, execution, visibility);

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

#[cfg(test)]
mod tests {
    use super::*;
    use koko_storage::InMemStorage;
    use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
    use std::time::Instant;

    /// A minimal context over an empty catalog/storage with a one-`INT64`-column
    /// layout — enough to drive the streaming harness without the binder/planner.
    fn int_layout() -> RowLayout {
        let mut layout = RowLayout::default();
        layout.col_types = vec![LogicalType::Int64];
        layout
    }

    struct EmptyTableRuntime;

    impl TableFunctionRuntime for EmptyTableRuntime {
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
            let sources = IcebugQuerySources::default();
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
            let sources = IcebugQuerySources::default();
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
        Exec::Buffered(BufferedState { chunks, idx: 0 })
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
        let visibility = ReadVisibilityCache::default();
        let ctx = OperatorContext::new(&catalog, &storage, &layout, &execution, &visibility);

        const N: i64 = (VECTOR_CAPACITY * 2 + 1) as i64;
        let mut src = buffered_source(N, VECTOR_CAPACITY / 3);
        let mut st = ExpandState::default();
        let mut eval = EvalState::new();
        let mut sizes = Vec::new();
        loop {
            // Identity expander: one output row per input row.
            let out = stream_expand(
                &mut st,
                &mut src,
                &ctx,
                &mut eval,
                |chunk, pos, out, _eval| {
                    out.push(vec![chunk.columns[0].get_value(pos)]);
                    Ok(())
                },
            )
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
        let visibility = ReadVisibilityCache::default();
        let ctx = OperatorContext::new(&catalog, &storage, &layout, &execution, &visibility);

        // One input row that fans out across three output chunks.
        const N: i64 = (VECTOR_CAPACITY * 2 + 1) as i64;
        let mut src = buffered_source(1, 1);
        let mut st = ExpandState::default();
        let mut eval = EvalState::new();
        let mut total = 0usize;
        let mut count = 0usize;
        loop {
            let out = stream_expand(
                &mut st,
                &mut src,
                &ctx,
                &mut eval,
                |_chunk, _pos, out, _eval| {
                    for i in 0..N {
                        out.push(vec![Value::Int64(i)]);
                    }
                    Ok(())
                },
            )
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
        let visibility = ReadVisibilityCache::default();
        let ctx = OperatorContext::new(&catalog, &storage, &layout, &execution, &visibility);
        let mut eval = EvalState::new();

        let mut src = buffered_source(3, 1); // three single-row chunks
        let n = drain_count(&mut src, &ctx, true, &mut eval).unwrap();
        assert_eq!(n, 1, "stopped at the first match");
        assert!(
            matches!(src, Exec::Buffered(BufferedState { idx, .. }) if idx == 1),
            "did not consume the remaining chunks"
        );

        // Without the flag, it drains everything.
        let mut src = buffered_source(3, 1);
        assert_eq!(drain_count(&mut src, &ctx, false, &mut eval).unwrap(), 3);
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
        let visibility = ReadVisibilityCache::default();
        let catalog = Catalog::new();
        let storage = InMemStorage::new();
        let ctx = OperatorContext::new(&catalog, &storage, &layout, &execution, &visibility);
        let resolver = LayoutResolver(&layout);
        let key_expr = BoundExpr::Column {
            col: 0,
            ty: LogicalType::Int64,
        };
        let mut join = Exec::HashJoin(HashJoinState {
            probe: Box::new(buffered_source(1, 1)),
            build: Box::new(buffered_source(256, 256)),
            probe_cols: (0, 1),
            build_cols: (0, 1),
            probe_keys: vec![compile(&key_expr, &resolver).unwrap()],
            build_keys: vec![compile(&key_expr, &resolver).unwrap()],
            table: None,
            kind: JoinKind::Inner,
            st: ExpandState::default(),
        });

        let error = join.next_chunk(&ctx, &mut EvalState::new()).unwrap_err();
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
        let visibility = ReadVisibilityCache::default();
        let catalog = Catalog::new();
        let storage = InMemStorage::new();
        let ctx = OperatorContext::new(&catalog, &storage, &layout, &execution, &visibility);
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
