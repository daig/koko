use koko_common::{Error, MemoryReservation, MemoryTracker, Result, VECTOR_CAPACITY};
use std::mem::size_of;

const MODULARITY_THRESHOLD: f64 = 1e-6;
const UNASSIGNED: u64 = u64::MAX;
const UNASSIGNED_INDEX: usize = usize::MAX;

/// Narrow selected-graph input for deterministic, unweighted Louvain.
///
/// Dense addresses may contain inactive holes. The vertex visitor is separate
/// because the optimization order is the ascending original `InternalId`
/// order, which need not equal caller-selected table/dense-address order.
pub trait LouvainGraph {
    /// Width of the selected graph's dense address space, including holes.
    fn vertex_count(&self) -> usize;

    /// Whether one dense address is visible in the statement snapshot.
    fn is_vertex_active(&self, vertex: usize) -> bool;

    /// Visit every active dense address exactly once in ascending original
    /// `InternalId` order.
    fn for_each_vertex_in_internal_id_order(
        &self,
        visit: impl FnMut(usize) -> Result<()>,
    ) -> Result<()>;

    /// Visit every selected relationship exactly once in its stored direction.
    fn for_each_edge(&self, visit: impl FnMut(usize, usize) -> Result<()>) -> Result<()>;
}

/// Immutable canonical community IDs over the input dense address space.
#[derive(Debug)]
pub struct LouvainCommunities {
    communities: Vec<u64>,
    active_count: usize,
    _memory: MemoryReservation,
}

impl LouvainCommunities {
    /// Compact community ID, or `None` for an inactive/out-of-range address.
    pub fn community(&self, vertex: usize) -> Option<u64> {
        self.communities
            .get(vertex)
            .copied()
            .filter(|&community| community != UNASSIGNED)
    }

    /// Dense result storage; inactive slots contain `u64::MAX`.
    pub fn communities(&self) -> &[u64] {
        &self.communities
    }

    /// Number of visible selected vertices represented by the result.
    pub const fn active_count(&self) -> usize {
        self.active_count
    }
}

