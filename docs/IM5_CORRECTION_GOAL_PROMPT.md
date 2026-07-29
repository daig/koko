# Archived IM5 correction goal entry prompt

> **ARCHIVED 2026-07-22:** the correction completed in ordered landings `86014ac`, `969a106`,
> `4e81b01`, `1902645`, and `ad866ad`, followed by a green fresh close gate. Do not resume or paste
> this prompt into a new session. `ROADMAP.md` now owns the current product and work.

```text
/goal Work in /Users/dai/code/koko-rs. Close the reopened ROADMAP.md “IM5 — Rust-embedded core
completion” correction end to end. The then-current ROADMAP.md owns product scope;
docs/IM5_PLAN.md §7 is the executable architecture, dependency order, and stopping rule;
docs/TRIAGE.tsv owns the exact expected residual corpus cases; the then-current decision record
owns intentional differences; /Users/dai/code/koko and its .test corpus are the
observable-semantics and architecture oracle.

Resume from the shipped behavior landings 57f87a3/a4d3046/47baae4/2b9c74b, provisional close
7c91faa, and reassessment 35c61d0. No R1–R3 correction implementation had started at that handoff.
The preserved baseline is strict corpus 1785 passed / 343 skipped / exactly 23 deferred-or-ledgered
failures, zero panic/unparsed/untriaged state, and nine correct timeout-free LSQB answers with every
median Rust/C++ ratio ≤2×. First re-read the current graph registry/catalog/storage/database/
connection/transaction/binder/planner/processor/interchange/loader seams, docs/IM5_PLAN.md §7, and
the relevant C++ hidden-ANY-table and icebug table implementations. Reproduce the documented
ANY/order/error, export, and source-lifetime failures before editing; remember that the generic diff
probe sorts rows and cannot by itself validate ORDER BY.

Land and commit the following in exact order:

1. R1 internal ANY storage and normal-pipeline lowering. Create hidden internal
   _nodes(id, label STRING[], data JSON) and _edges(_id, label STRING, data JSON) catalog/storage
   tables for each ANY graph. Extend the ordinary binder to lower dynamic labels and ordered-JSON
   properties/updates to those tables, then use the existing planner, processor, MVCC, expression,
   UDF, result, cancellation, and memory paths. Migrate every caller and delete AnyGraphData,
   AnyGraphState, Connection::execute_any_query, and the dedicated any_graph interpreter. An interim
   Arc copy-on-write snapshot may be used only during migration; a dual query engine or whole-graph
   read clone is not an acceptable landing.
2. R1 semantic/resource closure. Make ordered/distinct/skip/limit projection, OPTIONAL, WITH, UNION,
   MERGE, dynamic create/set/delete, missing/NULL properties, prepared statements, native UDF errors,
   runtime/type errors, transactions, concurrent drop/snapshots, cancellation/deadline, and tracked
   low-memory failure match the retained typed/C++ contracts. Add order-aware permanent regressions
   for every post-close probe before continuing.
3. R2 database-level logical interchange. Intercept EXPORT/IMPORT DATABASE above one selected
   GraphData; capture one graph-registry generation and shared read timestamp; emit a versioned,
   deterministic portable image for main plus every typed/ANY graph, catalog objects, dynamic data,
   relationship groups, and explicit HASH/ART metadata; import into detached graph states and publish
   atomically after full preflight and memory admission. Cover every selected-graph starting point,
   names/path collisions, concurrent graph changes, cancellation/OOM, rollback, and connection-local
   selection. Do not recursively call Connection::query under schema/database locks or mutate the
   live registry incrementally.
4. R3 genuine local read-only icebug-disk. Retain validated catalog descriptors and add a closed
   concrete in-memory-or-icebug scan/adjacency dispatch. Read projected node Parquet and relationship
   CSR/flat batches at query time, with pinned query source state and normal snapshot, worker,
   cancellation, deadline, and tracked-memory controls. Support forward/backward/undirected,
   recursive, filtered, joined, aggregate, and multi-vector scans; validate version/schema/endpoints/
   row counts/CSR; reject every mutation. Do not hydrate whole tables at DDL or first query, use
   whole-table temporary storage, add a generic StorageBackend/plugin/VFS, or add remote sources.
5. R4 fresh closure. Run every focused correction contract, then the complete docs/IM5_PLAN.md §5
   close gate in one fresh run. Synchronize ROADMAP.md, README.md, AGENTS.md, docs/IM5_PLAN.md,
   docs/PROGRESS.md, docs/PERF_GATE.md, docs/TRIAGE.tsv, and this prompt only after behavior works;
   mark this prompt historical at final closure.

Preserve all IM1–IM4 and provisional IM5 ownership, table-ID, MVCC/multi-writer, transaction,
prepared/result, UDF, cancellation/deadline, memory, Arrow-Rust/interchange, corpus, optimizer/
one-worker, and LSQB contracts. Probe ambiguous behavior against C++ and check the divergence ledger
before matching an upstream defect. Commit each verified landing with the required trailer and keep
status evidence current.

Do not implement or scaffold native durability/WAL/recovery/pages, physical storage_info rows,
Arrow C, extension/plugin infrastructure or modules, PROJECT_GRAPH/GDS, the full JSON extension,
FTS/vector, remote/object/connectors, foreign bindings, auth/multi-database attach, lazy/async API,
or shell/CLI. Do not try to reduce the expected 23 residual corpus failures: they remain exactly 15
physical-storage deferrals, 4 projected-graph/extension owner-deferrals, and 4 ledgered statement
divergences.

Do not stop at a plan, compatibility shim, second execution path, metadata-only export, eager scan,
narrow test, or the visible probes. Stop only after every R1–R3 retained contract works end to end
and one fresh close run is green: strict corpus 1785/343/exactly 23 with zero panic, unparsed,
unledgered, missing/stale/malformed TRIAGE, or arity state; P0/oracle/deviation and default/no-opt/
one-worker invariants green; focused ANY/interchange/icebug ownership, concurrency, rollback,
cancellation, deadline, memory, prepared, panic/error, source-corruption, and public-API contracts
green; repeated nine-query LSQB correct, timeout-free, and ≤2× C++; workspace release tests/build,
debug tests, all 52 corpus directories, fmt, and strict Clippy green; generated probes/exports removed;
required ordered commits present; status/scope docs synchronized; working tree clean. Report the final
scorecard and stop.
```
