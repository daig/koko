use koko_common::{Error, InternalId, MemoryReservation, MemoryTracker, Result, VECTOR_CAPACITY};
use std::mem::size_of;

/// Sentinel stored for physical slots that are outside the selected graph.
pub const UNASSIGNED_COMPONENT: u64 = u64::MAX;

/// Narrow, statically dispatched input contract for directed SCC decomposition.
///
/// Neighbor cursors let the iterative DFS suspend and resume a base-adjacency
/// scan without copying edges or retaining one stack entry per relationship.
pub trait StronglyConnectedGraph {
    type NeighborCursor: Default;

    /// Width of the dense address space. Inactive holes are allowed.
    fn vertex_count(&self) -> usize;

    /// Whether one dense slot belongs to the selected graph.
    fn is_vertex_active(&self, vertex: usize) -> bool;

    /// Return the next selected outgoing neighbor and advance `cursor`.
    fn next_out_neighbor(
        &self,
        source: usize,
        cursor: &mut Self::NeighborCursor,
    ) -> Result<Option<usize>>;

    /// Return the next selected incoming neighbor and advance `cursor`.
    fn next_in_neighbor(
        &self,
        destination: usize,
        cursor: &mut Self::NeighborCursor,
    ) -> Result<Option<usize>>;

    /// Visit active vertices once in ascending [`InternalId`] order.
    fn for_each_vertex_by_internal_id(
        &self,
        visit: impl FnMut(usize, InternalId) -> Result<()>,
    ) -> Result<()>;
}

/// Immutable compact component IDs over one dense address space.
#[derive(Debug)]
pub struct StronglyConnectedComponents {
    component_ids: Vec<u64>,
    active_count: usize,
    component_count: usize,
    _memory: MemoryReservation,
}

impl StronglyConnectedComponents {
    /// Compact component ID for a selected vertex, or `None` for an inactive/out-of-range slot.
    pub fn component_id(&self, vertex: usize) -> Option<u64> {
        self.component_ids
            .get(vertex)
            .copied()
            .filter(|&component| component != UNASSIGNED_COMPONENT)
    }

    /// Dense component IDs; inactive slots contain [`UNASSIGNED_COMPONENT`].
    pub fn component_ids(&self) -> &[u64] {
        &self.component_ids
    }

    /// Number of selected vertices represented by this result.
    pub const fn active_count(&self) -> usize {
        self.active_count
    }

    /// Number of strongly connected components in this result.
    pub const fn component_count(&self) -> usize {
        self.component_count
    }
}

/// Compute directed strongly connected components with iterative Kosaraju DFS.
///
/// The first pass records forward DFS finish order through resumable adjacency
/// cursors. The second pass labels reverse-reachable vertices. A final identity-
/// ordered pass remaps discovery labels to compact IDs ordered by each
/// component's minimum original [`InternalId`].
pub fn strongly_connected_components<G, C>(
    graph: &G,
    memory: &MemoryTracker,
    mut check_cancel: C,
) -> Result<StronglyConnectedComponents>
where
    G: StronglyConnectedGraph,
    C: FnMut() -> Result<()>,
{
    let vertex_count = graph.vertex_count();
    if u32::try_from(vertex_count).is_ok() {
        compute::<u32, _, _>(graph, memory, &mut check_cancel)
    } else {
        compute::<u64, _, _>(graph, memory, &mut check_cancel)
    }
}

trait DenseId: Copy {
    fn from_index(index: usize) -> Self;
    fn index(self) -> usize;
}

impl DenseId for u32 {
    fn from_index(index: usize) -> Self {
        index as Self
    }

    fn index(self) -> usize {
        self as usize
    }
}

impl DenseId for u64 {
    fn from_index(index: usize) -> Self {
        index as Self
    }

    fn index(self) -> usize {
        self as usize
    }
}

struct DfsFrame<I, C> {
    vertex: I,
    cursor: C,
}

