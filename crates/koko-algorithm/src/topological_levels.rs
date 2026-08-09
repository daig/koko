use koko_common::{Error, MemoryReservation, MemoryTracker, Result, VECTOR_CAPACITY};
use std::mem::size_of;

/// Sentinel stored for physical slots that are outside the selected graph.
pub const UNRANKED_LEVEL: u64 = u64::MAX;

/// Narrow, statically dispatched input contract for topological leveling.
///
/// Implementations expose only selected directed topology. Every edge yielded by
/// [`for_each_edge`](Self::for_each_edge) must be yielded exactly once by the
/// matching source's [`for_each_out_neighbor`](Self::for_each_out_neighbor).
pub trait TopologicalGraph {
    /// Width of the dense address space. Inactive holes are allowed.
    fn vertex_count(&self) -> usize;

    /// Whether one dense slot belongs to the selected graph.
    fn is_vertex_active(&self, vertex: usize) -> bool;

    /// Visit every selected directed edge once.
    fn for_each_edge(&self, visit: impl FnMut(usize, usize) -> Result<()>) -> Result<()>;

    /// Visit every selected outgoing edge from `source` once.
    fn for_each_out_neighbor(
        &self,
        source: usize,
        visit: impl FnMut(usize) -> Result<()>,
    ) -> Result<()>;
}

/// Immutable topological levels over one dense address space.
#[derive(Debug)]
pub struct TopologicalLevels {
    levels: Vec<u64>,
    active_count: usize,
    _memory: MemoryReservation,
}

impl TopologicalLevels {
    /// Level for a selected vertex, or `None` for an inactive/out-of-range slot.
    pub fn level(&self, vertex: usize) -> Option<u64> {
        self.levels
            .get(vertex)
            .copied()
            .filter(|&level| level != UNRANKED_LEVEL)
    }

    /// Dense levels; inactive slots contain [`UNRANKED_LEVEL`].
    pub fn levels(&self) -> &[u64] {
        &self.levels
    }

    /// Number of selected vertices represented by this result.
    pub const fn active_count(&self) -> usize {
        self.active_count
    }
}

/// Compute deterministic Kahn layers for a directed selected graph.
///
/// Sources and isolated vertices receive level 0. Removing a frontier exposes
/// the next level. A cycle is reported only after all rankable vertices have
/// been consumed, so the diagnostic remains accurate for cyclic graphs with an
/// acyclic tail.
pub fn topological_levels<G, C>(
    graph: &G,
    memory: &MemoryTracker,
    mut check_cancel: C,
) -> Result<TopologicalLevels>
where
    G: TopologicalGraph,
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

