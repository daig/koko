use super::*;
use koko_algorithm::{
    KCoreDecomposition, KCoreGraph, PageRankConfig, PageRankGraph, PageRankScores,
    StronglyConnectedComponents, StronglyConnectedGraph, TopologicalGraph, TopologicalLevels,
    UNASSIGNED_CORE, WeaklyConnectedComponents, WeaklyConnectedGraph, k_core_decomposition,
    page_rank, strongly_connected_components, topological_levels, weakly_connected_components,
};
use koko_ir::bound::{BoundGraphSelection, GraphAlgorithmScanId};
use koko_ir::plan::{
    KCorePlan, LouvainPlan, PageRankPlan, StronglyConnectedComponentsPlan, TopologicalLevelsPlan,
    WeaklyConnectedComponentsPlan,
};
use koko_storage::EdgeDir;
use std::mem::size_of;
use std::sync::Arc;

/// One mutex per logical scan prevents duplicate computation without serializing
/// independent algorithm scans.
#[derive(Default)]
pub(crate) struct GraphAlgorithmCache {
    entries: Mutex<HashMap<GraphAlgorithmScanId, Arc<GraphAlgorithmCacheEntry>>>,
}

#[derive(Default)]
struct GraphAlgorithmCacheEntry {
    topological_result: Mutex<Option<Result<Arc<TopologicalScanResult>>>>,
    weakly_connected_result: Mutex<Option<Result<Arc<WeaklyConnectedComponentsScanResult>>>>,
    strongly_connected_result: Mutex<Option<Result<Arc<StronglyConnectedScanResult>>>>,
    k_core_result: Mutex<Option<Result<Arc<KCoreScanResult>>>>,
    page_rank_result: Mutex<Option<Result<Arc<PageRankScanResult>>>>,
    louvain_result: Mutex<Option<Result<Arc<LouvainScanResult>>>>,
}

impl GraphAlgorithmCache {
    pub(crate) fn topological_levels(
        &self,
        scan: &TopologicalLevelsPlan,
        ctx: &OperatorContext<'_>,
    ) -> Result<Arc<TopologicalScanResult>> {
        let entry = {
            let mut entries = self
                .entries
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            entries
                .entry(scan.id)
                .or_insert_with(|| Arc::new(GraphAlgorithmCacheEntry::default()))
                .clone()
        };
        let mut result = entry
            .topological_result
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(result) = result.as_ref() {
            return result.clone();
        }
        let computed = compute_topological_levels(scan, ctx).map(Arc::new);
        *result = Some(computed.clone());
        computed
    }

    pub(crate) fn weakly_connected_components(
        &self,
        scan: &WeaklyConnectedComponentsPlan,
        ctx: &OperatorContext<'_>,
    ) -> Result<Arc<WeaklyConnectedComponentsScanResult>> {
        let entry = {
            let mut entries = self
                .entries
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            entries
                .entry(scan.id)
                .or_insert_with(|| Arc::new(GraphAlgorithmCacheEntry::default()))
                .clone()
        };
        let mut result = entry
            .weakly_connected_result
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(result) = result.as_ref() {
            return result.clone();
        }
        let computed = compute_weakly_connected_components(scan, ctx).map(Arc::new);
        *result = Some(computed.clone());
        computed
    }

    pub(crate) fn strongly_connected_components(
        &self,
        scan: &StronglyConnectedComponentsPlan,
        ctx: &OperatorContext<'_>,
    ) -> Result<Arc<StronglyConnectedScanResult>> {
        let entry = {
            let mut entries = self
                .entries
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            entries
                .entry(scan.id)
                .or_insert_with(|| Arc::new(GraphAlgorithmCacheEntry::default()))
                .clone()
        };
        let mut result = entry
            .strongly_connected_result
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(result) = result.as_ref() {
            return result.clone();
        }
        let computed = compute_strongly_connected_components(scan, ctx).map(Arc::new);
        *result = Some(computed.clone());
        computed
    }

    pub(crate) fn k_core_decomposition(
        &self,
        scan: &KCorePlan,
        ctx: &OperatorContext<'_>,
    ) -> Result<Arc<KCoreScanResult>> {
        let entry = {
            let mut entries = self
                .entries
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            entries
                .entry(scan.id)
                .or_insert_with(|| Arc::new(GraphAlgorithmCacheEntry::default()))
                .clone()
        };
        let mut result = entry
            .k_core_result
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(result) = result.as_ref() {
            return result.clone();
        }
        let computed = compute_k_core_decomposition(scan, ctx).map(Arc::new);
        *result = Some(computed.clone());
        computed
    }

    pub(crate) fn page_rank(
        &self,
        scan: &PageRankPlan,
        ctx: &OperatorContext<'_>,
    ) -> Result<Arc<PageRankScanResult>> {
        let entry = {
            let mut entries = self
                .entries
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            entries
                .entry(scan.id)
                .or_insert_with(|| Arc::new(GraphAlgorithmCacheEntry::default()))
                .clone()
        };
        let mut result = entry
            .page_rank_result
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(result) = result.as_ref() {
            return result.clone();
        }
        let computed = compute_page_rank(scan, ctx).map(Arc::new);
        *result = Some(computed.clone());
        computed
    }

    pub(crate) fn louvain(
        &self,
        scan: &LouvainPlan,
        ctx: &OperatorContext<'_>,
    ) -> Result<Arc<LouvainScanResult>> {
        let entry = {
            let mut entries = self
                .entries
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            entries
                .entry(scan.id)
                .or_insert_with(|| Arc::new(GraphAlgorithmCacheEntry::default()))
                .clone()
        };
        let mut result = entry
            .louvain_result
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(result) = result.as_ref() {
            return result.clone();
        }
        let computed = compute_louvain(scan, ctx).map(Arc::new);
        *result = Some(computed.clone());
        computed
    }

    pub(crate) fn clear(&self) {
        self.entries
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clear();
    }
}

pub(crate) struct TopologicalScanResult {
    levels: TopologicalLevels,
    /// One base per selected node table plus the final address-space width.
    table_bases: Vec<usize>,
    _mapping_memory: MemoryReservation,
}

pub(crate) struct TopologicalLevelsState<'a> {
    pub(crate) scan: &'a TopologicalLevelsPlan,
    pub(crate) result: Option<Arc<TopologicalScanResult>>,
    pub(crate) table_idx: usize,
    pub(crate) offset: u64,
    pub(crate) projected_columns: Vec<Vec<usize>>,
}

