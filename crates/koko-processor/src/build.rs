use super::*;

/// Adapts a [`RowLayout`] to the `koko-expr` column resolver.
pub(crate) struct LayoutResolver<'a>(pub(crate) &'a RowLayout);

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
pub(crate) struct ChunkBuilder<'a> {
    pub(crate) col_types: &'a [LogicalType],
    pub(crate) chunks: Vec<DataChunk>,
    pub(crate) current: DataChunk,
    pub(crate) len: usize,
}

impl<'a> ChunkBuilder<'a> {
    pub(crate) fn new(col_types: &'a [LogicalType]) -> Self {
        Self {
            col_types,
            chunks: Vec::new(),
            current: DataChunk::new(col_types),
            len: 0,
        }
    }

    pub(crate) fn push_row(&mut self, row: &[Value]) {
        for (c, v) in row.iter().enumerate() {
            self.current.columns[c].set_value(self.len, v);
        }
        self.len += 1;
        if self.len == VECTOR_CAPACITY {
            self.flush();
        }
    }

    pub(crate) fn flush(&mut self) {
        if self.len > 0 {
            self.current.set_flat(self.len);
            let full = std::mem::replace(&mut self.current, DataChunk::new(self.col_types));
            self.chunks.push(full);
            self.len = 0;
        }
    }

    pub(crate) fn finish(mut self) -> Vec<DataChunk> {
        self.flush();
        self.chunks
    }
}

// ---------------------------------------------------------------------------
// Streaming (pull) execution engine
// ---------------------------------------------------------------------------

/// Drive `root` to exhaustion, collecting all its chunks (a pipeline breaker —
/// used by write parts, `CrossProduct`'s build side, and `MERGE`'s per-row match).
pub(crate) fn drain_all<'a>(
    root: &mut Exec<'a>,
    ctx: &OperatorContext<'a>,
    eval: &mut EvalState,
) -> Result<Vec<DataChunk>> {
    let mut chunks = Vec::new();
    while let Some(c) = root.next_chunk(ctx, eval)? {
        ctx.memory.charge(c.allocated_bytes())?;
        chunks.push(c);
    }
    Ok(chunks)
}

/// Count the rows `root` produces. With `stop_at_first`, return as soon as one is
/// found (the `EXISTS {}` short-circuit); otherwise drain fully (`COUNT {}`).
pub(crate) fn drain_count<'a>(
    root: &mut Exec<'a>,
    ctx: &OperatorContext<'a>,
    stop_at_first: bool,
    eval: &mut EvalState,
) -> Result<i64> {
    let mut count = 0i64;
    while let Some(c) = root.next_chunk(ctx, eval)? {
        count += c.size() as i64;
        if stop_at_first && count > 0 {
            break;
        }
    }
    Ok(count)
}

/// Read a full-width binding row from a chunk position.
pub(crate) fn row_at(chunk: &DataChunk, pos: usize, width: usize) -> Vec<Value> {
    (0..width)
        .map(|c| chunk.columns[c].get_value(pos))
        .collect()
}

