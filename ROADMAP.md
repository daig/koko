# Koko: current product and roadmap

> **Current as of 2026-08-08.** This is the single authority for Koko's current product boundary,
> architecture map, active work, known limitations, candidate feature tracks, intentional semantic
> decisions, and verification policy. It does not retain completed milestone checklists or fixed-bug
> inventories; the implementation and regression suite are the record of completed work.

## 1. Product direction

Koko is an independent, Rust-first, in-memory Cypher property-graph database. It began as a
clean-room implementation of Ladybug 0.17, but Ladybug is no longer its specification or sequencing
authority. Supported Koko behavior is the non-regression contract unless an explicit product
decision changes it.

Four rules govern current work:

1. **Correctness before compatibility.** Use Ladybug and the historical differential corpus when a
   change intentionally owns compatibility. Never reproduce an upstream crash, corrupt value, or
   incoherent diagnostic merely to erase a difference.
2. **One product and one execution path.** Typed and schemaless graphs, direct and prepared queries,
   transactions, loaders, and first-party clients converge on the same parser/binder/planner/
   processor/storage pipeline. Do not add a second interpreter or speculative backend seam.
3. **Evidence follows the changed contract.** Start with a deterministic regression for the
   observable behavior being changed, then broaden verification only to affected shared surfaces.
4. **No completed-work ledger.** This document contains only the current product, work that remains,
   and decisions that still constrain future behavior. Remove an item when its contract and
   regression land; use source history for chronology.

## 2. Where Koko is now

### 2.1 Product boundary and public feature map

| Area | Current supported surface |
|---|---|
| Product mode | Embedded, in-memory databases created by `Database::new()` or `Database::with_config(...)`; independent database instances and named graphs |
| Graph model | Typed graphs and schemaless `ANY` graphs; node tables, relationship tables and multi-pair relationship groups; graph-scoped schemas, data and indexes |
| Schema and data | Primary keys, defaults, serial columns/sequences, macros, HASH/ART index DDL and introspection; `CREATE`, `MERGE`, `SET`, `DELETE`, table alteration and graph DDL |
| Query language | Koko's Cypher dialect: left-to-right composition of `MATCH`, `OPTIONAL MATCH`, `UNWIND`, typed table-producing `CALL`, and `LOAD FROM`; `WITH`, `RETURN`, `UNION`, nested `EXISTS`/`COUNT` subqueries, variable-length `WALK`/`TRAIL`/`ACYCLIC` paths, aggregation, ordering, pagination, `EXPLAIN` and `PROFILE` |
| Graph algorithms | Correlated `WALK`/`TRAIL`/`ACYCLIC`, unweighted/weighted shortest-path modes, and statement-bound directed `topological_levels` over explicit node/relationship table selections |
| Functions and values | Generated typed scalar/aggregate/algorithm catalog; nested list/array/struct/map/union values, temporal and decimal values, ordered JSON, nodes, relationships, paths and `UINT128` |
| Execution and storage | Typed columnar chunks, pull execution, static operator dispatch, versioned in-memory columns and adjacency, narrow topology visitors, PK lookup, costed planning, hash joins, selected pushdown/decorrelation and scoped source parallelism |
| Transactions and controls | Snapshot reads, autocommit and explicit read/write transactions, concurrent connections, bounded multi-writer conflict detection, atomic rollback, cancellation, deadlines, worker limits and tracked-memory errors |
| Local data movement | CSV and gzip CSV, Parquet and NPY input, native Rust `arrow-rs::RecordBatch` import/export for supported logical types, COPY, CSV/Parquet output, atomic database-wide logical export/import and validated local read-only `icebug-disk` scans |
| Embedded Rust API | `Database`, `Connection`, exclusively borrowing `Transaction`, `PreparedStatement`, owned `Parameter`, connection-local `ScalarFunction`, `InterruptHandle`, `QueryResult`, borrowed `Row`/cell/column views, structured diagnostics and immutable tooling snapshots |
| First-party CLI | Interactive and batch `koko`; parser-aware editing/completion, parameters, graph/transaction status, human and machine formats, atomic output files, cancellation, progress, history and portable logical save/restore |
| Regression surfaces | Workspace unit/integration/doctests, manifested end-to-end Cypher product fixtures with bundled datasets, public API and CLI process/PTY tests, plus optional compatibility/performance evidence |

