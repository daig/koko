# IM5 — Rust-embedded core completion

**Status:** complete 2026-07-22. The 2026-07-21 provisional close was reopened after a
plan-vs-code audit found retained-contract gaps outside the frozen corpus/probe matrix; §7 records
the ordered corrections and final fresh gate.

**Authority:** the then-current `../ROADMAP.md` owned the completed milestone and scope decisions.
This file preserves IM5 architecture, dependency order, acceptance criteria, provisional evidence,
correction history, and final stopping evidence. `TRIAGE.tsv` owns the exact residual corpus
inventory. Both IM5 goal prompts are historical. The C++ checkout at `/Users/dai/code/koko` and its
`.test` corpus were the observable-semantics oracle for this plan.

## 1. Product objective and boundary

Complete Koko as an embedded, in-memory Rust graph database. IM5 has five retained product
surfaces:

1. isolated typed and schemaless named graphs;
2. minimal ordered JSON semantics required by schemaless graphs;
3. in-memory primary-key HASH/ART index DDL and introspection;
4. local read-only `icebug-disk` tables; and
5. connection-local native Rust scalar UDF registration.

The Rust `koko` crate is the product API. Existing Rust-native Arrow `RecordBatch` interchange
remains supported, but no foreign ABI or distribution layer is part of IM5.

The following surfaces are owner-deferred until explicitly restored and are not IM5 prerequisites or
completion gates:

- Arrow C Data/C Stream;
- all extension/plugin infrastructure, `INSTALL`/`LOAD`/`UNINSTALL`, custom plugin loading, and all
  official extension modules (ADBC, ALGO, Azure, Delta, DuckDB, FTS, HTTPFS, Iceberg, JSON, LLM,
  Neo4j, Postgres, SQLite, Unity Catalog, and Vector);
- projected-graph extension substrate (`PROJECT_GRAPH`, `PROJECT_GRAPH_CYPHER`,
  `SHOW_PROJECTED_GRAPHS`, `PROJECTED_GRAPH_INFO`, and `DROP_PROJECTED_GRAPH`);
- foreign language bindings;
- shell/CLI, which is a fast follow-up after the embedded core closes;
- external Arrow tables, scan replacement, remote object stores, and connector backends; and
- native durability, database files, WAL/checkpoint/recovery, durable catalog/indexes/MVCC, native
  pages/compression, larger-than-RAM native tables, and physical `storage_info`.

Do not build generalized extension, storage-backend, FFI, connector, or distribution seams “for
later.” A future owner decision must re-scope those capabilities from current evidence.

## 2. Exact starting and ending state

The frozen post-IM4 strict gate is **1763 passed / 343 skipped / 45 exactly triaged failures**, with
no panic file, unparsed file, unledgered deviation, arity panic, or missing/stale/malformed TRIAGE
row. The 45 rows map as follows:

| Capability | Cases | IM5 disposition |
|---|---:|---|
| Named and `ANY` graphs | 8 | implement |
| HASH/ART primary-key index DDL | 5 | implement |
| Local `icebug-disk` | 9 | implement |
| `PROJECT_GRAPH` family | 3 | owner-deferred with extensions |
| Extension lifecycle | 1 | owner-deferred with extensions |
| Native physical `storage_info` | 15 | permanently deferred durability |
| Intentional semantic divergences | 4 | retain in the then-current decision record |

IM5 therefore owns **22 current corpus cases** plus native Rust UDF contracts not represented in the
main corpus. On the frozen corpus, successful closure is expected to be **1785 passed / 343 skipped /
23 exactly owned failures**. The 23 residual rows must be exactly 15 durability, 3 projected-graph,
1 extension-lifecycle, and 4 ledgered-divergence cases. A newly exposed blocker in an active case is
still IM5 work; it may not be hidden by reclassification.

The repeated LSQB gate starts green: all nine answers are correct, no run times out, and every median
Rust/C++ ratio is at most 2×. Preserve it.

## 3. Frozen architecture contracts

### 3.1 Graph ownership and routing

The current database has one catalog/storage/macro tuple. Replace that assumption before adding graph
syntax:

