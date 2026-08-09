use super::*;

pub(crate) struct ExtendState<'a> {
    pub(crate) input: Box<Exec<'a>>,
    pub(crate) extend: &'a Extend,
    pub(crate) st: ExpandState,
}

pub(crate) struct UnwindState<'a> {
    pub(crate) input: Box<Exec<'a>>,
    pub(crate) list: CompiledExpr,
    pub(crate) target: &'a UnwindTarget,
    pub(crate) st: ExpandState,
}

/// Accumulates rows into **one** [`DataChunk`] of at most [`VECTOR_CAPACITY`], the
/// single-chunk currency of streaming operators (unlike [`ChunkBuilder`], which
/// eagerly builds *all* chunks for the breaker/write path). The operator owns the
/// resumable cursor; this just fills the next chunk to yield.
pub(crate) struct ChunkAccum {
    pub(crate) chunk: DataChunk,
    pub(crate) len: usize,
    /// Per-row factorization multiplicity, allocated lazily on the first non-unit
    /// value (back-filling prior rows with 1) — so the common all-1 path pays
    /// nothing. The factorizing extend (P3 step 6) is the only producer of a non-1.
    pub(crate) mult: Option<Vec<u64>>,
}

impl ChunkAccum {
    pub(crate) fn new(col_types: &[LogicalType]) -> Self {
        Self {
            chunk: DataChunk::new(col_types),
            len: 0,
            mult: None,
        }
    }

    pub(crate) fn push_row(&mut self, row: &[Value]) {
        self.push_row_with_mult(row, 1);
    }

    /// Append a row that stands for `m` logical tuples (its factorization
    /// multiplicity). `m == 1` is the ordinary path; a non-unit `m` materializes the
    /// multiplicity vector (once) so the chunk carries it.
    pub(crate) fn push_row_with_mult(&mut self, row: &[Value], m: u64) {
        for (c, v) in row.iter().enumerate() {
            self.chunk.columns[c].set_value(self.len, v);
        }
        self.finish_row_with_mult(m);
    }