The product is intentionally eager at its public result boundary: a `QueryResult` owns materialized
columnar buffers, while rows, cells and typed columns borrow them. Koko does not currently expose a
lazy or asynchronous result stream.

### 2.2 High-level architecture

The workspace DAG makes ownership compile-time visible:

```text
koko-common      values, logical types, typed chunks, memory/statistics primitives
    ├─ koko-catalog       private schema/catalog invariants
    ├─ koko-parser        lexer, AST and recursive-descent/Pratt parser
    └─ koko-function      generated identities, signatures and function execution

koko-ir          shared bound semantics, variable IDs, row layouts and logical plans
koko-storage     concrete versioned in-memory columns, adjacency, PK indexes and undo
koko-algorithm   allocation-accounted whole-graph kernels over narrow typed contracts
koko-binder      name/type resolution over parser + catalog + function + IR
koko-expr        bound-expression compilation and typed evaluation
koko-planner     logical planning, pushdown, joins and cost optimization
koko-loader      local readers/writers and external scan protocols
koko-processor   pull operators, typed chunks, controls and parallel execution

koko             public composition root, runtime ownership, adapters and results
    ├─ koko-test-runner   hermetic and optional historical `.test` execution
    └─ koko-cli           first-party interactive/batch terminal client
```

`koko` owns cross-layer orchestration rather than engine algorithms. Its private runtime capsule
owns database, graph, connection, transaction, writer-admission and statement lifecycle state.
Lower crates receive immutable snapshots or narrow read/write capabilities; adapters do not reach
into runtime locks.

Every ordinary statement follows one flow:

```text
public API or CLI
  → connection serialization and statement controls
  → graph/catalog/storage/UDF snapshot + transaction admission
  → parse → bind → plan/optimize → typed pull execution
  → commit/publication or rollback/recovery
  → immutable columnar result + schema/timing/warnings/plan metadata
```

Calls through one `Connection` are serialized. Different connections may overlap. A query captures
one coherent graph and transaction view; successful publication advances only the state owned by
that transaction, while errors and caught panics release writer state and roll back to the correct
savepoint.

Detailed ownership and API contracts remain in
[`docs/FACADE_ARCHITECTURE.md`](docs/FACADE_ARCHITECTURE.md). Observable CLI behavior belongs to
[`docs/CLI_UX.md`](docs/CLI_UX.md), its implementation boundaries to
[`docs/CLI_ARCHITECTURE.md`](docs/CLI_ARCHITECTURE.md), runnable workflows to
[`docs/REPL_USAGE_GUIDE.md`](docs/REPL_USAGE_GUIDE.md), and regression ownership and commands to
[`docs/TESTING.md`](docs/TESTING.md).

## 3. What remains

### 3.1 Work-state vocabulary

- **Next:** selected work; do this before unselected roadmap items.
- **Planned:** Koko intends to close the limitation or add the capability, but no date is implied.
- **Evidence-gated:** implement only after a reproducible workload and profile justify it.
- **Candidate:** a possible product expansion requiring an explicit owner decision before design.
- **Deferred:** outside the current product; do not scaffold prerequisites.
- **Intentional:** a current behavior or limitation that must not be treated as a bug merely because
  Ladybug differs.



### 3.2 Planned gap closure: expression placement

Koko intends to remove these representation-driven restrictions:

- sequence calls in `WITH ... WHERE`;
- list-comprehension or subquery expressions in `ORDER BY` over projected output; and
- general expressions over aggregate output when the expression cannot currently use the
  post-projection order-key path.

The result schema and type of an expression must not depend on operand order or an incidental
planner layout. Evaluation count, sequence side effects, NULL behavior, aliases and ordering need
focused regressions. The owning paths begin in `crates/koko-binder/src/binder/query.rs` and
`crates/koko-processor/src/operator/project.rs`.