- `DatabaseState` owns an immutable, generation-published graph registry containing `main` and named
  `GraphState`s.
- Each `GraphState` owns its catalog, storage, macros, sequences/types, and graph metadata while
  sharing database configuration, memory tracking, commit timestamp generation, and writer
  admission.
- `ConnectionState` owns the selected graph. `USE GRAPH` on one connection never retargets another.
- Every statement captures one `Arc<GraphState>`, catalog generation, and storage `ReadView` before
  binding. Bind, plan, execute, result materialization, warnings, and introspection use that same
  snapshot.
- An explicit transaction is pinned to the graph selected at `BEGIN`. `USE GRAPH` during an active
  transaction is rejected; no transaction crosses storage domains.
- Allocate table ids database-wide so `InternalId { table_id, offset }` remains unambiguous without
  adding hidden current-graph dependence to values.
- Graph create/drop publishes through the existing coordination and transaction machinery. A
  running query may finish on its captured graph after a drop, but new lookup cannot discover the
  dropped graph. The dropping connection returns to `main`; other selected connections fail safely
  and reset rather than retaining a writable zombie graph.
- Relationships never cross named graphs. Catalog names, PK ownership, DDL reservations, undo,
  commit/rollback, and memory ownership remain atomic under IM4 multi-writer rules.
- Preserve a single concrete in-memory storage implementation. `GraphState` composition is not a
  dormant `StorageBackend` trait.

Original C++ stores the selected graph database-wide. Connection-local selection is the intentional
Rust concurrency contract; land a focused multi-connection regression and a narrow divergence entry
if an observable C++ probe differs.

### 3.2 Typed named graphs

Implement the complete in-memory core contract:

- parse, bind, and execute `CREATE GRAPH [IF NOT EXISTS]`, `USE GRAPH`, and
  `DROP GRAPH [IF EXISTS]` with oracle-compatible case-insensitive lookup, messages, error classes,
  and precedence;
- maintain independent table, type, macro, sequence, and data namespaces per graph;
- permit the same table name and primary-key values in different graphs without collision;
- make all DDL, DML, prepared metadata, table functions, statistics, and result assembly resolve
  through the captured graph;
- make `SHOW_TABLES` and related introspection enumerate graph identity exactly where the oracle
  exposes it;
- define create/drop/use behavior in auto-commit and explicit transactions, including rollback,
  conflicting graph names, dropping the selected graph, and concurrent readers; and
- extend logical `EXPORT DATABASE` / `IMPORT DATABASE` so all retained named-graph catalog/data state
  round-trips into a fresh in-memory database. Do not turn logical export into native persistence.

### 3.3 Schemaless `ANY` graphs and minimal JSON

`CREATE GRAPH name ANY` uses an internal representation equivalent to:

- `_nodes(id SERIAL, label STRING[], data JSON)`; and
- `_edges(_id INTERNAL_ID, label STRING, data JSON)`.

The tables remain internal implementation details. Implement:

- unlabeled, single-label, and conjunctive multi-label node matching;
- dynamic node and relationship creation;
- dynamic property access in projection, pattern maps, and `WHERE`;
- native scalar comparison and return values for dynamic properties;
- `n.*`, relationship values, labels, ids, NULL/missing-property behavior, deterministic property
  order, updates/deletes, transactions, and result formatting required by the core corpus; and
- isolation between `ANY`, typed named, and `main` graphs.

Add only the JSON machinery needed for this core contract:

- a distinct `LogicalType::Json` and runtime value backed by ordered UTF-8 JSON;
- insertion-order-preserving objects, arrays, scalars, and NULL;
- conversion between JSON properties and existing native `Value`s;
- hashing/equality, the core `STRING`↔`JSON` parse/cast contract (including
  `CAST('{}' AS JSON)`), and casts needed by dynamic property predicates;
- Rust result extraction, memory accounting, and logical export/import round-trip; and
- Rust-native Arrow mapping consistent with the existing owned `RecordBatch` API.

Do not implement the JSON extension's `json_*` functions, JSON file scanner, additional extension
casts, or extension test suite.

### 3.4 Core HASH/ART primary-key index DDL

