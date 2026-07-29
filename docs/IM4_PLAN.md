# IM4 — concurrency, controls, and performance

**Status:** completed 2026-07-20; frozen execution contract retained as historical evidence.  
**Authority:** the then-current `../ROADMAP.md` marked IM4 closed and IM5 next. `PROGRESS.md`
records implementation and verification; `PERF_GATE.md` records the repeated LSQB close run;
`TRIAGE.tsv` owns the frozen residual failures. Native durability and IM5 remained outside this
plan's implementation scope.

## 1. Re-measured starting point

Commands were run from `/Users/dai/code/koko-rs` against `/Users/dai/code/koko` and its release shell:

- `python3 scripts/goal_gate.py --strict`: GREEN — 1720 passed / 384 skipped / 47 exactly triaged; 0 panic files; 0 unparsed; demo DB ran; P0 43 clean / 5 ledgered / 4 skipped; 143 deviation probes / 76 ledgered differences / 0 unledgered; arity panics 0; TRIAGE 47 rows / 0 missing / 0 stale / 0 malformed.
- `python3 scripts/perf_gate.py`: all nine answers correct and no timeout. Single isolated-process samples were q1 5.64 s / 7.41×, q2 52 ms / 1.49×, q3 268 ms / 2.58×, q4 74 ms / 0.03×, q5 328 ms / 0.10×, q6 834 ms / 6.32×, q7 1.02 s / 0.36×, q8 1.22 s / 4.86×, q9 2.17 s / 2.88×. Ratios here use the raw displayed times and are diagnostic, not the final repeated gate.

The implementation baseline is exact:

- `Connection::execute_parsed` holds `Arc<Mutex<DatabaseState>>` through bind, plan, and execution.
- `InMemStorage` already uses writer-tagged row/cell versions, stable read timestamps, writer-specific undo records, and O(changes) commit/rollback. Explicit write transactions clone only catalog/macros, never storage.
- `debug_enable_multi_writes` exercises serial interleaving but is not product configuration and the global mutex prevents physical overlap.
- `timeout` and `spill_to_disk` are accepted settings without execution effects. There is no public interrupt handle.
- Persistent storage, loaders, result batches, and Arrow interchange have accounting. Processor hash/sort/frontier/output working sets do not.
- The two IM4-owned TRIAGE rows are `agg/hash_leak.LargeAggregateLeakTest` and `copy/spill_to_disk.DisableSpillToDisk`.

## 2. Frozen corpus surface

The strict sweep reports 41 active cases by first structural skip reason. A case can contain more than one directive; first-reason accounting must not hide its eventual execution.

### Loop and dynamic `-SET` — 7 cases

- `dml_rel/create_ldbc_sf01.CreateManyRelsSeparateCommit`
- `transaction/dml_empty_serial_execution.NodeUpdateOverlappedRows`
- `transaction/dml_empty_serial_execution.NodeUpdatesRollback`
- `transaction/dml_empty_serial_execution.NodeUpdatesMixedCommitAndRollback`
- `dml_node/set_empty.RandUpdateInt`
- `dml_node/create_random_int.TSSeedRandomIntInsertions`
- `dml_node/create_random_int.FixedSeedRandomIntInsertions`

`-LOOP` supports inclusive integer ranges with optional positive step and explicit value arrays. Expansion is parse-time and supports nested statement parsing. `-SET` implements `REPEAT`, `ARANGE`, quoted/integer literals, `current_timestamp()`, `random.set_seed(n)`, and oracle-compatible PCG32 `random.randInt32(max)`. Fixed-seed cases must reproduce the C++ values, not merely convenient random values.

### Concurrent block — 1 active case

- `transaction/dml_tinysnb_serial_execution.ConcurrentSingleWriteUpdate`

The four `ddl_concurrent_execution.*` cases are disabled by an upstream header `-SKIP`; `ConcurrentSingleWriteCreate` and `ConcurrentSingleWriteDelete` have upstream case-level `-SKIP`. They remain skipped. The runner must never override `-SKIP` or `-SKIP_IN_MEM` merely because it learns the directive.