### 3.3 Planned language feature: pattern comprehensions

Pattern comprehensions need a deliberate Koko grammar, scope and execution contract. The feature
must define introduced-variable visibility, correlation with the outer row, empty and NULL behavior,
path multiplicity, ordering, cancellation and memory accounting. It must not inherit Ladybug's
ambiguous parse of list-comprehension-like syntax or its unbound-new-variable failure by accident.

Until that design lands, pattern comprehensions remain a categorized clean rejection; the specific
new-variable behavior is recorded as an intentional boundary in section 4.

### 3.4 Supported built-in whole-graph algorithms

Koko supports six built-in graph-algorithm scans over explicit node/relationship table selections:

```cypher
CALL topological_levels(['Task'], ['DependsOn']) YIELD node, level
CALL weakly_connected_components(['Task'], ['DependsOn']) YIELD node, component_id
CALL strongly_connected_components(['Task'], ['DependsOn']) YIELD node, component_id
CALL page_rank(['Task'], ['DependsOn']) YIELD node, score
CALL k_core_decomposition(['Task'], ['DependsOn']) YIELD node, core
CALL louvain(['Task'], ['DependsOn']) YIELD node, community_id
```

Each binds table names against one graph/catalog snapshot, executes against the statement's MVCC
view, and composes through ordinary `YIELD`, `WHERE`, projection, aggregation and ordering. WCC
streams endpoints into union-find; SCC and k-core traverse base adjacency without copying topology;
PageRank builds transient incoming CSR; Louvain builds transient undirected weighted CSR.
Computation is eager and cached per logical scan, with bounded cancellation, tracked transient and
retained allocations, no partial rows on failure, and deterministic values independent of pull
worker scheduling.

PageRank also accepts positional damping, tolerance, maximum-iteration and initial-normalization
options; Louvain accepts positional maximum-iteration and maximum-phase limits. Current path modes
remain separate correlated `MATCH` operators. [`docs/GRAPH_ALGORITHMS.md`](docs/GRAPH_ALGORITHMS.md)
owns the full supported contract, logical graph selection, lowering, cost model and verification.

This surface does not activate `PROJECT_GRAPH`, a named projected-graph registry, topology caching,
extension/plugin lifecycle or an `algo` module. Algorithms bind directly to one captured Koko graph
and MVCC statement view; reusable named selections remain a separate product decision.

### 3.5 Evidence-gated performance work

These are not correctness bugs or scheduled implementation. A landing requires a representative
Koko workload, before/after profiles, fixed result checks and a workload-owned regression threshold.

| ID | Opportunity | Activation evidence |
|---|---|---|
| PERF-01 | Fill typed output directly from adjacency instead of staging general `BatchNeighbor` records | Staging, tagging or gather allocation dominates a supported traversal workload |
| PERF-02 | Bounded DP join enumeration and broader SIP/semi-mask planning | A selective or ≥4-way join produces materially excessive intermediates or runtime |
| PERF-03 | Broader correlated-subquery decorrelation | A supported correlated shape is dominated by repeated seeded-subplan construction |
| PERF-04 | Broader multiplicity/factorization propagation | Materialized fan-out dominates and existing count-only/multiplicity paths cannot express the consumer |
| PERF-05 | Parallel joins, sort, single-source frontiers or a persistent worker pool | A serial operator region or scoped-worker startup dominates a representative workload |
| PERF-06 | Parameter-independent prepared-plan caching | Repeated execution is planning-dominated and a sound parameter-slot plus catalog/UDF invalidation design exists |
| PERF-07 | WCOJ/intersection | A larger cyclic workload demonstrates excessive intermediates from the best available binary plan |

### 3.6 Candidate product expansions

The following separate product-expansion features remain unselected. They may become roadmap work
only after an explicit owner decision defines the user, representation, ownership and verification
contract:

- Arrow C Data/C Stream;
- extension/plugin lifecycle and extension modules;
- projected graphs and GDS lifecycle;
- remote object access, connectors and scan replacement; and
- Python, Node.js, Java, Swift, WASM or other foreign bindings.