/// Run serial deterministic Louvain over an unweighted selected graph.
///
/// Each selected relationship has unit weight. Non-self relationships produce
/// two undirected CSR entries; a self-loop produces one, matching Ladybug's
/// forward-plus-backward ingestion with its backward self-loop suppression.
/// Parallel relationships remain separate in the initial CSR. Local moves use
/// Ladybug's modularity gain and `1e-6` convergence/tie threshold. Vertices are
/// visited in ascending original `InternalId` order, and equal gains choose the
/// community with the lower canonical minimum-member rank.
pub fn louvain<G, C>(
    graph: &G,
    memory: &MemoryTracker,
    max_iterations: u64,
    max_phases: u64,
    mut check_cancel: C,
) -> Result<LouvainCommunities>
where
    G: LouvainGraph,
    C: FnMut() -> Result<()>,
{
    check_cancel()?;
    let address_count = graph.vertex_count();
    let result_bytes = allocation_bytes::<u64>(address_count)?;
    let result_memory = memory.try_reserve(result_bytes)?;
    let mut communities = vec![UNASSIGNED; address_count];

    let mut active_count = 0_usize;
    for vertex in 0..address_count {
        if graph.is_vertex_active(vertex) {
            active_count = active_count
                .checked_add(1)
                .ok_or_else(|| Error::runtime("Louvain vertex count overflow."))?;
        }
        if (vertex + 1) % VECTOR_CAPACITY == 0 {
            check_cancel()?;
        }
    }

    let mut visited_count = 0_usize;
    graph.for_each_vertex_in_internal_id_order(|vertex| {
        if vertex >= address_count || !graph.is_vertex_active(vertex) {
            return Err(Error::runtime(format!(
                "Louvain vertex order referenced unselected vertex {vertex}."
            )));
        }
        if communities[vertex] != UNASSIGNED {
            return Err(Error::runtime(format!(
                "Louvain vertex order repeated vertex {vertex}."
            )));
        }
        communities[vertex] = u64::try_from(visited_count)
            .map_err(|_| Error::runtime("Louvain vertex rank overflow."))?;
        visited_count = visited_count
            .checked_add(1)
            .ok_or_else(|| Error::runtime("Louvain vertex count overflow."))?;
        if visited_count % VECTOR_CAPACITY == 0 {
            check_cancel()?;
        }
        Ok(())
    })?;
    if visited_count != active_count {
        return Err(Error::runtime(format!(
            "Louvain vertex order visited {visited_count} of {active_count} selected vertices."
        )));
    }
    check_cancel()?;

    let mut csr = build_initial_csr(graph, &communities, active_count, memory, &mut check_cancel)?;
    let state_bytes = phase_state_bytes(active_count)?;
    let _state_memory = memory.try_reserve(state_bytes)?;
    let mut state = PhaseState::new(active_count);

    // A zero phase limit means no optimization: the rank map already contains
    // canonical singleton community IDs.
    for phase in 0..max_phases {
        check_cancel()?;
        state.initialize(csr.node_count(), &csr)?;
        let mut old_modularity = -1.0_f64;

        for _ in 0..max_iterations {
            check_cancel()?;
            let current_modularity = state.sweep(&csr, &mut check_cancel)?;
            check_cancel()?;
            if current_modularity - old_modularity < MODULARITY_THRESHOLD {
                break;
            }
            old_modularity = current_modularity;
            state.accept_and_advance(csr.node_count())?;
        }

        let old_community_count = csr.node_count();
        let new_community_count = state.renumber_accepted(old_community_count)?;

        if phase == 0 {
            for community in &mut communities {
                if *community != UNASSIGNED {
                    let rank = usize::try_from(*community)
                        .map_err(|_| Error::runtime("Louvain vertex rank overflow."))?;
                    *community = u64::try_from(state.accepted[rank])
                        .map_err(|_| Error::runtime("Louvain community ID overflow."))?;
                }
            }
        } else {
            for community in &mut communities {
                if *community != UNASSIGNED {
                    let previous = usize::try_from(*community)
                        .map_err(|_| Error::runtime("Louvain community ID overflow."))?;
                    *community = u64::try_from(state.accepted[previous])
                        .map_err(|_| Error::runtime("Louvain community ID overflow."))?;
                }
            }
        }
        check_cancel()?;

        if old_community_count == new_community_count || phase + 1 == max_phases {
            break;
        }
        csr = aggregate_communities(
            &csr,
            &state.accepted[..old_community_count],
            new_community_count,
            memory,
            &mut check_cancel,
        )?;
        check_cancel()?;
    }

    Ok(LouvainCommunities {
        communities,
        active_count,
        _memory: result_memory,
    })
}

#[derive(Debug, Clone, Copy, Default)]
struct WeightedNeighbor {
    vertex: usize,
    weight: u64,
}

struct WeightedCsr {
    offsets: Vec<usize>,
    neighbors: Vec<WeightedNeighbor>,
    total_weight: u64,
    _memory: MemoryReservation,
}

impl WeightedCsr {
    fn node_count(&self) -> usize {
        self.offsets.len().saturating_sub(1)
    }

    fn neighbors(&self, vertex: usize) -> &[WeightedNeighbor] {
        &self.neighbors[self.offsets[vertex]..self.offsets[vertex + 1]]
    }

    fn weighted_degree(&self, vertex: usize) -> Result<u64> {
        self.neighbors(vertex).iter().try_fold(0_u64, |sum, edge| {
            sum.checked_add(edge.weight)
                .ok_or_else(|| Error::runtime("Louvain weighted degree overflow."))
        })
    }
}