impl<I, C: Default> DfsFrame<I, C> {
    fn new(vertex: I) -> Self {
        Self {
            vertex,
            cursor: C::default(),
        }
    }
}

const UNVISITED: u8 = 0;
const DISCOVERED: u8 = 1;
const FINISHED: u8 = 2;

fn compute<I, G, C>(
    graph: &G,
    memory: &MemoryTracker,
    check_cancel: &mut C,
) -> Result<StronglyConnectedComponents>
where
    I: DenseId,
    G: StronglyConnectedGraph,
    C: FnMut() -> Result<()>,
{
    let vertex_count = graph.vertex_count();
    let state_bytes = allocation_bytes::<u8>(vertex_count)?;
    let order_bytes = allocation_bytes::<I>(vertex_count)?;
    let frame_bytes = allocation_bytes::<DfsFrame<I, G::NeighborCursor>>(vertex_count)?;
    let result_bytes = allocation_bytes::<u64>(vertex_count)?;
    let temporary_bytes = state_bytes
        .checked_add(order_bytes)
        .and_then(|bytes| bytes.checked_add(frame_bytes))
        .ok_or_else(Error::buffer_manager)?;

    // Admit all fixed-size allocations before constructing them. Temporary DFS
    // state is released before the immutable component result leaves the kernel.
    let mut temporary_memory = memory.try_reserve(temporary_bytes)?;
    let result_memory = memory.try_reserve(result_bytes)?;
    let mut state = vec![UNVISITED; vertex_count];
    let mut finish_order = Vec::<I>::with_capacity(vertex_count);
    let mut frames = Vec::<DfsFrame<I, G::NeighborCursor>>::with_capacity(vertex_count);
    let mut component_ids = vec![UNASSIGNED_COMPONENT; vertex_count];

    check_cancel()?;
    let mut active_count = 0_usize;
    let mut traversed_edges = 0_usize;
    let mut finished_vertices = 0_usize;

    for root in 0..vertex_count {
        if !graph.is_vertex_active(root) {
            if (root + 1) % VECTOR_CAPACITY == 0 {
                check_cancel()?;
            }
            continue;
        }
        active_count += 1;
        if state[root] != UNVISITED {
            if (root + 1) % VECTOR_CAPACITY == 0 {
                check_cancel()?;
            }
            continue;
        }

        check_cancel()?;
        state[root] = DISCOVERED;
        frames.push(DfsFrame::new(I::from_index(root)));
        while !frames.is_empty() {
            let next = {
                let frame = frames.last_mut().expect("DFS frame stack is non-empty");
                graph.next_out_neighbor(frame.vertex.index(), &mut frame.cursor)?
            };
            if let Some(destination) = next {
                validate_vertex(graph, vertex_count, destination)?;
                traversed_edges += 1;
                if traversed_edges % VECTOR_CAPACITY == 0 {
                    check_cancel()?;
                }
                if state[destination] == UNVISITED {
                    state[destination] = DISCOVERED;
                    frames.push(DfsFrame::new(I::from_index(destination)));
                }
                continue;
            }

            let source = frames
                .pop()
                .expect("DFS frame stack is non-empty")
                .vertex
                .index();
            if state[source] != DISCOVERED {
                return Err(Error::runtime(
                    "Strongly connected components encountered inconsistent DFS state.",
                ));
            }
            state[source] = FINISHED;
            finish_order.push(I::from_index(source));
            finished_vertices += 1;
            if finished_vertices % VECTOR_CAPACITY == 0 {
                check_cancel()?;
            }
        }
    }
    check_cancel()?;

    if finished_vertices != active_count {
        return Err(Error::runtime(
            "Strongly connected components omitted a selected vertex during forward traversal.",
        ));
    }

    drop(state);
    temporary_memory.resize(
        order_bytes
            .checked_add(frame_bytes)
            .ok_or_else(Error::buffer_manager)?,
    )?;

    let mut component_count = 0_usize;
    let mut reverse_edges = 0_usize;
    let mut assigned_vertices = 0_usize;
    while let Some(root) = finish_order.pop() {
        let root = root.index();
        if component_ids[root] != UNASSIGNED_COMPONENT {
            continue;
        }
        check_cancel()?;
        let component = u64::try_from(component_count)
            .map_err(|_| Error::runtime("Strongly connected component count overflow."))?;
        component_count = component_count
            .checked_add(1)
            .ok_or_else(|| Error::runtime("Strongly connected component count overflow."))?;
        component_ids[root] = component;
        assigned_vertices += 1;
        frames.push(DfsFrame::new(I::from_index(root)));

        while let Some(frame) = frames.pop() {
            let destination = frame.vertex.index();
            let mut cursor = G::NeighborCursor::default();
            while let Some(source) = graph.next_in_neighbor(destination, &mut cursor)? {
                validate_vertex(graph, vertex_count, source)?;
                reverse_edges += 1;
                if reverse_edges % VECTOR_CAPACITY == 0 {
                    check_cancel()?;
                }
                if component_ids[source] == UNASSIGNED_COMPONENT {
                    component_ids[source] = component;
                    assigned_vertices += 1;
                    frames.push(DfsFrame::new(I::from_index(source)));
                }
            }
            if assigned_vertices % VECTOR_CAPACITY == 0 {
                check_cancel()?;
            }
        }
    }
    check_cancel()?;

    if assigned_vertices != active_count {
        return Err(Error::runtime(
            "Strongly connected components omitted a selected vertex during reverse traversal.",
        ));
    }

    let remap_bytes = allocation_bytes::<u64>(component_count)?;
    let retained_temporary_bytes = order_bytes
        .checked_add(frame_bytes)
        .and_then(|bytes| bytes.checked_add(remap_bytes))
        .ok_or_else(Error::buffer_manager)?;
    temporary_memory.resize(retained_temporary_bytes)?;
    let mut remap = vec![UNASSIGNED_COMPONENT; component_count];
    let mut canonical_count = 0_usize;
    let mut next_component = 0_u64;
    let mut previous_id = None;
    graph.for_each_vertex_by_internal_id(|vertex, id| {
        validate_vertex(graph, vertex_count, vertex)?;
        if previous_id.is_some_and(|previous| previous >= id) {
            return Err(Error::runtime(
                "Strongly connected components received non-canonical vertex identity order.",
            ));
        }
        previous_id = Some(id);
        let provisional = component_ids[vertex];
        let provisional = usize::try_from(provisional).map_err(|_| {
            Error::runtime("Strongly connected components produced an invalid provisional ID.")
        })?;
        let mapped = remap.get_mut(provisional).ok_or_else(|| {
            Error::runtime("Strongly connected components produced an invalid provisional ID.")
        })?;
        if *mapped == UNASSIGNED_COMPONENT {
            *mapped = next_component;
            next_component = next_component
                .checked_add(1)
                .ok_or_else(|| Error::runtime("Strongly connected component count overflow."))?;
        }
        canonical_count += 1;
        if canonical_count % VECTOR_CAPACITY == 0 {
            check_cancel()?;
        }
        Ok(())
    })?;
    check_cancel()?;

    if canonical_count != active_count
        || usize::try_from(next_component).ok() != Some(component_count)
    {
        return Err(Error::runtime(
            "Strongly connected components received an incomplete canonical vertex order.",
        ));
    }

    for (vertex, component) in component_ids.iter_mut().enumerate() {
        if !graph.is_vertex_active(vertex) {
            continue;
        }
        let provisional = usize::try_from(*component).map_err(|_| {
            Error::runtime("Strongly connected components produced an invalid provisional ID.")
        })?;
        *component = remap.get(provisional).copied().ok_or_else(|| {
            Error::runtime("Strongly connected components produced an invalid provisional ID.")
        })?;
        if *component == UNASSIGNED_COMPONENT {
            return Err(Error::runtime(
                "Strongly connected components omitted a canonical component ID.",
            ));
        }
        if (vertex + 1) % VECTOR_CAPACITY == 0 {
            check_cancel()?;
        }
    }
    check_cancel()?;

    drop(remap);
    drop(finish_order);
    drop(frames);
    drop(temporary_memory);
    Ok(StronglyConnectedComponents {
        component_ids,
        active_count,
        component_count,
        _memory: result_memory,
    })
}