The engine already maintains a type-complete MVCC-aware PK map. Add the observable core index
contract without inventing a second unmeasured index engine:

- parse, bind, and execute `CREATE INDEX`, `CREATE HASH INDEX`, `CREATE ART INDEX`, and
  `DROP INDEX`, including `IF [NOT] EXISTS` forms supported by the oracle;
- represent graph-scoped index name, table, property, logical kind, default/explicit status, and
  catalog visibility;
- match default primary-key index creation, duplicate/missing-name behavior, node-PK-only
  restrictions, option validation, COPY interaction, transaction rollback, and exact error channel;
- make `SHOW_INDEXES` snapshot-correct and graph-aware; and
- route point lookup through the existing PK map for both HASH and ART logical kinds.

A physically distinct ART, FTS, vector index, durable index, optimizer project justified only by one,
or speculative index abstraction is out of scope. Add a distinct ART only after a new measured
workload proves an observable need.

### 3.5 Local read-only `icebug-disk`

`icebug-disk` is original core storage functionality, not an extension. Add the narrow immutable
columnar path required by the nine owned cases:

- parse and bind table `WITH (storage=..., format='icebug-disk')` options and central path
  resolution;
- scan node properties from `nodes_<table>.parquet` in bounded projected batches;
- scan relationship CSR files `indices_<table>.parquet` + `indptr_<table>.parquet`, and flat
  `rels_<table>.parquet` where required;
- validate schema, endpoints, primary keys, row counts, and `icebug_disk_version` metadata;
- support forward, backward, undirected, multi-hop, recursive, filtered, joined, aggregate, and
  larger-than-one-vector reads exercised by the corpus;
- preserve immutable-table semantics and reject ALTER/write/COPY operations and mixed native/
  `icebug-disk` relationship endpoints through the oracle channel;
- integrate cancellation, tracked temporary memory, projection, worker limits, and deterministic
  cleanup; and
- return deterministic missing/cannot-open errors for absent local or unsupported remote sources.

Do not add HTTP/S3/Azure readers, credentials, object-store dependencies, a generalized VFS plugin,
or native database-file behavior.

### 3.6 Native Rust scalar UDFs

Provide a safe Rust embedding API independent of extensions:

- connection-local register and remove operations;
- explicit name, parameter `LogicalType`s, result type, and NULL policy;
- `Send + Sync + 'static` Rust callbacks receiving typed values and returning `Result<Value>`;
- binder overload/arity/type checking and collision rules that cannot silently replace builtins;
- compiled expressions retain an `Arc` to the resolved callback rather than performing a name lookup
  for every row;
- registration generations invalidate/rebind prepared statements safely;
- callback errors use the query error channel; callback panics are caught at the boundary, roll back
  the current statement/transaction as required, and never poison connection/database locks;
- query cancellation/deadline checks surround callback execution; and
- registration/removal on one connection is invisible to peers and already-running query snapshots.

Scalar UDFs only: no aggregate, table, vectorized, foreign-language, dylib, extension, or source
registration API.

## 4. Dependency-ordered landings

### L1 — graph-state substrate

Introduce graph ids/registry/state, database-wide table-id allocation, connection selection, captured
query/transaction graph snapshots, and graph-aware coordination. Migrate every existing `main`
callsite without changing single-graph results.

**Gate:** existing focused database/transaction/prepared/storage tests and standing `agg`, `match`,
and `lsqb` outputs remain unchanged; deterministic tests prove connection-local graph selection,
query snapshot lifetime, and no cross-graph identity collision.

### L2 — typed named graphs

Land parser/AST/binder/execution, graph-scoped DDL/DML/introspection, transaction behavior, and
logical export/import. Retire the typed named-graph portion of the eight graph cases.

**Gate:** all typed cases in `test/test_files/graph/graph.test` pass against the C++ oracle; focused
multi-connection and rollback tests cover behavior the corpus does not.

### L3 — minimal JSON and `ANY`

Land ordered JSON core values, internal schemaless tables, dynamic labels/properties, formatting,
and interchange. Retire the remaining named-graph cases.

