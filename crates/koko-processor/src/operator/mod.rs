mod aggregate;
mod algorithm;
mod expand;
mod filter;
mod join;
mod louvain;
mod path;
mod project;
mod source;
mod update;

pub(crate) use aggregate::*;
pub(crate) use algorithm::*;
pub(crate) use expand::*;
pub(crate) use filter::*;
pub(crate) use join::*;
pub(crate) use louvain::*;
pub(crate) use path::*;
pub(crate) use project::*;
pub(crate) use source::*;
pub(crate) use update::*;

use super::*;

/// A pull operator: each [`PlanOp`] compiles (via [`build_exec`]) into one of
/// these, and `next_chunk` readies the next ≤[`VECTOR_CAPACITY`]-row chunk or
/// `None` at end of stream. Correlated operators store their sub-pattern as a
/// `&PlanOp` and (re)build a seeded sub-pipeline per input row.
pub(super) enum Exec<'a> {
    SingleRow(SingleRowState),
    InputScan(InputScanState<'a>),
    Buffered(BufferedState),
    ScanNode(ScanNodeState<'a>),
    KCore(KCoreState<'a>),
    TopologicalLevels(TopologicalLevelsState<'a>),
    PageRank(PageRankState<'a>),
    Louvain(LouvainState<'a>),
    WeaklyConnectedComponents(WeaklyConnectedComponentsState<'a>),
    StronglyConnectedComponents(StronglyConnectedComponentsState<'a>),
    IndexScan(IndexScanState<'a>),
    IndexLookup(IndexLookupState<'a>),
    ScanTableFunc(TableFunctionScanState<'a>),
    LoadScan(LoadScanState<'a>),
    Filter(FilterState<'a>),
    Extend(ExtendState<'a>),
    VarExtend(VarExtendState<'a>),
    ProjectPath(ProjectPathState<'a>),
    Unwind(UnwindState<'a>),
    CrossProduct(CrossProductState<'a>),
    HashJoin(HashJoinState<'a>),
    Optional(OptionalState<'a>),
    Subquery(SubqueryState<'a>),
    SequenceCall(SequenceCallState<'a>),
    MaterializeValues(MaterializeValuesState<'a>),
}

impl<'a> Exec<'a> {
    /// Pull the next chunk of ≤[`VECTOR_CAPACITY`] rows, or `None` when exhausted.
    pub(super) fn next_chunk(
        &mut self,
        ctx: &OperatorContext<'a>,
        eval: &mut EvalState,
    ) -> Result<Option<DataChunk>> {
        ctx.control.check()?;
        match self {
            Exec::SingleRow(SingleRowState { done }) => {
                if *done {
                    Ok(None)
                } else {
                    *done = true;
                    let mut chunk = DataChunk::new(&ctx.layout.col_types);
                    chunk.set_flat(1);
                    Ok(Some(chunk))
                }
            }
            Exec::InputScan(InputScanState { chunks, idx }) => {
                if *idx < chunks.len() {
                    let c = chunks[*idx].clone();
                    *idx += 1;
                    Ok(Some(c))
                } else {
                    Ok(None)
                }
            }
            Exec::Buffered(BufferedState { chunks, idx }) => {
                if *idx < chunks.len() {
                    // Move the chunk out (it is never revisited) to avoid a clone.
                    let c = std::mem::replace(&mut chunks[*idx], DataChunk::new(&[]));
                    *idx += 1;
                    Ok(Some(c))
                } else {
                    Ok(None)
                }
            }
            Exec::ScanNode(ScanNodeState {
                scan,
                table_idx,
                offset,
                end,
                single_table,
                projected_columns,
                external_reader,
            }) => loop {
                if *table_idx >= scan.tables.len() {
                    return Ok(None);
                }
                let scan_table = &scan.tables[*table_idx];
                let external_bound = ctx.sources.num_rows(scan_table.table);
                let bound = external_bound
                    .unwrap_or_else(|| ctx.storage.node_count(scan_table.table))
                    .min(*end);
                if external_bound.is_some() && *offset < bound {
                    if external_reader.is_none() {
                        *external_reader = Some(ctx.sources.open_node_scan(
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
                    if let Some(batch) = reader.next_chunk(ctx.control, ctx.memory.tracker())? {
                        let property_count = reader.property_count();
                        let size = batch.properties.size();
                        let mut output = DataChunk::new(&ctx.layout.col_types);
                        for position in 0..size {
                            output.columns[scan.id_col].set_value(
                                position,
                                &Value::InternalId(InternalId::new(
                                    batch.table,
                                    batch.start_offset + position as u64,
                                )),
                            );
                        }
                        for (property, source) in scan_table
                            .prop_cols
                            .iter()
                            .zip(batch.properties.columns.into_iter().take(property_count))
                        {
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
                        *offset = batch.start_offset + size as u64;
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
            Exec::KCore(state) => next_k_core_chunk(state, ctx),
            Exec::TopologicalLevels(state) => next_topological_levels_chunk(state, ctx),
            Exec::WeaklyConnectedComponents(state) => {
                next_weakly_connected_components_chunk(state, ctx)
            }
            Exec::StronglyConnectedComponents(state) => {
                next_strongly_connected_components_chunk(state, ctx)
            }
            Exec::PageRank(state) => next_page_rank_chunk(state, ctx),
            Exec::Louvain(state) => next_louvain_chunk(state, ctx),
            Exec::IndexScan(IndexScanState { scan, done }) => {
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
            Exec::IndexLookup(IndexLookupState {
                input,
                scan,
                key,
                st,
            }) => {
                let key = &*key;
                stream_expand(st, input, ctx, eval, |chunk, pos, out, eval| {
                    let value = key.eval(chunk, pos, ctx.random, eval)?;
                    expand_index_lookup_row(scan, &value, ctx, Some((chunk, pos)), out);
                    Ok(())
                })
            }
            Exec::ScanTableFunc(TableFunctionScanState {
                call,
                cols,
                rows,
                idx,
            }) => {
                if rows.is_none() {
                    *rows = Some(produce_table_function_rows(
                        ctx.catalog,
                        call.function,
                        &call.arguments,
                        ctx.table_functions,
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
            Exec::LoadScan(LoadScanState { cols, source }) => {
                let Some(batch) =
                    source.next_batch(ctx.control, ctx.memory.tracker(), ctx.warnings)?
                else {
                    return Ok(None);
                };
                let size = batch.columns.size();
                let mut output = DataChunk::new(&ctx.layout.col_types);
                for (&target, column) in cols.iter().zip(batch.columns.columns) {
                    output.columns[target] = column;
                }
                output.set_flat(size);
                Ok(Some(output))
            }
            Exec::Filter(FilterState { input, predicate }) => {
                // Pull child chunks, narrowing each selection; skip wholly-filtered
                // chunks so the consumer only sees rows.
                loop {
                    match input.next_chunk(ctx, eval)? {
                        None => return Ok(None),
                        Some(mut chunk) => {
                            let mut kept = Vec::with_capacity(chunk.sel.len());
                            for pos in chunk.sel.iter() {
                                if predicate.eval_predicate(&chunk, pos, ctx.random, eval)? {
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
            Exec::Extend(ExtendState { input, extend, st }) => {
                let extend = *extend;
                if extend.factorize {
                    // Factorized (P3 step 6): collapse the fan-out into a multiplicity.
                    factorized_extend(st, input, extend, ctx, eval)
                } else {
                    stream_extend_batch(st, input, extend, ctx, eval)
                }
            }
            Exec::VarExtend(VarExtendState {
                input,
                ve,
                filter,
                st,
            }) => {
                let ve = *ve;
                let filter = filter.as_ref();
                if ve.factorize {
                    factorized_var_extend(input, ve, filter, ctx, eval)
                } else {
                    stream_expand(st, input, ctx, eval, |chunk, pos, out, eval| {
                        expand_var_extend_row(ve, filter, ctx, chunk, pos, out, eval)
                    })
                }
            }
            Exec::ProjectPath(ProjectPathState { input, pp, st }) => {
                let pp = *pp;
                stream_expand(st, input, ctx, eval, |chunk, pos, out, _eval| {
                    expand_project_path_row(pp, ctx, chunk, pos, out)
                })
            }
            Exec::Unwind(UnwindState {
                input,
                list,
                target,
                st,
            }) => {
                let list = &*list;
                stream_expand(st, input, ctx, eval, |chunk, pos, out, eval| {
                    expand_unwind_row(list, target, ctx, chunk, pos, out, eval)
                })
            }
            Exec::CrossProduct(CrossProductState {
                left,
                right,
                left_width,
                right_width,
                right_buf,
                st,
            }) => {
                if right_buf.is_none() {
                    *right_buf = Some(drain_all(right, ctx, eval)?);
                }
                let right_buf = right_buf.as_ref().expect("buffered above");
                let left_width = *left_width;
                let right_width = *right_width;
                stream_expand(st, left, ctx, eval, |chunk, pos, out, _eval| {
                    expand_cross_row(right_buf, left_width, right_width, ctx, chunk, pos, out)
                })
            }
            Exec::HashJoin(HashJoinState {
                probe,
                build,
                probe_cols,
                build_cols,
                probe_keys,
                build_keys,
                table,
                kind,
                st,
            }) => {
                let (ps, pl) = *probe_cols;
                let (bs, bl) = *build_cols;
                let width = ctx.layout.width();
                // Build phase (once): drain the build side and hash it by its key,
                // storing each row's build-side columns. NULL-keyed rows are dropped
                // (a NULL key never matches, by Cypher `=` semantics).
                if table.is_none() {
                    let mut t: HashMap<JoinKey, Vec<Vec<Value>>> = HashMap::new();
                    for chunk in drain_all(build, ctx, eval)? {
                        for pos in chunk.sel.iter() {
                            let Some(key) =
                                eval_join_key(build_keys, &chunk, pos, ctx.random, eval)?
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
                            ctx.memory.charge(retained_bytes)?;
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
                    JoinKind::Inner => {
                        stream_expand(st, probe, ctx, eval, |chunk, pos, out, eval| {
                            let Some(key) =
                                eval_join_key(probe_keys, chunk, pos, ctx.random, eval)?
                            else {
                                return Ok(());
                            };
                            if let Some(matches) = table.get(&key) {
                                for (match_index, brow) in matches.iter().enumerate() {
                                    if match_index % VECTOR_CAPACITY == 0 {
                                        ctx.control.check()?;
                                    }
                                    let mut row = probe_row(chunk, pos);
                                    for (i, c) in (bs..bs + bl).enumerate() {
                                        row[c] = brow[i].clone();
                                    }
                                    out.push(row);
                                }
                            }
                            Ok(())
                        })
                    }
                    // Left outer: emit matches, or one probe row with the build
                    // columns left NULL when there is none (decorrelated OPTIONAL).
                    JoinKind::Left => {
                        stream_expand(st, probe, ctx, eval, |chunk, pos, out, eval| {
                            let key = eval_join_key(probe_keys, chunk, pos, ctx.random, eval)?;
                            match key.as_ref().and_then(|k| table.get(k)) {
                                Some(matches) if !matches.is_empty() => {
                                    for (match_index, brow) in matches.iter().enumerate() {
                                        if match_index % VECTOR_CAPACITY == 0 {
                                            ctx.control.check()?;
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
                        })
                    }
                    // Mark: exactly one output row per probe row. Annotate the
                    // probe chunk in place instead of expanding every row through a
                    // temporary `Vec<Value>` and rebuilding an identical chunk.
                    JoinKind::Mark {
                        mark_col,
                        kind: sqk,
                    } => {
                        let Some(mut chunk) = probe.next_chunk(ctx, eval)? else {
                            return Ok(None);
                        };
                        for pos in chunk.sel.iter() {
                            let key = eval_join_key(probe_keys, &chunk, pos, ctx.random, eval)?;
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
            Exec::Optional(OptionalState {
                input,
                pattern,
                new_cols,
                st,
            }) => {
                let pattern = *pattern;
                let new_cols = *new_cols;
                stream_expand(st, input, ctx, eval, |chunk, pos, out, eval| {
                    expand_optional_row(pattern, new_cols, ctx, chunk, pos, out, eval)
                })
            }
            Exec::Subquery(SubqueryState {
                input,
                pattern,
                result_col,
                kind,
                st,
            }) => {
                let pattern = *pattern;
                let result_col = *result_col;
                let kind = *kind;
                let spec = SubquerySpec {
                    pattern,
                    result_col,
                    kind,
                };
                stream_expand(st, input, ctx, eval, |chunk, pos, out, eval| {
                    expand_subquery_row(&spec, ctx, chunk, pos, out, eval)
                })
            }
            Exec::SequenceCall(SequenceCallState {
                input,
                func,
                name,
                result_col,
                st,
            }) => {
                let func = *func;
                let name = *name;
                let result_col = *result_col;
                stream_expand(st, input, ctx, eval, |chunk, pos, out, _eval| {
                    expand_sequence_row(func, name, result_col, ctx, chunk, pos, out)
                })
            }
            Exec::MaterializeValues(MaterializeValuesState { input, items }) => {
                // 1:1 pass-through: assemble each selected row's node/rel value
                // from its internal id into the value column (audit V12 seam).
                match input.next_chunk(ctx, eval)? {
                    None => Ok(None),
                    Some(mut chunk) => {
                        let positions: Vec<usize> = chunk.sel.iter().collect();
                        for it in *items {
                            for &pos in &positions {
                                let v = match chunk.columns[it.id_col].get_value(pos) {
                                    Value::InternalId(id) if id.table_id.0 != u64::MAX => {
                                        let entity = EntityReader::from_ctx(ctx);
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
