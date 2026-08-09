use koko_common::{Error, InternalId, MemoryReservation, MemoryTracker, Result, VECTOR_CAPACITY};
use std::mem::size_of;

/// Sentinel stored for physical slots that are outside the selected graph.
pub const UNASSIGNED_COMPONENT_ID: i64 = -1;

/// Narrow, statically dispatched input contract for weakly connected components.
///
/// Implementations expose every selected relationship exactly once, in either
/// stored direction. Canonical vertex enumeration must visit every active vertex
/// exactly once in strictly ascending [`InternalId`] order.
pub trait WeaklyConnectedGraph {
    /// Width of the dense address space. Inactive holes are allowed.
    fn vertex_count(&self) -> usize;

    /// Whether one dense slot belongs to the selected graph.
    fn is_vertex_active(&self, vertex: usize) -> bool;

    /// Stable external identity for an active dense vertex.
    fn vertex_id(&self, vertex: usize) -> InternalId;

    /// Visit every active vertex in strictly ascending external-identity order.
    fn for_each_vertex_by_id(&self, visit: impl FnMut(usize) -> Result<()>) -> Result<()>;

    /// Visit every selected relationship once. Direction is ignored by the kernel.
    fn for_each_edge(&self, visit: impl FnMut(usize, usize) -> Result<()>) -> Result<()>;
}

/// Immutable compact component IDs over one dense address space.
#[derive(Debug)]
pub struct WeaklyConnectedComponents {
    component_ids: Vec<i64>,
    active_count: usize,
    component_count: usize,
    _memory: MemoryReservation,
}

impl WeaklyConnectedComponents {
    /// Compact component ID for a selected vertex, or `None` for an inactive or
    /// out-of-range slot.
    pub fn component_id(&self, vertex: usize) -> Option<i64> {
        self.component_ids
            .get(vertex)
            .copied()
            .filter(|&component_id| component_id != UNASSIGNED_COMPONENT_ID)
    }

    /// Dense IDs; inactive slots contain [`UNASSIGNED_COMPONENT_ID`].
    pub fn component_ids(&self) -> &[i64] {
        &self.component_ids
    }

    /// Number of selected vertices represented by this result.
    pub const fn active_count(&self) -> usize {
        self.active_count
    }

    /// Number of weakly connected components represented by this result.
    pub const fn component_count(&self) -> usize {
        self.component_count
    }
}

/// Compute weakly connected components with union-by-rank and path compression.
///
/// Relationship direction is ignored. Self-loops are explicit no-ops and
/// parallel relationships naturally repeat an idempotent union. Components are
/// labeled by the first member encountered in canonical external-identity order,
/// which is their minimum [`InternalId`].
pub fn weakly_connected_components<G, C>(
    graph: &G,
    memory: &MemoryTracker,
    mut check_cancel: C,
) -> Result<WeaklyConnectedComponents>
where
    G: WeaklyConnectedGraph,
    C: FnMut() -> Result<()>,
{
    let vertex_count = graph.vertex_count();
    let parent_memory = memory.try_reserve(allocation_bytes::<usize>(vertex_count)?)?;
    let rank_memory = memory.try_reserve(allocation_bytes::<u8>(vertex_count)?)?;
    let mut parents = vec![usize::MAX; vertex_count];
    let mut ranks = vec![0_u8; vertex_count];

    check_cancel()?;
    let mut active_count = 0_usize;
    for (vertex, parent) in parents.iter_mut().enumerate() {
        if graph.is_vertex_active(vertex) {
            *parent = vertex;
            active_count = active_count.checked_add(1).ok_or_else(|| {
                Error::runtime("Weakly connected components vertex count overflow.")
            })?;
        }
        if (vertex + 1) % VECTOR_CAPACITY == 0 {
            check_cancel()?;
        }
    }
    check_cancel()?;

    let mut edge_count = 0_usize;
    graph.for_each_edge(|source, destination| {
        validate_vertex(graph, vertex_count, source)?;
        validate_vertex(graph, vertex_count, destination)?;
        if source != destination {
            union(&mut parents, &mut ranks, source, destination);
        }
        edge_count = edge_count
            .checked_add(1)
            .ok_or_else(|| Error::runtime("Weakly connected components edge count overflow."))?;
        if edge_count % VECTOR_CAPACITY == 0 {
            check_cancel()?;
        }
        Ok(())
    })?;
    check_cancel()?;

    drop(ranks);
    drop(rank_memory);

    let result_memory = memory.try_reserve(allocation_bytes::<i64>(vertex_count)?)?;
    let mut component_ids = vec![UNASSIGNED_COMPONENT_ID; vertex_count];
    let mut component_count = 0_usize;
    let mut visited_count = 0_usize;
    let mut previous_id = None;
    graph.for_each_vertex_by_id(|vertex| {
        validate_vertex(graph, vertex_count, vertex)?;
        let vertex_id = graph.vertex_id(vertex);
        if let Some(previous_id) = previous_id {
            if vertex_id <= previous_id {
                return Err(Error::runtime(
                    "Weakly connected components vertices are not in canonical identity order.",
                ));
            }
        }
        previous_id = Some(vertex_id);

        let root = find_root(&mut parents, vertex);
        let component_id = if component_ids[root] == UNASSIGNED_COMPONENT_ID {
            let component_id = i64::try_from(component_count).map_err(|_| {
                Error::runtime("Weakly connected components count exceeds the INT64 result range.")
            })?;
            component_count = component_count.checked_add(1).ok_or_else(|| {
                Error::runtime("Weakly connected components component count overflow.")
            })?;
            component_ids[root] = component_id;
            component_id
        } else {
            component_ids[root]
        };
        component_ids[vertex] = component_id;
        visited_count = visited_count
            .checked_add(1)
            .ok_or_else(|| Error::runtime("Weakly connected components vertex count overflow."))?;
        if visited_count % VECTOR_CAPACITY == 0 {
            check_cancel()?;
        }
        Ok(())
    })?;
    check_cancel()?;

    if visited_count != active_count {
        return Err(Error::runtime(format!(
            "Weakly connected components canonical input visited {visited_count} of {active_count} selected vertices."
        )));
    }

    drop(parents);
    drop(parent_memory);
    Ok(WeaklyConnectedComponents {
        component_ids,
        active_count,
        component_count,
        _memory: result_memory,
    })
}