**Gate:** `graph/any.test` and `graph/any_graph.test` pass; ordered output, multi-label matching,
dynamic property predicates, transaction rollback, memory ownership, and typed/ANY isolation have
focused regressions. Main strict residual is 37 if later landings have not completed.

### L4 — HASH/ART DDL

Land index grammar, metadata, DDL, introspection, errors, and existing-PK-map execution. Retire all
five `create-index-ddl` TRIAGE rows.

**Gate:** every affected `ddl/ddl_empty` case passes, graph-scoped indexes remain isolated, and PK
lookup/DDL rollback tests stay green. Main strict residual is 32 if L5 has not completed.

### L5 — local `icebug-disk`

Land node, CSR/flat relationship, and mixed-table validation paths over existing Parquet/batch
primitives. Retire all nine `ice_disk` rows.

**Gate:** every affected `ice_disk` and `demo_db_icebug_disk` case passes; repeated scan,
cancellation, malformed/missing file, projection, graph traversal, and low-memory failures are
catchable and leak-free. Main strict residual is exactly 23.

### L6 — native Rust scalar UDFs

Land the connection-local public API, binder overlay, compiled callback, prepared-generation, NULL,
error, panic, isolation, and documentation contracts.

**Gate:** focused public-API tests cover zero/multiple args, scalar/nested types, NULL policies,
wrong return values, duplicate/remove, prepared statements, callback error/panic, transaction
rollback, concurrent connections, and already-running snapshots. No extension registry or generic
plugin seam appears.

### L7 — full closure

Run the complete verification contract, synchronize status/evidence, remove temporary artifacts, and
commit a clean closure. Do not relabel a failed active case as deferred.

## 5. Verification and stopping criteria

IM5 is complete only when all of the following hold in one fresh close run:

1. **Retained behavior:** typed/`ANY` graphs, minimal JSON, HASH/ART DDL, local `icebug-disk`, and
   native Rust scalar UDFs work end to end; no retained syntax ends in a stub, fake fallback, silent
   no-op, or metadata-only implementation.
2. **Exact main corpus:** `scripts/goal_gate.py --strict` reports the frozen-equivalent
   **1785 passed / 343 skipped / 23 failed**, zero panics, zero unparsed files, zero unledgered
   differences, zero arity panics, and no missing/stale/malformed TRIAGE row. The 23 failures are
   exactly 15 durability, 3 projected-graph, 1 extension-lifecycle, and 4 ledgered divergences.
3. **Oracle discipline:** affected graph/index/`icebug-disk` statements are re-probed against the
   C++ shell; new/changed P0 fixtures pass `docs/fable-audit/p0_to_probe.py`; any intentional
   connection-local graph difference has a narrow decision and regression.
4. **Regression invariants:** default versus `KOKO_NO_OPTIMIZE=1` and `KOKO_THREADS=1` outputs remain
   byte-identical for the standing and affected suites except an exact documented scheduling-only
   control.
5. **Resources and concurrency:** cancellation, timeout, memory exhaustion, rollback, graph drop,
   multi-writer conflicts, UDF panic/error, and repeated `icebug-disk` scans leave no partial
   mutation, stale reservation, poisoned connection, or leaked source/query state.
6. **Performance:** `scripts/perf_gate.py` retains nine correct answers, no timeout, every median
   Rust/C++ ratio at most 2×, and the preserved q4/q5/q7 wins. Performance changes are
   profile-justified; no speculative optimizer or parallel subsystem lands.
7. **Workspace:** release build/tests, `cargo test --workspace`, `cargo fmt --all --check`, strict
   workspace Clippy, focused public API tests, and all 52 corpus directories are green under their
   exact active/deferred contract.
8. **Closure evidence:** `ROADMAP.md`, `README.md`, `AGENTS.md`, `docs/PROGRESS.md`,
   `docs/TRIAGE.tsv`, and performance evidence agree; generated
   probes/exports are removed; every landing and final closure is committed with the required
   repository trailer; the working tree is clean.

Do not declare completion after planning, parser scaffolding, a narrow test, or only retiring the 22
visible corpus cases. Native UDFs and the focused ownership/concurrency/resource contracts are equal
completion requirements.