fn build_initial_csr<G, C>(
    graph: &G,
    dense_to_rank: &[u64],
    active_count: usize,
    memory: &MemoryTracker,
    check_cancel: &mut C,
) -> Result<WeightedCsr>
where
    G: LouvainGraph,
    C: FnMut() -> Result<()>,
{
    check_cancel()?;
    let degree_bytes = allocation_bytes::<usize>(active_count)?;
    let degree_memory = memory.try_reserve(degree_bytes)?;
    let mut degrees = vec![0_usize; active_count];
    let mut adjacency_count = 0_usize;
    let mut relationship_count = 0_usize;
    let mut total_weight = 0_u64;

    graph.for_each_edge(|source, destination| {
        let source = selected_rank(dense_to_rank, source)?;
        let destination = selected_rank(dense_to_rank, destination)?;
        if source == destination {
            degrees[source] = degrees[source]
                .checked_add(1)
                .ok_or_else(|| Error::runtime("Louvain CSR degree overflow."))?;
            adjacency_count = adjacency_count
                .checked_add(1)
                .ok_or_else(|| Error::runtime("Louvain CSR size overflow."))?;
            total_weight = total_weight
                .checked_add(1)
                .ok_or_else(|| Error::runtime("Louvain total weight overflow."))?;
        } else {
            degrees[source] = degrees[source]
                .checked_add(1)
                .ok_or_else(|| Error::runtime("Louvain CSR degree overflow."))?;
            degrees[destination] = degrees[destination]
                .checked_add(1)
                .ok_or_else(|| Error::runtime("Louvain CSR degree overflow."))?;
            adjacency_count = adjacency_count
                .checked_add(2)
                .ok_or_else(|| Error::runtime("Louvain CSR size overflow."))?;
            total_weight = total_weight
                .checked_add(2)
                .ok_or_else(|| Error::runtime("Louvain total weight overflow."))?;
        }
        relationship_count = relationship_count
            .checked_add(1)
            .ok_or_else(|| Error::runtime("Louvain relationship count overflow."))?;
        if relationship_count % VECTOR_CAPACITY == 0 {
            check_cancel()?;
        }
        Ok(())
    })?;
    check_cancel()?;

    let offset_count = active_count
        .checked_add(1)
        .ok_or_else(Error::buffer_manager)?;
    let csr_bytes = allocation_bytes::<usize>(offset_count)?
        .checked_add(allocation_bytes::<WeightedNeighbor>(adjacency_count)?)
        .ok_or_else(Error::buffer_manager)?;
    let csr_memory = memory.try_reserve(csr_bytes)?;
    let mut offsets = vec![0_usize; offset_count];
    for vertex in 0..active_count {
        offsets[vertex + 1] = offsets[vertex]
            .checked_add(degrees[vertex])
            .ok_or_else(|| Error::runtime("Louvain CSR offset overflow."))?;
        degrees[vertex] = offsets[vertex];
    }
    let mut neighbors = vec![WeightedNeighbor::default(); adjacency_count];

    relationship_count = 0;
    graph.for_each_edge(|source, destination| {
        let source = selected_rank(dense_to_rank, source)?;
        let destination = selected_rank(dense_to_rank, destination)?;
        let source_offset = degrees[source];
        neighbors[source_offset] = WeightedNeighbor {
            vertex: destination,
            weight: 1,
        };
        degrees[source] = source_offset
            .checked_add(1)
            .ok_or_else(|| Error::runtime("Louvain CSR cursor overflow."))?;
        if source != destination {
            let destination_offset = degrees[destination];
            neighbors[destination_offset] = WeightedNeighbor {
                vertex: source,
                weight: 1,
            };
            degrees[destination] = destination_offset
                .checked_add(1)
                .ok_or_else(|| Error::runtime("Louvain CSR cursor overflow."))?;
        }
        relationship_count = relationship_count
            .checked_add(1)
            .ok_or_else(|| Error::runtime("Louvain relationship count overflow."))?;
        if relationship_count % VECTOR_CAPACITY == 0 {
            check_cancel()?;
        }
        Ok(())
    })?;
    for vertex in 0..active_count {
        if degrees[vertex] != offsets[vertex + 1] {
            return Err(Error::runtime(
                "Louvain relationship scan changed between CSR passes.",
            ));
        }
    }
    check_cancel()?;
    drop(degrees);
    drop(degree_memory);
    Ok(WeightedCsr {
        offsets,
        neighbors,
        total_weight,
        _memory: csr_memory,
    })
}

fn selected_rank(dense_to_rank: &[u64], vertex: usize) -> Result<usize> {
    let rank = dense_to_rank.get(vertex).copied().ok_or_else(|| {
        Error::runtime(format!(
            "Louvain input referenced out-of-range vertex {vertex}."
        ))
    })?;
    if rank == UNASSIGNED {
        return Err(Error::runtime(format!(
            "Louvain input referenced unselected vertex {vertex}."
        )));
    }
    usize::try_from(rank).map_err(|_| Error::runtime("Louvain vertex rank overflow."))
}

struct PhaseState {
    current: Vec<usize>,
    accepted: Vec<usize>,
    next: Vec<usize>,
    community_sizes: Vec<u64>,
    community_degrees: Vec<u64>,
    node_degrees: Vec<u64>,
    neighbor_weights: Vec<u64>,
    neighbor_seen: Vec<u8>,
    touched: Vec<usize>,
}

