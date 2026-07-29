use super::*;

// ---------------------------------------------------------------------------
// Writes (CREATE / SET / DELETE)
// ---------------------------------------------------------------------------

/// Short-lived mutation capabilities for one updating clause.
pub(crate) struct WriteExecutor<'a, 'e> {
    pub(crate) catalog: &'a Catalog,
    pub(crate) storage: &'a mut InMemStorage,
    pub(crate) execution: &'a ExecutionContext<'e>,
    pub(crate) visibility: &'a ReadVisibilityCache,
}

impl<'a, 'e> WriteExecutor<'a, 'e> {
    pub(crate) fn new(
        catalog: &'a Catalog,
        storage: &'a mut InMemStorage,
        execution: &'a ExecutionContext<'e>,
        visibility: &'a ReadVisibilityCache,
    ) -> Self {
        Self {
            catalog,
            storage,
            execution,
            visibility,
        }
    }

    /// Apply one updating clause to the matched rows. Most clauses edit the rows
    /// in place; `MERGE` returns fresh matched/created bindings.
    pub(crate) fn apply(
        &mut self,
        op: &UpdateOp,
        layout: &RowLayout,
        mut chunks: Vec<DataChunk>,
    ) -> Result<Vec<DataChunk>> {
        let read = self.execution.storage_read;
        let write = self
            .execution
            .storage_write
            .expect("write operator requires a storage write handle");
        match op {
            UpdateOp::Create(create) => run_creates(
                create,
                layout,
                &mut chunks,
                self.catalog,
                self.storage,
                read,
                write,
                self.execution.control,
                self.execution.random,
            )?,
            UpdateOp::Set(set) => run_set(
                set,
                layout,
                &mut chunks,
                self.catalog,
                self.storage,
                write,
                self.execution.control,
                self.execution.random,
            )?,
            UpdateOp::Delete(delete) => run_delete(
                delete,
                layout,
                &chunks,
                self.catalog,
                self.storage,
                read,
                write,
                self.execution.control,
            )?,
            UpdateOp::Merge(merge) => {
                return run_merge(
                    merge,
                    layout,
                    &chunks,
                    self.catalog,
                    self.storage,
                    self.execution,
                    self.visibility,
                );
            }
        }
        Ok(chunks)
    }
}

