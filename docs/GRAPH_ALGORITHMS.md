# Graph algorithm architecture and supported surface

> **Status as of 2026-08-08.** [`ROADMAP.md`](../ROADMAP.md) remains the product and work
> authority. This document owns the supported query-time path operators and all six built-in
> whole-graph algorithms, including their graph-selection, lowering, execution and result
> contracts.

## 1. Status vocabulary and scope

| Status | Meaning |
|---|---|
| **Supported** | Implemented Koko behavior and a non-regression contract. |
| **Planned** | Selected target surface, but not supported until the implementation and owning regressions land. |
| **Unselected** | Not part of this work; requires a separate product decision. |

The built-in whole-graph surface does **not** select or require:

- `PROJECT_GRAPH`, `PROJECT_GRAPH_CYPHER`, or a named projected-graph registry;
- extension installation, dynamic plugins, or an `algo` extension;
- a persistent topology cache; or
- a second storage backend or virtual `Graph` interface.

Koko's existing named graphs remain database/catalog/data namespaces. A **graph selection** below is
instead one statement's declarative choice of node rows and relationship rows inside the selected
Koko graph.

## 2. Algorithm inventory

### 2.1 Supported query-time path algorithms

These are correlated `MATCH` operators: each input/start node drives a bounded traversal and may
produce paths or destinations before the next input row is processed. They continue to operate
against base adjacency and do not use the whole-graph selection/compiler described in section 3.

| Cypher surface | Status | Scope and semantics | Physical input | Kernel state | Observable result |
|---|---|---|---|---|---|
| `*` / `WALK` | Supported | Enumerate every walk within hop bounds; nodes and relationships may repeat. | Base adjacency, expanded per source | DFS path stack | Every admitted path/destination within bounds |
| `TRAIL` | Supported | Enumerate bounded paths without repeating a relationship. | Base adjacency, expanded per source | DFS path stack plus relationship-membership check | Every admitted relationship-unique path |
| `ACYCLIC` | Supported | Enumerate bounded paths without repeating a node. | Base adjacency, expanded per source | DFS path stack plus node-membership check | Every admitted node-unique path |
| `SHORTEST` | Supported | Find one minimum-hop path from the source to each reachable destination. | Base adjacency, expanded per source | BFS distance map plus one predecessor per destination | One shortest path/destination |
| `ALL SHORTEST` | Supported | Find every minimum-hop path from the source to each reachable destination. | Base adjacency, expanded per source | BFS distances plus predecessor multimap | All tied shortest paths |
| `WSHORTEST(weight)` | Supported | Find one minimum-total-weight path to each reachable destination; weights must be non-negative. | Base adjacency plus typed relationship weight reads | Dijkstra cost map, heap, and one predecessor | One minimum-weight path and `cost()` |
| `ALL WSHORTEST(weight)` | Supported | Find every minimum-total-weight path to each reachable destination; weights must be non-negative. | Base adjacency plus typed relationship weight reads | Dijkstra costs, heap, and predecessor multimap | All tied minimum-weight paths and `cost()` |

### 2.2 Whole-graph algorithms

Whole-graph algorithms execute once over a statement-bound graph selection and produce one typed
result per selected vertex. They are eager pipeline breakers internally, then expose their results
as normal in-query sources so `YIELD`, `WHERE`, `RETURN`, aggregation and ordering compose through
the existing query pipeline.

| Algorithm | Status | Graph interpretation | Physical lowering | Transient topology | Primary state | Output |
|---|---|---|---|---|---|---|
| Layered topological sort / `topological_levels` | Supported | Directed; preserve selected relationship direction | Narrow selected-endpoint pass for indegrees, then base forward adjacency | No copied edges; node-visibility bitset only when the statement view has holes | Indegree, FIFO frontier queue and output levels | `node`, zero-based `level`; cyclic input errors with no rows |
| Weakly connected components / `weakly_connected_components` | Supported | Undirected; ignore relationship direction | Selected endpoint stream into union-find | No copied edges | Parent/rank arrays | `node`, deterministic compact `component_id` |
| Strongly connected components / `strongly_connected_components` | Supported | Directed | Iterative Kosaraju traversal over base forward/reverse adjacency with resumable storage cursors | No copied edges | DFS state/order stack and component labels | `node`, deterministic compact `component_id` |
| PageRank / `page_rank` | Supported | Directed, unweighted | Two selected-endpoint passes build incoming CSR and outgoing degree | One incoming source ID per selected edge during computation | Current/next `f64` score arrays and outgoing degree | `node`, `score` |
| K-core decomposition / `k_core_decomposition` | Supported | Undirected multigraph | Endpoint degree pass followed by repeated base forward/backward adjacency | No copied edges | Degree bins, positions, vertex order and coreness | `node`, `core` |
| Louvain community detection / `louvain` | Supported | Undirected, unit relationship weight | Two selected-endpoint passes build an undirected weighted CSR; later phases aggregate communities | Two weighted neighbor entries per non-self relationship during computation | Community assignment, community weights and sweep scratch | `node`, deterministic compact `community_id` |