Native Rust Arrow remains distinct from Arrow C. Local `icebug-disk` remains a query-time source,
not a connector or native database mode.

### 3.7 Confirmed bugs

No known wrong-result, transaction-safety, resource-recovery or public-API defect on the supported
surface is currently open. A new reproduction belongs above the planned feature work with:

- the smallest deterministic failing scenario;
- the intended Koko behavior;
- the owning source path;
- a focused regression; and
- any compatibility requirement stated explicitly.

Do not create historical milestone buckets or keep the item after the fix and regression land.

## 4. Intentional behavioral decisions

These are current Koko decisions, not waivers for unexplained failures. A change may replace one,
but must update this section and its focused regression in the same landing.

| Decision ID | Koko contract |
|---|---|
| `contextual-null-union-typing` | Bare `NULL` and unresolved direct prepared parameters are bottom/type-variable terms: a concrete UNION peer supplies their result type. Runtime-dynamic `ANY` remains the top type. UNION metadata is operand-order independent. |
| `alter-table-kind-first` | DDL validates the target table kind before relationship-pair details and returns the truthful table-kind error without mutation. |
| `internal-id-numbering` | Internal IDs use Koko's own stable table/offset allocation; Ladybug's catalog numbering is not a compatibility contract. |
| `connection-local-selected-graph` | `USE GRAPH` is connection-local. One connection cannot retarget another, and each running statement retains its captured graph snapshot. |
| `count-distinct-factorized` | `count(DISTINCT x)` counts logical distinct values, not factorized head slots; one repeated value counts as one even where Ladybug returns two. |
| `collect-empty-list` | `collect(x)` ignores `NULL` inputs and returns `[]` whenever an aggregate state has zero non-`NULL` values, including global zero-row input and an existing all-`NULL` group. It does not invent a group when grouping keys have no input rows. |
| `ddl-error-vector` | Valid DDL/DML continues normally where Ladybug enters its opaque `Error: vector` state; Koko preserves the successful mutation and rows. |
| `pk-float-probe` | Numeric equality and PK lookup agree: an integral PK equal to an exactly representable float literal is found rather than lost by an index-probe mismatch. |
| `acyclic-semantics` | `ACYCLIC` means no repeated node. Koko does not reproduce Ladybug behavior that collapses it toward `WALK` on cyclic data. |
| `factorial-overflow` | Factorial overflow returns a categorized overflow error; it never silently wraps an integer. |
| `sign-negative-zero` | `sign(-0.0)` is `0`, independent of floating-width bit-pattern artifacts. |
| `utf8-byte-slicing` | String slicing always returns valid UTF-8; Koko does not expose invalid continuation-byte strings. |
| `null-any-and-reference-crashes` | Untyped NULL inputs resolve to clean NULL/typed behavior or a categorized binder error. Koko never reproduces Ladybug's internal ANY-vector failures, SIGSEGVs or unreachable branches. |
| `explain-profile-plans` | `EXPLAIN` and `PROFILE` expose Koko's physical plan and presentation. Ladybug operator names, trees and box art are not a contract. |
| `nondeterministic-by-design` | Unseeded `random()` is nondeterministic. Queries without `ORDER BY`, including `LIMIT`, do not promise a particular row identity or physical scan order. Seeded RNG and explicitly ordered results remain deterministic contracts. |
| `pure-semantic-supersets` | Koko may retain coherent Koko-only syntax/functions such as `exp`, `power`, list aliases, one-argument `round`, two-argument `substr`, `INTERVAL * INT`, `WALK` and `INTEGER`; Ladybug rejection alone is not a reason to remove them. |
| `list-comprehension-syntax` | `[variable IN list [WHERE predicate] [\| projection]]` is Koko list-comprehension syntax. The predicate must be boolean, NULL predicates exclude their element, NULL input returns NULL, nested comprehensions use lexical shadowing, and bodies may capture outer-row values. Parenthesize `variable IN list` when it is intended as a list-literal membership expression. |
| `call-yield-name-selection` | A row-producing `CALL` without `YIELD` imports every declared output in declaration order. An explicit nonempty `YIELD` list selects declared outputs by Koko identifier name in caller-written order; each source output may appear once, `AS` replaces its exposed name, and only the selected names enter scope. The immediate `WHERE` sees incoming variables plus those selected names. Unknown outputs, duplicate selections, duplicate exposed names and collisions with incoming scope are binder errors. Selection and aliasing never change source row cardinality or order. `YIELD *` is not supported. |
| `numeric-variadic-extrema` | `greatest`/`least` accept two or more all-numeric arguments, coerce them to Koko's common numeric type, propagate any NULL, and retain the leftmost value on ties. Their inherited DATE/TIMESTAMP overloads remain binary. |
| `aggregate-in-where-orderby` | Aggregates in scalar positions where no aggregate scope exists are rejected during binding. Koko does not execute a global aggregate accidentally or defer the failure to runtime. |
| `subquery-in-recursive-lambda` | `EXISTS`/`COUNT` subqueries and `nextval`/`currval` inside a recursive relationship's per-step lambda are intentionally rejected. Supporting them would require a correlated sub-pipeline inside the frontier loop and is not currently planned. |
| `pattern-comprehension-newvar` | Pattern comprehensions, especially forms introducing a new variable, remain rejected until section 3.3 defines their scope. Koko will not copy Ladybug's ambiguous parse or accidental unbound-variable behavior. |
| `storage-info-physical-counts` | In-memory `storage_info` does not invent page, compression or physical chunk rows. Physical storage introspection belongs only to a separately selected native-storage product. |