impl PhaseState {
    fn new(capacity: usize) -> Self {
        Self {
            current: vec![0; capacity],
            accepted: vec![0; capacity],
            next: vec![0; capacity],
            community_sizes: vec![0; capacity],
            community_degrees: vec![0; capacity],
            node_degrees: vec![0; capacity],
            neighbor_weights: vec![0; capacity],
            neighbor_seen: vec![0; capacity],
            touched: Vec::with_capacity(capacity),
        }
    }

    fn initialize(&mut self, node_count: usize, csr: &WeightedCsr) -> Result<()> {
        for vertex in 0..node_count {
            let degree = csr.weighted_degree(vertex)?;
            self.current[vertex] = vertex;
            self.accepted[vertex] = vertex;
            self.next[vertex] = vertex;
            self.community_sizes[vertex] = 1;
            self.community_degrees[vertex] = degree;
            self.node_degrees[vertex] = degree;
            self.neighbor_weights[vertex] = 0;
            self.neighbor_seen[vertex] = 0;
        }
        Ok(())
    }

    fn sweep<C>(&mut self, csr: &WeightedCsr, check_cancel: &mut C) -> Result<f64>
    where
        C: FnMut() -> Result<()>,
    {
        let node_count = csr.node_count();
        let modularity_constant = if csr.total_weight == 0 {
            0.0
        } else {
            1.0 / csr.total_weight as f64
        };
        let mut sum_intra_weights = 0_u64;
        let mut traversed = 0_usize;

        // Phase nodes are numbered by ascending minimum original InternalId, so
        // this serial loop is also the required original-identity visitation.
        for vertex in 0..node_count {
            self.touched.clear();
            let current_community = self.current[vertex];
            self.touch_community(current_community);
            let mut self_loop_weight = 0_u64;
            for edge in csr.neighbors(vertex) {
                if edge.vertex == vertex {
                    self_loop_weight = self_loop_weight
                        .checked_add(edge.weight)
                        .ok_or_else(|| Error::runtime("Louvain self-loop weight overflow."))?;
                }
                let neighbor_community = self.current[edge.vertex];
                self.touch_community(neighbor_community);
                self.neighbor_weights[neighbor_community] = self.neighbor_weights
                    [neighbor_community]
                    .checked_add(edge.weight)
                    .ok_or_else(|| Error::runtime("Louvain neighbor weight overflow."))?;
                traversed = traversed
                    .checked_add(1)
                    .ok_or_else(|| Error::runtime("Louvain edge sweep overflow."))?;
                if traversed % VECTOR_CAPACITY == 0 {
                    check_cancel()?;
                }
            }

            sum_intra_weights = sum_intra_weights
                .checked_add(self.neighbor_weights[current_community])
                .ok_or_else(|| Error::runtime("Louvain intra-community weight overflow."))?;
            self.next[vertex] = self.best_community(
                vertex,
                current_community,
                self_loop_weight,
                modularity_constant,
            );
            for &community in &self.touched {
                self.neighbor_weights[community] = 0;
                self.neighbor_seen[community] = 0;
            }
            if (vertex + 1) % VECTOR_CAPACITY == 0 {
                check_cancel()?;
            }
        }

        Ok(modularity(
            sum_intra_weights,
            &self.community_degrees[..node_count],
            csr.total_weight,
        ))
    }

    fn touch_community(&mut self, community: usize) {
        if self.neighbor_seen[community] == 0 {
            self.neighbor_seen[community] = 1;
            self.neighbor_weights[community] = 0;
            self.touched.push(community);
        }
    }