Betweenness centrality, triangle counting, label propagation, spanning trees and other analytics are
not selected by this document. Adding one requires an explicit algorithm contract and a row in the
inventory before implementation.

## 3. Shared logical architecture

### 3.1 `BoundGraphSelection`

The implemented binder resolves the two table-name arguments into data-only IR:

```rust
struct BoundGraphSelection {
    node_tables: Vec<TableId>,
    rel_tables: Vec<BoundRelSelection>,
}

struct BoundRelSelection {
    group: TableId,
    table: TableId,
    source_domain: u32,
    destination_domain: u32,
}
```

Node tables retain caller order, which is also result-scan order absent an explicit `ORDER BY`.
Relationship-group names expand to their physical endpoint-pair members in declaration order.
Dense source/destination domains point into `node_tables`; the bound form owns no copied nodes,
edges, algorithm state, storage guard or connection-local lifecycle.

All six binders evaluate the first two `LIST<STRING>` arguments as statement constants and reject:

- wrong argument counts or types, including non-string list elements;
- duplicate or unknown node/relationship table names; and
- a selected relationship member whose source or destination node table is absent.

`page_rank` additionally accepts the positional tuple `(damping_factor DOUBLE, tolerance DOUBLE,
max_iterations INT64, normalize_initial BOOL)`, defaulting to `(0.85, 1e-7, 20, true)`.
`louvain` additionally accepts `(max_iterations INT64, max_phases INT64)`, defaulting to
`(20, 20)`. These options must be statement constants; PageRank rejects non-finite/out-of-range
numeric values and both algorithms reject negative iteration limits.

Filtered row selections and algorithm property/weight expressions remain planned extensions to this
same IR, not hidden string predicates. When added, they must reject writes, subqueries, sequence
calls and other non-local side effects and must evaluate each candidate-row predicate once.

### 3.2 Plan and crate boundaries

The intended dependency and execution flow is:

```text
parser / Rust API
    -> typed CALL arguments + YIELD
koko-binder
    -> BoundGraphSelection + GraphAlgorithmPlan
koko-ir / koko-planner
    -> PlanOp::GraphAlgorithmScan
koko-processor
    -> snapshot-aware selection and physical-input lowering
koko-algorithm
    -> pure kernels over typed batches/arrays
koko-processor
    -> node identity + typed scalar DataChunks
```

`koko-algorithm` depends only on `koko-common`. It does not bind names, inspect catalogs, read
MVCC metadata, materialize `Value::Node`, or know about `InMemStorage`. The processor owns the
adapter from one concrete `InMemStorage` statement snapshot to the input form declared by the
algorithm.

Dispatch is static. The implemented enum has one typed variant per algorithm:

```rust
enum GraphAlgorithmPlan {
    KCoreDecomposition(KCorePlan),
    Louvain(LouvainPlan),
    PageRank(PageRankPlan),
    StronglyConnectedComponents(StronglyConnectedComponentsPlan),
    TopologicalLevels(TopologicalLevelsPlan),
    WeaklyConnectedComponents(WeaklyConnectedComponentsPlan),
}
```

There is no generic Pregel/BSP runtime. Kernels share narrow storage visitors, dense-ID conventions,
memory accounting and cancellation cadence while retaining specialized state transitions.

### 3.3 Query integration