pub(crate) fn next_topological_levels_chunk(
    state: &mut TopologicalLevelsState<'_>,
    ctx: &OperatorContext<'_>,
) -> Result<Option<DataChunk>> {
    if state.result.is_none() {
        state.result = Some(
            ctx.visibility
                .graph_algorithms
                .topological_levels(state.scan, ctx)?,
        );
    }
    let result = state
        .result
        .as_ref()
        .expect("topological result was initialized");

    loop {
        if state.table_idx >= state.scan.node.tables.len() {
            return Ok(None);
        }
        let scan_table = &state.scan.node.tables[state.table_idx];
        let bound = ctx.storage.node_count(scan_table.table);
        if state.offset >= bound {
            state.table_idx += 1;
            state.offset = 0;
            continue;
        }

        let offset_count = (bound - state.offset).min(VECTOR_CAPACITY as u64) as usize;
        let batch = ctx.storage.scan_node_batch(
            ctx.read(),
            scan_table.table,
            &state.projected_columns[state.table_idx],
            state.offset,
            offset_count,
        );
        state.offset += offset_count as u64;
        let size = batch.size();
        if size == 0 {
            continue;
        }

        let mut output = DataChunk::new(&ctx.layout.col_types);
        let mut source_columns = batch.columns.into_iter();
        output.columns[state.scan.node.id_col] = source_columns
            .next()
            .expect("node batch includes its internal id");
        for (property, source) in scan_table.prop_cols.iter().zip(source_columns) {
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

        let table_base = result.table_bases[state.table_idx];
        let (columns_before_level, level_and_after) =
            output.columns.split_at_mut(state.scan.level_col);
        let ids = match &columns_before_level[state.scan.node.id_col].data {
            ColumnData::InternalId(ids) => ids,
            _ => unreachable!("topological node id column is INTERNAL_ID"),
        };
        let level_column = &mut level_and_after[0];
        for (row, id) in ids.iter().copied().enumerate().take(size) {
            let offset = usize::try_from(id.offset.0)
                .map_err(|_| Error::runtime("Graph algorithm vertex offset overflow."))?;
            let vertex = table_base
                .checked_add(offset)
                .ok_or_else(|| Error::runtime("Graph algorithm vertex address overflow."))?;
            let level = result.levels.level(vertex).ok_or_else(|| {
                Error::runtime("Topological result omitted a visible selected vertex.")
            })?;
            level_column.set_value_owned(
                row,
                Value::Int64(i64::try_from(level).map_err(|_| {
                    Error::runtime("Topological level exceeds the INT64 result range.")
                })?),
            );
        }
        output.set_flat(size);
        return Ok(Some(output));
    }
}

pub(crate) struct WeaklyConnectedComponentsScanResult {
    components: WeaklyConnectedComponents,
    /// One base per selected node table plus the final address-space width.
    table_bases: Vec<usize>,
    _mapping_memory: MemoryReservation,
}

pub(crate) struct WeaklyConnectedComponentsState<'a> {
    pub(crate) scan: &'a WeaklyConnectedComponentsPlan,
    pub(crate) result: Option<Arc<WeaklyConnectedComponentsScanResult>>,
    pub(crate) table_idx: usize,
    pub(crate) offset: u64,
    pub(crate) projected_columns: Vec<Vec<usize>>,
}

pub(crate) fn next_weakly_connected_components_chunk(
    state: &mut WeaklyConnectedComponentsState<'_>,
    ctx: &OperatorContext<'_>,
) -> Result<Option<DataChunk>> {
    if state.result.is_none() {
        state.result = Some(
            ctx.visibility
                .graph_algorithms
                .weakly_connected_components(state.scan, ctx)?,
        );
    }
    let result = state
        .result
        .as_ref()
        .expect("weakly connected components result was initialized");

    loop {
        if state.table_idx >= state.scan.node.tables.len() {
            return Ok(None);
        }
        let scan_table = &state.scan.node.tables[state.table_idx];
        let bound = ctx.storage.node_count(scan_table.table);
        if state.offset >= bound {
            state.table_idx += 1;
            state.offset = 0;
            continue;
        }

        let offset_count = (bound - state.offset).min(VECTOR_CAPACITY as u64) as usize;
        let batch = ctx.storage.scan_node_batch(
            ctx.read(),
            scan_table.table,
            &state.projected_columns[state.table_idx],
            state.offset,
            offset_count,
        );
        state.offset += offset_count as u64;
        let size = batch.size();
        if size == 0 {
            continue;
        }

        let mut output = DataChunk::new(&ctx.layout.col_types);
        let mut source_columns = batch.columns.into_iter();
        output.columns[state.scan.node.id_col] = source_columns
            .next()
            .expect("node batch includes its internal id");
        for (property, source) in scan_table.prop_cols.iter().zip(source_columns) {
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

        let table_base = result.table_bases[state.table_idx];
        let (columns_before_component, component_and_after) =
            output.columns.split_at_mut(state.scan.component_id_col);
        let ids = match &columns_before_component[state.scan.node.id_col].data {
            ColumnData::InternalId(ids) => ids,
            _ => unreachable!("weakly connected components node id column is INTERNAL_ID"),
        };
        let component_column = &mut component_and_after[0];
        for (row, id) in ids.iter().copied().enumerate().take(size) {
            let offset = usize::try_from(id.offset.0)
                .map_err(|_| Error::runtime("Graph algorithm vertex offset overflow."))?;
            let vertex = table_base
                .checked_add(offset)
                .ok_or_else(|| Error::runtime("Graph algorithm vertex address overflow."))?;
            let component_id = result.components.component_id(vertex).ok_or_else(|| {
                Error::runtime(
                    "Weakly connected components result omitted a visible selected vertex.",
                )
            })?;
            component_column.set_value_owned(row, Value::Int64(component_id));
        }
        output.set_flat(size);
        return Ok(Some(output));
    }
}

pub(crate) struct PageRankScanResult {
    scores: PageRankScores,
    /// One base per selected node table plus the final address-space width.
    table_bases: Vec<usize>,
    _mapping_memory: MemoryReservation,
}

pub(crate) struct PageRankState<'a> {
    pub(crate) scan: &'a PageRankPlan,
    pub(crate) result: Option<Arc<PageRankScanResult>>,
    pub(crate) table_idx: usize,
    pub(crate) offset: u64,
    pub(crate) projected_columns: Vec<Vec<usize>>,
}