## 6. Provisional close evidence — 2026-07-21

- Every retained statement selected by the graph, ordered-JSON/`ANY`, HASH/ART, local
  `icebug-disk`, and native scalar-UDF corpus/focused matrix passed. Focused public-API/resource
  coverage passes 9 IM5 tests; the affected upstream graph, index, `ice_disk`, and demo cases pass.
- `python3 scripts/goal_gate.py --strict` is green: **1785 passed / 343 skipped / exactly 23
  failed**; 0 panic files, 0 unparsed files, 0 unledgered differences, 0 arity panics, and 0
  missing/stale/malformed TRIAGE rows. P0 is 43 clean / 5 ledgered / 4 skipped; the deviation
  battery is 143 probes / 75 ledgered differences / 0 unledgered.
- Differential probes cover 24 graph/JSON/index statements and 7 local `icebug-disk` statements
  with 0 C++ mismatches. Default, `KOKO_NO_OPTIMIZE=1`, and `KOKO_THREADS=1` outputs are
  byte-identical over the standing and affected suites.
- The repeated nine-query LSQB gate has 9 correct answers, no timeout, and median Rust/C++ ratios
  q1 0.494853, q2 0.369108, q3 1.053812, q4 0.004162, q5 0.010752, q6 0.670381,
  q7 0.282723, q8 1.308519, and q9 0.677366. The q4/q5/q7 wins remain.
- Release workspace build/tests, debug workspace tests, all 52 corpus directories, strict workspace
  Clippy, and formatting are green. Ordered implementation commits include `57f87a3` (graph-state
  ownership plus graph/JSON/index/`icebug-disk` surfaces), `a4d3046` (native scalar UDFs),
  `47baae4` (focused retained-surface contracts), and `2b9c74b` (transaction-oracle preservation);
  `7c91faa` synchronizes the close scorecard and deferred boundary.

## 7. Post-close reassessment, correction gate, and final closure

The provisional §6 gate was real but incomplete: it proved the 22 visible corpus rows and nine
focused contracts, not every retained acceptance criterion in §§3–5. A fresh implementation audit
and new probes found three gaps, all closed by the correction landings below.

### Historical resume point and cause classification

The implementation baseline was the graph/JSON/index/icebug landing `57f87a3`, UDF and focused
contract landings `a4d3046`/`47baae4`/`2b9c74b`, and provisional closure `7c91faa`. Reassessment
commit `35c61d0` reopened IM5 before R1, R2, or R3 correction code existed. The correction preserved
the factual 1785/343/23 corpus result, the nine-query ≤2× LSQB result, and every IM1–IM4 contract.

The failures were not scope deferrals:

- R1's separate interpreter and R3's eager load were implementation shortcuts that were
  accidentally accepted as satisfying broader retained contracts.
- R2 was an integration omission: the IM3 exporter remained graph-local and its placeholder
  `index.cypher` remained empty.
- The exact 23 TRIAGE rows are unrelated expected corpus residuals. Never lower that number by
  faking physical-storage rows, starting extension/GDS work, or changing intentional IDs.

Three architecture decisions are frozen for the correction:

1. `ANY` finishes on the ordinary binder/planner/processor/MVCC path, not a second ever-growing
   interpreter.
2. Logical database interchange operates at graph-registry ownership, not inside one `GraphData`.
3. `icebug-disk` uses a closed concrete in-memory-or-icebug scan dispatch, not eager hydration,
   generic backend/plugin scaffolding, or remote-source scope.

### R1 — schemaless execution, errors, and immutable snapshots

- `Connection::execute_any_query` clones the complete committed `AnyGraphData` for every query and
  transaction before execution. The clone is not admitted through `MemoryTracker`, so large or
  concurrent reads violate the generation-published snapshot and tracked-memory contracts.
- The separate `any_graph` evaluator ignores return modifiers (`DISTINCT`, `ORDER BY`, `SKIP`,
  `LIMIT`), returns `NotImplemented` for ordinary `WITH`, `UNION`, and `MERGE`, and uses
  `unwrap_or(false)`/`matches!(Ok(...))` predicate paths that turn evaluation and native-UDF errors
  into a false predicate.