Exact probe entries for current intentional differences remain in
`docs/fable-audit/active_divergences.json`; the frozen migration-close battery inventory remains in
`docs/fable-audit/historical_differences.json`. They are machine evidence, not secondary product
policy. Historical audit reports and scorecards remain evidence only.

## 5. Deliberate non-goals and deferred scope

### 5.1 Native durability: permanently outside the current product

Do not start or scaffold:

- native database files or `Database::open(path)`;
- persistent catalogs, sequences, macros, statistics or indexes;
- WAL, checkpoint/recovery, crash replay, durable MVCC or page buffering;
- compression or larger-than-memory native tables; or
- physical page/chunk/index introspection.

Logical export/import is explicit portable interchange, not crash-safe native persistence.
`CHECKPOINT` remains a deliberate in-memory no-op. Restoring native durability requires an explicit
project-owner decision that replaces this boundary.

### 5.2 Explicit representation boundaries

Native Rust Arrow import/export supports its documented logical types. `ANY`, graph entities, paths,
internal IDs and recursive relationships currently return explicit unsupported-type errors rather
than being stringified or losing type information. Expanding this boundary requires a public
representation decision; it is not an implicit bug.

URI sources and remote transports are rejected before local filesystem I/O. Local paths, lists,
globs, home/search-path resolution, CSV/gzip, Parquet, NPY and local read-only `icebug-disk` remain
the supported source boundary until a connector feature is selected.

## 6. Verification policy

1. Run the narrowest deterministic regression that exercises the changed observable contract.
2. Run `cargo test --workspace` for cross-crate, public API, runtime or broadly shared changes; it
   includes every manifested Cypher product fixture and rejects skipped product cases.
3. Run the focused strict CLI gate for behavior visible through the first-party CLI.
4. Keep touched Rust code Clippy- and formatting-clean; check the generated function registry when
   its declarative input or generator changes.
5. Add tests at the owning layer and avoid duplicate assertions unless a second public boundary is
   independently observable. Update the product fixture manifest with every fixture change.
6. Run external corpus, differential, arity or paired-performance tools only when a change owns
   their compatibility, panic-safety or performance contract, or when explicitly requested.

The full taxonomy, placement rules and optional-tool prerequisites are in
[`docs/TESTING.md`](docs/TESTING.md).

`KOKO_NO_OPTIMIZE=1` remains a supported diagnostic escape hatch. Hermetic optimizer fixtures cover
both optimized and naive execution; the historical exhaustive external naive sweep is not a
standing completion requirement.