A concurrent block queues statements by connection, starts one worker per connection behind one barrier, preserves statement order within each connection, joins all workers, and reports failures deterministically by original statement index. This mirrors the C++ `ConcurrentTestExecutor` rather than spawning one thread per statement.

### Batch statement files — 25 cases

- `nested_types/nested_types_errors.SizeError`
- `transaction/int_delete_create_transaction.SimpleAdd{Commit,Rollback}{NormalExecution,Recovery}`
- `transaction/small_list_becomes_large_list_after_insertion.smallListBecomesLargeListAfterInsertion{Commit,Rollback}{NormalExecution,Recovery}`
- `transaction/update_each_element_of_small_list.updateEachElementOfSmallList{Commit,Rollback}{NormalExecution,Recovery}`
- `transaction/update_each_element_of_large_list.updateEachElementOfLargeList{Commit,Rollback}{NormalExecution,Recovery}`
- `transaction/delete_rels_from_small_list.deleteRelsFromSmallList{Commit,Rollback}{NormalExecution,Recovery}`
- `transaction/delete_rels_from_large_list.deleteRelsFromLargeList{Commit,Rollback}{NormalExecution,Recovery}`

`-BATCH_STATEMENTS [conn] <FILE:>name` resolves under `<KOKO_ROOT_DIRECTORY>/test/statements`, executes each physical line sequentially on the selected connection, and applies the directive's one expectation to each line, matching C++.

### Manual dataset construction — 8 active first-skip cases

- `transaction/create_large.InsertLDBCNode{Commit,Rollback}{,Recovery}`
- `transaction/dml_empty_serial_execution.WWConflict{Node,Rel}Insert{Update,Delete}`

`-CREATE_DATASET_SCHEMA name` executes only `dataset/name/schema.cypher`. `-INSERT_DATASET_BY_ROW name` derives typed `LOAD WITH HEADERS ... CREATE` statements from `copy.cypher` plus catalog introspection, matching `InsertDatasetByRow`; it must not route through bulk `COPY`. Four upstream-skipped `create_large.InsertLDBCFull*` row-wise cases remain skipped.

### Frozen stress scenarios beyond corpus syntax

1. Two independent read queries rendezvous inside execution; both complete and the measured maximum active execution count is at least two.
2. A DML writer's read pipeline and a reader rendezvous concurrently. A reader whose snapshot predates commit continues seeing the old value; a later reader sees the committed value.
3. Single-writer default rejects a second writer with the existing oracle text.
4. Product multi-writer mode covers disjoint commits, same-row update/delete conflict, duplicate-PK insert conflict, same-name and different-name DDL, interleaved commit order, statement error, explicit rollback, and conflict-triggered whole-transaction rollback.
5. Interrupt and timeout cover scan/range, join/aggregate, recursive traversal, and mutating/COPY paths. Cancellation rolls an auto-commit statement back, releases reservations, and does not poison the next query.
6. Low-memory hash aggregate and COPY fail repeatedly with a catchable buffer-manager error, preserve database state, and return tracked current usage to the pre-query value.

## 3. Frozen product contracts

### Synchronization and snapshots

Replace the query-duration database mutex with separated coordination/catalog and storage synchronization:

- Metadata, writer admission, catalog reservations, commit generation, and catalog publication use a short-lived coordination lock.
- Catalog/macros used for bind, plan, and execution are immutable for that query/transaction. DDL publication is serialized; ordinary DML does not take an exclusive catalog lock for its execution duration.
- Storage exposes shared read phases and short exclusive mutation/commit phases. Processor write parts already drain their read pipeline before applying update batches; preserve that boundary rather than locking a write query for its whole execution.
- A `ReadView` is captured once per statement or explicit transaction and passed through every storage operation. New commits cannot change that view. No storage clone or O(database) snapshot is permitted.
- Lock order is coordination/catalog before storage when both are unavoidable; execution must not hold coordination exclusively while waiting on user I/O, workers, or storage scans.
- `Connection` remains one-thread-at-a-time and cheap to create. Separate connections are `Send` and may execute on separate threads.

