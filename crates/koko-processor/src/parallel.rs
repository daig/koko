use super::*;

// ---------------------------------------------------------------------------
// Morsel-driven parallel execution (P3 step 9)
// ---------------------------------------------------------------------------

/// Below this many candidate rows a scan runs serially — the per-query thread spawn
/// + merge would dominate a small scan.
pub(crate) const PARALLEL_MIN_ROWS: u64 = 4 * VECTOR_CAPACITY as u64;

/// One unit of parallel scan work: a single table's `[start, end)` offset slice.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Morsel {
    pub(crate) table_idx: usize,
    pub(crate) start: u64,
    pub(crate) end: u64,
}

/// Slice each candidate table's `[0, count)` offset space into `morsel_rows`-sized
/// morsels, in scan order (table 0 first). An empty table contributes none.
pub(crate) fn slice_morsels(counts: &[u64], morsel_rows: u64) -> Vec<Morsel> {
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
pub(crate) fn scan_node_count(
    storage: &InMemStorage,
    sources: &IcebugQuerySources,
    table: TableId,
) -> u64 {
    sources
        .num_rows(table)
        .unwrap_or_else(|| storage.node_count(table))
}

pub(crate) struct ScanDispatcher {
    pub(crate) morsels: Vec<Morsel>,
    pub(crate) next: std::sync::atomic::AtomicUsize,
}

impl ScanDispatcher {
    /// Slice each candidate table's `[0, node_count)` into `morsel_rows`-sized
    /// morsels (tombstoned offsets inside a slice are skipped at scan time; an empty
    /// table contributes none).
    pub(crate) fn new(
        scan: &ScanNode,
        storage: &InMemStorage,
        sources: &IcebugQuerySources,
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
    pub(crate) fn next_morsel(&self) -> Option<(usize, Morsel)> {
        let i = self.next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.morsels.get(i).map(|m| (i, *m))
    }
}

/// Target ~`4 × threads` morsels (good load balance), each ≥ `min_morsel` rows.
/// `min_morsel` is normally one vector wide; for a small scan feeding a heavy
/// fan-out (P3 step 10b L4) it is lowered so the scan still splits across workers
/// (a 1.7K-row scan under a 2K floor would be one morsel = no parallelism).
pub(crate) fn morsel_rows_for(
    scan: &ScanNode,
    storage: &InMemStorage,
    sources: &IcebugQuerySources,
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
pub(crate) fn run_morsels<T, F>(
    threads: usize,
    dispatcher: &ScanDispatcher,
    f: F,
) -> Result<Vec<(usize, T)>>
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
pub(crate) fn parallel_scan_source<'a>(
    root: &'a PlanOp,
    projection: &BoundProjection,
    ctx: &OperatorContext<'_>,
) -> Option<&'a ScanNode> {
    if ctx.worker_count <= 1 || !projection_parallel_safe(projection) {
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
        .map(|table| scan_node_count(ctx.storage, ctx.sources, table.table))
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
    let enough_to_split = (ctx.worker_count as u64) * 2;
    (steps >= 1 && rows >= enough_to_split && effective >= PARALLEL_MIN_ROWS as f64).then_some(scan)
}

/// The leaf `ScanNode` of a **linear stateless spine** (only `Filter`/`Extend`/
/// `VarLengthExtend`/`ProjectPath`/`Unwind` above it), or `None` if `root` branches,
/// is stateful/correlated, or its leaf is not a `ScanNode`. These ops carry no
/// cross-row state, so a per-morsel pipeline is correct with no redundant work.
pub(crate) fn spine_scan(op: &PlanOp) -> Option<&ScanNode> {
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
pub(crate) fn spine_fanout_steps(op: &PlanOp) -> usize {
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
pub(crate) const ASSUMED_FANOUT: f64 = 3.0;

/// Whether every aggregate in the projection merges **bit-identically** across
/// morsels: no `DISTINCT` (cross-morsel dedup is order-sensitive) and no `SUM`/`AVG`
/// over a floating-point argument (f64 addition is non-associative, so a partitioned
/// float sum would diverge from the serial left-to-right sum). Integer `SUM`/`AVG`
/// (exact `i128`), `COUNT`, `MIN`/`MAX`, `COLLECT` are all safe.
pub(crate) fn projection_parallel_safe(projection: &BoundProjection) -> bool {
    projection.items.iter().all(|item| match item {
        ProjItem::Var { .. } => true,
        ProjItem::Scalar { expr, .. } => aggs_parallel_safe(expr),
    })
}

pub(crate) fn is_float_type(ty: &LogicalType) -> bool {
    matches!(
        ty,
        LogicalType::Double | LogicalType::Float | LogicalType::Decimal(..)
    )
}

pub(crate) fn aggs_parallel_safe(expr: &BoundExpr) -> bool {
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
pub(crate) fn read_part_results<'a>(
    projection: &BoundProjection,
    part_plan: &'a PartPlan,
    ctx: &OperatorContext<'a>,
    input: &'a [DataChunk],
) -> Result<ExecResult> {
    if let Some(scan) = parallel_scan_source(&part_plan.root, projection, ctx) {
        return parallel_results(projection, &part_plan.root, scan, ctx, input);
    }
    let mut root = build_exec(&part_plan.root, ctx, input)?;
    produce_results(projection, &mut root, ctx)
}

/// Output column names for a projection (shared by the serial + parallel sinks).
pub(crate) fn projection_column_names(projection: &BoundProjection) -> Vec<String> {
    projection
        .items
        .iter()
        .map(|i| match i {
            ProjItem::Scalar { name, .. } | ProjItem::Var { name, .. } => name.clone(),
        })
        .collect()
}

pub(crate) fn projection_column_types(
    projection: &BoundProjection,
    layout: &RowLayout,
) -> Vec<LogicalType> {
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
pub(crate) fn parallel_results<'a>(
    projection: &BoundProjection,
    root: &'a PlanOp,
    scan: &ScanNode,
    ctx: &OperatorContext<'a>,
    input: &'a [DataChunk],
) -> Result<ExecResult> {
    let threads = ctx.worker_count.max(1);
    // A small scan only got here via the heavy-fan-out path, so let it split below the
    // one-vector floor; a large scan keeps the vector-wide morsel.
    let scan_rows: u64 = scan
        .tables
        .iter()
        .map(|table| scan_node_count(ctx.storage, ctx.sources, table.table))
        .sum();
    let min_morsel = if scan_rows >= PARALLEL_MIN_ROWS {
        VECTOR_CAPACITY as u64
    } else {
        1
    };
    let morsel_rows = morsel_rows_for(scan, ctx.storage, ctx.sources, threads, min_morsel);
    let dispatcher = ScanDispatcher::new(scan, ctx.storage, ctx.sources, morsel_rows);

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
            output.append(block, ctx.memory)?;
        }
        output
    };
    finish_projection(
        output,
        projection_column_names(projection),
        projection,
        ctx.memory,
    )
}