pub(crate) fn next_page_rank_chunk(
    state: &mut PageRankState<'_>,
    ctx: &OperatorContext<'_>,
) -> Result<Option<DataChunk>> {
    if state.result.is_none() {
        state.result = Some(ctx.visibility.graph_algorithms.page_rank(state.scan, ctx)?);
    }
    let result = state
        .result
        .as_ref()
        .expect("PageRank result was initialized");

    loop {
        if state.table_idx >= state.scan.node.tables.len() {
            return Ok(None);
        }
        let scan_table = &state.scan.node.tables[state.table_idx];
        let bound = ctx.storage.node_count(scan_table.table);
        if state.offset >= bound {
            state.table_idx += 1;
            state.offset = 0;
            continue;
        }

        let offset_count = (bound - state.offset).min(VECTOR_CAPACITY as u64) as usize;
        let batch = ctx.storage.scan_node_batch(
            ctx.read(),
            scan_table.table,
            &state.projected_columns[state.table_idx],
            state.offset,
            offset_count,
        );
        state.offset += offset_count as u64;
        let size = batch.size();
        if size == 0 {
            continue;
        }

        let mut output = DataChunk::new(&ctx.layout.col_types);
        let mut source_columns = batch.columns.into_iter();
        output.columns[state.scan.node.id_col] = source_columns
            .next()
            .expect("node batch includes its internal id");
        for (property, source) in scan_table.prop_cols.iter().zip(source_columns) {
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

        let table_base = result.table_bases[state.table_idx];
        let (columns_before_score, score_and_after) =
            output.columns.split_at_mut(state.scan.score_col);
        let ids = match &columns_before_score[state.scan.node.id_col].data {
            ColumnData::InternalId(ids) => ids,
            _ => unreachable!("PageRank node id column is INTERNAL_ID"),
        };
        let score_column = &mut score_and_after[0];
        for (row, id) in ids.iter().copied().enumerate().take(size) {
            let offset = usize::try_from(id.offset.0)
                .map_err(|_| Error::runtime("Graph algorithm vertex offset overflow."))?;
            let vertex = table_base
                .checked_add(offset)
                .ok_or_else(|| Error::runtime("Graph algorithm vertex address overflow."))?;
            let score = result.scores.score(vertex).ok_or_else(|| {
                Error::runtime("PageRank result omitted a visible selected vertex.")
            })?;
            score_column.set_value_owned(row, Value::Double(score));
        }
        output.set_flat(size);
        return Ok(Some(output));
    }
}
fn compute_topological_levels(
    scan: &TopologicalLevelsPlan,
    ctx: &OperatorContext<'_>,
) -> Result<TopologicalScanResult> {
    let base_bytes = allocation_bytes::<usize>(scan.graph.node_tables.len().saturating_add(1))?;
    let mapping_memory = ctx.memory.temporary_reservation(base_bytes)?;
    let mut table_bases: Vec<usize> =
        Vec::with_capacity(scan.graph.node_tables.len().saturating_add(1));
    table_bases.push(0_usize);
    for &table in &scan.graph.node_tables {
        let width = usize::try_from(ctx.storage.node_count(table))
            .map_err(|_| Error::runtime("Graph algorithm node-table width overflow."))?;
        let next = table_bases
            .last()
            .copied()
            .unwrap_or_default()
            .checked_add(width)
            .ok_or_else(|| Error::runtime("Graph algorithm vertex address overflow."))?;
        table_bases.push(next);
    }
    let vertex_count = table_bases.last().copied().unwrap_or_default();

    let all_nodes_visible = scan
        .graph
        .node_tables
        .iter()
        .all(|&table| ctx.storage.node_rows_all_visible(ctx.read(), table));
    let active_bytes = if all_nodes_visible {
        0
    } else {
        bitset_bytes(vertex_count)?
    };
    let _active_memory = ctx.memory.temporary_reservation(active_bytes)?;
    let mut active = (!all_nodes_visible).then(|| vec![false; vertex_count]);
    if let Some(active) = &mut active {
        for (domain, &table) in scan.graph.node_tables.iter().enumerate() {
            let base = table_bases[domain];
            ctx.storage
                .visit_node_offsets(ctx.read(), table, |offset| {
                    let offset = usize::try_from(offset)
                        .map_err(|_| Error::runtime("Graph algorithm vertex offset overflow."))?;
                    let vertex = base.checked_add(offset).ok_or_else(|| {
                        Error::runtime("Graph algorithm vertex address overflow.")
                    })?;
                    active[vertex] = true;
                    Ok(())
                })?;
        }
    }

    let rel_visibility_bytes = bitset_bytes(scan.graph.rel_tables.len())?;
    let _rel_visibility_memory = ctx.memory.temporary_reservation(rel_visibility_bytes)?;
    let rel_all_visible: Vec<bool> = scan
        .graph
        .rel_tables
        .iter()
        .map(|rel| {
            ctx.visibility
                .rel_rows_all_visible(ctx.storage, ctx.read(), rel.table)
        })
        .collect();

    let graph = StorageTopologicalGraph {
        selection: &scan.graph,
        storage: ctx.storage,
        read: ctx.read(),
        table_bases: &table_bases,
        active: active.as_deref(),
        rel_all_visible: &rel_all_visible,
    };
    let levels = topological_levels(&graph, ctx.memory.tracker(), || ctx.control.check())?;
    Ok(TopologicalScanResult {
        levels,
        table_bases,
        _mapping_memory: mapping_memory,
    })
}

fn compute_page_rank(scan: &PageRankPlan, ctx: &OperatorContext<'_>) -> Result<PageRankScanResult> {
    let base_len = scan
        .graph
        .node_tables
        .len()
        .checked_add(1)
        .ok_or_else(Error::buffer_manager)?;
    let base_bytes = allocation_bytes::<usize>(base_len)?;
    let mapping_memory = ctx.memory.temporary_reservation(base_bytes)?;
    let mut table_bases = Vec::with_capacity(base_len);
    table_bases.push(0_usize);
    for (domain, &table) in scan.graph.node_tables.iter().enumerate() {
        let width = usize::try_from(ctx.storage.node_count(table))
            .map_err(|_| Error::runtime("Graph algorithm node-table width overflow."))?;
        let next = table_bases
            .last()
            .copied()
            .unwrap_or_default()
            .checked_add(width)
            .ok_or_else(|| Error::runtime("Graph algorithm vertex address overflow."))?;
        table_bases.push(next);
        if (domain + 1) % VECTOR_CAPACITY == 0 {
            ctx.control.check()?;
        }
    }
    let vertex_count = table_bases.last().copied().unwrap_or_default();

    let all_nodes_visible = scan
        .graph
        .node_tables
        .iter()
        .all(|&table| ctx.storage.node_rows_all_visible(ctx.read(), table));
    let active_bytes = if all_nodes_visible {
        0
    } else {
        bitset_bytes(vertex_count)?
    };
    let _active_memory = ctx.memory.temporary_reservation(active_bytes)?;
    let mut active = (!all_nodes_visible).then(|| vec![false; vertex_count]);
    if let Some(active) = &mut active {
        let mut visited = 0_usize;
        for (domain, &table) in scan.graph.node_tables.iter().enumerate() {
            let base = table_bases[domain];
            ctx.storage
                .visit_node_offsets(ctx.read(), table, |offset| {
                    let offset = usize::try_from(offset)
                        .map_err(|_| Error::runtime("Graph algorithm vertex offset overflow."))?;
                    let vertex = base.checked_add(offset).ok_or_else(|| {
                        Error::runtime("Graph algorithm vertex address overflow.")
                    })?;
                    active[vertex] = true;
                    visited = visited
                        .checked_add(1)
                        .ok_or_else(|| Error::runtime("PageRank visible vertex count overflow."))?;
                    if visited % VECTOR_CAPACITY == 0 {
                        ctx.control.check()?;
                    }
                    Ok(())
                })?;
            ctx.control.check()?;
        }
    }

    let rel_visibility_bytes = bitset_bytes(scan.graph.rel_tables.len())?;
    let _rel_visibility_memory = ctx.memory.temporary_reservation(rel_visibility_bytes)?;
    let mut rel_all_visible = Vec::with_capacity(scan.graph.rel_tables.len());
    for (index, rel) in scan.graph.rel_tables.iter().enumerate() {
        rel_all_visible.push(ctx.visibility.rel_rows_all_visible(
            ctx.storage,
            ctx.read(),
            rel.table,
        ));
        if (index + 1) % VECTOR_CAPACITY == 0 {
            ctx.control.check()?;
        }
    }

    let graph = StorageTopologicalGraph {
        selection: &scan.graph,
        storage: ctx.storage,
        read: ctx.read(),
        table_bases: &table_bases,
        active: active.as_deref(),
        rel_all_visible: &rel_all_visible,
    };
    let config = PageRankConfig {
        damping_factor: scan.config.damping_factor,
        tolerance: scan.config.tolerance,
        max_iterations: scan.config.max_iterations,
        normalize_initial: scan.config.normalize_initial,
    };
    let scores = page_rank(&graph, config, ctx.memory.tracker(), || ctx.control.check())?;
    Ok(PageRankScanResult {
        scores,
        table_bases,
        _mapping_memory: mapping_memory,
    })
}
struct StorageTopologicalGraph<'a> {
    selection: &'a BoundGraphSelection,
    storage: &'a InMemStorage,
    read: StorageReadHandle,
    table_bases: &'a [usize],
    active: Option<&'a [bool]>,
    rel_all_visible: &'a [bool],
}

