use super::*;

/// Classification of a projection item for the aggregate path.
pub(crate) enum GroupItem {
    /// A grouping key that is a whole node/rel variable (keyed by id).
    Var(VarId),
    /// A grouping key that is a scalar expression.
    Scalar(CompiledExpr),
    /// An aggregate output expression (its `Agg` leaves index into `aggs`).
    Agg(CompiledExpr),
}

pub(crate) struct GroupData {
    pub(crate) key_values: Vec<Value>,
    pub(crate) states: Vec<AggState>,
}

/// The compiled shape of an aggregating projection, reused by the serial path and by
/// every parallel morsel: which output items are grouping keys vs aggregates, the
/// lifted aggregate specs, and the grouping-key item indices. Built once; shared by
/// `&` across morsel workers; each worker owns its evaluation state.
pub(crate) struct AggPlan {
    pub(crate) item_execs: Vec<GroupItem>,
    pub(crate) aggs: Vec<AggSpec>,
    /// Indices (into `item_execs`) of the grouping-key items.
    pub(crate) group_keys: Vec<usize>,
}

impl AggPlan {
    pub(crate) fn build(projection: &BoundProjection, layout: &RowLayout) -> Result<AggPlan> {
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

    pub(crate) fn new_states(&self) -> Vec<AggState> {
        self.aggs
            .iter()
            .map(|s| AggState::new(s.op, s.distinct))
            .collect()
    }
}

/// One pipeline's partial aggregate: its groups plus the first-seen key order.
pub(crate) struct AggPartial {
    pub(crate) groups: HashMap<Vec<ValueKey>, GroupData>,
    pub(crate) order: Vec<Vec<ValueKey>>,
}

/// Global (no grouping-key) aggregate. There is exactly one state vector, so avoid
/// constructing and hashing an empty `Vec<ValueKey>` for every input row. The common
/// analytical `count(*)` shape further reduces each chunk to its multiplicity sum.
pub(crate) fn accumulate_global_group<'a>(
    plan: &AggPlan,
    root: &mut Exec<'a>,
    ctx: &OperatorContext<'a>,
    eval: &mut EvalState,
) -> Result<AggPartial> {
    let mut states = plan.new_states();
    let count_star = matches!(
        plan.aggs.as_slice(),
        [spec] if spec.op == AggOp::Count && !spec.distinct && spec.arg.is_none()
    );
    if count_star {
        let mut count = 0u64;
        while let Some(chunk) = root.next_chunk(ctx, eval)? {
            for pos in chunk.sel.iter() {
                count = count.saturating_add(chunk.multiplicity(pos));
            }
        }
        states[0].update_n(&Value::Null, count);
    } else {
        while let Some(chunk) = root.next_chunk(ctx, eval)? {
            for pos in chunk.sel.iter() {
                let multiplicity = chunk.multiplicity(pos);
                for (index, spec) in plan.aggs.iter().enumerate() {
                    let value = match &spec.arg {
                        Some(expr) => expr.eval(&chunk, pos, ctx.random, eval)?,
                        None => Value::Null,
                    };
                    ctx.memory
                        .charge(states[index].reservation_bytes_for_update(&value, multiplicity))?;
                    states[index].update_n(&value, multiplicity);
                }
            }
        }
    }
    ctx.memory.charge(
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
pub(crate) fn accumulate_groups<'a>(
    plan: &AggPlan,
    root: &mut Exec<'a>,
    ctx: &OperatorContext<'a>,
) -> Result<AggPartial> {
    let mut eval = EvalState::new();
    if plan.group_keys.is_empty() {
        return accumulate_global_group(plan, root, ctx, &mut eval);
    }
    let layout = ctx.layout;
    let mut groups: HashMap<Vec<ValueKey>, GroupData> = HashMap::new();
    let mut order: Vec<Vec<ValueKey>> = Vec::new(); // first-seen order

    while let Some(chunk) = root.next_chunk(ctx, &mut eval)? {
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
                    GroupItem::Scalar(ce) => ce.eval(&chunk, pos, ctx.random, &mut eval)?,
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
                    ctx.memory.charge(retained_bytes)?;
                    order.push(entry.key().clone());
                    entry.insert(GroupData { key_values, states })
                }
            };
            for (i, spec) in plan.aggs.iter().enumerate() {
                let v = match &spec.arg {
                    Some(ce) => ce.eval(&chunk, pos, ctx.random, &mut eval)?,
                    None => Value::Null, // count(*)
                };
                ctx.memory
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
pub(crate) fn merge_partial(global: &mut AggPartial, partial: AggPartial) {
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
pub(crate) fn emit_groups<'a>(
    plan: &AggPlan,
    partial: AggPartial,
    projection: &BoundProjection,
    ctx: &OperatorContext<'a>,
) -> Result<OutputBuffer> {
    let mut eval = EvalState::new();
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
                    assemble_id(*var, id, layout, EntityReader::from_ctx(ctx))?
                }
                GroupItem::Scalar(_) => key_iter.next().unwrap(),
                GroupItem::Agg(ce) => {
                    ce.eval_with_aggs(&dummy, 0, &agg_values, ctx.random, &mut eval)?
                }
            });
        }
        // ORDER BY for the aggregate path references output columns only.
        let order_keys = order_keys_from_output(projection, &values, ctx.random)?;
        deep_materialize_values(&mut values, &deep_types, ctx)?;
        ctx.memory.charge(output.push(values, order_keys))?;
    }
    Ok(output)
}

/// Serial aggregate (a pipeline breaker): accumulate the whole pipeline, then emit.
pub(crate) fn aggregate<'a>(
    projection: &BoundProjection,
    root: &mut Exec<'a>,
    ctx: &OperatorContext<'a>,
) -> Result<OutputBuffer> {
    let plan = AggPlan::build(projection, ctx.layout)?;
    let partial = accumulate_groups(&plan, root, ctx)?;
    emit_groups(&plan, partial, projection, ctx)
}