    fn best_community(
        &self,
        vertex: usize,
        current_community: usize,
        self_loop_weight: u64,
        modularity_constant: f64,
    ) -> usize {
        let degree = self.node_degrees[vertex] as f64;
        let previous_intra_weight =
            (self.neighbor_weights[current_community] - self_loop_weight) as f64;
        let previous_weighted_degrees =
            (self.community_degrees[current_community] - self.node_degrees[vertex]) as f64;
        let mut target = current_community;
        let mut best_gain = 0.0_f64;

        for &candidate in &self.touched {
            if candidate == current_community {
                continue;
            }
            let new_intra_weight = self.neighbor_weights[candidate] as f64;
            let new_weighted_degrees = self.community_degrees[candidate] as f64;
            let change_intra_weights = 2.0 * (new_intra_weight - previous_intra_weight);
            let change_sum_weighted_degrees = 2.0
                * degree
                * modularity_constant
                * (new_weighted_degrees - previous_weighted_degrees);
            let gain = change_intra_weights - change_sum_weighted_degrees;
            if gain > best_gain
                || ((best_gain - gain) < MODULARITY_THRESHOLD && gain != 0.0 && candidate < target)
            {
                best_gain = gain;
                target = candidate;
            }
        }

        // Ladybug's singleton swap protection keeps the lower canonical ID.
        if self.community_sizes[target] == 1
            && self.community_sizes[current_community] == 1
            && target > current_community
        {
            current_community
        } else {
            target
        }
    }

    fn accept_and_advance(&mut self, node_count: usize) -> Result<()> {
        self.accepted[..node_count].copy_from_slice(&self.current[..node_count]);
        self.current[..node_count].copy_from_slice(&self.next[..node_count]);
        self.community_sizes[..node_count].fill(0);
        self.community_degrees[..node_count].fill(0);
        for vertex in 0..node_count {
            let community = self.current[vertex];
            self.community_sizes[community] = self.community_sizes[community]
                .checked_add(1)
                .ok_or_else(|| Error::runtime("Louvain community size overflow."))?;
            self.community_degrees[community] = self.community_degrees[community]
                .checked_add(self.node_degrees[vertex])
                .ok_or_else(|| Error::runtime("Louvain community degree overflow."))?;
        }
        Ok(())
    }

    fn renumber_accepted(&mut self, node_count: usize) -> Result<usize> {
        self.next[..node_count].fill(UNASSIGNED_INDEX);
        let mut next_community = 0_usize;
        for vertex in 0..node_count {
            let old_community = self.accepted[vertex];
            if self.next[old_community] == UNASSIGNED_INDEX {
                self.next[old_community] = next_community;
                next_community = next_community
                    .checked_add(1)
                    .ok_or_else(|| Error::runtime("Louvain community count overflow."))?;
            }
            self.accepted[vertex] = self.next[old_community];
        }
        Ok(next_community)
    }
}

fn modularity(sum_intra_weights: u64, community_degrees: &[u64], total_weight: u64) -> f64 {
    if total_weight == 0 {
        return 0.0;
    }
    let constant = 1.0 / total_weight as f64;
    let sum_weighted_degrees = community_degrees.iter().fold(0.0, |sum, &degree| {
        let degree = degree as f64;
        sum + degree * degree
    });
    sum_intra_weights as f64 * constant - sum_weighted_degrees * constant * constant
}

#[derive(Debug, Clone, Copy, Default)]
struct AggregateEdge {
    source: usize,
    destination: usize,
    weight: u64,
}