impl StorageTopologicalGraph<'_> {
    fn dense_vertex(&self, domain: u32, offset: u64) -> Result<usize> {
        let domain = domain as usize;
        let offset = usize::try_from(offset)
            .map_err(|_| Error::runtime("Graph algorithm vertex offset overflow."))?;
        self.table_bases[domain]
            .checked_add(offset)
            .filter(|&vertex| vertex < self.table_bases[domain + 1])
            .ok_or_else(|| Error::runtime("Graph algorithm edge endpoint is out of bounds."))
    }

    fn active(&self, vertex: usize) -> bool {
        match self.active {
            Some(active) => active[vertex],
            None => true,
        }
    }

    fn source_domain(&self, source: usize) -> Option<(usize, u64)> {
        if source >= self.table_bases.last().copied().unwrap_or_default() {
            return None;
        }
        let domain = self
            .table_bases
            .partition_point(|&base| base <= source)
            .checked_sub(1)?;
        let offset = source.checked_sub(self.table_bases[domain])?;
        Some((domain, u64::try_from(offset).ok()?))
    }
}

impl TopologicalGraph for StorageTopologicalGraph<'_> {
    fn vertex_count(&self) -> usize {
        self.table_bases.last().copied().unwrap_or_default()
    }

    fn is_vertex_active(&self, vertex: usize) -> bool {
        vertex < self.table_bases.last().copied().unwrap_or_default() && self.active(vertex)
    }

    fn for_each_edge(&self, mut visit: impl FnMut(usize, usize) -> Result<()>) -> Result<()> {
        for (index, rel) in self.selection.rel_tables.iter().enumerate() {
            let mut visit_edge = |_: u64, source_offset: u64, destination_offset: u64| {
                let source = self.dense_vertex(rel.source_domain, source_offset)?;
                let destination = self.dense_vertex(rel.destination_domain, destination_offset)?;
                if self.active(source) && self.active(destination) {
                    visit(source, destination)?;
                }
                Ok(())
            };
            if self.rel_all_visible[index] {
                self.storage.visit_rel_endpoints_all_visible(
                    self.read,
                    rel.table,
                    &mut visit_edge,
                )?;
            } else {
                self.storage
                    .visit_rel_endpoints(self.read, rel.table, &mut visit_edge)?;
            }
        }
        Ok(())
    }

    fn for_each_out_neighbor(
        &self,
        source: usize,
        mut visit: impl FnMut(usize) -> Result<()>,
    ) -> Result<()> {
        let Some((source_domain, source_offset)) = self.source_domain(source) else {
            return Err(Error::runtime(
                "Topological traversal referenced an unknown source vertex.",
            ));
        };
        let source_id = InternalId::new(self.selection.node_tables[source_domain], source_offset);
        for (index, rel) in self.selection.rel_tables.iter().enumerate() {
            if rel.source_domain as usize != source_domain {
                continue;
            }
            let mut visit_neighbor = |_: u64, destination_offset: u64| {
                let destination = self.dense_vertex(rel.destination_domain, destination_offset)?;
                if self.active(destination) {
                    visit(destination)?;
                }
                Ok(())
            };
            if self.rel_all_visible[index] {
                self.storage.visit_neighbors_all_visible(
                    self.read,
                    rel.table,
                    source_id,
                    EdgeDir::Fwd,
                    &mut visit_neighbor,
                )?;
            } else {
                self.storage.visit_neighbors(
                    self.read,
                    rel.table,
                    source_id,
                    EdgeDir::Fwd,
                    &mut visit_neighbor,
                )?;
            }
        }
        Ok(())
    }
}

fn compute_weakly_connected_components(
    scan: &WeaklyConnectedComponentsPlan,
    ctx: &OperatorContext<'_>,
) -> Result<WeaklyConnectedComponentsScanResult> {
    ctx.control.check()?;
    let base_bytes = allocation_bytes::<usize>(scan.graph.node_tables.len().saturating_add(1))?;
    let mapping_memory = ctx.memory.temporary_reservation(base_bytes)?;
    let mut table_bases: Vec<usize> =
        Vec::with_capacity(scan.graph.node_tables.len().saturating_add(1));
    table_bases.push(0_usize);
    for (index, &table) in scan.graph.node_tables.iter().enumerate() {
        let width = usize::try_from(ctx.storage.node_count(table))
            .map_err(|_| Error::runtime("Graph algorithm node-table width overflow."))?;
        let next = table_bases
            .last()
            .copied()
            .unwrap_or_default()
            .checked_add(width)
            .ok_or_else(|| Error::runtime("Graph algorithm vertex address overflow."))?;
        table_bases.push(next);
        if (index + 1) % VECTOR_CAPACITY == 0 {
            ctx.control.check()?;
        }
    }
    let vertex_count = table_bases.last().copied().unwrap_or_default();

    let all_nodes_visible = scan
        .graph
        .node_tables
        .iter()
        .all(|&table| ctx.storage.node_rows_all_visible(ctx.read(), table));
    let active_bytes = if all_nodes_visible {
        0
    } else {
        bitset_bytes(vertex_count)?
    };
    let _active_memory = ctx.memory.temporary_reservation(active_bytes)?;
    let mut active = (!all_nodes_visible).then(|| vec![false; vertex_count]);
    if let Some(active) = &mut active {
        let mut visible_count = 0_usize;
        for (domain, &table) in scan.graph.node_tables.iter().enumerate() {
            let base = table_bases[domain];
            let end = table_bases[domain + 1];
            ctx.storage
                .visit_node_offsets(ctx.read(), table, |offset| {
                    let offset = usize::try_from(offset)
                        .map_err(|_| Error::runtime("Graph algorithm vertex offset overflow."))?;
                    let vertex = base
                        .checked_add(offset)
                        .filter(|&vertex| vertex < end)
                        .ok_or_else(|| {
                            Error::runtime("Graph algorithm vertex address overflow.")
                        })?;
                    active[vertex] = true;
                    visible_count = visible_count.checked_add(1).ok_or_else(|| {
                        Error::runtime("Graph algorithm visible vertex count overflow.")
                    })?;
                    if visible_count % VECTOR_CAPACITY == 0 {
                        ctx.control.check()?;
                    }
                    Ok(())
                })?;
            ctx.control.check()?;
        }
    }

    let rel_visibility_bytes = bitset_bytes(scan.graph.rel_tables.len())?;
    let _rel_visibility_memory = ctx.memory.temporary_reservation(rel_visibility_bytes)?;
    let mut rel_all_visible = Vec::with_capacity(scan.graph.rel_tables.len());
    for (index, rel) in scan.graph.rel_tables.iter().enumerate() {
        rel_all_visible.push(ctx.visibility.rel_rows_all_visible(
            ctx.storage,
            ctx.read(),
            rel.table,
        ));
        if (index + 1) % VECTOR_CAPACITY == 0 {
            ctx.control.check()?;
        }
    }

    let canonical_bytes = allocation_bytes::<usize>(scan.graph.node_tables.len())?;
    let _canonical_memory = ctx.memory.temporary_reservation(canonical_bytes)?;
    let mut canonical_domains: Vec<usize> = (0..scan.graph.node_tables.len()).collect();
    canonical_domains.sort_unstable_by_key(|&domain| scan.graph.node_tables[domain]);

    let graph = StorageWeaklyConnectedGraph {
        selection: &scan.graph,
        storage: ctx.storage,
        read: ctx.read(),
        table_bases: &table_bases,
        active: active.as_deref(),
        rel_all_visible: &rel_all_visible,
        canonical_domains: &canonical_domains,
    };
    let components =
        weakly_connected_components(&graph, ctx.memory.tracker(), || ctx.control.check())?;
    Ok(WeaklyConnectedComponentsScanResult {
        components,
        table_bases,
        _mapping_memory: mapping_memory,
    })
}

