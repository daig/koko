use super::*;
use koko_algorithm::{LouvainCommunities, LouvainGraph, louvain};
use koko_ir::bound::BoundGraphSelection;
use koko_ir::plan::LouvainPlan;
use std::mem::size_of;
use std::sync::Arc;

pub(crate) struct LouvainScanResult {
    communities: LouvainCommunities,
    /// One base per caller-selected node table plus the address-space width.
    table_bases: Vec<usize>,
    _mapping_memory: MemoryReservation,
}

pub(crate) struct LouvainState<'a> {
    pub(crate) scan: &'a LouvainPlan,
    pub(crate) result: Option<Arc<LouvainScanResult>>,
    pub(crate) table_idx: usize,
    pub(crate) offset: u64,
    pub(crate) projected_columns: Vec<Vec<usize>>,
}

pub(crate) fn next_louvain_chunk(
    state: &mut LouvainState<'_>,
    ctx: &OperatorContext<'_>,
) -> Result<Option<DataChunk>> {
    if state.result.is_none() {
        state.result = Some(ctx.visibility.graph_algorithms.louvain(state.scan, ctx)?);
    }
    let result = state
        .result
        .as_ref()
        .expect("Louvain result was initialized");

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
        let (columns_before_community, community_and_after) =
            output.columns.split_at_mut(state.scan.community_col);
        let ids = match &columns_before_community[state.scan.node.id_col].data {
            ColumnData::InternalId(ids) => ids,
            _ => unreachable!("Louvain node id column is INTERNAL_ID"),
        };
        let community_column = &mut community_and_after[0];
        for (row, id) in ids.iter().copied().enumerate().take(size) {
            let offset = usize::try_from(id.offset.0)
                .map_err(|_| Error::runtime("Graph algorithm vertex offset overflow."))?;
            let vertex = table_base
                .checked_add(offset)
                .ok_or_else(|| Error::runtime("Graph algorithm vertex address overflow."))?;
            let community = result.communities.community(vertex).ok_or_else(|| {
                Error::runtime("Louvain result omitted a visible selected vertex.")
            })?;
            community_column.set_value_owned(
                row,
                Value::Int64(i64::try_from(community).map_err(|_| {
                    Error::runtime("Louvain community ID exceeds the INT64 result range.")
                })?),
            );
        }
        output.set_flat(size);
        return Ok(Some(output));
    }
}

pub(crate) fn compute_louvain(
    scan: &LouvainPlan,
    ctx: &OperatorContext<'_>,
) -> Result<LouvainScanResult> {
    let base_bytes = allocation_bytes::<usize>(scan.graph.node_tables.len().saturating_add(1))?;
    let mapping_memory = ctx.memory.temporary_reservation(base_bytes)?;
    let mut table_bases = Vec::with_capacity(scan.graph.node_tables.len().saturating_add(1));
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

    let domain_order_bytes = allocation_bytes::<usize>(scan.graph.node_tables.len())?;
    let _domain_order_memory = ctx.memory.temporary_reservation(domain_order_bytes)?;
    let mut internal_id_domains: Vec<usize> = (0..scan.graph.node_tables.len()).collect();
    internal_id_domains.sort_unstable_by_key(|&domain| scan.graph.node_tables[domain]);

    let graph = StorageLouvainGraph {
        selection: &scan.graph,
        storage: ctx.storage,
        read: ctx.read(),
        table_bases: &table_bases,
        active: active.as_deref(),
        rel_all_visible: &rel_all_visible,
        internal_id_domains: &internal_id_domains,
    };
    let communities = louvain(
        &graph,
        ctx.memory.tracker(),
        scan.max_iterations,
        scan.max_phases,
        || ctx.control.check(),
    )?;
    Ok(LouvainScanResult {
        communities,
        table_bases,
        _mapping_memory: mapping_memory,
    })
}

struct StorageLouvainGraph<'a> {
    selection: &'a BoundGraphSelection,
    storage: &'a InMemStorage,
    read: StorageReadHandle,
    table_bases: &'a [usize],
    active: Option<&'a [bool]>,
    rel_all_visible: &'a [bool],
    internal_id_domains: &'a [usize],
}

impl StorageLouvainGraph<'_> {
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
}

impl LouvainGraph for StorageLouvainGraph<'_> {
    fn vertex_count(&self) -> usize {
        self.table_bases.last().copied().unwrap_or_default()
    }

    fn is_vertex_active(&self, vertex: usize) -> bool {
        vertex < self.vertex_count() && self.active(vertex)
    }

    fn for_each_vertex_in_internal_id_order(
        &self,
        mut visit: impl FnMut(usize) -> Result<()>,
    ) -> Result<()> {
        for &domain in self.internal_id_domains {
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
