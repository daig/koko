use koko_common::{Error, MemoryReservation, MemoryTracker, Result, VECTOR_CAPACITY};
use std::mem::size_of;

/// Ladybug-derived PageRank options on Koko's positional call surface.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PageRankConfig {
    pub damping_factor: f64,
    pub tolerance: f64,
    pub max_iterations: i64,
    pub normalize_initial: bool,
}

impl Default for PageRankConfig {
    fn default() -> Self {
        Self {
            damping_factor: 0.85,
            tolerance: 1e-7,
            max_iterations: 20,
            normalize_initial: true,
        }
    }
}

impl PageRankConfig {
    /// Reject every invalid option before graph work or allocation begins.
    pub fn validate(self) -> Result<()> {
        if !self.damping_factor.is_finite() || !(0.0..1.0).contains(&self.damping_factor) {
            return Err(Error::runtime(
                "PageRank damping factor must be finite and in [0, 1).",
            ));
        }
        if !self.tolerance.is_finite() || self.tolerance < 0.0 {
            return Err(Error::runtime(
                "PageRank tolerance must be finite and non-negative.",
            ));
        }
        if self.max_iterations < 0 {
            return Err(Error::runtime(
                "PageRank maximum iterations must be non-negative.",
            ));
        }
        Ok(())
    }
}

/// Narrow, statically dispatched selected-edge input for PageRank lowering.
pub trait PageRankGraph {
    /// Width of the dense address space. Inactive MVCC holes are allowed.
    fn vertex_count(&self) -> usize;

    /// Whether one dense slot is a selected, visible vertex.
    fn is_vertex_active(&self, vertex: usize) -> bool;

    /// Visit every selected directed edge once, preserving multiplicity.
    fn for_each_edge(&self, visit: impl FnMut(usize, usize) -> Result<()>) -> Result<()>;
}

/// Immutable PageRank scores over one dense address space.
#[derive(Debug)]
pub struct PageRankScores {
    scores: Vec<f64>,
    active_count: usize,
    sweeps: u64,
    _memory: MemoryReservation,
}

impl PageRankScores {
    /// Score for a selected vertex, or `None` for an inactive/out-of-range slot.
    pub fn score(&self, vertex: usize) -> Option<f64> {
        self.scores
            .get(vertex)
            .copied()
            .filter(|score| score.is_finite())
    }

    /// Dense scores; inactive slots contain `NaN`.
    pub fn scores(&self) -> &[f64] {
        &self.scores
    }

    /// Number of selected vertices represented by this result.
    pub const fn active_count(&self) -> usize {
        self.active_count
    }

    /// Number of completed pull-update sweeps.
    pub const fn sweeps(&self) -> u64 {
        self.sweeps
    }
}

