# IM2 — columnar engine and stable Rust API

**Status:** complete 2026-07-19. This document is the executed historical contract for IM2.
At its close IM3 was next; IM3 is now also complete, `ROADMAP.md` makes IM4 next, and
`docs/PROGRESS.md` owns the recorded close evidence.

## Goal

Make typed batches the load-bearing storage and result currency, replace hidden storage visibility
state with explicit per-statement handles, and publish a safe Rust embedding API with truthful schema,
prepared-statement, timing, configuration, accounting, and in-memory statistics metadata.

Preserve all existing Cypher semantics, MVCC behavior, transaction rules, factorization wins, and
performance gates. This is a clean cutover: no dual scalar/batch result stores, deprecated aliases,
or abstractions retained solely for a hypothetical durable backend.

## Starting boundary (before IM2)

- `DataChunk`/`ColumnData` already provide typed vectorized execution buffers.
- `InMemStorage` stores property columns as `Vec<Vec<Value>>` behind the single-implementation
  `StorageBackend` trait.
- Storage reads depend on mutable `set_read_view` state, while the processor performs per-cell
  property reads and per-row adjacency extension.
- `ExecResult` materializes `Vec<Vec<Value>>`; `QueryResult` transposes those rows back into columns.
- `PreparedStatement` caches only the parsed AST and exposes no parameter/result metadata.
- `DatabaseConfig` is private; the buffer-pool limit is reporting-only and `bm_info.mem_usage` is
  always zero.
- Per-property `TableStats` exist, but `stats_info` exposes only cardinality and the PK distinct count.
- The database mutex still serializes query execution. IM2 must establish contracts that IM4 can make
  concurrent, but IM2 does not implement or claim reader/writer overlap.

## Fixed scope decisions

### Product boundary

- `:memory:` remains the only database backend.
- Remove or collapse `StorageBackend`; do not keep hot-path dynamic dispatch for a future durable
  implementation.
- Keep `koko-storage` as the owner of in-memory storage contracts and implementation.
- No native database files, WAL, checkpoint/recovery, buffer-manager pages, compression, physical
  `storage_info`, or larger-than-RAM storage.
- Preserve the existing in-memory PK index. New HASH/ART/FTS/vector index contracts remain IM5 unless
  an IM2 correctness requirement or measurement proves one necessary.

### Results and lifetimes

- `Connection::query` remains eager and returns owned typed result batches.
- Results must not borrow a database lock, connection, transaction, or processor state.
- Row iteration remains thin compatibility sugar over the same batch buffers; it must not own or
  materialize a second complete result representation.
- Lazy public streaming, cancellation, and deadlines remain IM4.

### Memory ownership boundary

IM2 owns public memory configuration, database-local reservation/accounting primitives, real
`bm_info`, and enforcement for allocations introduced or materially replaced by IM2. IM4 owns
exhaustive whole-query deterministic OOM behavior, concurrent admission, cancellation, and measured
spill. IM2 must name any legacy operator allocation still outside the tracker rather than claiming
complete database memory enforcement.

## Landing A — explicit storage context and resource primitives

### Explicit read/write handles

Replace `set_read_view` and the stored current view with explicit storage handles passed to every
visibility-sensitive operation.

The handles must carry or own, as appropriate:

- read timestamp;
- optional writer identity;
- writer savepoint/undo context;
- database-local resource-accounting context.

Every scan, PK probe, projected property read, adjacency extension, statistics read, mutation,
commit, and rollback must receive the correct handle explicitly. A write handle sees its own
uncommitted versions while other connections do not. No process-global or database-global mutable
"current query" or "current read view" may remain.

Keep current serialized execution and single-writer behavior. The contract must permit IM4 to make
multiple read handles concurrent without changing planner or processor semantics.

### Public database configuration

Publish an immutable `DatabaseConfig`, following existing API naming conventions, with:

- an optional maximum worker count;
- an optional tracked-memory limit;
- validated construction and unlimited/current-compatible defaults;
- a fallible configured in-memory database constructor.

`Database::in_memory()` remains the default constructor. Replace
`in_memory_with_buffer_pool_limit` cleanly and migrate the test runner.

An explicit worker cap must constrain the effective per-query worker count. A connection setting
above an explicit cap must follow one observable rule—prefer a clear error over silent clamping.
With no cap, retain current environment/session behavior.

### Resource tracker

Add a database-owned, future-concurrency-safe tracker with:

- current and peak usage;
- optional limit;
- fallible reserve-before-grow;
- RAII release for temporary/result reservations;
- persistent reservations retained by storage allocations;
- a catchable `Error` on rejection, never panic or abort;
- rollback-safe failure during mutations;
- no process-global counters.