fn find_root(parents: &mut [usize], vertex: usize) -> usize {
    let mut root = vertex;
    while parents[root] != root {
        root = parents[root];
    }

    let mut current = vertex;
    while parents[current] != current {
        let next = parents[current];
        parents[current] = root;
        current = next;
    }
    root
}

fn union(parents: &mut [usize], ranks: &mut [u8], left: usize, right: usize) {
    let mut left_root = find_root(parents, left);
    let mut right_root = find_root(parents, right);
    if left_root == right_root {
        return;
    }
    if ranks[left_root] < ranks[right_root] {
        std::mem::swap(&mut left_root, &mut right_root);
    }
    parents[right_root] = left_root;
    if ranks[left_root] == ranks[right_root] {
        ranks[left_root] += 1;
    }
}

fn validate_vertex<G: WeaklyConnectedGraph>(
    graph: &G,
    vertex_count: usize,
    vertex: usize,
) -> Result<()> {
    if vertex >= vertex_count || !graph.is_vertex_active(vertex) {
        return Err(Error::runtime(format!(
            "Weakly connected components input referenced unselected vertex {vertex}."
        )));
    }
    Ok(())
}

fn allocation_bytes<T>(len: usize) -> Result<u64> {
    u64::try_from(len)
        .ok()
        .and_then(|len| len.checked_mul(size_of::<T>() as u64))
        .ok_or_else(Error::buffer_manager)
}

#[cfg(test)]
mod tests {
    use super::*;
    use koko_common::TableId;
    use std::cell::Cell;

    struct Graph {
        active: Vec<bool>,
        ids: Vec<InternalId>,
        canonical_order: Vec<usize>,
        edges: Vec<(usize, usize)>,
    }

    impl Graph {
        fn new(active: Vec<bool>, ids: Vec<InternalId>, edges: &[(usize, usize)]) -> Self {
            assert_eq!(active.len(), ids.len());
            let mut canonical_order: Vec<_> =
                (0..active.len()).filter(|&vertex| active[vertex]).collect();
            canonical_order.sort_unstable_by_key(|&vertex| ids[vertex]);
            Self {
                active,
                ids,
                canonical_order,
                edges: edges.to_vec(),
            }
        }

        fn all(vertex_count: usize, edges: &[(usize, usize)]) -> Self {
            Self::new(
                vec![true; vertex_count],
                (0..vertex_count)
                    .map(|offset| id(0, offset as u64))
                    .collect(),
                edges,
            )
        }
    }

    impl WeaklyConnectedGraph for Graph {
        fn vertex_count(&self) -> usize {
            self.active.len()
        }

        fn is_vertex_active(&self, vertex: usize) -> bool {
            self.active.get(vertex).copied().unwrap_or(false)
        }

        fn vertex_id(&self, vertex: usize) -> InternalId {
            self.ids[vertex]
        }

        fn for_each_vertex_by_id(&self, mut visit: impl FnMut(usize) -> Result<()>) -> Result<()> {
            for &vertex in &self.canonical_order {
                visit(vertex)?;
            }
            Ok(())
        }