### Multi-writer API and atomicity

Add `DatabaseConfig::with_max_concurrent_writers(usize) -> Result<Self>` and `max_concurrent_writers() -> usize`; zero is invalid and the default is one. The existing `CALL debug_enable_multi_writes=true` remains only as oracle compatibility and maps admission to effectively unbounded writers for that database.

A transaction owns one writer id and undo sequence interval. Every row, property, PK, relationship, catalog name, sequence, and macro mutation either publishes under one commit timestamp or is fully reverted. Conflict errors never expose a partial transaction. Commit order, not thread scheduling, assigns commit timestamps. PK ownership is version-aware: an uncommitted owner conflicts, a snapshot-visible committed owner rejects duplicates, and a rolled-back owner releases the key.

### Cancellation, deadline, and errors

Add a cloneable, `Send + Sync` `InterruptHandle` returned by `Connection::interrupt_handle()`, with `interrupt()` advancing an epoch. A query captures that epoch at start, so an interrupt stops only queries already running and never poisons the next query. Add `Connection::set_query_timeout_ms(u64)`; zero disables the deadline and positive values apply independently to each subsequent query. `CALL timeout=...` updates the same connection-local setting.

A query-owned control object carries the interrupt epoch and optional `Instant` deadline through processor, table functions, loaders, interchange, and parallel workers. Long loops check at least once per vector/chunk/frontier and before mutation batches. Cancellation is `Error::Interrupt` rendered exactly `Interrupted.`. Memory exhaustion is `Error::BufferManager` rendered `Buffer manager exception: Unable to allocate memory! The buffer pool is full and no memory could be freed!`. Both are ordinary `Result` errors, never panics.

### Memory-accounting matrix

Accounting is admission-before-growth and RAII release. Exact collection capacity plus owned payload is preferred; a documented conservative upper bound is acceptable. Do not double-charge a buffer when ownership moves.

| Allocation owner | Starting state | IM4 contract |
|---|---|---|
| Property column chunks/payload, row stamps, prior versions | tracked | preserve |
| PK maps, adjacency vectors, undo records | partial | charge capacity and owned keys/entries |
| CSV/Parquet/NPY/Arrow batches | tracked | preserve; add cancellation checks |
| Final `QueryResult` batches | tracked after execution | reserve during production, transfer one reservation to result |
| Carried `WITH` chunks and write-pipeline materialization | untracked | charge while live |
| Hash aggregate groups/states | untracked | charge map capacity, keys, aggregate payload; release on every exit |
| Hash join build table and match lists | untracked | charge build capacity and owned row references |
| `DISTINCT`/`UNION` sets | untracked | charge set capacity and owned keys |
| Sort keys/permutation/output | untracked | charge capacities; external spill only if profiling proves needed |
| Recursive frontier/path/dedup state | untracked | charge frontier, paths, and visited sets |
| MERGE per-statement key maps/sets | untracked | charge keys and id vectors |
| Planner/binder scratch | bounded by query text/schema | no IM4 charge unless measurement shows it substantial |

`spill_to_disk=false` is effective: no spill path may bypass the memory limit. `spill_to_disk=true` gains a temporary operator spill implementation only if the measured matrix shows a gate workload cannot meet its contract in memory. Spill files use an engine-owned temporary directory, contain no durable database state, are removed on success/error/interrupt/drop, and are never introduced merely to satisfy a metadata knob.

### Repeatable LSQB protocol

Upgrade `scripts/perf_gate.py` from one rounded sample to an enforcing gate:

- Release binaries built once; same machine, test file, dataset, environment, timeout, and answer checks for both engines.
- One warm-up plus three measured isolated-process samples per engine/query. Alternate engine order per repetition. Dataset load remains outside the reported query time.
- Report every raw sample plus median milliseconds, median raw Rust/C++ ratio, hardware/toolchain metadata, answer status, and timeout status.
- Fail nonzero if any answer is wrong, any measured run times out, any median ratio exceeds 2.00, or existing wins q4/q5/q7 reach 1.00.
- A query within 10% of either boundary gets two additional paired samples; the median of all five is authoritative.
- Performance changes require a before/after profile naming the dominant operator or allocation. No speculative parallel operator, plan cache, SIP, or join-order change lands.

## 4. Dependency-ordered landings and close gates

### L1 — corpus harness fidelity

Implement dynamic `-SET`/loops, batch files, manual dataset schema/row loading, and concurrent groups. Gate: parser unit tests plus all 41 active first-skip cases execute; only oracle behavior can fail. Upstream `-SKIP` and `-SKIP_IN_MEM` counts remain unchanged.

### L2 — synchronization substrate and read overlap

Separate coordination/catalog/storage locking, split processor read and mutation phases, and add deterministic overlap instrumentation used only by tests. Gate: reader/reader and reader/writer rendezvous tests prove physical overlap and snapshot assertions prove visibility; existing serial transaction cases remain green; no storage clone appears.

### L3 — product multi-writer MVCC

Add `max_concurrent_writers`, repair row/PK/catalog/undo behavior exposed by actual overlap, and make commit/rollback atomic. Gate: the frozen conflict matrix and now-executable WW corpus cases pass in both single- and multi-writer modes.

### L4 — cancellation and deadlines

Thread query control through every long path and worker; implement public interrupt and timeout APIs. Gate: each frozen cancellation scenario returns promptly, mutation and accounting are unchanged after abort, and a following query succeeds.

### L5 — memory ownership and TRIAGE retirement

Add tracked operator/storage collections from the matrix, introduce the buffer-manager error category, measure spill need, and implement only justified temporary spill. Gate: both IM4 TRIAGE cases match the oracle repeatedly; no current-memory increase after repeated failures; no partial mutations or temporary files; remove exactly those two TRIAGE rows.

### L6 — measured LSQB closure

Land the enforcing repeated harness, profile q1/q3/q6/q8/q9, and optimize only evidenced bottlenecks while continuously checking q2/q4/q5/q7. Gate: authoritative repeated protocol has nine correct answers, no timeouts, every median ratio at most 2.00, and q4/q5/q7 below 1.00.

### L7 — full closure

Run focused regressions, `cargo fmt --all --check`, strict workspace Clippy, workspace tests, all 52 corpus directories, P0 C++ re-diff, deviation battery, arity sweep, default/`KOKO_NO_OPTIMIZE=1`/`KOKO_THREADS=1` A/B checks, and the repeated performance gate. Require zero panic files, zero unparsed files, zero unledgered differences, zero missing/stale/malformed TRIAGE rows, and no IM4-owned TRIAGE row. Synchronize `ROADMAP.md`, `README.md`, `AGENTS.md`, `docs/PROGRESS.md`, `docs/PERF_GATE.md`, and `docs/TRIAGE.tsv`; remove generated artifacts; commit a clean IM4 closure with the repository trailer.

**Close result (2026-07-20):** all landings completed. The strict gate is green at 1763 passed /
343 skipped / 45 exactly triaged failures, with no panic, unparsed file, unledgered difference,
arity panic, or missing/stale/malformed TRIAGE row. Full default and one-worker corpus output is
byte-identical; the standing `agg`, `match`, and `lsqb` suites are byte-identical with the optimizer
disabled. Workspace build/tests, formatting, strict Clippy, and the repeated LSQB gate are green.

## 5. Explicit non-goals

No native database files, WAL/checkpoint/recovery, buffer-managed durable pages, physical storage introspection, larger-than-RAM native tables, graph/GDS, indexes, extensions, connectors, CLI, language bindings, JSON ecosystem work, or IM5 surface. Temporary query spill, if justified, is not a persistence backend.