`bm_info.mem_limit` and `mem_usage` must report this real tracker. Peak usage belongs in the Rust API
even if the Cypher function retains its C++ schema. Separate `Database` instances must have
independent counters and limits.

### Landing A exit

- No production `set_read_view` or mutable current-view field remains.
- All storage tests use explicit handles.
- Existing snapshot, own-write, commit, rollback, and savepoint behavior passes focused tests.
- Database configuration is public, validated, and observably effective.
- Tracker current/peak/release and independent-database tests pass.

## Landing B — typed batch storage

### Typed chunked columns

Replace node and relationship property `Vec<Vec<Value>>` storage with appendable typed, chunked
columns. Reuse or factor `PhysicalType`/`ColumnData` so the repository has one coherent physical
vector convention.

Requirements:

- fixed-width scalar types use fixed-width buffers without per-cell `Value` allocation;
- common variable-width types, especially `STRING`, have typed storage and explicit validity;
- exact `LogicalType` remains separate from physical representation;
- cold nested/graph types may use an explicit generic fallback where required, but LSQB hot scalar
  paths may not;
- nulls, integer widths, decimal metadata, temporal types, UUID, BLOB, nested values, graph values,
  and recursive paths retain existing semantics;
- stable `InternalId` offsets, PK rules, relationship endpoints, multiplicity, MVCC prior values,
  and transaction-local relationship rendering remain correct.

Account typed properties, null masks, adjacency/endpoints, PK entries touched by batch writes, and
MVCC version/prior-value records through the IM2 tracker.

### Batch storage operations

Add and consume explicit batch operations:

1. **Projected node scan**
   - reads only requested ID/property columns;
   - produces at most `VECTOR_CAPACITY` visible rows per batch;
   - preserves projection order and exact types;
   - skips invisible/deleted rows at the supplied handle.

2. **Batched adjacency extension**
   - accepts a batch of source IDs;
   - returns compact offsets/lengths plus neighbor and relationship IDs, or an equivalently
     allocation-conscious representation;
   - supports both directions, polymorphic relationship members, existing-target filtering, and
     high-fanout continuation across output chunks;
   - batch-projects requested relationship and neighbor properties;
   - preserves factorized neighbor counting without materializing collapsed branches.

3. **Snapshot-aware PK lookup**
   - keeps constant and correlated probes as explicit fast paths;
   - never changes results relative to scan plus filter.

4. **Batch mutations**
   - `CREATE`, `SET`, `DELETE`, `MERGE`, `COPY`, and loader paths submit selected typed batches or
     typed column slices;
   - validation occurs before partially publishing a batch where possible;
   - statement and explicit-transaction rollback remain complete;
   - processor/loader code does not merely loop over the former public scalar trait.

Small loops inside typed storage are acceptable. Per-cell dynamic dispatch and scalar scan/extend
calls from processor hot paths are not.

### Real `stats_info`

Match the observable C++ node-table contract:

- one row containing `cardinality` and `<property>_distinct_count` for every property in catalog
  order;
- exact column names and integer types;
- binder errors for missing tables and non-node/relationship tables;
- standalone and in-query calls use the executing catalog and storage snapshot;
- own writes are visible inside a transaction; another connection's uncommitted writes are not.

Cardinality must describe the selected snapshot, not the current insert upper bound after deletes.
The existing HLL approximation may remain for distinct counts, but delete/update/rollback or an old
snapshot must not knowingly return stale transaction metadata. A rare lazy rebuild through a
projected scan is preferable to false metadata.

Physical storage introspection remains durability-deferred.

### Landing B exit

- Node/relationship property storage is typed and chunked.
- Processor scan/extend/write hot paths use batch APIs; scalar hot-path property/extend calls are gone.
- Loader and COPY use the batch mutation path.
- Tests cover all physical types, nulls, batches larger than `VECTOR_CAPACITY`, high fan-out,
  polymorphic relationships, MVCC views, PK parity, failed-write rollback, and factorization.
- `stats_info` tests cover all properties plus insert/update/delete/rollback/transaction visibility.
- The migrated persistent and temporary allocations are accounted.

## Landing C — typed result and prepared API

### Processor/result cutover

Change `ExecResult` to own an exact schema and typed output batches, not `Vec<Vec<Value>>`. Move final
processor chunks into `QueryResult` directly. Compact internal selections column-wise when needed;
never transpose a complete row result.

Preserve behavior for:

- empty and write-only results;
- `UNION`/`UNION ALL`, `DISTINCT`, `ORDER BY`, `SKIP`, and `LIMIT`;
- aggregate, table-function, write-return, `EXPLAIN`, and `PROFILE` results;
- transaction-local relationship ID rendering.

Refactor final-result operators as required. Do not rewrite unrelated join/aggregate internals unless
needed for correctness or to remove final row materialization.