        fn for_each_edge(&self, mut visit: impl FnMut(usize, usize) -> Result<()>) -> Result<()> {
            for &(source, destination) in &self.edges {
                visit(source, destination)?;
            }
            Ok(())
        }
    }

    fn id(table: u64, offset: u64) -> InternalId {
        InternalId::new(TableId(table), offset)
    }

    fn run(graph: &Graph) -> WeaklyConnectedComponents {
        weakly_connected_components(graph, &MemoryTracker::default(), || Ok(())).unwrap()
    }

    #[test]
    fn empty_and_isolated_graphs() {
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
    fn direction_parallel_edges_and_self_loops_do_not_change_membership() {
        let graph = Graph::new(
            vec![true, true, true, true, false],
            vec![id(9, 0), id(4, 1), id(4, 0), id(7, 0), id(2, 0)],
            &[(0, 2), (2, 0), (0, 2), (0, 0), (2, 2), (3, 3)],
        );
        let result = run(&graph);
        assert_eq!(
            result.component_ids(),
            [0, 1, 0, 2, UNASSIGNED_COMPONENT_ID]
        );
        assert_eq!(result.component_id(4), None);
        assert_eq!(result.active_count(), 4);
        assert_eq!(result.component_count(), 3);
    }

    #[test]
    fn reversed_directed_chain_forms_one_weak_component() {
        let result = run(&Graph::all(5, &[(4, 3), (3, 2), (2, 1), (1, 0)]));
        assert_eq!(result.component_ids(), [0, 0, 0, 0, 0]);
        assert_eq!(result.component_count(), 1);
    }

    #[test]
    fn invalid_or_inactive_edge_endpoints_fail() {
        let inactive = weakly_connected_components(
            &Graph::new(vec![true, false], vec![id(0, 0), id(0, 1)], &[(0, 1)]),
            &MemoryTracker::default(),
            || Ok(()),
        )
        .unwrap_err();
        assert_eq!(
            inactive.to_string(),
            "Runtime exception: Weakly connected components input referenced unselected vertex 1."
        );

        let out_of_bounds = weakly_connected_components(
            &Graph::all(1, &[(0, 2)]),
            &MemoryTracker::default(),
            || Ok(()),
        )
        .unwrap_err();
        assert_eq!(
            out_of_bounds.to_string(),
            "Runtime exception: Weakly connected components input referenced unselected vertex 2."
        );
    }

    #[test]
    fn canonical_enumeration_must_cover_every_vertex_in_identity_order() {
        let mut missing = Graph::all(2, &[]);
        missing.canonical_order.pop();
        let error = weakly_connected_components(&missing, &MemoryTracker::default(), || Ok(()))
            .unwrap_err();
        assert_eq!(
            error.to_string(),
            "Runtime exception: Weakly connected components canonical input visited 1 of 2 selected vertices."
        );

        let mut reversed = Graph::all(2, &[]);
        reversed.canonical_order.reverse();
        let error = weakly_connected_components(&reversed, &MemoryTracker::default(), || Ok(()))
            .unwrap_err();
        assert_eq!(
            error.to_string(),
            "Runtime exception: Weakly connected components vertices are not in canonical identity order."
        );
    }

    #[test]
    fn cancellation_interrupts_endpoint_streaming() {
        let edges = vec![(0, 1); VECTOR_CAPACITY * 2];
        let checks = Cell::new(0_usize);
        let error =
            weakly_connected_components(&Graph::all(2, &edges), &MemoryTracker::default(), || {
                let next = checks.get() + 1;
                checks.set(next);
                if next == 4 {
                    Err(Error::runtime("cancelled by test"))
                } else {
                    Ok(())
                }
            })
            .unwrap_err();
        assert_eq!(error.to_string(), "Runtime exception: cancelled by test");
    }

    #[test]
    fn memory_limit_rejects_state_before_allocation() {
        let tracker = MemoryTracker::new(Some(1));
        let error =
            weakly_connected_components(&Graph::all(1, &[]), &tracker, || Ok(())).unwrap_err();
        assert_eq!(
            error.to_string(),
            "Buffer manager exception: Unable to allocate memory! The buffer pool is full and no memory could be freed!"
        );
        assert_eq!(tracker.usage().current, 0);
    }

    #[test]
    fn retained_result_releases_union_find_state() {
        let tracker = MemoryTracker::default();
        let result =
            weakly_connected_components(&Graph::all(3, &[(0, 1)]), &tracker, || Ok(())).unwrap();
        assert_eq!(tracker.usage().current, 3 * size_of::<i64>() as u64);
        drop(result);
        assert_eq!(tracker.usage().current, 0);
    }
}