struct StorageWeaklyConnectedGraph<'a> {
    selection: &'a BoundGraphSelection,
    storage: &'a InMemStorage,
    read: StorageReadHandle,
    table_bases: &'a [usize],
    active: Option<&'a [bool]>,
    rel_all_visible: &'a [bool],
    canonical_domains: &'a [usize],
}

impl StorageWeaklyConnectedGraph<'_> {
    fn dense_vertex(&self, domain: u32, offset: u64) -> Option<usize> {
        let domain = domain as usize;
        let offset = usize::try_from(offset).ok()?;
        let base = *self.table_bases.get(domain)?;
        let end = *self.table_bases.get(domain + 1)?;
        base.checked_add(offset).filter(|&vertex| vertex < end)
    }

    fn active(&self, vertex: usize) -> bool {
        if vertex >= self.vertex_count() {
            return false;
        }
        match self.active {
            Some(active) => active[vertex],
            None => true,
        }
    }

    fn vertex_domain(&self, vertex: usize) -> (usize, u64) {
        let domain = self
            .table_bases
            .partition_point(|&base| base <= vertex)
            .checked_sub(1)
            .expect("active WCC vertex has a selected node-table domain");
        let offset = vertex - self.table_bases[domain];
        (
            domain,
            u64::try_from(offset).expect("storage offsets originate as u64"),
        )
    }
}

impl WeaklyConnectedGraph for StorageWeaklyConnectedGraph<'_> {
    fn vertex_count(&self) -> usize {
        self.table_bases.last().copied().unwrap_or_default()
    }

    fn is_vertex_active(&self, vertex: usize) -> bool {
        self.active(vertex)
    }

    fn vertex_id(&self, vertex: usize) -> InternalId {
        let (domain, offset) = self.vertex_domain(vertex);
        InternalId::new(self.selection.node_tables[domain], offset)
    }

    fn for_each_vertex_by_id(&self, mut visit: impl FnMut(usize) -> Result<()>) -> Result<()> {
        for &domain in self.canonical_domains {
            for vertex in self.table_bases[domain]..self.table_bases[domain + 1] {
                if self.active(vertex) {
                    visit(vertex)?;
                }
            }
        }
        Ok(())
    }

    fn for_each_edge(&self, mut visit: impl FnMut(usize, usize) -> Result<()>) -> Result<()> {
        for (index, rel) in self.selection.rel_tables.iter().enumerate() {
            let mut visit_edge = |_: u64, source_offset: u64, destination_offset: u64| {
                let Some(source) = self.dense_vertex(rel.source_domain, source_offset) else {
                    return Ok(());
                };
                let Some(destination) =
                    self.dense_vertex(rel.destination_domain, destination_offset)
                else {
                    return Ok(());
                };
                if self.active(source) && self.active(destination) {
                    visit(source, destination)?;
                }
                Ok(())
            };
            if self.rel_all_visible[index] {
                self.storage.visit_rel_endpoints_all_visible(
                    self.read,
                    rel.table,
                    &mut visit_edge,
                )?;
            } else {
                self.storage
                    .visit_rel_endpoints(self.read, rel.table, &mut visit_edge)?;
            }
        }
        Ok(())
    }
}

pub(crate) struct StronglyConnectedScanResult {
    components: StronglyConnectedComponents,
    /// One base per selected node table plus the final address-space width.
    table_bases: Vec<usize>,
    _mapping_memory: MemoryReservation,
}

pub(crate) struct StronglyConnectedComponentsState<'a> {
    pub(crate) scan: &'a StronglyConnectedComponentsPlan,
    pub(crate) result: Option<Arc<StronglyConnectedScanResult>>,
    pub(crate) table_idx: usize,
    pub(crate) offset: u64,
    pub(crate) projected_columns: Vec<Vec<usize>>,
}

