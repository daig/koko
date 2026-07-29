use super::*;

pub(crate) struct VarExtendState<'a> {
    pub(crate) input: Box<Exec<'a>>,
    pub(crate) ve: &'a VarLengthExtend,
    pub(crate) filter: Option<CompiledFilter>,
    pub(crate) st: ExpandState,
}

pub(crate) struct ProjectPathState<'a> {
    pub(crate) input: Box<Exec<'a>>,
    pub(crate) pp: &'a ProjectPath,
    pub(crate) st: ExpandState,
}

/// One enumerated path: its end node, the intermediate node ids (endpoints
/// excluded), and the relationship ids in order. The id vecs are populated only
/// when the rel value is needed (`build_value`); otherwise just `end` is used.
pub(crate) struct EnumPath {
    pub(crate) end: InternalId,
    pub(crate) node_ids: Vec<InternalId>,
    pub(crate) rel_ids: Vec<InternalId>,
    /// Accumulated edge weight for a (ALL) WSHORTEST path; `None` otherwise.
    pub(crate) cost: Option<f64>,
}

/// A compiled per-step recursive filter (the bound `(r, n | WHERE …)` split into
/// a relationship gate and an intermediate-node gate).
pub(crate) struct CompiledFilter {
    pub(crate) rel_param: LambdaVarId,
    pub(crate) node_param: LambdaVarId,
    pub(crate) rel_pred: Option<CompiledExpr>,
    pub(crate) node_pred: Option<CompiledExpr>,
}

impl CompiledFilter {
    /// Evaluate one of the predicates against the current `(rel, node)` binding.
    pub(crate) fn check(
        pred: &Option<CompiledExpr>,
        binds: &[(LambdaVarId, Value)],
        random: &RandomState,
        eval: &mut EvalState,
    ) -> bool {
        match pred {
            None => true,
            Some(ce) => {
                let dummy = DataChunk::new(&[]);
                matches!(
                    ce.eval_with_bindings(&dummy, 0, binds, random, eval),
                    Ok(v) if v.as_bool() == Some(true)
                )
            }
        }
    }
}

/// Enumerate paths from `start`: DFS for the `All` mode, BFS for shortest modes.
pub(crate) fn enum_path_bytes(path: &EnumPath) -> u64 {
    (std::mem::size_of::<EnumPath>()
        + path.node_ids.capacity() * std::mem::size_of::<InternalId>()
        + path.rel_ids.capacity() * std::mem::size_of::<InternalId>()) as u64
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn extend_dispatch(
    sources: &IcebugQuerySources,
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
    if !sources.extend_batch_into(
        rel_table,
        nodes,
        dir,
        out,
        catalog,
        control,
        memory.tracker(),
    )? {
        storage.extend_batch_into(read, rel_table, nodes, dir, out);
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn enumerate_paths(
    ve: &VarLengthExtend,
    catalog: &Catalog,
    storage: &InMemStorage,
    sources: &IcebugQuerySources,
    read: StorageReadHandle,
    filter: Option<&CompiledFilter>,
    random: &RandomState,
    eval: &mut EvalState,
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
                ve, catalog, storage, sources, read, filter, random, eval, memory, control, start,
                &mut inter, &mut rels, &mut out,
            )?;
            Ok(out)
        }
        RecursiveMode::Shortest | RecursiveMode::AllShortest => enumerate_shortest(
            ve, catalog, storage, sources, read, filter, random, eval, memory, control, start,
        ),
        RecursiveMode::WShortest | RecursiveMode::AllWShortest => enumerate_wshortest(
            ve, catalog, storage, sources, read, filter, random, eval, memory, control, start,
        ),
    }
}

/// Assemble the `(rel_param, rel_value)` + `(node_param, node_value)` bindings for
/// one edge, for evaluating the per-step filter predicates.
pub(crate) fn edge_binds(
    filter: &CompiledFilter,
    rel_id: InternalId,
    nbr_id: InternalId,
    entity: EntityReader<'_>,
) -> Result<[(LambdaVarId, Value); 2]> {
    Ok([
        (
            filter.rel_param,
            Value::Rel(Box::new(assemble_rel_value(rel_id, entity)?)),
        ),
        (
            filter.node_param,
            Value::Node(Box::new(assemble_node_value(nbr_id, entity)?)),
        ),
    ])
}

/// Depth-first enumeration of every walk (subject to the path semantic + filter)
/// within `[lower, upper]` from `current`. `inter` holds the intermediate nodes
/// visited so far (n1..n_depth); `rels` the relationships taken.
#[allow(clippy::too_many_arguments)]
pub(crate) fn dfs_all(
    ve: &VarLengthExtend,
    catalog: &Catalog,
    storage: &InMemStorage,
    sources: &IcebugQuerySources,
    read: StorageReadHandle,
    filter: Option<&CompiledFilter>,
    random: &RandomState,
    eval: &mut EvalState,
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
                    EntityReader {
                        catalog,
                        storage,
                        read,
                        sources,
                        memory,
                        control,
                    },
                )?;
                if !CompiledFilter::check(&f.rel_pred, &binds, random, eval) {
                    continue;
                }
                recurse_ok = CompiledFilter::check(&f.node_pred, &binds, random, eval);
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
                    ve, catalog, storage, sources, read, filter, random, eval, memory, control,
                    n.nbr, inter, rels, out,
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
pub(crate) fn enumerate_shortest(
    ve: &VarLengthExtend,
    catalog: &Catalog,
    storage: &InMemStorage,
    sources: &IcebugQuerySources,
    read: StorageReadHandle,
    filter: Option<&CompiledFilter>,
    random: &RandomState,
    eval: &mut EvalState,
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
                            EntityReader {
                                catalog,
                                storage,
                                read,
                                sources,
                                memory,
                                control,
                            },
                        )?;
                        if !CompiledFilter::check(&f.rel_pred, &binds, random, eval) {
                            continue;
                        }
                        expand_ok = CompiledFilter::check(&f.node_pred, &binds, random, eval);
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
pub(crate) fn enumerate_wshortest(
    ve: &VarLengthExtend,
    catalog: &Catalog,
    storage: &InMemStorage,
    sources: &IcebugQuerySources,
    read: StorageReadHandle,
    filter: Option<&CompiledFilter>,
    random: &RandomState,
    eval: &mut EvalState,
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
                    .columns()
                    .iter()
                    .find(|column| column.name().eq_ignore_ascii_case(weight_col))
                    .map(|column| column.column_id().0 as usize)
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
                memory.tracker(),
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
                        EntityReader {
                            catalog,
                            storage,
                            read,
                            sources,
                            memory,
                            control,
                        },
                    )?;
                    if !CompiledFilter::check(&f.rel_pred, &binds, random, eval) {
                        continue;
                    }
                    expand_ok = CompiledFilter::check(&f.node_pred, &binds, random, eval);
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
pub(crate) fn format_weight(w: f64) -> String {
    if w.fract() == 0.0 {
        format!("{}", w as i64)
    } else {
        format!("{w}")
    }
}

/// Reconstruct one shortest path to `end` (following the first predecessor),
/// filling intermediate node ids and rel ids in forward order.
pub(crate) fn reconstruct_one(
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
pub(crate) fn reconstruct_all(
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
pub(crate) fn assemble_path(
    pp: &ProjectPath,
    layout: &RowLayout,
    chunk: &DataChunk,
    pos: usize,
    entity: EntityReader<'_>,
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