    /// Append selected columns from one input row without materializing them as
    /// [`Value`]s. The planner supplies the columns that remain live above the
    /// operator; every other output column stays NULL.
    pub(crate) fn push_chunk_row_with_mult(
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

    pub(crate) fn finish_row_with_mult(&mut self, m: u64) {
        if m != 1 {
            let len = self.len;
            self.mult.get_or_insert_with(|| vec![1u64; len]).push(m);
        } else if let Some(values) = &mut self.mult {
            values.push(1);
        }
        self.len += 1;
    }

    pub(crate) fn is_full(&self) -> bool {
        self.len >= VECTOR_CAPACITY
    }

    /// Finalize the accumulated rows into a flat chunk (attaching the multiplicity
    /// vector only when some row carried a non-unit multiplicity).
    pub(crate) fn into_chunk(mut self) -> DataChunk {
        self.chunk.set_flat(self.len);
        if let Some(m) = self.mult {
            self.chunk.mult = Some(m.into_boxed_slice());
        }
        self.chunk
    }

    /// The accumulated chunk, or `None` if no rows were pushed.
    pub(crate) fn take(self) -> Option<DataChunk> {
        if self.len == 0 {
            None
        } else {
            Some(self.into_chunk())
        }
    }
}

/// Reusable per-expand adjacency output; ownership follows the pull operator.
#[derive(Default)]
pub(crate) struct NeighborScratch {
    pub(crate) rows: Vec<BatchNeighbor>,
}

/// Per-operator state for the row-expansion streaming pattern (see
/// [`stream_expand`]): the current input chunk + position cursor, and buffered output.
#[derive(Default)]
pub(crate) struct ExpandState {
    pub(crate) cur: Option<DataChunk>,
    pub(crate) positions: Vec<usize>,
    pub(crate) pi: usize,
    pub(crate) pending: Vec<Vec<Value>>,
    pub(crate) pending_row_memory: Option<MemoryReservation>,
    pub(crate) pend_i: usize,
    /// Multiplicity parallel to `pending`; batched expanders can mix source rows.
    pub(crate) pending_mults: Vec<u64>,
    /// Columnar output waiting to be pulled by the parent extend operator.
    pub(crate) pending_chunks: VecDeque<DataChunk>,
    /// Tracks queued chunks plus the most recently yielded chunk until the parent pulls again.
    pub(crate) pending_chunk_memory: Option<MemoryReservation>,
    pub(crate) yielded_chunk_bytes: u64,
    /// Reusable batched-adjacency scratch.
    pub(crate) neighbors: NeighborScratch,
    pub(crate) tagged_neighbors: Vec<(usize, usize, BatchNeighbor)>,
    pub(crate) nodes: Vec<InternalId>,
    pub(crate) node_positions: Vec<usize>,
    /// Per-relationship-branch MVCC fast-path decision for this statement view.
    pub(crate) all_visible: Vec<Option<bool>>,
}

impl ExpandState {
    pub(crate) fn release_yielded_chunk(&mut self) {
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

    pub(crate) fn release_pending_rows(&mut self) {
        if let Some(reservation) = &mut self.pending_row_memory {
            reservation
                .resize(0)
                .expect("shrinking a pending-row reservation cannot fail");
        }
    }

    pub(crate) fn charge_pending_rows(&mut self, memory: &QueryMemory, bytes: u64) -> Result<()> {
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

    pub(crate) fn retain_yielded_chunk(
        &mut self,
        memory: &QueryMemory,
        chunk: &DataChunk,
    ) -> Result<()> {
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

    pub(crate) fn push_pending_chunk(
        &mut self,
        memory: &QueryMemory,
        chunk: DataChunk,
    ) -> Result<()> {
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

    pub(crate) fn take_pending_chunk(&mut self) -> Option<DataChunk> {
        let chunk = self.pending_chunks.pop_front()?;
        self.yielded_chunk_bytes = chunk.allocated_bytes();
        Some(chunk)
    }
}

/// Drive the row-expansion streaming pattern shared by all 1-child operators that
/// map each input row to zero or more output rows.
pub(crate) fn stream_expand<'a>(
    st: &mut ExpandState,
    child: &mut Exec<'a>,
    ctx: &OperatorContext<'a>,
    eval: &mut EvalState,
    mut expand: impl FnMut(&DataChunk, usize, &mut Vec<Vec<Value>>, &mut EvalState) -> Result<()>,
) -> Result<Option<DataChunk>> {
    st.release_yielded_chunk();
    let mut accum = ChunkAccum::new(&ctx.layout.col_types);
    loop {
        while st.pend_i < st.pending.len() {
            accum.push_row_with_mult(&st.pending[st.pend_i], st.pending_mults[st.pend_i]);
            st.pend_i += 1;
            if accum.is_full() {
                let output = accum.into_chunk();
                st.retain_yielded_chunk(ctx.memory, &output)?;
                return Ok(Some(output));
            }
        }
        st.release_pending_rows();
        st.pending.clear();
        st.pending_mults.clear();
        st.pend_i = 0;

        loop {
            if st.cur.is_none() {
                match child.next_chunk(ctx, eval)? {
                    Some(chunk) => {
                        st.positions = chunk.sel.iter().collect();
                        st.pi = 0;
                        st.cur = Some(chunk);
                    }
                    None => {
                        let output = accum.take();
                        if let Some(chunk) = &output {
                            st.retain_yielded_chunk(ctx.memory, chunk)?;
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
            expand(cur, pos, &mut st.pending, eval)?;
            let pending_bytes = st.pending[pending_before..]
                .iter()
                .map(|row| {
                    (row.capacity() * std::mem::size_of::<Value>()) as u64
                        + row.iter().map(value_payload_bytes).sum::<u64>()
                })
                .sum();
            st.charge_pending_rows(ctx.memory, pending_bytes)?;
            st.pending_mults.resize(st.pending.len(), multiplicity);
            break;
        }
    }
}

pub(crate) struct ExtendPropertyCache {
    pub(crate) rel: Vec<DataChunk>,
    pub(crate) node_locations: Option<Vec<Option<(usize, usize)>>>,
    pub(crate) nodes: Vec<Vec<DataChunk>>,
}

pub(crate) fn gathered_property(batches: &[DataChunk], index: usize, column: usize) -> Value {
    batches[index / VECTOR_CAPACITY].columns[column].get_value(index % VECTOR_CAPACITY)
}

pub(crate) fn take_gathered_property(
    batches: &mut [DataChunk],
    index: usize,
    column: usize,
) -> Value {
    batches[index / VECTOR_CAPACITY].columns[column].take_value(index % VECTOR_CAPACITY)
}

/// Batched adjacency extension. One storage dispatch handles all non-null endpoints
/// in the input chunk for each relationship-table branch.
pub(crate) fn stream_extend_batch<'a>(
    st: &mut ExpandState,
    child: &mut Exec<'a>,
    extend: &Extend,
    ctx: &OperatorContext<'a>,
    eval: &mut EvalState,
) -> Result<Option<DataChunk>> {
    st.release_yielded_chunk();
    if let Some(chunk) = st.take_pending_chunk() {
        return Ok(Some(chunk));
    }
    loop {
        let chunk = match child.next_chunk(ctx, eval)? {
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
            ctx.control.check()?;
            st.neighbors.rows.clear();
            let all_visible = match st.all_visible[branch_index] {
                Some(all_visible) => all_visible,
                None => {
                    let all_visible = ctx.visibility.rel_rows_all_visible(
                        ctx.storage,
                        ctx.read(),
                        branch.rel_table,
                    );
                    st.all_visible[branch_index] = Some(all_visible);
                    all_visible
                }
            };
            let external = ctx.sources.extend_batch_into(
                branch.rel_table,
                &st.nodes,
                extend.dir,
                &mut st.neighbors.rows,
                ctx.catalog,
                ctx.control,
                ctx.memory.tracker(),
            )?;
            if !external {
                if all_visible {
                    ctx.storage.extend_batch_all_visible_into(
                        ctx.read(),
                        branch.rel_table,
                        &st.nodes,
                        extend.dir,
                        &mut st.neighbors.rows,
                    );
                } else {
                    ctx.storage.extend_batch_into(
                        ctx.read(),
                        branch.rel_table,
                        &st.nodes,
                        extend.dir,
                        &mut st.neighbors.rows,
                    );
                }
            }
            let rel = if branch.rel_prop_cols.is_empty() {
                Vec::new()
            } else {
                let rel_offsets: Vec<u64> = st
                    .neighbors
                    .rows
                    .iter()
                    .map(|neighbor| neighbor.rel.offset.0)
                    .collect();
                let rel_columns: Vec<usize> = branch
                    .rel_prop_cols
                    .iter()
                    .map(|property| property.column_id as usize)
                    .collect();
                if external {
                    ctx.sources
                        .projected_rows(
                            branch.rel_table,
                            &rel_offsets,
                            &rel_columns,
                            ctx.catalog,
                            ctx.control,
                            ctx.memory.tracker(),
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
                    let mut locations = vec![None; st.neighbors.rows.len()];
                    let mut batches = Vec::with_capacity(to_tables.len());
                    for (table_index, table) in to_tables.iter().enumerate() {
                        let mut offsets = Vec::new();
                        for (neighbor_index, neighbor) in st.neighbors.rows.iter().enumerate() {
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
                            if let Some(rows) = ctx.sources.projected_rows(
                                table.table,
                                &offsets,
                                &columns,
                                ctx.catalog,
                                ctx.control,
                                ctx.memory.tracker(),
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
                    .rows
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
                ctx.control.check()?;
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
                st.push_pending_chunk(ctx.memory, finished)?;
            }
        }
        if let Some(output) = output.and_then(ChunkAccum::take) {
            st.push_pending_chunk(ctx.memory, output)?;
        }
        if let Some(output) = st.take_pending_chunk() {
            return Ok(Some(output));
        }
    }
}
// --- per-row expanders (one input row → zero or more output rows) ---

/// Factorized extend (P3 step 6): the optimizer proved this extend's introduced
/// columns (the rel, and the new node for a `New` target) are never read — only
/// counted — so instead of fanning out one row per neighbor, count the valid
/// neighbors and fold that into the input row's multiplicity. A row with no neighbor
/// is dropped (inner-join semantics, identical to the fan-out producing zero rows).
/// One input chunk is consumed per call (1:1, so the output never exceeds a chunk).
pub(crate) fn factorized_extend<'a>(
    st: &mut ExpandState,
    input: &mut Exec<'a>,
    extend: &Extend,
    ctx: &OperatorContext<'a>,
    eval: &mut EvalState,
) -> Result<Option<DataChunk>> {
    loop {
        let Some(chunk) = input.next_chunk(ctx, eval)? else {
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
pub(crate) fn count_extend_row(
    st: &mut ExpandState,
    extend: &Extend,
    ctx: &OperatorContext,
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
    let neighbors = &mut st.neighbors.rows;
    if st.all_visible.len() < extend.branches.len() {
        st.all_visible.resize(extend.branches.len(), None);
    }
    let mut count = 0u64;
    for (branch_index, branch) in extend.branches.iter().enumerate() {
        ctx.control.check()?;
        neighbors.clear();
        let all_visible = match st.all_visible[branch_index] {
            Some(all_visible) => all_visible,
            None => {
                let all_visible =
                    ctx.visibility
                        .rel_rows_all_visible(ctx.storage, ctx.read(), branch.rel_table);
                st.all_visible[branch_index] = Some(all_visible);
                all_visible
            }
        };
        let external = ctx.sources.extend_batch_into(
            branch.rel_table,
            std::slice::from_ref(&from_id),
            extend.dir,
            neighbors,
            ctx.catalog,
            ctx.control,
            ctx.memory.tracker(),
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
                    neighbors,
                );
            } else {
                ctx.storage.extend_batch_into(
                    ctx.read(),
                    branch.rel_table,
                    std::slice::from_ref(&from_id),
                    extend.dir,
                    neighbors,
                );
            }
        }
        for (neighbor_index, n) in neighbors.iter().enumerate() {
            if neighbor_index % VECTOR_CAPACITY == 0 {
                ctx.control.check()?;
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
    Ok(count)
}

/// Factorized variable-length extend (P3 step 6): count the valid paths from the
/// start node and fold that into the row's multiplicity, instead of materializing one
/// row per path. The companion of [`factorized_extend`] for recursive rels.
pub(crate) fn factorized_var_extend<'a>(
    input: &mut Exec<'a>,
    ve: &VarLengthExtend,
    filter: Option<&CompiledFilter>,
    ctx: &OperatorContext<'a>,
    eval: &mut EvalState,
) -> Result<Option<DataChunk>> {
    let width = ctx.layout.width();
    loop {
        let Some(chunk) = input.next_chunk(ctx, eval)? else {
            return Ok(None);
        };
        let mut accum = ChunkAccum::new(&ctx.layout.col_types);
        for pos in chunk.sel.iter() {
            let count = count_var_extend_row(ve, filter, ctx, &chunk, pos, eval)?;
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
pub(crate) fn count_var_extend_row(
    ve: &VarLengthExtend,
    filter: Option<&CompiledFilter>,
    ctx: &OperatorContext,
    chunk: &DataChunk,
    pos: usize,
    eval: &mut EvalState,
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
        ctx.sources,
        ctx.read(),
        filter,
        ctx.random,
        eval,
        ctx.memory,
        ctx.control,
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
pub(crate) fn expand_var_extend_row(
    ve: &VarLengthExtend,
    filter: Option<&CompiledFilter>,
    ctx: &OperatorContext,
    chunk: &DataChunk,
    pos: usize,
    out: &mut Vec<Vec<Value>>,
    eval: &mut EvalState,
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
        ctx.sources,
        ctx.read(),
        filter,
        ctx.random,
        eval,
        ctx.memory,
        ctx.control,
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
            let entity = EntityReader::from_ctx(ctx);
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
            let properties = if let Some(values) = ctx.sources.projected_values(
                path.end.table_id,
                path.end.offset.0,
                &columns,
                ctx.catalog,
                ctx.control,
                ctx.memory.tracker(),
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
pub(crate) fn expand_project_path_row(
    pp: &ProjectPath,
    ctx: &OperatorContext,
    chunk: &DataChunk,
    pos: usize,
    out: &mut Vec<Vec<Value>>,
) -> Result<()> {
    let width = ctx.layout.width();
    let mut row = row_at(chunk, pos, width);
    row[pp.path_col] = assemble_path(pp, ctx.layout, chunk, pos, EntityReader::from_ctx(ctx))?;
    out.push(row);
    Ok(())
}

/// `UNWIND list AS var`: emit one row per list element (a NULL/non-list yields none).
pub(crate) fn expand_unwind_row(
    list: &CompiledExpr,
    target: &UnwindTarget,
    ctx: &OperatorContext,
    chunk: &DataChunk,
    pos: usize,
    out: &mut Vec<Vec<Value>>,
    eval: &mut EvalState,
) -> Result<()> {
    let width = ctx.layout.width();
    let items = match list.eval(chunk, pos, ctx.random, eval)? {
        Value::List(items) => items,
        _ => return Ok(()),
    };
    for (index, item) in items.into_iter().enumerate() {
        if index % VECTOR_CAPACITY == 0 {
            ctx.control.check()?;
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

pub(crate) fn expand_index_lookup_row(
    scan: &IndexScan,
    key: &Value,
    ctx: &OperatorContext,
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
pub(crate) fn expand_cross_row(
    right_buf: &[DataChunk],
    left_width: usize,
    right_width: usize,
    ctx: &OperatorContext,
    chunk: &DataChunk,
    pos: usize,
    out: &mut Vec<Vec<Value>>,
) -> Result<()> {
    let width = ctx.layout.width();
    for rc in right_buf {
        ctx.control.check()?;
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
pub(crate) fn expand_optional_row(
    pattern: &PlanOp,
    new_cols: &[usize],
    ctx: &OperatorContext,
    chunk: &DataChunk,
    pos: usize,
    out: &mut Vec<Vec<Value>>,
    eval: &mut EvalState,
) -> Result<()> {
    let width = ctx.layout.width();
    let row = row_at(chunk, pos, width);
    let seed = seed_chunk(&row, &ctx.layout.col_types);
    let mut sub = build_exec(pattern, ctx, std::slice::from_ref(&seed))?;
    let mut any = false;
    while let Some(mc) = sub.next_chunk(ctx, eval)? {
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

pub(crate) fn fill_optional_empty_paths(
    pattern: &PlanOp,
    ctx: &OperatorContext,
    row: &mut [Value],
) -> Result<()> {
    match pattern {
        PlanOp::ProjectPath(pp) => {
            fill_optional_empty_paths(&pp.input, ctx, row)?;
            row[pp.path_col] =
                assemble_empty_optional_path(pp, ctx.layout, row, EntityReader::from_ctx(ctx))?;
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
        | PlanOp::ScanGraphAlgorithm(_)
        | PlanOp::LoadScan { .. }
        | PlanOp::ScanNode(_) => {}
    }
    Ok(())
}

pub(crate) fn read_var_id_from_row(var: VarId, layout: &RowLayout, row: &[Value]) -> InternalId {
    let id_col = layout.var(var).id_col;
    match row.get(id_col) {
        Some(Value::InternalId(id)) => *id,
        _ => InternalId::new(TableId(u64::MAX), u64::MAX),
    }
}

pub(crate) fn assemble_empty_optional_path(
    pp: &ProjectPath,
    layout: &RowLayout,
    row: &[Value],
    entity: EntityReader<'_>,
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

pub(crate) struct SubquerySpec<'a> {
    pub(crate) pattern: &'a PlanOp,
    pub(crate) result_col: usize,
    pub(crate) kind: SubqueryKind,
}

/// `EXISTS {}` / `COUNT {}`: run the seeded sub-pipeline, write the result column,
/// emit one row. EXISTS short-circuits at the first match.
pub(crate) fn expand_subquery_row(
    spec: &SubquerySpec<'_>,
    ctx: &OperatorContext,
    chunk: &DataChunk,
    pos: usize,
    out: &mut Vec<Vec<Value>>,
    eval: &mut EvalState,
) -> Result<()> {
    let width = ctx.layout.width();
    let mut row = row_at(chunk, pos, width);
    let seed = seed_chunk(&row, &ctx.layout.col_types);
    // A correlated subplan is rebuilt and fully consumed for this one outer row.
    // Release its temporary accounting before evaluating the next row; retaining
    // every sequential subplan's high-water mark would turn bounded execution into
    // a false OOM on large datasets.
    let memory_before = ctx.memory.bytes();
    let result = (|| {
        let mut sub = build_exec(spec.pattern, ctx, std::slice::from_ref(&seed))?;
        drain_count(
            &mut sub,
            ctx,
            matches!(spec.kind, SubqueryKind::Exists),
            eval,
        )
    })();
    ctx.memory.release_to(memory_before);
    let count = result?;
    row[spec.result_col] = match spec.kind {
        SubqueryKind::Exists => Value::Bool(count > 0),
        SubqueryKind::Count => Value::Int64(count),
    };
    out.push(row);
    Ok(())
}

/// `nextval`/`currval`: advance/read the named sequence into the result column.
pub(crate) fn expand_sequence_row(
    func: SequenceFn,
    name: &str,
    result_col: usize,
    ctx: &OperatorContext,
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