- Fresh C++ differential evidence: the 10-statement breadth probe has 6 mismatches; a 5-statement
  modulo/type-error probe has 2 mismatches. `ORDER BY` produces insertion order, `LIMIT` is ignored,
  empty `OPTIONAL MATCH` returns 0 instead of 1, valid `WITH`/`UNION`/`MERGE` are rejected, and
  modulo-by-zero/type errors silently return count 0.

Reproduce the breadth result, in one connection, with:

```cypher
CREATE GRAPH g ANY;
USE GRAPH g;
CREATE (:N {name: 'B'}), (:N {name: 'A'});
MATCH (n:N) RETURN n.name ORDER BY n.name;
MATCH (n:N) RETURN n.name ORDER BY n.name LIMIT 1;
OPTIONAL MATCH (n:Missing) RETURN count(*);
MATCH (n:N) WITH n WHERE n.name = 'A' RETURN n.name;
MATCH (n:N) RETURN n.name UNION ALL MATCH (n:N) RETURN n.name;
MERGE (:N {name: 'C'});
MATCH (n:N) RETURN n.name ORDER BY n.name;
```

Reproduce the error-channel result after creating one `:N` row with:

```cypher
MATCH (n:N) WHERE 1 % 0 = 0 RETURN count(*);
MATCH (n:N) WHERE abs('bad') > 0 RETURN count(*);
```

The close regression must compare ordered output where the statement requests order; the generic
diff probe's multiset comparison alone does not detect the first `ORDER BY` violation.

**Correction:** create hidden internal `_nodes(id, label STRING[], data JSON)` and
`_edges(_id, label STRING, data JSON)` catalog/storage tables with each `ANY` graph. Extend the
ordinary binder to resolve dynamic labels to those tables, lower conjunctive labels to list
predicates, lower dynamic properties to ordered-JSON extraction/update expressions, and bind
dynamic create/set/delete/merge through normal typed mutations. Run all clauses, expressions,
UDFs, planning, MVCC, cancellation, and result assembly through the existing processor. Migrate
every caller and remove `AnyGraphData`, `AnyGraphState`, `Connection::execute_any_query`, and the
dedicated interpreter. An interim `Arc` copy-on-write snapshot is allowed only to keep a landing
safe while migrating; it is not an R1 exit state.

**Gate:** the two fresh probes are order-aware and byte-identical to C++; focused tests cover
ordered/distinct/limited projection, optional, `WITH`/`UNION`/`MERGE`, callback/runtime error
propagation, dynamic create/update/delete, prepared statements, concurrent snapshot/drop, rollback,
cancellation/deadline, and deterministic low-memory failure on non-trivial `ANY` data. No
schemaless query execution branch, whole-graph read clone, or silent unsupported-clause fallback
remains.

### R2 — portable database-wide graph/index interchange

- `EXPORT DATABASE` passes only the currently selected graph to `export_database`.
- A selected `ANY` graph has no typed catalog, so the emitted `schema.cypher` and `copy.cypher` are
  empty and no node, relationship, label, property, or JSON data is written.
- `index.cypher` is unconditionally empty, including after explicit `CREATE HASH/ART INDEX`.

**Correction:** intercept logical export/import at `Connection`/`DatabaseState`, before execution is
narrowed to one selected `GraphData`. Capture one graph-registry generation and shared read
timestamp, then write a versioned deterministic manifest with separate graph-name/kind namespaces
and per-graph typed/`ANY` catalog/data files. Preserve macros/types/sequences, relationship groups,
and explicit index name/type/property metadata; default PK indexes are recreated by table DDL.
Import into detached graph states with database-wide table-id allocation and memory admission,
validate every file first, then publish the complete registry atomically. Do not recursively invoke
`Connection::query` while schema/database locks are held or mutate the live registry incrementally.

**Gate:** export a database containing non-empty `main`, typed, and `ANY` graphs plus HASH and ART
indexes; import into a fresh `Database`; compare graph inventory, introspection, and representative
queries byte-for-byte. Cover export from every selected graph, graph/file-name collisions,
overwrite/preflight failure, cancellation, low memory, concurrent graph drop/write snapshots, and
atomic rollback of all staged state. Selection must remain connection-local and deterministic.