pub(crate) fn next_strongly_connected_components_chunk(
    state: &mut StronglyConnectedComponentsState<'_>,
    ctx: &OperatorContext<'_>,
) -> Result<Option<DataChunk>> {
    if state.result.is_none() {
        state.result = Some(
            ctx.visibility
                .graph_algorithms
                .strongly_connected_components(state.scan, ctx)?,
        );
    }
    let result = state
        .result
        .as_ref()
        .expect("strongly connected component result was initialized");

    loop {
        if state.table_idx >= state.scan.node.tables.len() {
            return Ok(None);
        }
        let scan_table = &state.scan.node.tables[state.table_idx];
        let bound = ctx.storage.node_count(scan_table.table);
        if state.offset >= bound {
            state.table_idx += 1;
            state.offset = 0;
            continue;
        }

        let offset_count = (bound - state.offset).min(VECTOR_CAPACITY as u64) as usize;
        let batch = ctx.storage.scan_node_batch(
            ctx.read(),
            scan_table.table,
            &state.projected_columns[state.table_idx],
            state.offset,
            offset_count,
        );
        state.offset += offset_count as u64;
        let size = batch.size();
        if size == 0 {
            continue;
        }

        let mut output = DataChunk::new(&ctx.layout.col_types);
        let mut source_columns = batch.columns.into_iter();
        output.columns[state.scan.node.id_col] = source_columns
            .next()
            .expect("node batch includes its internal id");
        for (property, source) in scan_table.prop_cols.iter().zip(source_columns) {
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

        let table_base = result.table_bases[state.table_idx];
        let (columns_before_component, component_and_after) =
            output.columns.split_at_mut(state.scan.component_col);
        let ids = match &columns_before_component[state.scan.node.id_col].data {
            ColumnData::InternalId(ids) => ids,
            _ => unreachable!("strongly connected component node id column is INTERNAL_ID"),
        };
        let component_column = &mut component_and_after[0];
        for (row, id) in ids.iter().copied().enumerate().take(size) {
            let offset = usize::try_from(id.offset.0)
                .map_err(|_| Error::runtime("Graph algorithm vertex offset overflow."))?;
            let vertex = table_base
                .checked_add(offset)
                .ok_or_else(|| Error::runtime("Graph algorithm vertex address overflow."))?;
            let component = result.components.component_id(vertex).ok_or_else(|| {
                Error::runtime(
                    "Strongly connected component result omitted a visible selected vertex.",
                )
            })?;
            component_column.set_value_owned(
                row,
                Value::Int64(i64::try_from(component).map_err(|_| {
                    Error::runtime(
                        "Strongly connected component ID exceeds the INT64 result range.",
                    )
                })?),
            );
        }
        output.set_flat(size);
        return Ok(Some(output));
    }
}

fn compute_strongly_connected_components(
    scan: &StronglyConnectedComponentsPlan,
    ctx: &OperatorContext<'_>,
) -> Result<StronglyConnectedScanResult> {
    ctx.control.check()?;
    let base_bytes = allocation_bytes::<usize>(scan.graph.node_tables.len().saturating_add(1))?;
    let mapping_memory = ctx.memory.temporary_reservation(base_bytes)?;
    let mut table_bases: Vec<usize> =
        Vec::with_capacity(scan.graph.node_tables.len().saturating_add(1));
    table_bases.push(0_usize);
    for &table in &scan.graph.node_tables {
        let width = usize::try_from(ctx.storage.node_count(table))
            .map_err(|_| Error::runtime("Graph algorithm node-table width overflow."))?;
        let next = table_bases
            .last()
            .copied()
            .unwrap_or_default()
            .checked_add(width)
            .ok_or_else(|| Error::runtime("Graph algorithm vertex address overflow."))?;
        table_bases.push(next);
        ctx.control.check()?;
    }
    let vertex_count = table_bases.last().copied().unwrap_or_default();

    let all_nodes_visible = scan
        .graph
        .node_tables
        .iter()
        .all(|&table| ctx.storage.node_rows_all_visible(ctx.read(), table));
    let active_bytes = if all_nodes_visible {
        0
    } else {
        bitset_bytes(vertex_count)?
    };
    let _active_memory = ctx.memory.temporary_reservation(active_bytes)?;
    let mut active = (!all_nodes_visible).then(|| vec![false; vertex_count]);
    if let Some(active) = &mut active {
        for (domain, &table) in scan.graph.node_tables.iter().enumerate() {
            let base = table_bases[domain];
            ctx.storage
                .visit_node_offsets(ctx.read(), table, |offset| {
                    let offset = usize::try_from(offset)
                        .map_err(|_| Error::runtime("Graph algorithm vertex offset overflow."))?;
                    let vertex = base.checked_add(offset).ok_or_else(|| {
                        Error::runtime("Graph algorithm vertex address overflow.")
                    })?;
                    active[vertex] = true;
                    Ok(())
                })?;
            ctx.control.check()?;
        }
    }

    let rel_visibility_bytes = bitset_bytes(scan.graph.rel_tables.len())?;
    let _rel_visibility_memory = ctx.memory.temporary_reservation(rel_visibility_bytes)?;
    let rel_all_visible: Vec<bool> = scan
        .graph
        .rel_tables
        .iter()
        .map(|rel| {
            ctx.visibility
                .rel_rows_all_visible(ctx.storage, ctx.read(), rel.table)
        })
        .collect();

    let canonical_domain_bytes = allocation_bytes::<usize>(scan.graph.node_tables.len())?;
    let _canonical_domain_memory = ctx.memory.temporary_reservation(canonical_domain_bytes)?;
    let mut canonical_domains: Vec<usize> = (0..scan.graph.node_tables.len()).collect();
    canonical_domains.sort_unstable_by_key(|&domain| scan.graph.node_tables[domain]);
    ctx.control.check()?;

    let graph = StorageStronglyConnectedGraph {
        selection: &scan.graph,
        storage: ctx.storage,
        read: ctx.read(),
        control: ctx.control,
        table_bases: &table_bases,
        canonical_domains: &canonical_domains,
        active: active.as_deref(),
        rel_all_visible: &rel_all_visible,
    };
    let components =
        strongly_connected_components(&graph, ctx.memory.tracker(), || ctx.control.check())?;
    Ok(StronglyConnectedScanResult {
        components,
        table_bases,
        _mapping_memory: mapping_memory,
    })
}

#[derive(Default)]
struct StorageSccNeighborCursor {
    rel_index: usize,
    adjacency_index: usize,
}

struct StorageStronglyConnectedGraph<'a> {
    selection: &'a BoundGraphSelection,
    storage: &'a InMemStorage,
    read: StorageReadHandle,
    control: QueryControl<'a>,
    table_bases: &'a [usize],
    canonical_domains: &'a [usize],
    active: Option<&'a [bool]>,
    rel_all_visible: &'a [bool],
}

impl StorageStronglyConnectedGraph<'_> {
    fn vertex_count(&self) -> usize {
        self.table_bases.last().copied().unwrap_or_default()
    }
    fn dense_vertex(&self, domain: u32, offset: u64) -> Result<usize> {
        let domain = domain as usize;
        let offset = usize::try_from(offset)
            .map_err(|_| Error::runtime("Graph algorithm vertex offset overflow."))?;
        self.table_bases[domain]
            .checked_add(offset)
            .filter(|&vertex| vertex < self.table_bases[domain + 1])
            .ok_or_else(|| Error::runtime("Graph algorithm edge endpoint is out of bounds."))
    }

    fn active(&self, vertex: usize) -> bool {
        match self.active {
            Some(active) => active[vertex],
            None => true,
        }
    }

    fn vertex_domain(&self, vertex: usize) -> Option<(usize, u64)> {
        if vertex >= self.vertex_count() {
            return None;
        }
        let domain = self
            .table_bases
            .partition_point(|&base| base <= vertex)
            .checked_sub(1)?;
        let offset = vertex.checked_sub(self.table_bases[domain])?;
        Some((domain, u64::try_from(offset).ok()?))
    }

    fn next_neighbor(
        &self,
        vertex: usize,
        cursor: &mut StorageSccNeighborCursor,
        direction: EdgeDir,
    ) -> Result<Option<usize>> {
        let Some((domain, offset)) = self.vertex_domain(vertex) else {
            return Err(Error::runtime(
                "Strongly connected traversal referenced an unknown vertex.",
            ));
        };
        let node_id = InternalId::new(self.selection.node_tables[domain], offset);
        while cursor.rel_index < self.selection.rel_tables.len() {
            let index = cursor.rel_index;
            let rel = &self.selection.rel_tables[index];
            let (bound_domain, neighbor_domain) = match direction {
                EdgeDir::Fwd => (rel.source_domain, rel.destination_domain),
                EdgeDir::Bwd => (rel.destination_domain, rel.source_domain),
            };
            if bound_domain as usize != domain {
                cursor.rel_index += 1;
                if cursor.rel_index % VECTOR_CAPACITY == 0 {
                    self.control.check()?;
                }
                cursor.adjacency_index = 0;
                continue;
            }

            let adjacency_before = cursor.adjacency_index;
            let next = if self.rel_all_visible[index] {
                self.storage.next_neighbor_all_visible(
                    self.read,
                    rel.table,
                    node_id,
                    direction,
                    &mut cursor.adjacency_index,
                )
            } else {
                self.storage.next_neighbor(
                    self.read,
                    rel.table,
                    node_id,
                    direction,
                    &mut cursor.adjacency_index,
                )
            };
            if cursor.adjacency_index / VECTOR_CAPACITY != adjacency_before / VECTOR_CAPACITY {
                self.control.check()?;
            }
            if let Some((_, neighbor_offset)) = next {
                let neighbor = self.dense_vertex(neighbor_domain, neighbor_offset)?;
                if self.active(neighbor) {
                    return Ok(Some(neighbor));
                }
                continue;
            }
            cursor.rel_index += 1;
            cursor.adjacency_index = 0;
        }
        Ok(None)
    }
}