The parser's general in-query `CALL` model carries `Vec<Expr>` typed arguments and an optional
`YIELD` list. The binder resolves the generated function identity and declared output schema before
constructing the dedicated graph-algorithm scan:

```cypher
CALL topological_levels(
  ['Task'],
  ['DependsOn']
)
YIELD level AS depth, node AS task
WHERE depth > 0
RETURN task.name, depth
ORDER BY depth, task.name
```

The complete built-in surface is:

```cypher
CALL topological_levels(node_tables, rel_tables) YIELD node, level
CALL weakly_connected_components(node_tables, rel_tables) YIELD node, component_id
CALL strongly_connected_components(node_tables, rel_tables) YIELD node, component_id
CALL page_rank(node_tables, rel_tables) YIELD node, score
CALL page_rank(node_tables, rel_tables, damping_factor, tolerance, max_iterations, normalize_initial)
  YIELD node, score
CALL k_core_decomposition(node_tables, rel_tables) YIELD node, core
CALL louvain(node_tables, rel_tables) YIELD node, community_id
CALL louvain(node_tables, rel_tables, max_iterations, max_phases) YIELD node, community_id
```

The table-name arguments may be list literals or direct-execution parameters whose values are
constant for the statement. Node and relationship names resolve in the currently selected Koko
graph. Omitting `YIELD` exposes both declared outputs in order. An explicit list selects either or
both outputs by name in caller-written order; aliases replace source names and omitted outputs are
not in scope. The immediate `WHERE` sees the selected aliases. Output selection does not restrict
the graph input or change the algorithm's one-row-per-selected-vertex cardinality.

Duplicate selections, duplicate exposed names, unknown outputs and collisions with incoming
variables are binder errors. `YIELD *` is not supported. These are the same contracts as every other
row-producing `CALL`.

`PROJECT_GRAPH` is not an intermediate step. If reusable named selections are selected later, they
store only a declarative graph-selection specification and rebind for each invocation; they do not
own CSR, results or algorithm state.

A graph-algorithm scan performs its selection and computation on first pull, withholds rows until
successful completion, then emits at most `VECTOR_CAPACITY` rows per pull. It writes original
`InternalId`s into normal entity slots; existing projection code materializes node properties only
when requested.

## 4. Physical graph-input strategies

The algorithm declares one physical requirement. The planner/processor lowers the same logical graph
selection without virtual dispatch.

| Strategy | Transient edge data | Use when | Algorithms |
|---|---:|---|---|
| Per-source base adjacency | None beyond existing path state | Traversal is correlated, bounded or likely to terminate early | Current path modes |
| Selected endpoint stream | None | Kernel consumes every selected edge once and does not revisit neighborhoods | WCC |
| Base adjacency plus degree/state | Optional node/relationship visibility bitsets | Kernel needs neighbor access but does not need a copied topology | Topological levels, k-core, SCC |
| Transient incoming CSR | `offsets[V+1]`, `sources[E]`, `out_degree[V]` | Repeated directed pull sweeps | PageRank |
| Transient undirected weighted CSR | `offsets[V+1]`, `neighbors[2E]` containing dense ID and weight | Repeated undirected neighborhood sweeps and community aggregation | Louvain |

### 4.1 Dense vertex identity

Koko's external identity remains `InternalId { table_id, offset }`. A materialized or compact
algorithm input may assign `DenseVertexId` for array indexing.

For full unfiltered node tables, use affine table ranges:

```text
dense_id = table_base + node_offset
```

This needs no per-vertex hash map or reverse vector; table ranges recover the original identity.
Deleted/invisible slots may be represented by an active bitset when their density is low.

For filtered selections, use an array-indexed physical-offset-to-dense map plus a reverse identity
vector. Do not use `HashMap<InternalId, _>` in edge loops. The physical lowering may retain sparse
selected endpoint pairs instead of rescanning a much larger candidate relationship table.

Use statically dispatched 32-bit dense IDs when the selected vertex count fits and 64-bit IDs
otherwise; never branch on ID width inside an edge loop.

### 4.2 Narrow storage access

The implemented storage path does not construct algorithm input through general
`scan_rel_batch`, which would write relationship ID, source ID and destination ID vectors. Narrow
visitors stream visible endpoint offsets or one source's visible neighbors under the existing read
guard, with allocation-free all-visible fast paths. The processor maps offsets to dense IDs and
excludes edges whose endpoint is not visible without allocating per-edge `Value` or
`BatchNeighbor` records.