fn compute<I, G, C>(
    graph: &G,
    memory: &MemoryTracker,
    check_cancel: &mut C,
) -> Result<TopologicalLevels>
where
    I: DenseId,
    G: TopologicalGraph,
    C: FnMut() -> Result<()>,
{
    let vertex_count = graph.vertex_count();
    let indegree_bytes = allocation_bytes::<u64>(vertex_count)?;
    let queue_bytes = allocation_bytes::<I>(vertex_count)?;
    let level_bytes = allocation_bytes::<u64>(vertex_count)?;
    let state_bytes = indegree_bytes
        .checked_add(queue_bytes)
        .ok_or_else(Error::buffer_manager)?;

    // Admission precedes every allocation. Temporary ranking state and retained
    // output own separate reservations so the former can be released promptly.
    let state_memory = memory.try_reserve(state_bytes)?;
    let result_memory = memory.try_reserve(level_bytes)?;
    let mut indegrees = vec![0_u64; vertex_count];
    let mut queue = Vec::<I>::with_capacity(vertex_count);
    let mut levels = vec![UNRANKED_LEVEL; vertex_count];

    check_cancel()?;
    let mut edge_count = 0_usize;
    graph.for_each_edge(|source, destination| {
        validate_edge(graph, vertex_count, source, destination)?;
        indegrees[destination] = indegrees[destination]
            .checked_add(1)
            .ok_or_else(|| Error::runtime("Topological leveling indegree overflow."))?;
        edge_count += 1;
        if edge_count % VECTOR_CAPACITY == 0 {
            check_cancel()?;
        }
        Ok(())
    })?;
    check_cancel()?;

    let mut active_count = 0_usize;
    for (vertex, &indegree) in indegrees.iter().enumerate() {
        if graph.is_vertex_active(vertex) {
            active_count += 1;
            if indegree == 0 {
                queue.push(I::from_index(vertex));
            }
        }
        if (vertex + 1) % VECTOR_CAPACITY == 0 {
            check_cancel()?;
        }
    }

    let mut cursor = 0_usize;
    let mut layer_end = queue.len();
    let mut current_level = 0_u64;
    let mut processed_count = 0_usize;
    let mut traversed_edges = 0_usize;

    while cursor < queue.len() {
        if cursor == layer_end {
            check_cancel()?;
            layer_end = queue.len();
            current_level = current_level
                .checked_add(1)
                .ok_or_else(|| Error::runtime("Topological leveling level overflow."))?;
        }

        let source = queue[cursor].index();
        cursor += 1;
        levels[source] = current_level;
        processed_count += 1;

        graph.for_each_out_neighbor(source, |destination| {
            validate_vertex(graph, vertex_count, destination)?;
            indegrees[destination] = indegrees[destination].checked_sub(1).ok_or_else(|| {
                Error::runtime(
                    "Topological input enumerated an outgoing edge absent from its edge scan.",
                )
            })?;
            if indegrees[destination] == 0 {
                queue.push(I::from_index(destination));
            }
            traversed_edges += 1;
            if traversed_edges % VECTOR_CAPACITY == 0 {
                check_cancel()?;
            }
            Ok(())
        })?;

        if processed_count % VECTOR_CAPACITY == 0 {
            check_cancel()?;
        }
    }

    if processed_count != active_count {
        return Err(Error::runtime(format!(
            "Topological leveling requires an acyclic graph; {processed_count} of {active_count} selected vertices were ranked."
        )));
    }

    drop(indegrees);
    drop(queue);
    drop(state_memory);
    Ok(TopologicalLevels {
        levels,
        active_count,
        _memory: result_memory,
    })
}

fn allocation_bytes<T>(len: usize) -> Result<u64> {
    u64::try_from(len)
        .ok()
        .and_then(|len| len.checked_mul(size_of::<T>() as u64))
        .ok_or_else(Error::buffer_manager)
}

fn validate_edge<G: TopologicalGraph>(
    graph: &G,
    vertex_count: usize,
    source: usize,
    destination: usize,
) -> Result<()> {
    validate_vertex(graph, vertex_count, source)?;
    validate_vertex(graph, vertex_count, destination)
}