impl StronglyConnectedGraph for StorageStronglyConnectedGraph<'_> {
    type NeighborCursor = StorageSccNeighborCursor;

    fn vertex_count(&self) -> usize {
        StorageStronglyConnectedGraph::vertex_count(self)
    }

    fn is_vertex_active(&self, vertex: usize) -> bool {
        vertex < self.vertex_count() && self.active(vertex)
    }

    fn next_out_neighbor(
        &self,
        source: usize,
        cursor: &mut Self::NeighborCursor,
    ) -> Result<Option<usize>> {
        self.next_neighbor(source, cursor, EdgeDir::Fwd)
    }

    fn next_in_neighbor(
        &self,
        destination: usize,
        cursor: &mut Self::NeighborCursor,
    ) -> Result<Option<usize>> {
        self.next_neighbor(destination, cursor, EdgeDir::Bwd)
    }

    fn for_each_vertex_by_internal_id(
        &self,
        mut visit: impl FnMut(usize, InternalId) -> Result<()>,
    ) -> Result<()> {
        for &domain in self.canonical_domains {
            let table = self.selection.node_tables[domain];
            let base = self.table_bases[domain];
            let width = self.table_bases[domain + 1] - base;
            for offset in 0..width {
                let vertex = base + offset;
                if self.active(vertex) {
                    let offset = u64::try_from(offset)
                        .map_err(|_| Error::runtime("Graph algorithm vertex offset overflow."))?;
                    visit(vertex, InternalId::new(table, offset))?;
                }
                if (offset + 1) % VECTOR_CAPACITY == 0 {
                    self.control.check()?;
                }
            }
            self.control.check()?;
        }
        Ok(())
    }
}

pub(crate) struct KCoreScanResult {
    cores: KCoreDecomposition,
    /// One base per selected node table plus the final address-space width.
    table_bases: Vec<usize>,
    _mapping_memory: MemoryReservation,
}

pub(crate) struct KCoreState<'a> {
    pub(crate) scan: &'a KCorePlan,
    pub(crate) result: Option<Arc<KCoreScanResult>>,
    pub(crate) table_idx: usize,
    pub(crate) offset: u64,
    pub(crate) projected_columns: Vec<Vec<usize>>,
}

pub(crate) fn next_k_core_chunk(
    state: &mut KCoreState<'_>,
    ctx: &OperatorContext<'_>,
) -> Result<Option<DataChunk>> {
    if state.result.is_none() {
        state.result = Some(
            ctx.visibility
                .graph_algorithms
                .k_core_decomposition(state.scan, ctx)?,
        );
    }
    let result = state
        .result
        .as_ref()
        .expect("k-core result was initialized");

    loop {
        if state.table_idx >= state.scan.node.tables.len() {
            return Ok(None);
        }
        let scan_table = &state.scan.node.tables[state.table_idx];
        let bound = ctx.storage.node_count(scan_table.table);
        if state.offset >= bound {
            state.table_idx += 1;
            state.offset = 0;
            continue;
        }

        let offset_count = (bound - state.offset).min(VECTOR_CAPACITY as u64) as usize;
        let batch = ctx.storage.scan_node_batch(
            ctx.read(),
            scan_table.table,
            &state.projected_columns[state.table_idx],
            state.offset,
            offset_count,
        );
        state.offset += offset_count as u64;
        let size = batch.size();
        if size == 0 {
            continue;
        }

        let mut output = DataChunk::new(&ctx.layout.col_types);
        let mut source_columns = batch.columns.into_iter();
        output.columns[state.scan.node.id_col] = source_columns
            .next()
            .expect("node batch includes its internal id");
        for (property, source) in scan_table.prop_cols.iter().zip(source_columns) {
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

        let table_base = result.table_bases[state.table_idx];
        let (columns_before_core, core_and_after) =
            output.columns.split_at_mut(state.scan.core_col);
        let ids = match &columns_before_core[state.scan.node.id_col].data {
            ColumnData::InternalId(ids) => ids,
            _ => unreachable!("k-core node id column is INTERNAL_ID"),
        };
        let core_column = &mut core_and_after[0];
        for (row, id) in ids.iter().copied().enumerate().take(size) {
            let offset = usize::try_from(id.offset.0)
                .map_err(|_| Error::runtime("Graph algorithm vertex offset overflow."))?;
            let vertex = table_base
                .checked_add(offset)
                .ok_or_else(|| Error::runtime("Graph algorithm vertex address overflow."))?;
            let core = result.cores.core(vertex).ok_or_else(|| {
                Error::runtime("K-core result omitted a visible selected vertex.")
            })?;
            core_column.set_value_owned(
                row,
                Value::Int64(i64::try_from(core).map_err(|_| {
                    Error::runtime("K-core number exceeds the INT64 result range.")
                })?),
            );
        }
        output.set_flat(size);
        return Ok(Some(output));
    }
}
struct StorageKCoreGraph<'a> {
    selection: &'a BoundGraphSelection,
    storage: &'a InMemStorage,
    read: StorageReadHandle,
    table_bases: &'a [usize],
    active: Option<&'a [bool]>,
    rel_all_visible: &'a [bool],
}

impl StorageKCoreGraph<'_> {
    fn dense_vertex(&self, domain: u32, offset: u64) -> Result<usize> {
        let domain = domain as usize;
        let offset = usize::try_from(offset)
            .map_err(|_| Error::runtime("Graph algorithm vertex offset overflow."))?;
        self.table_bases[domain]
            .checked_add(offset)
            .filter(|&vertex| vertex < self.table_bases[domain + 1])
            .ok_or_else(|| Error::runtime("Graph algorithm edge endpoint is out of bounds."))
    }

    fn active(&self, vertex: usize) -> bool {
        match self.active {
            Some(active) => active[vertex],
            None => true,
        }
    }

    fn vertex_domain(&self, vertex: usize) -> Option<(usize, u64)> {
        if vertex >= self.vertex_count() {
            return None;
        }
        let domain = self
            .table_bases
            .partition_point(|&base| base <= vertex)
            .checked_sub(1)?;
        let offset = vertex.checked_sub(self.table_bases[domain])?;
        Some((domain, u64::try_from(offset).ok()?))
    }
}