This is not a storage-backend trait: it is an explicit read path on the one supported
`InMemStorage`, consumed only by the processor's algorithm lowering.

## 5. Cost model

Let:

- `P` be physical node slots in selected node tables;
- `V` be admitted vertices;
- `R` be candidate relationship rows in selected relationship tables; and
- `E` be admitted edges.

With 32-bit dense source IDs and 64-bit offsets/degrees:

| Compiled structure | Incremental transient bytes |
|---|---:|
| PageRank incoming CSR plus outgoing degree | approximately `16V + 4E` |
| Louvain undirected weighted CSR with the current 16-byte neighbor record | approximately `8V + 32E` |
| Straightforward filtered vertex maps | approximately `4P + 16V` |
| Relationship visibility bitset | `R / 8` |

The topological kernel allocates no `E`-sized topology. For `P <= u32::MAX`, its tracked array
capacity is approximately `20P` bytes: `8P` indegrees, a `4P` frontier queue and `8P` retained
levels. WCC retains one `i64` result per physical slot and uses transient parent/rank arrays. SCC
uses iterative vertex stacks and storage adjacency cursors rather than copied edges. K-core retains
one `u64` result per slot and uses transient degree-bin/order state. A statement view with
deleted/invisible node slots adds a `P / 8` visibility bitmap; table-domain mappings and
relationship fast-path flags are proportional to the number of selected tables.

Examples, excluding vertex maps and algorithm state:

| Selected graph | Directed CSR | Forward + reverse CSR | Undirected CSR |
|---:|---:|---:|---:|
| 1M vertices / 10M edges | 48 MB | 96 MB | 88 MB |
| 10M vertices / 100M edges | 480 MB | 960 MB | 880 MB |

A low-peak two-pass directed-CSR build over Koko's current 16-byte endpoint identities moves a bulk
minimum of about `68E` bytes when every candidate edge is admitted and already known visible. MVCC
or predicate selection raises the main endpoint/timestamp/mask traffic to about `84E` bytes before
predicate-property reads, degree-counter cache traffic and dense-ID mapping. A compact CSR generally
recovers that build cost after roughly two to five complete edge sweeps; it is therefore mandatory
for repeated-sweep PageRank/Louvain-style kernels and normally wasteful for WCC/topological leveling.

For dense selections, count degrees in pass one, prefix-sum offsets, and fill CSR in pass two without
an `E`-sized staging edge list. For sparse selections, retaining selected dense endpoint pairs may be
cheaper than rescanning all `R` candidates. Every allocation and temporary bitset is charged to the
statement memory tracker.

## 6. Algorithm contracts

### 6.1 Layered topological sort / topological levels

- Preserve edge direction.
- Sources and isolated vertices have level `0`.
- For every other vertex, `level(v) = 1 + max(level(u))` over selected edges `u -> v`.
- Parallel edges increment and decrement indegree separately.
- A self-loop is a cycle.
- If Kahn processing visits fewer than `V` vertices, return a categorized runtime error and no rows;
  do not claim every residual vertex is itself in a cycle.
- Level values are deterministic across worker counts; row order within a level is unspecified
  without `ORDER BY`.
- Count indegrees with one narrow MVCC-visible endpoint pass, then decrement them through existing
  forward adjacency; do not copy selected edges.
- Use one FIFO queue with a layer boundary, 32-bit entries when the dense address space fits, and
  affine table ranges with logarithmic domain lookup.
- The initial implementation is serial. Parallel work, if justified by profiles, is confined within
  one layer and uses a barrier before constructing the next layer.

### 6.2 Weakly connected components

- Ignore selected relationship direction.
- Parallel relationships and self-loops do not change component membership.
- Consume selected endpoint batches directly into union-find; do not materialize CSR.
- Canonicalize components by their minimum member `InternalId`, then assign compact integer IDs in
  ascending canonical-member order so results do not depend on worker scheduling.

### 6.3 Strongly connected components

- Preserve edge direction.
- Use iterative rather than recursive DFS to avoid call-stack limits.
- The initial direct lowering uses existing forward and backward adjacency plus selection masks.
- A compact forward/reverse CSR lowering may replace it only when a filtered workload and profile
  demonstrate a win.