fn allocation_bytes<T>(len: usize) -> Result<u64> {
    u64::try_from(len)
        .ok()
        .and_then(|len| len.checked_mul(size_of::<T>() as u64))
        .ok_or_else(Error::buffer_manager)
}

fn validate_vertex<G: StronglyConnectedGraph>(
    graph: &G,
    vertex_count: usize,
    vertex: usize,
) -> Result<()> {
    if vertex >= vertex_count || !graph.is_vertex_active(vertex) {
        return Err(Error::runtime(format!(
            "Strongly connected components input referenced unselected vertex {vertex}."
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use koko_common::TableId;
    use std::cell::Cell;

    struct Graph {
        active: Vec<bool>,
        ids: Vec<InternalId>,
        outgoing: Vec<Vec<usize>>,
        incoming: Vec<Vec<usize>>,
    }

    impl Graph {
        fn all(vertex_count: usize, edges: &[(usize, usize)]) -> Self {
            let ids = (0..vertex_count)
                .map(|offset| InternalId::new(TableId(0), offset as u64))
                .collect();
            Self::with_ids(vec![true; vertex_count], ids, edges)
        }

        fn with_ids(active: Vec<bool>, ids: Vec<InternalId>, edges: &[(usize, usize)]) -> Self {
            let mut outgoing = vec![Vec::new(); active.len()];
            let mut incoming = vec![Vec::new(); active.len()];
            for &(source, destination) in edges {
                outgoing[source].push(destination);
                incoming[destination].push(source);
            }
            Self {
                active,
                ids,
                outgoing,
                incoming,
            }
        }
    }

    impl StronglyConnectedGraph for Graph {
        type NeighborCursor = usize;

        fn vertex_count(&self) -> usize {
            self.active.len()
        }

        fn is_vertex_active(&self, vertex: usize) -> bool {
            self.active.get(vertex).copied().unwrap_or(false)
        }

        fn next_out_neighbor(
            &self,
            source: usize,
            cursor: &mut Self::NeighborCursor,
        ) -> Result<Option<usize>> {
            let neighbor = self.outgoing[source].get(*cursor).copied();
            if neighbor.is_some() {
                *cursor += 1;
            }
            Ok(neighbor)
        }

        fn next_in_neighbor(
            &self,
            destination: usize,
            cursor: &mut Self::NeighborCursor,
        ) -> Result<Option<usize>> {
            let neighbor = self.incoming[destination].get(*cursor).copied();
            if neighbor.is_some() {
                *cursor += 1;
            }
            Ok(neighbor)
        }

        fn for_each_vertex_by_internal_id(
            &self,
            mut visit: impl FnMut(usize, InternalId) -> Result<()>,
        ) -> Result<()> {
            let mut vertices: Vec<_> = self
                .active
                .iter()
                .enumerate()
                .filter(|(_, active)| **active)
                .map(|(vertex, _)| (self.ids[vertex], vertex))
                .collect();
            vertices.sort_unstable();
            for (id, vertex) in vertices {
                visit(vertex, id)?;
            }
            Ok(())
        }
    }

    fn run(graph: &Graph) -> StronglyConnectedComponents {
        strongly_connected_components(graph, &MemoryTracker::default(), || Ok(())).unwrap()
    }

    #[test]
    fn empty_and_isolated_graphs_get_compact_identity_order() {
        let empty = run(&Graph::all(0, &[]));
        assert_eq!(empty.component_ids(), []);
        assert_eq!(empty.active_count(), 0);
        assert_eq!(empty.component_count(), 0);

        let isolated = run(&Graph::all(3, &[]));
        assert_eq!(isolated.component_ids(), [0, 1, 2]);
        assert_eq!(isolated.active_count(), 3);
        assert_eq!(isolated.component_count(), 3);
    }

    #[test]
    fn directed_chain_does_not_collapse_reachability() {
        let result = run(&Graph::all(4, &[(0, 1), (1, 2), (2, 3)]));
        assert_eq!(result.component_ids(), [0, 1, 2, 3]);
    }

    #[test]
    fn cycles_connected_as_a_dag_remain_distinct_components() {
        let edges = [(0, 1), (1, 0), (1, 2), (2, 3), (3, 4), (4, 2), (4, 5)];
        let result = run(&Graph::all(6, &edges));
        assert_eq!(result.component_ids(), [0, 0, 1, 1, 1, 2]);
        assert_eq!(result.component_count(), 3);
    }

    #[test]
    fn self_loops_and_parallel_edges_preserve_directed_membership() {
        let edges = [(0, 0), (0, 1), (0, 1), (1, 2), (2, 1), (2, 2)];
        let result = run(&Graph::all(3, &edges));
        assert_eq!(result.component_ids(), [0, 1, 1]);
    }

    #[test]
    fn inactive_holes_and_caller_dense_order_do_not_define_component_ids() {
        let active = vec![true, true, false, true, true];
        let ids = vec![
            InternalId::new(TableId(9), 0),
            InternalId::new(TableId(2), 1),
            InternalId::new(TableId(2), 0),
            InternalId::new(TableId(2), 2),
            InternalId::new(TableId(9), 1),
        ];
        let result = run(&Graph::with_ids(active, ids, &[(0, 4), (4, 0)]));
        assert_eq!(result.component_ids(), [2, 0, UNASSIGNED_COMPONENT, 1, 2]);
        assert_eq!(result.component_id(2), None);
        assert_eq!(result.active_count(), 4);
        assert_eq!(result.component_count(), 3);
    }

    #[test]
    fn long_chain_is_processed_without_recursion() {
        let vertex_count = 100_000;
        let edges: Vec<_> = (1..vertex_count)
            .map(|destination| (destination - 1, destination))
            .collect();
        let result = run(&Graph::all(vertex_count, &edges));
        assert_eq!(result.component_id(0), Some(0));
        assert_eq!(result.component_id(vertex_count - 1), Some(99_999));
    }

    #[test]
    fn cancellation_interrupts_a_high_degree_adjacency_scan() {
        let edges: Vec<_> = (0..(VECTOR_CAPACITY * 2)).map(|_| (0, 1)).collect();
        let checks = Cell::new(0_usize);
        let error = strongly_connected_components(
            &Graph::all(2, &edges),
            &MemoryTracker::default(),
            || {
                let next = checks.get() + 1;
                checks.set(next);
                if next == 4 {
                    Err(Error::runtime("cancelled by test"))
                } else {
                    Ok(())
                }
            },
        )
        .unwrap_err();
        assert_eq!(error.to_string(), "Runtime exception: cancelled by test");
    }

    #[test]
    fn memory_limit_rejects_state_before_allocation() {
        let tracker = MemoryTracker::new(Some(1));
        let error =
            strongly_connected_components(&Graph::all(1, &[]), &tracker, || Ok(())).unwrap_err();
        assert_eq!(
            error.to_string(),
            "Buffer manager exception: Unable to allocate memory! The buffer pool is full and no memory could be freed!"
        );
        assert_eq!(tracker.usage().current, 0);
    }

    #[test]
    fn retained_result_releases_temporary_state() {
        let tracker = MemoryTracker::default();
        let result =
            strongly_connected_components(&Graph::all(3, &[(0, 1), (1, 0)]), &tracker, || Ok(()))
                .unwrap();
        assert_eq!(tracker.usage().current, 3 * size_of::<u64>() as u64);
        drop(result);
        assert_eq!(tracker.usage().current, 0);
    }
}