fn aggregate_communities<C>(
    old: &WeightedCsr,
    assignments: &[usize],
    new_node_count: usize,
    memory: &MemoryTracker,
    check_cancel: &mut C,
) -> Result<WeightedCsr>
where
    C: FnMut() -> Result<()>,
{
    check_cancel()?;
    let staging_bytes = allocation_bytes::<AggregateEdge>(old.neighbors.len())?;
    let staging_memory = memory.try_reserve(staging_bytes)?;
    let mut staging = Vec::with_capacity(old.neighbors.len());
    let mut traversed = 0_usize;

    for source in 0..old.node_count() {
        let source_community = assignments[source];
        for edge in old.neighbors(source) {
            let destination_community = assignments[edge.vertex];
            if source_community >= destination_community {
                push_aggregate_edge(
                    &mut staging,
                    AggregateEdge {
                        source: source_community,
                        destination: destination_community,
                        weight: edge.weight,
                    },
                )?;
                if source_community != destination_community {
                    push_aggregate_edge(
                        &mut staging,
                        AggregateEdge {
                            source: destination_community,
                            destination: source_community,
                            weight: edge.weight,
                        },
                    )?;
                }
            }
            traversed = traversed
                .checked_add(1)
                .ok_or_else(|| Error::runtime("Louvain aggregation edge count overflow."))?;
            if traversed % VECTOR_CAPACITY == 0 {
                check_cancel()?;
            }
        }
    }
    check_cancel()?;

    staging.sort_unstable_by_key(|edge| (edge.source, edge.destination));
    let mut write = 0_usize;
    for read in 0..staging.len() {
        let edge = staging[read];
        if write != 0
            && staging[write - 1].source == edge.source
            && staging[write - 1].destination == edge.destination
        {
            staging[write - 1].weight = staging[write - 1]
                .weight
                .checked_add(edge.weight)
                .ok_or_else(|| Error::runtime("Louvain aggregated edge weight overflow."))?;
        } else {
            staging[write] = edge;
            write += 1;
        }
    }
    staging.truncate(write);

    let offset_count = new_node_count
        .checked_add(1)
        .ok_or_else(Error::buffer_manager)?;
    let csr_bytes = allocation_bytes::<usize>(offset_count)?
        .checked_add(allocation_bytes::<WeightedNeighbor>(staging.len())?)
        .ok_or_else(Error::buffer_manager)?;
    let csr_memory = memory.try_reserve(csr_bytes)?;
    let mut offsets = vec![0_usize; offset_count];
    let mut total_weight = 0_u64;
    for edge in &staging {
        offsets[edge.source + 1] = offsets[edge.source + 1]
            .checked_add(1)
            .ok_or_else(|| Error::runtime("Louvain aggregated degree overflow."))?;
        total_weight = total_weight
            .checked_add(edge.weight)
            .ok_or_else(|| Error::runtime("Louvain total weight overflow."))?;
    }
    for vertex in 0..new_node_count {
        offsets[vertex + 1] = offsets[vertex + 1]
            .checked_add(offsets[vertex])
            .ok_or_else(|| Error::runtime("Louvain CSR offset overflow."))?;
    }
    let neighbors = staging
        .iter()
        .map(|edge| WeightedNeighbor {
            vertex: edge.destination,
            weight: edge.weight,
        })
        .collect();
    check_cancel()?;
    drop(staging);
    drop(staging_memory);
    Ok(WeightedCsr {
        offsets,
        neighbors,
        total_weight,
        _memory: csr_memory,
    })
}

fn push_aggregate_edge(edges: &mut Vec<AggregateEdge>, edge: AggregateEdge) -> Result<()> {
    if edges.len() == edges.capacity() {
        return Err(Error::runtime(
            "Louvain aggregation exceeded its accounted edge capacity.",
        ));
    }
    edges.push(edge);
    Ok(())
}