/// Build a one-row seed chunk from a full-width row (the per-row input to a
/// correlated sub-pipeline).
pub(crate) fn seed_chunk(row: &[Value], col_types: &[LogicalType]) -> DataChunk {
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
pub(crate) enum JoinKey {
    InternalId(InternalId),
    InternalIds(InternalId, InternalId),
    One(ValueKey),
    Many(Vec<ValueKey>),
}

impl JoinKey {
    pub(crate) fn retained_bytes(&self) -> u64 {
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

pub(crate) fn compiled_internal_id_column(expr: &CompiledExpr) -> Option<usize> {
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
pub(crate) fn eval_join_key(
    keys: &[CompiledExpr],
    chunk: &DataChunk,
    pos: usize,
    random: &RandomState,
    eval: &mut EvalState,
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
        let value = expr.eval(chunk, pos, random, eval)?;
        return Ok((!value.is_null()).then(|| JoinKey::One(ValueKey::from_value(&value))));
    }
    let mut key = Vec::with_capacity(keys.len());
    for expr in keys {
        let value = expr.eval(chunk, pos, random, eval)?;
        if value.is_null() {
            return Ok(None);
        }
        key.push(ValueKey::from_value(&value));
    }
    Ok(Some(JoinKey::Many(key)))
}

/// Compile a [`PlanOp`] tree into a streaming [`Exec`] tree. Children are built
/// recursively and owned by their parent; correlated sub-patterns are kept as
/// `&PlanOp` (rebuilt per input row, seeded with that row). `input` is the carried
/// scope replayed by an `InputScan` leaf.
pub(crate) fn build_exec<'a>(
    op: &'a PlanOp,
    ctx: &OperatorContext<'a>,
    input: &'a [DataChunk],
) -> Result<Exec<'a>> {
    build_exec_morsel(op, ctx, input, None)
}

/// A single scan morsel: scan only `table_idx`, offsets `[start, end)`. The
/// parallel driver (P3 step 9) builds one bounded pipeline per morsel; serial
/// builds pass `None` (full multi-table scan).
pub(crate) type MorselBound = (usize, u64, u64);

/// Compile a [`PlanOp`] tree into a streaming [`Exec`] tree, optionally bounding the
/// driving spine's `ScanNode` to one morsel. `morsel` flows down the linear spine
/// (`Filter`/`Extend`/…`/`input` children) to the single leaf `ScanNode`; branch and
/// correlated children always get `None` (they are never on a parallelizable spine —
/// see `parallel_scan_source`).
pub(crate) fn build_exec_morsel<'a>(
    op: &'a PlanOp,
    ctx: &OperatorContext<'a>,
    input: &'a [DataChunk],
    morsel: Option<MorselBound>,
) -> Result<Exec<'a>> {
    let resolver = LayoutResolver(ctx.layout);
    Ok(match op {
        PlanOp::SingleRow => Exec::SingleRow(SingleRowState { done: false }),
        // The carried rows already sit in this part's layout, so replay them.
        PlanOp::InputScan => Exec::InputScan(InputScanState {
            chunks: input,
            idx: 0,
        }),
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
                Some((table_idx, offset, end)) => Exec::ScanNode(ScanNodeState {
                    scan,
                    table_idx,
                    offset,
                    end,
                    single_table: true,
                    projected_columns: projected_columns(),
                    external_reader: None,
                }),
                // Full scan: all candidate tables, each in full.
                None => Exec::ScanNode(ScanNodeState {
                    scan,
                    table_idx: 0,
                    offset: 0,
                    end: u64::MAX,
                    single_table: false,
                    projected_columns: projected_columns(),
                    external_reader: None,
                }),
            }
        }
        PlanOp::IndexScan(scan) => {
            if let Some(child) = &scan.input {
                Exec::IndexLookup(IndexLookupState {
                    input: Box::new(build_exec(child, ctx, input)?),
                    scan,
                    key: compile(&scan.pk_value, &resolver)?,
                    st: ExpandState::default(),
                })
            } else {
                Exec::IndexScan(IndexScanState { scan, done: false })
            }
        }
        PlanOp::ScanTableFunc { func, arg, cols } => Exec::ScanTableFunc(TableFunctionScanState {
            func: *func,
            arg: arg.as_deref(),
            cols,
            rows: None,
            idx: 0,
        }),
        PlanOp::LoadScan {
            cols,
            col_names,
            path: _,
            paths,
            format,
            options,
            bare,
        } => {
            let column_types = cols
                .iter()
                .map(|&column| ctx.layout.col_types[column].clone())
                .collect();
            Exec::LoadScan(LoadScanState {
                cols,
                source: SourceLoadScan::new(
                    paths,
                    col_names,
                    column_types,
                    options,
                    *format,
                    *bare,
                ),
            })
        }
        PlanOp::Filter {
            input: child,
            predicate,
        } => Exec::Filter(FilterState {
            input: Box::new(build_exec_morsel(child, ctx, input, morsel)?),
            predicate: compile(predicate, &resolver)?,
        }),
        PlanOp::Extend(extend) => Exec::Extend(ExtendState {
            input: Box::new(build_exec_morsel(&extend.input, ctx, input, morsel)?),
            extend,
            st: ExpandState::default(),
        }),
        PlanOp::VarLengthExtend(ve) => Exec::VarExtend(VarExtendState {
            input: Box::new(build_exec_morsel(&ve.input, ctx, input, morsel)?),
            ve,
            filter: build_recursive_filter(ve, &resolver)?,
            st: ExpandState::default(),
        }),
        PlanOp::ProjectPath(pp) => Exec::ProjectPath(ProjectPathState {
            input: Box::new(build_exec_morsel(&pp.input, ctx, input, morsel)?),
            pp,
            st: ExpandState::default(),
        }),
        PlanOp::Unwind {
            input: child,
            list,
            target,
        } => Exec::Unwind(UnwindState {
            input: Box::new(build_exec_morsel(child, ctx, input, morsel)?),
            list: compile(list, &resolver)?,
            target,
            st: ExpandState::default(),
        }),
        // Branch / correlated children are never on a parallelizable spine, so they
        // always build a full (`None`) scan.
        PlanOp::CrossProduct {
            left,
            left_width,
            right,
            right_width,
        } => Exec::CrossProduct(CrossProductState {
            left: Box::new(build_exec(left, ctx, input)?),
            right: Box::new(build_exec(right, ctx, input)?),
            left_width: *left_width,
            right_width: *right_width,
            right_buf: None,
            st: ExpandState::default(),
        }),
        PlanOp::HashJoin {
            probe,
            build,
            probe_cols,
            build_cols,
            keys,
            kind,
        } => Exec::HashJoin(HashJoinState {
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
        }),
        PlanOp::Optional {
            input: child,
            pattern,
            new_cols,
        } => Exec::Optional(OptionalState {
            input: Box::new(build_exec(child, ctx, input)?),
            pattern,
            new_cols,
            st: ExpandState::default(),
        }),
        PlanOp::Subquery {
            input: child,
            pattern,
            result_col,
            kind,
        } => Exec::Subquery(SubqueryState {
            input: Box::new(build_exec(child, ctx, input)?),
            pattern,
            result_col: *result_col,
            kind: *kind,
            st: ExpandState::default(),
        }),
        PlanOp::SequenceCall {
            input: child,
            func,
            name,
            result_col,
        } => Exec::SequenceCall(SequenceCallState {
            input: Box::new(build_exec(child, ctx, input)?),
            func: *func,
            name,
            result_col: *result_col,
            st: ExpandState::default(),
        }),
        PlanOp::MaterializeValues {
            input: child,
            items,
        } => Exec::MaterializeValues(MaterializeValuesState {
            input: Box::new(build_exec(child, ctx, input)?),
            items,
        }),
    })
}

/// Compile a recursive rel's per-step `(r, n | WHERE …)` filter (a relationship
/// gate + an intermediate-node gate) for a [`VarLengthExtend`].
pub(crate) fn build_recursive_filter(
    ve: &VarLengthExtend,
    resolver: &LayoutResolver,
) -> Result<Option<CompiledFilter>> {
    Ok(match &ve.filter {
        None => None,
        Some(f) => Some(CompiledFilter {
            rel_param: f.rel_param,
            node_param: f.node_param,
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
