use koko_common::{Error, MemoryReservation, MemoryTracker, Result, VECTOR_CAPACITY};
use std::mem::size_of;

/// Sentinel stored for physical slots outside the selected graph.
pub const UNASSIGNED_CORE: u64 = u64::MAX;

/// Narrow, statically dispatched input contract for k-core decomposition.
///
/// [`for_each_edge`](Self::for_each_edge) yields every selected relationship
/// exactly once. [`for_each_neighbor`](Self::for_each_neighbor) exposes the
/// undirected incidence of those relationships: once at each endpoint, so a
/// self-loop is yielded twice and parallel relationships remain distinct.
pub trait KCoreGraph {
    /// Width of the dense address space. Inactive holes are allowed.
    fn vertex_count(&self) -> usize;

    /// Whether one dense slot belongs to the selected graph.
    fn is_vertex_active(&self, vertex: usize) -> bool;

    /// Visit every selected relationship once, preserving both endpoints.
    fn for_each_edge(&self, visit: impl FnMut(usize, usize) -> Result<()>) -> Result<()>;

    /// Visit every undirected adjacency entry incident to `vertex`.
    fn for_each_neighbor(
        &self,
        vertex: usize,
        visit: impl FnMut(usize) -> Result<()>,
    ) -> Result<()>;
}

/// Immutable coreness values over one dense address space.
#[derive(Debug)]
pub struct KCoreDecomposition {
    cores: Vec<u64>,
    active_count: usize,
    _memory: MemoryReservation,
}

impl KCoreDecomposition {
    /// Maximum core number for a selected vertex, or `None` for an inactive slot.
    pub fn core(&self, vertex: usize) -> Option<u64> {
        self.cores
            .get(vertex)
            .copied()
            .filter(|&core| core != UNASSIGNED_CORE)
    }

    /// Dense coreness values; inactive slots contain [`UNASSIGNED_CORE`].
    pub fn cores(&self) -> &[u64] {
        &self.cores
    }

    /// Number of selected vertices represented by this result.
    pub const fn active_count(&self) -> usize {
        self.active_count
    }
}