/// Compute deterministic unweighted PageRank over a selected directed graph.
///
/// The update follows Ladybug's initialization, strict L1 convergence test, and
/// `current_iteration < max_iterations` boundary. Koko additionally redistributes
/// dangling mass across all selected vertices. Each sweep pulls from a transient
/// incoming CSR, so parallel relationships and self-loops contribute separately
/// without atomic floating-point updates.
pub fn page_rank<G, C>(
    graph: &G,
    config: PageRankConfig,
    memory: &MemoryTracker,
    mut check_cancel: C,
) -> Result<PageRankScores>
where
    G: PageRankGraph,
    C: FnMut() -> Result<()>,
{
    config.validate()?;
    let vertex_count = graph.vertex_count();
    let score_bytes = allocation_bytes::<f64>(vertex_count)?;
    let result_memory = memory.try_reserve(score_bytes)?;
    let mut current = vec![f64::NAN; vertex_count];

    check_cancel()?;
    let mut active_count = 0_usize;
    for vertex in 0..vertex_count {
        if graph.is_vertex_active(vertex) {
            active_count = active_count
                .checked_add(1)
                .ok_or_else(|| Error::runtime("PageRank selected vertex count overflow."))?;
        }
        if (vertex + 1) % VECTOR_CAPACITY == 0 {
            check_cancel()?;
        }
    }
    check_cancel()?;

    if active_count == 0 {
        return Ok(PageRankScores {
            scores: current,
            active_count,
            sweeps: 0,
            _memory: result_memory,
        });
    }

    let initial_value = if config.normalize_initial {
        1.0 / active_count as f64
    } else {
        1.0
    };
    for (vertex, score) in current.iter_mut().enumerate() {
        if graph.is_vertex_active(vertex) {
            *score = initial_value;
        }
    }

    // Ladybug begins with current_iteration = 1 and performs a sweep only while
    // current_iteration < max_iterations. Preserve that observable boundary.
    if config.max_iterations <= 1 {
        return Ok(PageRankScores {
            scores: current,
            active_count,
            sweeps: 0,
            _memory: result_memory,
        });
    }

    if u32::try_from(vertex_count).is_ok() {
        compute::<u32, _, _>(
            graph,
            config,
            memory,
            &mut check_cancel,
            current,
            active_count,
            result_memory,
        )
    } else {
        compute::<u64, _, _>(
            graph,
            config,
            memory,
            &mut check_cancel,
            current,
            active_count,
            result_memory,
        )
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

struct IncomingCsr<I> {
    offsets: Vec<usize>,
    sources: Vec<I>,
    outgoing_degree: Vec<u64>,
    _memory: MemoryReservation,
}

fn compute<I, G, C>(
    graph: &G,
    config: PageRankConfig,
    memory: &MemoryTracker,
    check_cancel: &mut C,
    mut current: Vec<f64>,
    active_count: usize,
    result_memory: MemoryReservation,
) -> Result<PageRankScores>
where
    I: DenseId,
    G: PageRankGraph,
    C: FnMut() -> Result<()>,
{
    let topology = build_incoming_csr::<I, _, _>(graph, memory, check_cancel)?;
    let vertex_count = graph.vertex_count();
    let state_bytes = allocation_bytes::<f64>(vertex_count)?;
    let state_memory = memory.try_reserve(state_bytes)?;
    let mut next = vec![f64::NAN; vertex_count];
    let initial_value = if config.normalize_initial {
        1.0 / active_count as f64
    } else {
        1.0
    };
    let teleport = (1.0 - config.damping_factor) * initial_value;
    let mut current_iteration = 1_i64;
    let mut sweeps = 0_u64;

    while current_iteration < config.max_iterations {
        check_cancel()?;

        let mut dangling_mass = 0.0_f64;
        for (vertex, &score) in current.iter().enumerate() {
            if graph.is_vertex_active(vertex) && topology.outgoing_degree[vertex] == 0 {
                dangling_mass += score;
            }
            if (vertex + 1) % VECTOR_CAPACITY == 0 {
                check_cancel()?;
            }
        }
        let dangling_share = dangling_mass / active_count as f64;

        let mut difference = 0.0_f64;
        let mut traversed_edges = 0_usize;
        for vertex in 0..vertex_count {
            if !graph.is_vertex_active(vertex) {
                continue;
            }
            let mut incoming = dangling_share;
            for &source in &topology.sources[topology.offsets[vertex]..topology.offsets[vertex + 1]]
            {
                let source = source.index();
                let degree = topology.outgoing_degree[source];
                if degree == 0 {
                    return Err(Error::runtime(
                        "PageRank incoming topology referenced a zero-degree source.",
                    ));
                }
                incoming += current[source] / degree as f64;
                traversed_edges = traversed_edges
                    .checked_add(1)
                    .ok_or_else(|| Error::runtime("PageRank edge traversal count overflow."))?;
                if traversed_edges % VECTOR_CAPACITY == 0 {
                    check_cancel()?;
                }
            }
            let score = config.damping_factor * incoming + teleport;
            if !score.is_finite() {
                return Err(Error::runtime("PageRank produced a non-finite score."));
            }
            next[vertex] = score;
            difference += (score - current[vertex]).abs();
            if (vertex + 1) % VECTOR_CAPACITY == 0 {
                check_cancel()?;
            }
        }

        std::mem::swap(&mut current, &mut next);
        sweeps = sweeps
            .checked_add(1)
            .ok_or_else(|| Error::runtime("PageRank sweep count overflow."))?;
        if difference < config.tolerance {
            break;
        }
        current_iteration = current_iteration
            .checked_add(1)
            .ok_or_else(|| Error::runtime("PageRank iteration count overflow."))?;
    }

    drop(next);
    drop(state_memory);
    drop(topology);
    Ok(PageRankScores {
        scores: current,
        active_count,
        sweeps,
        _memory: result_memory,
    })
}

fn build_incoming_csr<I, G, C>(
    graph: &G,
    memory: &MemoryTracker,
    check_cancel: &mut C,
) -> Result<IncomingCsr<I>>
where
    I: DenseId,
    G: PageRankGraph,
    C: FnMut() -> Result<()>,
{
    let vertex_count = graph.vertex_count();
    let offset_len = vertex_count
        .checked_add(1)
        .ok_or_else(Error::buffer_manager)?;
    let offset_bytes = allocation_bytes::<usize>(offset_len)?;
    let degree_bytes = allocation_bytes::<u64>(vertex_count)?;
    let index_bytes = offset_bytes
        .checked_add(degree_bytes)
        .ok_or_else(Error::buffer_manager)?;
    let mut topology_memory = memory.try_reserve(index_bytes)?;
    let mut offsets = vec![0_usize; offset_len];
    let mut outgoing_degree = vec![0_u64; vertex_count];

    check_cancel()?;
    let mut counted_edges = 0_usize;
    graph.for_each_edge(|source, destination| {
        validate_edge(graph, vertex_count, source, destination)?;
        offsets[destination + 1] = offsets[destination + 1]
            .checked_add(1)
            .ok_or_else(|| Error::runtime("PageRank incoming degree overflow."))?;
        outgoing_degree[source] = outgoing_degree[source]
            .checked_add(1)
            .ok_or_else(|| Error::runtime("PageRank outgoing degree overflow."))?;
        counted_edges = counted_edges
            .checked_add(1)
            .ok_or_else(|| Error::runtime("PageRank selected edge count overflow."))?;
        if counted_edges % VECTOR_CAPACITY == 0 {
            check_cancel()?;
        }
        Ok(())
    })?;
    check_cancel()?;

    for index in 1..offsets.len() {
        offsets[index] = offsets[index]
            .checked_add(offsets[index - 1])
            .ok_or_else(|| Error::runtime("PageRank CSR offset overflow."))?;
        if index % VECTOR_CAPACITY == 0 {
            check_cancel()?;
        }
    }
    let edge_count = offsets.last().copied().unwrap_or_default();
    if edge_count != counted_edges {
        return Err(Error::runtime(
            "PageRank CSR edge count disagreed with incoming degrees.",
        ));
    }

    let source_bytes = allocation_bytes::<I>(edge_count)?;
    topology_memory.resize(
        index_bytes
            .checked_add(source_bytes)
            .ok_or_else(Error::buffer_manager)?,
    )?;
    let mut sources = vec![I::from_index(0); edge_count];
    let cursor_bytes = allocation_bytes::<usize>(vertex_count)?;
    let cursor_memory = memory.try_reserve(cursor_bytes)?;
    let mut cursors = offsets[..vertex_count].to_vec();

    let mut filled_edges = 0_usize;
    graph.for_each_edge(|source, destination| {
        validate_edge(graph, vertex_count, source, destination)?;
        let slot = cursors[destination];
        if slot >= offsets[destination + 1] {
            return Err(Error::runtime(
                "PageRank graph changed while building incoming topology.",
            ));
        }
        sources[slot] = I::from_index(source);
        cursors[destination] = slot + 1;
        filled_edges = filled_edges
            .checked_add(1)
            .ok_or_else(|| Error::runtime("PageRank selected edge count overflow."))?;
        if filled_edges % VECTOR_CAPACITY == 0 {
            check_cancel()?;
        }
        Ok(())
    })?;
    check_cancel()?;

    if filled_edges != edge_count
        || cursors
            .iter()
            .zip(offsets.iter().skip(1))
            .any(|(cursor, end)| cursor != end)
    {
        return Err(Error::runtime(
            "PageRank graph changed while building incoming topology.",
        ));
    }
    drop(cursors);
    drop(cursor_memory);

    Ok(IncomingCsr {
        offsets,
        sources,
        outgoing_degree,
        _memory: topology_memory,
    })
}

fn validate_edge<G: PageRankGraph>(
    graph: &G,
    vertex_count: usize,
    source: usize,
    destination: usize,
) -> Result<()> {
    validate_vertex(graph, vertex_count, source)?;
    validate_vertex(graph, vertex_count, destination)
}

fn validate_vertex<G: PageRankGraph>(graph: &G, vertex_count: usize, vertex: usize) -> Result<()> {
    if vertex >= vertex_count || !graph.is_vertex_active(vertex) {
        return Err(Error::runtime(format!(
            "PageRank input referenced unselected vertex {vertex}."
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
    use std::cell::Cell;

    struct Graph {
        active: Vec<bool>,
        edges: Vec<(usize, usize)>,
    }

    impl Graph {
        fn all(vertex_count: usize, edges: &[(usize, usize)]) -> Self {
            Self {
                active: vec![true; vertex_count],
                edges: edges.to_vec(),
            }
        }
    }

    impl PageRankGraph for Graph {
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
    }

    fn configured(max_iterations: i64) -> PageRankConfig {
        PageRankConfig {
            max_iterations,
            tolerance: 0.0,
            ..PageRankConfig::default()
        }
    }

    fn run(graph: &Graph, config: PageRankConfig) -> PageRankScores {
        page_rank(graph, config, &MemoryTracker::default(), || Ok(())).unwrap()
    }

    fn assert_close(actual: f64, expected: f64, tolerance: f64) {
        assert!(
            (actual - expected).abs() <= tolerance,
            "expected {expected:.16}, got {actual:.16} (tolerance {tolerance})"
        );
    }

    fn assert_scores(result: &PageRankScores, expected: &[f64], tolerance: f64) {
        assert_eq!(result.scores().len(), expected.len());
        for (vertex, &expected) in expected.iter().enumerate() {
            assert_close(result.score(vertex).unwrap(), expected, tolerance);
        }
    }

    #[test]
    fn empty_singleton_and_iteration_boundaries_preserve_initialization() {
        let empty = run(&Graph::all(0, &[]), PageRankConfig::default());
        assert_eq!(empty.scores(), []);
        assert_eq!(empty.active_count(), 0);
        assert_eq!(empty.sweeps(), 0);

        for max_iterations in [0, 1] {
            let normalized = run(&Graph::all(1, &[]), configured(max_iterations));
            assert_scores(&normalized, &[1.0], 0.0);
            assert_eq!(normalized.sweeps(), 0);
        }

        let one_sweep = run(&Graph::all(2, &[(0, 1)]), configured(2));
        assert_scores(&one_sweep, &[0.2875, 0.7125], 1e-15);
        assert_eq!(one_sweep.sweeps(), 1);

        let two_sweeps = run(&Graph::all(2, &[(0, 1)]), configured(3));
        assert_scores(&two_sweeps, &[0.3778125, 0.6221875], 1e-15);
        assert_eq!(two_sweeps.sweeps(), 2);
    }

    #[test]
    fn cycles_converge_and_dangling_mass_is_redistributed() {
        let cycle = run(
            &Graph::all(3, &[(0, 1), (1, 2), (2, 0)]),
            PageRankConfig::default(),
        );
        assert_scores(&cycle, &[1.0 / 3.0; 3], 1e-15);
        assert_eq!(cycle.sweeps(), 1);

        let chain = run(&Graph::all(3, &[(0, 1), (1, 2)]), configured(20));
        assert_scores(
            &chain,
            &[0.184416624629836, 0.34117087344058555, 0.4744125019295781],
            1e-15,
        );
        assert_close(chain.scores().iter().sum(), 1.0, 1e-15);
    }

    #[test]
    fn directed_parallel_edges_and_self_loops_contribute_separately() {
        let graph = Graph::all(3, &[(0, 0), (0, 1), (0, 1), (1, 2)]);
        let result = run(&graph, configured(3));
        assert_scores(
            &result,
            &[0.23888888888888887, 0.306574074074074, 0.45453703703703696],
            1e-15,
        );
    }

    #[test]
    fn star_and_unnormalized_initial_values_follow_config() {
        let star = Graph::all(4, &[(0, 1), (0, 2), (0, 3)]);
        let normalized = run(&star, configured(2));
        assert_scores(
            &normalized,
            &[
                0.196875,
                0.2677083333333333,
                0.2677083333333333,
                0.2677083333333333,
            ],
            1e-15,
        );

        let unnormalized = run(
            &star,
            PageRankConfig {
                max_iterations: 2,
                tolerance: 0.0,
                normalize_initial: false,
                ..PageRankConfig::default()
            },
        );
        assert_scores(
            &unnormalized,
            &[
                0.7875,
                1.0708333333333333,
                1.0708333333333333,
                1.0708333333333333,
            ],
            1e-15,
        );
        assert_close(unnormalized.scores().iter().sum(), 4.0, 1e-15);
    }

    #[test]
    fn convergence_is_strict_l1_and_stops_after_a_completed_sweep() {
        let graph = Graph::all(2, &[(0, 1)]);
        let early = run(
            &graph,
            PageRankConfig {
                tolerance: 1.0,
                max_iterations: 20,
                ..PageRankConfig::default()
            },
        );
        assert_eq!(early.sweeps(), 1);
        assert_scores(&early, &[0.2875, 0.7125], 1e-15);

        let zero_tolerance = run(&Graph::all(2, &[(0, 1), (1, 0)]), configured(4));
        assert_eq!(zero_tolerance.sweeps(), 3);
        assert_scores(&zero_tolerance, &[0.5, 0.5], 0.0);
    }

    #[test]
    fn inactive_holes_are_excluded_from_scores_and_mass() {
        let graph = Graph {
            active: vec![true, false, true],
            edges: vec![(0, 2)],
        };
        let result = run(&graph, configured(2));
        assert_scores_for_active(&result, &[(0, 0.2875), (2, 0.7125)], 1e-15);
        assert!(result.scores()[1].is_nan());
        assert_eq!(result.score(1), None);
        assert_eq!(result.active_count(), 2);
    }

    fn assert_scores_for_active(
        result: &PageRankScores,
        expected: &[(usize, f64)],
        tolerance: f64,
    ) {
        for &(vertex, score) in expected {
            assert_close(result.score(vertex).unwrap(), score, tolerance);
        }
    }

    #[test]
    fn every_invalid_option_is_rejected_before_allocation() {
        let graph = Graph::all(1, &[]);
        let tracker = MemoryTracker::default();
        for config in [
            PageRankConfig {
                damping_factor: -0.1,
                ..PageRankConfig::default()
            },
            PageRankConfig {
                damping_factor: 1.0,
                ..PageRankConfig::default()
            },
            PageRankConfig {
                damping_factor: f64::NAN,
                ..PageRankConfig::default()
            },
            PageRankConfig {
                damping_factor: f64::INFINITY,
                ..PageRankConfig::default()
            },
            PageRankConfig {
                tolerance: -0.1,
                ..PageRankConfig::default()
            },
            PageRankConfig {
                tolerance: f64::NAN,
                ..PageRankConfig::default()
            },
            PageRankConfig {
                tolerance: f64::INFINITY,
                ..PageRankConfig::default()
            },
            PageRankConfig {
                max_iterations: -1,
                ..PageRankConfig::default()
            },
        ] {
            assert!(page_rank(&graph, config, &tracker, || Ok(())).is_err());
            assert_eq!(tracker.usage().current, 0);
        }
    }

    #[test]
    fn cancellation_interrupts_csr_build_and_pull_sweeps() {
        let edges = vec![(0, 1); VECTOR_CAPACITY * 2];
        let checks = Cell::new(0_usize);
        let error = page_rank(
            &Graph::all(2, &edges),
            configured(20),
            &MemoryTracker::default(),
            || {
                let next = checks.get() + 1;
                checks.set(next);
                if next == 3 {
                    Err(Error::runtime("cancelled by test"))
                } else {
                    Ok(())
                }
            },
        )
        .unwrap_err();
        assert_eq!(error.to_string(), "Runtime exception: cancelled by test");

        let checks = Cell::new(0_usize);
        let error = page_rank(
            &Graph::all(2, &[(0, 1)]),
            configured(20),
            &MemoryTracker::default(),
            || {
                let next = checks.get() + 1;
                checks.set(next);
                if next == 10 {
                    Err(Error::runtime("cancelled during sweep"))
                } else {
                    Ok(())
                }
            },
        )
        .unwrap_err();
        assert_eq!(
            error.to_string(),
            "Runtime exception: cancelled during sweep"
        );
    }

    #[test]
    fn memory_is_admitted_and_only_the_result_remains_retained() {
        let denied = MemoryTracker::new(Some(1));
        let error = page_rank(
            &Graph::all(1, &[]),
            PageRankConfig::default(),
            &denied,
            || Ok(()),
        )
        .unwrap_err();
        assert_eq!(
            error.to_string(),
            "Buffer manager exception: Unable to allocate memory! The buffer pool is full and no memory could be freed!"
        );
        assert_eq!(denied.usage().current, 0);

        let tracker = MemoryTracker::default();
        let result = page_rank(
            &Graph::all(3, &[(0, 1), (1, 2)]),
            PageRankConfig::default(),
            &tracker,
            || Ok(()),
        )
        .unwrap();
        assert_eq!(tracker.usage().current, 3 * size_of::<f64>() as u64);
        assert!(tracker.usage().peak > tracker.usage().current);
        drop(result);
        assert_eq!(tracker.usage().current, 0);
    }
}