### R3 — genuine local read-only `icebug-disk` scans

- `load_node_table` and `load_rel_table` decode the complete Parquet/CSR source during table DDL,
  append the rows to `InMemStorage`, mark that ordinary storage immutable, and retain no runtime
  external-table scan state.
- A direct probe created an `icebug-disk` table, removed its source directory, and still returned
  all rows. Query-time cancellation, projection, worker limits, and temporary-memory accounting
  therefore do not govern file reads; table creation is eager logical import.

**Correction:** retain a validated immutable `IcebugDescriptor` in the catalog and dispatch scans
through a closed enum such as `TableScanSource::{InMemory, Icebug}`. The processor already depends
on `koko-loader`; reuse its bounded Parquet reader without adding a generic backend. Node scans read
only projected columns. Relationship scans read CSR `indptr`/`indices` ranges or flat
`rels_*.parquet` batches and expose the same forward/backward/undirected neighbor contract used by
normal extend/recursive operators. Pin source descriptors/file handles for the query snapshot and
perform cancellation, deadline, worker, and tracked-memory checks between batches. Never hydrate a
whole table at DDL or first query, populate whole-table temporary storage, add remote/VFS behavior,
or weaken mutation rejection.

**Gate:** repeated and concurrent scans depend on the source files, project only requested columns,
honor cancellation/deadline/worker/memory controls during reads, traverse CSR and flat
relationships correctly in every direction, and fail deterministically on removal, truncation,
schema/endpoint/row-count/CSR/version corruption without partial catalog/storage publication.

### R4 — fresh closure — complete 2026-07-22

Execute the correction in this exact order:

1. R1 internal storage/snapshot representation and binder lowering;
2. R1 migration to the normal planner/processor plus deletion of the interpreter;
3. R2 database-level typed/`ANY`/index interchange;
4. R3 query-time node and relationship external scans; and
5. one fresh complete §5 close run.

Permanent regressions cover every correction landing. The strict corpus target remains
**1785 / 343 / 23** because `TRIAGE.tsv` is a corpus manifest, not an exhaustive feature ledger.
No fix became a compatibility waiver.

The original migration roadmap's native durable store remains permanently deferred. Its broader P5
ecosystem/binding/extension scope, multi-database attach/auth inventory, and lazy/async API ideas
remain historical or owner-deferred; this correction did not reactivate them.
`IM5_CORRECTION_GOAL_PROMPT.md` is historical.

### Final close evidence — 2026-07-22

- Ordered landings: `86014ac` hidden-table/normal-pipeline `ANY`; `969a106` schemaless
  semantic/resource closure; `4e81b01` atomic database-wide logical interchange; `1902645`
  query-time local `icebug-disk`; `ad866ad` corpus compatibility.
- `python3 scripts/goal_gate.py --strict` — GREEN: **1785 passed / 343 skipped / exactly 23
  deferred-or-ledgered failures**; zero panic files, unparsed files, corpus-visible unledgered
  differences, arity panics, and missing/stale/malformed TRIAGE rows.
- Focused correction regressions — green: normal-pipeline `ANY` ordering/clauses/errors/
  prepared/transaction/concurrency/cancellation/deadline/memory; complete atomic database
  interchange; pinned projected node and CSR/flat relationship scans including recursive,
  directional, joined, aggregate, multi-vector, corruption, cancellation, deadline, and memory.
- Corpus invariants — green: all 52 directories in the release sweep; default/no-opt/one-worker
  compared modes are byte-identical. Debug workspace tests are green.
- `python3 scripts/perf_gate.py` — PASS: all nine answers correct and timeout-free. Median
  Rust/C++ ratios: q1 0.559096, q2 0.450113, q3 0.990719, q4 0.004810, q5 0.015467,
  q6 0.843355, q7 0.277126, q8 1.217283, q9 0.576137.
- Workspace release build/tests, debug tests (**298 passed / 26 suites**), formatting, and strict
  Clippy are green. Generated probes/exports are absent. The stopping rule is satisfied.