/// Compute maximum core numbers for an undirected selected multigraph.
///
/// Degrees are initialized in one endpoint pass and then peeled with the
/// Batagelj-Zaversnik degree-bin algorithm. The degree array is mutated into the
/// retained coreness result, while bin ordering and position state are released
/// before returning.
pub fn k_core_decomposition<G, C>(
    graph: &G,
    memory: &MemoryTracker,
    mut check_cancel: C,
) -> Result<KCoreDecomposition>
where
    G: KCoreGraph,
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
) -> Result<KCoreDecomposition>
where
    I: DenseId,
    G: KCoreGraph,
    C: FnMut() -> Result<()>,
{
    let vertex_count = graph.vertex_count();
    let result_bytes = allocation_bytes::<u64>(vertex_count)?;
    let result_memory = memory.try_reserve(result_bytes)?;
    let mut cores = vec![0_u64; vertex_count];

    check_cancel()?;
    let mut active_count = 0_usize;
    for (vertex, core) in cores.iter_mut().enumerate() {
        if graph.is_vertex_active(vertex) {
            active_count += 1;
        } else {
            *core = UNASSIGNED_CORE;
        }
        if (vertex + 1) % VECTOR_CAPACITY == 0 {
            check_cancel()?;
        }
    }
    check_cancel()?;

    let mut edge_count = 0_usize;
    graph.for_each_edge(|source, destination| {
        validate_edge(graph, vertex_count, source, destination)?;
        cores[source] = cores[source]
            .checked_add(1)
            .ok_or_else(|| Error::runtime("K-core degree overflow."))?;
        cores[destination] = cores[destination]
            .checked_add(1)
            .ok_or_else(|| Error::runtime("K-core degree overflow."))?;
        edge_count += 1;
        if edge_count % VECTOR_CAPACITY == 0 {
            check_cancel()?;
        }
        Ok(())
    })?;
    check_cancel()?;

    if active_count == 0 {
        return Ok(KCoreDecomposition {
            cores,
            active_count,
            _memory: result_memory,
        });
    }

    let max_degree = cores
        .iter()
        .copied()
        .filter(|&degree| degree != UNASSIGNED_CORE)
        .max()
        .unwrap_or_default();
    let max_degree = usize::try_from(max_degree)
        .map_err(|_| Error::runtime("K-core degree exceeds the addressable range."))?;
    let bin_count = max_degree
        .checked_add(1)
        .ok_or_else(Error::buffer_manager)?;

    let position_bytes = allocation_bytes::<usize>(vertex_count)?;
    let vertex_bytes = allocation_bytes::<I>(active_count)?;
    let bin_bytes = allocation_bytes::<usize>(bin_count)?;
    let state_bytes = position_bytes
        .checked_add(vertex_bytes)
        .and_then(|bytes| bytes.checked_add(bin_bytes))
        .ok_or_else(Error::buffer_manager)?;
    let state_memory = memory.try_reserve(state_bytes)?;
    let mut positions = vec![0_usize; vertex_count];
    let mut vertices = vec![I::from_index(0); active_count];
    let mut bins = vec![0_usize; bin_count];

    for (vertex, &degree) in cores.iter().enumerate() {
        if degree == UNASSIGNED_CORE {
            continue;
        }
        let degree = usize::try_from(degree)
            .map_err(|_| Error::runtime("K-core degree exceeds the addressable range."))?;
        bins[degree] = bins[degree]
            .checked_add(1)
            .ok_or_else(|| Error::runtime("K-core bin cardinality overflow."))?;
        if (vertex + 1) % VECTOR_CAPACITY == 0 {
            check_cancel()?;
        }
    }

    let mut start = 0_usize;
    for (degree, count) in bins.iter_mut().enumerate() {
        let cardinality = *count;
        *count = start;
        start = start
            .checked_add(cardinality)
            .ok_or_else(|| Error::runtime("K-core bin cardinality overflow."))?;
        if (degree + 1) % VECTOR_CAPACITY == 0 {
            check_cancel()?;
        }
    }

    for (vertex, &degree) in cores.iter().enumerate() {
        if degree == UNASSIGNED_CORE {
            continue;
        }
        let degree = usize::try_from(degree)
            .map_err(|_| Error::runtime("K-core degree exceeds the addressable range."))?;
        let position = bins[degree];
        positions[vertex] = position;
        vertices[position] = I::from_index(vertex);
        bins[degree] = position
            .checked_add(1)
            .ok_or_else(|| Error::runtime("K-core bin position overflow."))?;
        if (vertex + 1) % VECTOR_CAPACITY == 0 {
            check_cancel()?;
        }
    }

    for degree in (1..bin_count).rev() {
        bins[degree] = bins[degree - 1];
    }
    bins[0] = 0;
    check_cancel()?;

    let mut traversed_entries = 0_usize;
    let mut previous_core = None;
    for order in 0..active_count {
        let vertex = vertices[order].index();
        let vertex_core = cores[vertex];
        if previous_core != Some(vertex_core) {
            check_cancel()?;
            previous_core = Some(vertex_core);
        }

        graph.for_each_neighbor(vertex, |neighbor| {
            validate_vertex(graph, vertex_count, neighbor)?;
            if cores[neighbor] > vertex_core {
                let neighbor_degree = usize::try_from(cores[neighbor])
                    .map_err(|_| Error::runtime("K-core degree exceeds the addressable range."))?;
                let neighbor_position = positions[neighbor];
                let bin_position = bins[neighbor_degree];
                let swapped = vertices[bin_position].index();
                if neighbor != swapped {
                    vertices.swap(neighbor_position, bin_position);
                    positions[neighbor] = bin_position;
                    positions[swapped] = neighbor_position;
                }
                bins[neighbor_degree] = bin_position
                    .checked_add(1)
                    .ok_or_else(|| Error::runtime("K-core bin position overflow."))?;
                cores[neighbor] = cores[neighbor]
                    .checked_sub(1)
                    .ok_or_else(|| Error::runtime("K-core degree underflow."))?;
            }
            traversed_entries += 1;
            if traversed_entries % VECTOR_CAPACITY == 0 {
                check_cancel()?;
            }
            Ok(())
        })?;

        if (order + 1) % VECTOR_CAPACITY == 0 {
            check_cancel()?;
        }
    }
    check_cancel()?;

    drop(positions);
    drop(vertices);
    drop(bins);
    drop(state_memory);
    Ok(KCoreDecomposition {
        cores,
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

fn validate_edge<G: KCoreGraph>(
    graph: &G,
    vertex_count: usize,
    source: usize,
    destination: usize,
) -> Result<()> {
    validate_vertex(graph, vertex_count, source)?;
    validate_vertex(graph, vertex_count, destination)
}

fn validate_vertex<G: KCoreGraph>(graph: &G, vertex_count: usize, vertex: usize) -> Result<()> {
    if vertex >= vertex_count || !graph.is_vertex_active(vertex) {
        return Err(Error::runtime(format!(
            "K-core input referenced unselected vertex {vertex}."
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
        edges: Vec<(usize, usize)>,
        neighbors: Vec<Vec<usize>>,
    }

    impl Graph {
        fn all(vertex_count: usize, edges: &[(usize, usize)]) -> Self {
            Self::selected(vec![true; vertex_count], edges)
        }

        fn selected(active: Vec<bool>, edges: &[(usize, usize)]) -> Self {
            let mut neighbors = vec![Vec::new(); active.len()];
            for &(source, destination) in edges {
                if source < neighbors.len() && destination < neighbors.len() {
                    neighbors[source].push(destination);
                    neighbors[destination].push(source);
                }
            }
            Self {
                active,
                edges: edges.to_vec(),
                neighbors,
            }
        }
    }

    impl KCoreGraph for Graph {
        fn vertex_count(&self) -> usize {
            self.active.len()
        }

        fn is_vertex_active(&self, vertex: usize) -> bool {
            self.active.get(vertex).copied().unwrap_or(false)
        }

        fn for_each_edge(&self, mut visit: impl FnMut(usize, usize) -> Result<()>) -> Result<()> {
            for &(source, destination) in &self.edges {
                visit(source, destination)?;
            }
            Ok(())
        }

        fn for_each_neighbor(
            &self,
            vertex: usize,
            mut visit: impl FnMut(usize) -> Result<()>,
        ) -> Result<()> {
            for &neighbor in &self.neighbors[vertex] {
                visit(neighbor)?;
            }
            Ok(())
        }
    }

    fn run(graph: &Graph) -> KCoreDecomposition {
        k_core_decomposition(graph, &MemoryTracker::default(), || Ok(())).unwrap()
    }

    #[test]
    fn empty_and_isolated_vertices_have_zero_coreness() {
        let empty = run(&Graph::all(0, &[]));
        assert_eq!(empty.cores(), []);
        assert_eq!(empty.active_count(), 0);

        let isolated = run(&Graph::all(3, &[]));
        assert_eq!(isolated.cores(), [0, 0, 0]);
        assert_eq!(isolated.active_count(), 3);
    }

    #[test]
    fn trees_cycles_and_cliques_preserve_peeling_invariants() {
        let tree = run(&Graph::all(6, &[(0, 1), (1, 2), (1, 3), (3, 4), (3, 5)]));
        assert_eq!(tree.cores(), [1, 1, 1, 1, 1, 1]);

        let cycle = run(&Graph::all(5, &[(0, 1), (1, 2), (2, 3), (3, 4), (4, 0)]));
        assert_eq!(cycle.cores(), [2, 2, 2, 2, 2]);

        let clique = run(&Graph::all(
            4,
            &[(0, 1), (0, 2), (0, 3), (1, 2), (1, 3), (2, 3)],
        ));
        assert_eq!(clique.cores(), [3, 3, 3, 3]);
    }

    #[test]
    fn mixed_core_numbers_match_degree_frontier_peeling() {
        let result = run(&Graph::all(
            10,
            &[
                (0, 1),
                (1, 2),
                (2, 3),
                (2, 7),
                (3, 4),
                (3, 5),
                (3, 6),
                (4, 5),
                (4, 6),
                (5, 6),
                (6, 7),
                (8, 9),
            ],
        ));
        assert_eq!(result.cores(), [1, 1, 2, 3, 3, 3, 3, 2, 1, 1]);
    }

    #[test]
    fn parallel_relationships_and_self_loops_retain_multiplicity() {
        let parallel = run(&Graph::all(2, &[(0, 1), (0, 1), (0, 1)]));
        assert_eq!(parallel.cores(), [3, 3]);

        let loop_only = run(&Graph::all(1, &[(0, 0)]));
        assert_eq!(loop_only.cores(), [2]);

        let loop_and_leaf = run(&Graph::all(2, &[(0, 0), (0, 1)]));
        assert_eq!(loop_and_leaf.cores(), [2, 1]);
    }

    #[test]
    fn disconnected_components_and_inactive_holes_are_independent() {
        let graph = Graph::selected(
            vec![true, true, true, false, true, true, true],
            &[(0, 1), (1, 2), (2, 0), (4, 5)],
        );
        let result = run(&graph);
        assert_eq!(result.cores(), [2, 2, 2, UNASSIGNED_CORE, 1, 1, 0]);
        assert_eq!(result.core(3), None);
        assert_eq!(result.active_count(), 6);
    }

    #[test]
    fn invalid_selected_topology_is_rejected() {
        let error = k_core_decomposition(
            &Graph::selected(vec![true, false], &[(0, 1)]),
            &MemoryTracker::default(),
            || Ok(()),
        )
        .unwrap_err();
        assert_eq!(
            error.to_string(),
            "Runtime exception: K-core input referenced unselected vertex 1."
        );
    }

    #[test]
    fn cancellation_interrupts_high_degree_peeling() {
        let edges: Vec<_> = (0..(VECTOR_CAPACITY * 2)).map(|_| (0, 1)).collect();
        let checks = Cell::new(0_usize);
        let error = k_core_decomposition(&Graph::all(2, &edges), &MemoryTracker::default(), || {
            let next = checks.get() + 1;
            checks.set(next);
            if next == 10 {
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
        let tracker = MemoryTracker::new(Some(size_of::<u64>() as u64));
        let error = k_core_decomposition(&Graph::all(1, &[]), &tracker, || Ok(())).unwrap_err();
        assert_eq!(
            error.to_string(),
            "Buffer manager exception: Unable to allocate memory! The buffer pool is full and no memory could be freed!"
        );
        assert_eq!(tracker.usage().current, 0);
    }

    #[test]
    fn retained_result_releases_temporary_peeling_state() {
        let tracker = MemoryTracker::default();
        let result =
            k_core_decomposition(&Graph::all(3, &[(0, 1), (1, 2)]), &tracker, || Ok(())).unwrap();
        assert_eq!(tracker.usage().current, 3 * size_of::<u64>() as u64);
        drop(result);
        assert_eq!(tracker.usage().current, 0);
    }
}