### Public API contract

Publish documented public types from `koko` without leaking planner, binder, processor, or storage
internals:

- ordered query schema/field metadata with exact names and `LogicalType`s;
- indexed and named lookup;
- explicit missing-name and duplicate-name ambiguity errors;
- immutable owned result batches;
- batch row count, logical type, null/validity access, safe typed column views, and generic value
  access where no specialized view exists;
- total result row/column counts and batch iteration;
- existing bounds-checked `Row::get`/`FromValue` behavior and named row access over the same buffers;
- no public unsafe API.

All result buffers remain valid after query return without retaining engine locks. Exact logical type
metadata must survive empty results, null-only columns, unions, aggregates, graph/path values, and
parameterized output.

### Query summaries

Attach a query summary to every successful result:

- compiling and execution times exposed as Rust `Duration`s;
- optional millisecond convenience access;
- documented treatment of direct versus prepared execution;
- zero-duration measurements are valid.

### Prepared metadata

Preparation must validate against the current catalog, not only parse. Expose:

- ordered unique parameter names;
- inferred `LogicalType` when the binder can determine it;
- explicit unknown/unconstrained type otherwise, never a fabricated NULL type;
- exact result schema, with `Any` only for genuinely unconstrained output;
- read-only/write classification;
- a small stable statement-kind enum only if it improves the public contract.

Use the binder as the single source of parameter constraints. A symbolic bound parameter and shared
parameter-type environment are appropriate; do not add an independent partial inference pass.
Repeated uses unify constraints, incompatible constraints fail preparation, and execution validates
values through existing Cypher cast rules. Missing values retain the oracle-verified NULL behavior;
unexpected supplied names return an error.

Prepared metadata must not silently become false after catalog changes. Capture catalog version and
invalidate with a clear re-prepare error, or refresh all metadata atomically. Global prepared-plan
caching remains measurement-driven IM4 work.

### Landing C exit

- Final `ExecResult`/`QueryResult` storage is typed batches with exact schema.
- No complete result is represented and retained as both rows and columns.
- A result larger than `VECTOR_CAPACITY` iterates identically through batches and rows.
- Bounds, missing-name, duplicate-name, empty-result, type, and result-lifetime tests pass.
- Prepared tests cover parameter order/deduplication, inferred and unconstrained types, conflicting
  constraints, missing/extra/wrong-type values, result schema, read-only classification, catalog
  invalidation, and repeated compatible executions.
- Holding/dropping results updates tracker usage correctly.

## Landing D — closure and evidence

Only after the behavior works and focused smoke tests pass:

1. Run formatting and strict Clippy.
2. Run focused storage, processor, public API, prepared, resource, and `stats_info` tests.
3. Run the full workspace suite.
4. Run the strict standing gate and preserve A/B invariants.
5. Run the LSQB performance gate. IM2 must not regress the working gate; the final universal
   per-query `≤2×` target remains IM4.
6. Update `ROADMAP.md`, `docs/PROGRESS.md`, README/API examples, and affected ledgers so they agree
   that IM2 is closed and IM3 is next.
7. Record exact commands, counts, artifacts, and timings in `docs/PROGRESS.md`.
8. Commit each independently green landing using the trailer required by `AGENTS.md`.

## Verification commands

Use the repository's current commands and environment rather than inventing a second harness:

```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
python3 scripts/goal_gate.py --strict
KOKO_ROOT_DIRECTORY=/Users/dai/code/koko scripts/perf_gate.py
```

Also compare standing outputs under default execution, `KOKO_NO_OPTIMIZE=1`, and
`KOKO_THREADS=1`. New or changed p0 fixtures must be checked with
`python3 docs/fable-audit/p0_to_probe.py`; oracle-sensitive behavior must be probed against the C++
shell named in `docs/PROGRESS.md`.

## Definition of done

IM2 is complete only when all of the following are true:

- storage visibility is explicit and no hidden current-view state remains;
- hot storage paths are typed, projected, batched, and free of trait-object/per-cell dispatch;
- typed owned batches and exact schemas are the sole final result representation;
- the row API is a safe adapter over those batches;
- prepared metadata, result metadata, and timings are truthful;
- accepted configuration has observable effects;
- migrated storage/result allocations are enforced and reported by real database-local accounting;
- `stats_info` reports snapshot-correct node statistics for every property;
- existing semantics, transaction isolation, factorization, A/B invariants, corpus gates, and the
  working LSQB gate do not regress;
- no durability, IM3 interchange, IM4 concurrency/control/spill, or IM5 ecosystem work is pulled in;
- all verification gates are green and the status documents agree.

Do not close IM2 with stubs, no-op configuration, placeholder metadata, dual old/new paths, or an
"IM2 foundation" that leaves the requested cutover incomplete.