- Canonicalize component IDs by minimum member `InternalId`, as for WCC.

### 6.4 PageRank

- Preserve edge direction; parallel relationships contribute separately and self-loops are ordinary
  edges.
- The supported surface is unweighted.
- Build incoming CSR and outgoing degree once, then perform deterministic serial pull updates.
- Redistribute dangling-node mass across all selected vertices.
- Defaults are damping `0.85`, tolerance `1e-7`, maximum iterations `20`, and normalized initial
  scores; the six-argument overload overrides all four positionally.
- Reject damping outside `[0, 1)`, negative tolerance/iterations, and non-finite damping/tolerance.
- Use Ladybug's strict total-L1 convergence test and iteration boundary, with Koko's explicit
  dangling-mass correction.

### 6.5 K-core decomposition

- Treat selected relationships as an undirected multigraph.
- Parallel relationships contribute separately to degree; a self-loop contributes two.
- Initialize degree once, then use a degree-bin/peeling kernel over base adjacency.
- Return each selected vertex's maximum core number.
- CSR is not the default because each admitted adjacency entry is consumed only a bounded number of
  times; a profile is required to replace direct adjacency.

### 6.6 Louvain community detection

- Treat selected relationships as an undirected unit-weight multigraph; weighted input is not part
  of the supported surface.
- Build compact undirected weighted CSR because optimization performs repeated neighborhood sweeps.
- Visit vertices and resolve equal modularity gains in ascending original `InternalId` order.
- Use deterministic serial local-move sweeps and community aggregation with a `1e-6` modularity
  threshold.
- Defaults are 20 local-move sweeps per phase and 20 aggregation phases; the four-argument overload
  overrides both positionally.
- Canonicalize final community IDs by the ascending minimum original member identity.
- Parallel Louvain remains evidence-gated because scheduling changes can alter the selected local
  optimum.

## 7. Shared execution invariants

Every whole-graph algorithm must:

1. bind against one immutable catalog generation and execute against one captured MVCC read view;
2. exclude a relationship when either endpoint is excluded;
3. reuse one node-visibility decision across edge passes; future row predicates must likewise be
   evaluated once per candidate row and reused;
4. check cancellation/deadline at bounded vertex/edge intervals and every frontier/sweep boundary;
5. account for topology, maps, masks, queues, heaps, per-worker state and result arrays;
6. avoid `Value`, node materialization and string conversion in kernel loops;
7. produce the same semantic values for `KOKO_THREADS=1` and greater worker counts;
8. make no row-order promise without `ORDER BY`;
9. return no partial rows after a cycle, cancellation, memory error or kernel failure; and
10. release temporary topology/state after computation and retained results at statement completion.

## 8. Implementation and verification

| Status | Slice | Architectural proof |
|---|---|---|
| Supported | General typed in-query `CALL`, `BoundGraphSelection`, graph-algorithm scan | One composable language/IR/execution path; no catalog-string special case |
| Supported | Topological levels | Base-adjacency plus indegree lowering, layered frontier, cycle error, typed result source |
| Supported | WCC | Endpoint-stream union-find and deterministic component canonicalization |
| Supported | SCC | Allocation-free resumable forward/reverse storage cursors and iterative Kosaraju |
| Supported | PageRank | Incoming CSR, typed numeric options, deterministic pull sweeps and dangling redistribution |
| Supported | K-core | Degree-bin peeling over direct undirected adjacency |
| Supported | Louvain | Undirected weighted CSR, deterministic local moves and community aggregation |

The manifested `graph_algorithms.test`, `graph_algorithm_wcc.test`, `graph_algorithm_scc.test`,
`graph_algorithm_page_rank.test`, `graph_algorithm_k_core.test` and
`graph_algorithm_louvain.test` fixtures own query composition, empty/isolated/disconnected shapes,
self-loops, parallel relationships, multi-table selections, deterministic IDs/scores, option and
graph-selection validation, transaction visibility and catalog discovery. Focused
`koko-algorithm` tests own kernel invariants, cancellation cadence, memory admission and release;
storage tests own narrow visitor and adjacency-cursor visibility.