fn validate_vertex<G: TopologicalGraph>(
    graph: &G,
    vertex_count: usize,
    vertex: usize,
) -> Result<()> {
    if vertex >= vertex_count || !graph.is_vertex_active(vertex) {
        return Err(Error::runtime(format!(
            "Topological input referenced unselected vertex {vertex}."
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    struct Graph {
        active: Vec<bool>,
        outgoing: Vec<Vec<usize>>,
    }

    impl Graph {
        fn all(vertex_count: usize, edges: &[(usize, usize)]) -> Self {
            let mut outgoing = vec![Vec::new(); vertex_count];
            for &(source, destination) in edges {
                outgoing[source].push(destination);
            }
            Self {
                active: vec![true; vertex_count],
                outgoing,
            }
        }
    }

    impl TopologicalGraph for Graph {
        fn vertex_count(&self) -> usize {
            self.active.len()
        }

        fn is_vertex_active(&self, vertex: usize) -> bool {
            self.active.get(vertex).copied().unwrap_or(false)
        }

        fn for_each_edge(&self, mut visit: impl FnMut(usize, usize) -> Result<()>) -> Result<()> {
            for (source, neighbors) in self.outgoing.iter().enumerate() {
                if !self.active[source] {
                    continue;
                }
                for &destination in neighbors {
                    visit(source, destination)?;
                }
            }
            Ok(())
        }

        fn for_each_out_neighbor(
            &self,
            source: usize,
            mut visit: impl FnMut(usize) -> Result<()>,
        ) -> Result<()> {
            for &destination in &self.outgoing[source] {
                visit(destination)?;
            }
            Ok(())
        }
    }

    fn run(graph: &Graph) -> TopologicalLevels {
        topological_levels(graph, &MemoryTracker::default(), || Ok(())).unwrap()
    }

    #[test]
    fn empty_and_isolated_graphs() {
        let empty = run(&Graph::all(0, &[]));
        assert_eq!(empty.levels(), []);
        assert_eq!(empty.active_count(), 0);

        let isolated = run(&Graph::all(3, &[]));
        assert_eq!(isolated.levels(), [0, 0, 0]);
        assert_eq!(isolated.active_count(), 3);
    }

    #[test]
    fn chain_and_diamond_levels() {
        let chain = run(&Graph::all(4, &[(0, 1), (1, 2), (2, 3)]));
        assert_eq!(chain.levels(), [0, 1, 2, 3]);

        let diamond = run(&Graph::all(4, &[(0, 1), (0, 2), (1, 3), (2, 3)]));
        assert_eq!(diamond.levels(), [0, 1, 1, 2]);
    }

    #[test]
    fn disconnected_graph_and_inactive_hole() {
        let graph = Graph {
            active: vec![true, true, false, true, true, true],
            outgoing: vec![vec![1], vec![], vec![], vec![4], vec![5], vec![]],
        };
        let result = run(&graph);
        assert_eq!(result.levels(), [0, 1, UNRANKED_LEVEL, 0, 1, 2]);
        assert_eq!(result.level(2), None);
        assert_eq!(result.active_count(), 5);
    }

    #[test]
    fn parallel_edges_are_counted_independently() {
        let result = run(&Graph::all(2, &[(0, 1), (0, 1)]));
        assert_eq!(result.levels(), [0, 1]);
    }

    #[test]
    fn self_loop_and_multi_vertex_cycle_fail() {
        let self_loop =
            topological_levels(&Graph::all(1, &[(0, 0)]), &MemoryTracker::default(), || {
                Ok(())
            })
            .unwrap_err();
        assert_eq!(
            self_loop.to_string(),
            "Runtime exception: Topological leveling requires an acyclic graph; 0 of 1 selected vertices were ranked."
        );

        let cycle_with_tail = topological_levels(
            &Graph::all(4, &[(0, 1), (1, 0), (1, 2), (2, 3)]),
            &MemoryTracker::default(),
            || Ok(()),
        )
        .unwrap_err();
        assert_eq!(
            cycle_with_tail.to_string(),
            "Runtime exception: Topological leveling requires an acyclic graph; 0 of 4 selected vertices were ranked."
        );
    }

    #[test]
    fn long_chain_uses_iterative_frontiers() {
        let vertex_count = 100_000;
        let edges: Vec<_> = (1..vertex_count)
            .map(|destination| (destination - 1, destination))
            .collect();
        let result = run(&Graph::all(vertex_count, &edges));
        assert_eq!(result.level(0), Some(0));
        assert_eq!(
            result.level(vertex_count - 1),
            Some((vertex_count - 1) as u64)
        );
    }

    #[test]
    fn cancellation_interrupts_a_high_degree_vertex() {
        let edges: Vec<_> = (0..(VECTOR_CAPACITY * 2)).map(|_| (0, 1)).collect();
        let checks = Cell::new(0_usize);
        let error = topological_levels(&Graph::all(2, &edges), &MemoryTracker::default(), || {
            let next = checks.get() + 1;
            checks.set(next);
            if next == 6 {
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
        let error = topological_levels(&Graph::all(1, &[]), &tracker, || Ok(())).unwrap_err();
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
            topological_levels(&Graph::all(3, &[(0, 1), (1, 2)]), &tracker, || Ok(())).unwrap();
        assert_eq!(tracker.usage().current, 3 * size_of::<u64>() as u64);
        drop(result);
        assert_eq!(tracker.usage().current, 0);
    }
}