/// Execute a `MERGE` per input row: run the seeded match against live storage; on
/// a hit, apply `ON MATCH SET` and emit each match; on a miss, create the pattern,
/// apply `ON CREATE SET`, and emit the created row. Returns the resulting rows.
pub(crate) fn run_merge(
    merge: &MergePlan,
    layout: &RowLayout,
    chunks: &[DataChunk],
    catalog: &Catalog,
    storage: &mut InMemStorage,
    execution: &ExecutionContext<'_>,
    visibility: &ReadVisibilityCache,
) -> Result<Vec<DataChunk>> {
    let read = execution.storage_read;
    let write = execution
        .storage_write
        .expect("MERGE requires a storage write handle");
    let width = layout.width();
    let mut builder = ChunkBuilder::new(&layout.col_types);
    let resolver = LayoutResolver(layout);
    let mut eval = EvalState::new();

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
                .map(|ce| {
                    Ok(ce
                        .eval(&seed, 0, execution.random, &mut eval)?
                        .to_result_string())
                })
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
                let ctx = OperatorContext::new(catalog, &*storage, layout, execution, visibility);
                let mut m = build_exec(&merge.match_pattern, &ctx, std::slice::from_ref(&seed))?;
                drain_all(&mut m, &ctx, &mut eval)?
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
pub(crate) fn collect_created_ids(
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
pub(crate) fn push_all_rows(chunks: &[DataChunk], width: usize, builder: &mut ChunkBuilder) {
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
pub(crate) struct PropertyUpdate {
    pub(crate) id: InternalId,
    pub(crate) is_node: bool,
    pub(crate) column_id: usize,
    pub(crate) ty: LogicalType,
    pub(crate) value: Value,
}

pub(crate) struct PendingPropertyBatch {
    pub(crate) is_node: bool,
    pub(crate) table: TableId,
    pub(crate) column_id: usize,
    pub(crate) ty: LogicalType,
    pub(crate) chunk: DataChunk,
    pub(crate) len: usize,
}

pub(crate) fn flush_property_batch(
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

pub(crate) fn push_property_update(
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
pub(crate) fn run_set(
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
    let mut eval = EvalState::new();

    for chunk in chunks.iter_mut() {
        control.check()?;
        let positions: Vec<usize> = chunk.sel.iter().collect();
        for pos in positions {
            for (item, ce) in set.items.iter().zip(&compiled) {
                let value = ce.eval(chunk, pos, random, &mut eval)?;
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
pub(crate) fn prepare_one_property(
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
    let coerced = cast_value(value, column.logical_type()).unwrap_or(Value::Null);
    if let Some(chunk_column) = layout.column(var, Some(prop)) {
        chunk.columns[chunk_column].set_value(pos, &coerced);
    }
    Some(PropertyUpdate {
        id,
        is_node,
        column_id: column.column_id().0 as usize,
        ty: column.logical_type().clone(),
        value: coerced,
    })
}

/// Update one key inside an ANY graph's ordered JSON object and mirror the complete object into
/// the live hidden `data` column.
#[allow(clippy::too_many_arguments)]
pub(crate) fn prepare_dynamic_property(
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
        column_id: table_column.column_id().0 as usize,
        ty: LogicalType::Json,
        value,
    }))
}

/// Prepare whole-value assignments in catalog order. Unlisted properties and node primary
/// keys are preserved, matching the scalar `SET n = {k: v}` contract.
#[allow(clippy::too_many_arguments)]
pub(crate) fn prepare_whole_value(
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
            .columns()
            .iter()
            .enumerate()
            .map(|(index, column)| {
                (
                    column.name().to_string(),
                    column.column_id().0 as usize,
                    column.logical_type().clone(),
                    index == table.primary_key_index(),
                )
            })
            .collect()
    } else {
        catalog
            .rel_table(id.table_id)
            .expect("bound relationship table")
            .columns()
            .iter()
            .map(|column| {
                (
                    column.name().to_string(),
                    column.column_id().0 as usize,
                    column.logical_type().clone(),
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
pub(crate) fn run_delete(
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
                    let rel_name = catalog
                        .rel_table(rel_table)
                        .map_or("", |table| table.name());
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

pub(crate) fn bound_ids(var: VarId, layout: &RowLayout, chunks: &[DataChunk]) -> Vec<InternalId> {
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

pub(crate) fn id_batches(ids: &[InternalId]) -> Vec<DataChunk> {
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

/// Fold a bound `SKIP`/`LIMIT` count and validate it — C++ defers the value
/// check to execution: anything but a non-negative integer is the *runtime*
/// error, after the binder already ensured the expression is constant.
pub(crate) fn fold_skip_limit(e: Option<&BoundExpr>) -> Result<Option<i64>> {
    let Some(e) = e else { return Ok(None) };
    let v = eval_constant(e)?;
    match v.as_i64() {
        Some(n) if n >= 0 => Ok(Some(n)),
        _ => Err(Error::runtime(
            "The number of rows to skip/limit must be a non-negative integer.".to_string(),
        )),
    }
}

/// The catalog defaults for the columns a CREATE pattern does not supply (skipping
/// `None` — those stay NULL). A `SERIAL` column carries a `NextVal` default (its
/// implicit sequence), so it is applied here like any other `nextval` default.
pub(crate) fn omitted_defaults(
    catalog: &Catalog,
    table: TableId,
    num_columns: usize,
    props: &[(usize, BoundExpr)],
) -> Vec<(usize, ColumnDefault)> {
    (0..num_columns)
        .filter(|&c| !props.iter().any(|(pc, _)| *pc == c))
        .filter_map(|column| {
            catalog
                .column_default(table, column)
                .map(|default| (column, default))
        })
        .collect()
}

/// Write each precomputed column default into `values` for one inserted row.
/// `NextVal` advances its sequence once (per row); `Const` reuses its folded value.
pub(crate) fn apply_defaults(
    defaults: &[(usize, ColumnDefault)],
    values: &mut [Value],
    catalog: &Catalog,
) -> Result<()> {
    for (col, def) in defaults {
        values[*col] = match def {
            ColumnDefault::Constant(value) => value.clone(),
            ColumnDefault::NextVal(sequence) => Value::Int64(catalog.sequence_next_val(sequence)?),
        };
    }
    Ok(())
}

pub(crate) fn insert_node_row_batch(
    storage: &mut InMemStorage,
    write: StorageWriteHandle,
    catalog: &Catalog,
    table: TableId,
    values: &[Value],
) -> Result<InternalId> {
    let types: Vec<LogicalType> = catalog
        .node_table(table)
        .expect("bound node table")
        .columns()
        .iter()
        .map(|column| column.logical_type().clone())
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

pub(crate) fn insert_rel_row_batch(
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
            .columns()
            .iter()
            .map(|column| column.logical_type().clone()),
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

pub(crate) fn coerce_stored_value(value: Value, target: &LogicalType) -> Result<Value> {
    if matches!(target, LogicalType::Json) && !matches!(value, Value::Json(_)) {
        return koko_common::JsonValue::from_value(&value).map(Value::Json);
    }
    Ok(value)
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn run_creates(
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
    let mut eval = EvalState::new();

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
            .columns()
            .iter()
            .map(|column| column.logical_type().clone())
            .collect();
        for chunk in chunks {
            control.check()?;
            let positions: Vec<usize> = chunk.sel.iter().collect();
            let mut batch = DataChunk::new(&types);
            for (batch_position, &position) in positions.iter().enumerate() {
                let mut values = vec![Value::Null; plan.num_columns];
                for (column, expression) in &plan.props {
                    values[*column] = coerce_stored_value(
                        expression.eval(chunk, position, random, &mut eval)?,
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
                        .map_or("", |table| table.name());
                    return Err(Error::runtime(format!(
                        "Nodes are not connected through relationship table {name}."
                    )));
                };
                let mut values = vec![Value::Null; plan.num_columns];
                for (column, expression) in &plan.props {
                    let ty = catalog
                        .rel_table(plan.table)
                        .expect("bound relationship table")
                        .columns()[*column]
                        .logical_type();
                    values[*column] = coerce_stored_value(
                        expression.eval(chunk, position, random, &mut eval)?,
                        ty,
                    )?;
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
                        .columns()
                        .iter()
                        .map(|column| column.logical_type().clone()),
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
                    let ty = catalog
                        .node_table(np.table)
                        .expect("bound node table")
                        .columns()[*col]
                        .logical_type();
                    values[*col] =
                        coerce_stored_value(ce.eval(chunk, pos, random, &mut eval)?, ty)?;
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
                    let rel_name = catalog.rel_table(rp.table).map_or("", |table| table.name());
                    return Err(Error::runtime(format!(
                        "Nodes are not connected through relationship table {rel_name}."
                    )));
                };
                let mut values = vec![Value::Null; rp.num_columns];
                for (col, ce) in &rp.props {
                    let ty = catalog
                        .rel_table(rp.table)
                        .expect("bound relationship table")
                        .columns()[*col]
                        .logical_type();
                    values[*col] =
                        coerce_stored_value(ce.eval(chunk, pos, random, &mut eval)?, ty)?;
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
pub(crate) fn fill_created_rel(
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
                .map(|column| (property.col_index, column.column_id().0 as usize))
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
pub(crate) fn fill_created_node(
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
                .map(|column| (property.col_index, column.column_id().0 as usize))
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
pub(crate) fn resolve_endpoint(
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