impl KCoreGraph for StorageKCoreGraph<'_> {
    fn vertex_count(&self) -> usize {
        self.table_bases.last().copied().unwrap_or_default()
    }

    fn is_vertex_active(&self, vertex: usize) -> bool {
        vertex < self.vertex_count() && self.active(vertex)
    }

    fn for_each_edge(&self, mut visit: impl FnMut(usize, usize) -> Result<()>) -> Result<()> {
        for (index, rel) in self.selection.rel_tables.iter().enumerate() {
            let mut visit_edge = |_: u64, source_offset: u64, destination_offset: u64| {
                let source = self.dense_vertex(rel.source_domain, source_offset)?;
                let destination = self.dense_vertex(rel.destination_domain, destination_offset)?;
                if self.active(source) && self.active(destination) {
                    visit(source, destination)?;
                }
                Ok(())
            };
            if self.rel_all_visible[index] {
                self.storage.visit_rel_endpoints_all_visible(
                    self.read,
                    rel.table,
                    &mut visit_edge,
                )?;
            } else {
                self.storage
                    .visit_rel_endpoints(self.read, rel.table, &mut visit_edge)?;
            }
        }
        Ok(())
    }

    fn for_each_neighbor(
        &self,
        vertex: usize,
        mut visit: impl FnMut(usize) -> Result<()>,
    ) -> Result<()> {
        let Some((vertex_domain, vertex_offset)) = self.vertex_domain(vertex) else {
            return Err(Error::runtime(
                "K-core traversal referenced an unknown vertex.",
            ));
        };
        let vertex_id = InternalId::new(self.selection.node_tables[vertex_domain], vertex_offset);
        for (index, rel) in self.selection.rel_tables.iter().enumerate() {
            if rel.source_domain as usize == vertex_domain {
                let mut visit_neighbor = |_: u64, destination_offset: u64| {
                    let destination =
                        self.dense_vertex(rel.destination_domain, destination_offset)?;
                    if self.active(destination) {
                        visit(destination)?;
                    }
                    Ok(())
                };
                if self.rel_all_visible[index] {
                    self.storage.visit_neighbors_all_visible(
                        self.read,
                        rel.table,
                        vertex_id,
                        EdgeDir::Fwd,
                        &mut visit_neighbor,
                    )?;
                } else {
                    self.storage.visit_neighbors(
                        self.read,
                        rel.table,
                        vertex_id,
                        EdgeDir::Fwd,
                        &mut visit_neighbor,
                    )?;
                }
            }
            if rel.destination_domain as usize == vertex_domain {
                let mut visit_neighbor = |_: u64, source_offset: u64| {
                    let source = self.dense_vertex(rel.source_domain, source_offset)?;
                    if self.active(source) {
                        visit(source)?;
                    }
                    Ok(())
                };
                if self.rel_all_visible[index] {
                    self.storage.visit_neighbors_all_visible(
                        self.read,
                        rel.table,
                        vertex_id,
                        EdgeDir::Bwd,
                        &mut visit_neighbor,
                    )?;
                } else {
                    self.storage.visit_neighbors(
                        self.read,
                        rel.table,
                        vertex_id,
                        EdgeDir::Bwd,
                        &mut visit_neighbor,
                    )?;
                }
            }
        }
        Ok(())
    }
}
fn compute_k_core_decomposition(
    scan: &KCorePlan,
    ctx: &OperatorContext<'_>,
) -> Result<KCoreScanResult> {
    let base_bytes = allocation_bytes::<usize>(scan.graph.node_tables.len().saturating_add(1))?;
    let mapping_memory = ctx.memory.temporary_reservation(base_bytes)?;
    let mut table_bases: Vec<usize> =
        Vec::with_capacity(scan.graph.node_tables.len().saturating_add(1));
    table_bases.push(0_usize);
    for &table in &scan.graph.node_tables {
        let width = usize::try_from(ctx.storage.node_count(table))
            .map_err(|_| Error::runtime("Graph algorithm node-table width overflow."))?;
        let next = table_bases
            .last()
            .copied()
            .unwrap_or_default()
            .checked_add(width)
            .ok_or_else(|| Error::runtime("Graph algorithm vertex address overflow."))?;
        table_bases.push(next);
    }
    let vertex_count = table_bases.last().copied().unwrap_or_default();

    let all_nodes_visible = scan
        .graph
        .node_tables
        .iter()
        .all(|&table| ctx.storage.node_rows_all_visible(ctx.read(), table));
    let active_bytes = if all_nodes_visible {
        0
    } else {
        bitset_bytes(vertex_count)?
    };
    let _active_memory = ctx.memory.temporary_reservation(active_bytes)?;
    let mut active = (!all_nodes_visible).then(|| vec![false; vertex_count]);
    if let Some(active) = &mut active {
        let mut visited_nodes = 0_usize;
        for (domain, &table) in scan.graph.node_tables.iter().enumerate() {
            ctx.control.check()?;
            let base = table_bases[domain];
            ctx.storage
                .visit_node_offsets(ctx.read(), table, |offset| {
                    let offset = usize::try_from(offset)
                        .map_err(|_| Error::runtime("Graph algorithm vertex offset overflow."))?;
                    let vertex = base.checked_add(offset).ok_or_else(|| {
                        Error::runtime("Graph algorithm vertex address overflow.")
                    })?;
                    active[vertex] = true;
                    visited_nodes += 1;
                    if visited_nodes % VECTOR_CAPACITY == 0 {
                        ctx.control.check()?;
                    }
                    Ok(())
                })?;
        }
        ctx.control.check()?;
    }

    let rel_visibility_bytes = bitset_bytes(scan.graph.rel_tables.len())?;
    let _rel_visibility_memory = ctx.memory.temporary_reservation(rel_visibility_bytes)?;
    let mut rel_all_visible = Vec::with_capacity(scan.graph.rel_tables.len());
    for rel in &scan.graph.rel_tables {
        ctx.control.check()?;
        rel_all_visible.push(ctx.visibility.rel_rows_all_visible(
            ctx.storage,
            ctx.read(),
            rel.table,
        ));
    }

    let graph = StorageKCoreGraph {
        selection: &scan.graph,
        storage: ctx.storage,
        read: ctx.read(),
        table_bases: &table_bases,
        active: active.as_deref(),
        rel_all_visible: &rel_all_visible,
    };
    let cores = k_core_decomposition(&graph, ctx.memory.tracker(), || ctx.control.check())?;
    for (vertex, &core) in cores.cores().iter().enumerate() {
        if core != UNASSIGNED_CORE && i64::try_from(core).is_err() {
            return Err(Error::runtime(
                "K-core number exceeds the INT64 result range.",
            ));
        }
        if (vertex + 1) % VECTOR_CAPACITY == 0 {
            ctx.control.check()?;
        }
    }
    ctx.control.check()?;
    Ok(KCoreScanResult {
        cores,
        table_bases,
        _mapping_memory: mapping_memory,
    })
}
impl PageRankGraph for StorageTopologicalGraph<'_> {
    fn vertex_count(&self) -> usize {
        <Self as TopologicalGraph>::vertex_count(self)
    }

    fn is_vertex_active(&self, vertex: usize) -> bool {
        <Self as TopologicalGraph>::is_vertex_active(self, vertex)
    }

    fn for_each_edge(&self, visit: impl FnMut(usize, usize) -> Result<()>) -> Result<()> {
        <Self as TopologicalGraph>::for_each_edge(self, visit)
    }
}
fn allocation_bytes<T>(len: usize) -> Result<u64> {
    u64::try_from(len)
        .ok()
        .and_then(|len| len.checked_mul(size_of::<T>() as u64))
        .ok_or_else(Error::buffer_manager)
}

fn bitset_bytes(bits: usize) -> Result<u64> {
    let bits_per_word = usize::BITS as usize;
    let words = bits
        .checked_add(bits_per_word - 1)
        .ok_or_else(Error::buffer_manager)?
        / bits_per_word;
    allocation_bytes::<usize>(words)
}