fn phase_state_bytes(capacity: usize) -> Result<u64> {
    let index_bytes = allocation_bytes::<usize>(capacity)?
        .checked_mul(4)
        .ok_or_else(Error::buffer_manager)?;
    let weight_bytes = allocation_bytes::<u64>(capacity)?
        .checked_mul(4)
        .ok_or_else(Error::buffer_manager)?;
    index_bytes
        .checked_add(weight_bytes)
        .and_then(|bytes| bytes.checked_add(allocation_bytes::<u8>(capacity).ok()?))
        .ok_or_else(Error::buffer_manager)
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
    use std::cell::Cell;

    const DEFAULT_MAX_ITERATIONS: u64 = 20;
    const DEFAULT_MAX_PHASES: u64 = 20;

    struct Graph {
        active: Vec<bool>,
        internal_id_order: Vec<usize>,
        edges: Vec<(usize, usize)>,
    }

    impl Graph {
        fn all(vertex_count: usize, edges: &[(usize, usize)]) -> Self {
            Self {
                active: vec![true; vertex_count],
                internal_id_order: (0..vertex_count).collect(),
                edges: edges.to_vec(),
            }
        }
    }

    impl LouvainGraph for Graph {
        fn vertex_count(&self) -> usize {
            self.active.len()
        }

        fn is_vertex_active(&self, vertex: usize) -> bool {
            self.active.get(vertex).copied().unwrap_or(false)
        }

        fn for_each_vertex_in_internal_id_order(
            &self,
            mut visit: impl FnMut(usize) -> Result<()>,
        ) -> Result<()> {
            for &vertex in &self.internal_id_order {
                if self.active[vertex] {
                    visit(vertex)?;
                }
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

    fn run(graph: &Graph) -> LouvainCommunities {
        louvain(
            graph,
            &MemoryTracker::default(),
            DEFAULT_MAX_ITERATIONS,
            DEFAULT_MAX_PHASES,
            || Ok(()),
        )
        .unwrap()
    }

    #[test]
    fn empty_isolated_and_self_loop_vertices_stay_canonical_singletons() {
        assert_eq!(run(&Graph::all(0, &[])).communities(), []);
        assert_eq!(run(&Graph::all(3, &[])).communities(), [0, 1, 2]);
        assert_eq!(run(&Graph::all(1, &[(0, 0)])).communities(), [0]);
    }

    #[test]
    fn modularity_counts_self_loops_once_and_parallel_edges_separately() {
        let graph = Graph::all(2, &[(0, 0), (0, 1), (0, 1)]);
        let dense_to_rank = vec![0, 1];
        let csr = build_initial_csr(
            &graph,
            &dense_to_rank,
            2,
            &MemoryTracker::default(),
            &mut || Ok(()),
        )
        .unwrap();
        assert_eq!(csr.total_weight, 5);
        assert_eq!(csr.weighted_degree(0).unwrap(), 3);
        assert_eq!(csr.weighted_degree(1).unwrap(), 2);
        assert!((modularity(1, &[3, 2], 5) - -0.32).abs() < 1e-12);
        assert_eq!(modularity(5, &[5, 0], 5), 0.0);
    }

    #[test]
    fn disconnected_cliques_with_a_bridge_form_canonical_communities() {
        let graph = Graph::all(6, &[(0, 1), (0, 2), (1, 2), (3, 4), (3, 5), (4, 5), (2, 3)]);
        assert_eq!(run(&graph).communities(), [0, 0, 0, 1, 1, 1]);
    }

    #[test]
    fn visitation_and_ties_follow_original_internal_id_order() {
        let graph = Graph {
            active: vec![true; 4],
            internal_id_order: vec![3, 2, 1, 0],
            edges: vec![(0, 1), (1, 2), (2, 3)],
        };
        assert_eq!(run(&graph).communities(), [1, 1, 0, 0]);
    }

    #[test]
    fn configured_limits_are_observable() {
        let graph = Graph::all(2, &[(0, 1)]);
        let no_sweeps = louvain(&graph, &MemoryTracker::default(), 0, 20, || Ok(())).unwrap();
        assert_eq!(no_sweeps.communities(), [0, 1]);
        let no_phases = louvain(&graph, &MemoryTracker::default(), 20, 0, || Ok(())).unwrap();
        assert_eq!(no_phases.communities(), [0, 1]);
        let merged = louvain(&graph, &MemoryTracker::default(), 2, 1, || Ok(())).unwrap();
        assert_eq!(merged.communities(), [0, 0]);
    }

    #[test]
    fn inactive_holes_are_excluded_from_edges_and_results() {
        let graph = Graph {
            active: vec![true, false, true],
            internal_id_order: vec![0, 2],
            edges: vec![(0, 2)],
        };
        let result = run(&graph);
        assert_eq!(result.community(0), Some(0));
        assert_eq!(result.community(1), None);
        assert_eq!(result.community(2), Some(0));
        assert_eq!(result.active_count(), 2);
    }

    #[test]
    fn cancellation_interrupts_csr_construction() {
        let checks = Cell::new(0_usize);
        let error = louvain(
            &Graph::all(2, &[(0, 1)]),
            &MemoryTracker::default(),
            DEFAULT_MAX_ITERATIONS,
            DEFAULT_MAX_PHASES,
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
    fn memory_is_admitted_and_only_the_result_remains_reserved() {
        let rejected = MemoryTracker::new(Some(1));
        let error = louvain(
            &Graph::all(1, &[]),
            &rejected,
            DEFAULT_MAX_ITERATIONS,
            DEFAULT_MAX_PHASES,
            || Ok(()),
        )
        .unwrap_err();
        assert_eq!(
            error.to_string(),
            "Buffer manager exception: Unable to allocate memory! The buffer pool is full and no memory could be freed!"
        );
        assert_eq!(rejected.usage().current, 0);

        let tracker = MemoryTracker::default();
        let result = louvain(
            &Graph::all(3, &[(0, 1)]),
            &tracker,
            DEFAULT_MAX_ITERATIONS,
            DEFAULT_MAX_PHASES,
            || Ok(()),
        )
        .unwrap();
        assert_eq!(tracker.usage().current, 3 * size_of::<u64>() as u64);
        drop(result);
        assert_eq!(tracker.usage().current, 0);
    }
}
