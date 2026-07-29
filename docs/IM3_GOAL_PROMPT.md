# IM3 goal entry prompt

> **COMPLETED 2026-07-20.** This prompt is retained as the executed IM3 contract. Do not resume it;
> `ROADMAP.md` makes IM4 next and `IM4_GOAL_PROMPT.md` is the current entry prompt.

Work in `/Users/dai/code/koko-rs`. Execute **IM3 — Ingestion & interchange** end to end. Treat the
then-current `ROADMAP.md` as phase authority, `docs/TRIAGE.tsv` as the exact starting failure
inventory, and the C++ checkout at `/Users/dai/code/koko` plus its `.test` corpus as the
observable-semantics oracle. Create `docs/IM3_PLAN.md` as the detailed execution contract, then
implement that plan completely. Do not stop after planning, scaffolding, parsing syntax, or landing
only one format.

## Product objective

Make the in-memory Koko engine capable of production-quality bulk ingestion and portable logical
interchange:

- one deterministic resolver for local file literals, lists, wildcards, home-directory resolution,
  and `file_search_path`;
- oracle-faithful CSV behavior with genuinely parallel typed-batch loading;
- Parquet and NPY ingestion plus Rust-native Arrow batch import/export;
- complete relationship-group, query-source, partial-column/default/serial, and `COPY ... TO`
  behavior;
- logical `EXPORT DATABASE` / `IMPORT DATABASE` that reconstructs a fresh in-memory database.

Preserve IM1/IM2 ownership, MVCC, transaction, typed-batch, memory-accounting, result, and prepared
API contracts. Keep `Database::in_memory()` as the product mode.

## Required first actions

1. Re-read the then-current `ROADMAP.md` IM3 contract, `docs/TRIAGE.tsv`, the current parser/binder/
   loader/storage/processor/database paths, and the relevant C++ tests and source.
2. Freeze the exact starting IM3 case set in `docs/IM3_PLAN.md`. The current ownership table maps 94
   cases across CSV/options/gzip/sniffing (33), Parquet (19), file expansion/multi-file (12), logical
   export/import (12), NPY (10), relationship-group COPY (7), and query-source COPY (1). Use exact
   case IDs and current observed blockers; do not select cases from the historical `p4` label alone.
3. Probe ambiguous semantics against the C++ shell before designing behavior. Check
   `fable-audit.md` §4 and the then-current compatibility decisions before matching an upstream defect.
4. Write `docs/IM3_PLAN.md` with dependency-ordered landings, affected case IDs, source/API
   contracts, focused verification, rollback/error invariants, and a close gate for every landing.
   Then execute every landing in the same goal.
5. Establish the shared file/source and typed-batch contracts before parallelizing work. After that,
   parallelize genuinely independent CSV, Parquet, NPY, Arrow, export, and corpus-audit slices when
   they do not contend on the same core files. Keep one owner for shared contracts and final
   integration.

## Mandatory implementation scope and acceptance criteria

### A. Central local-file resolution

Replace the separate narrow `LOAD` glob code and direct file-backed `COPY` path handling with one
shared resolver used by `LOAD FROM`, file-backed `COPY FROM`, all format readers, and logical import.
It must:

- resolve existing literal files;
- flatten ordered file lists;
- expand at least the corpus-required `*` and `?` wildcard forms, including long paths;
- apply the connection's `home_directory` and `file_search_path` settings with oracle-matched
  precedence;
- return deterministic, oracle-compatible ordering and duplicate behavior;
- preserve the path-as-written where diagnostics require it;
- reject empty matches, directories, unsupported/ambiguous formats, and cross-file schema mismatch
  through the correct Binder/Copy/IO/Runtime channel and in the correct precedence order;
- preflight all sources that C++ validates before mutation, so a later bad file cannot leave earlier
  files inserted when the oracle rejects the statement before execution;
- support local files only. Remote URIs/connectors remain IM5.

Focused tests must cover literals, lists, `*`, `?`, zero matches, home directory, search-path
precedence, deterministic ordering, mixed formats, long paths, and multi-file preflight.

### B. Complete CSV semantics

Retire every IM3-owned CSV/options/gzip/sniffing failure. Implement and oracle-verify:

- UTF-8 BOM behavior;
- delimiter/quote/escape/header/skip/autodetect interactions;
- short, wide, ragged, malformed, and unterminated-quote rows;
- quoted newlines in serial and parallel modes;
- `IGNORE_ERRORS`, including which failures are skippable, inserted-row counts, warning totals,
  `warning_limit`, exact warning payload fields/text/truncation, source location, and deterministic
  warning order across files and queries;
- `SAMPLE_SIZE`, including type/range validation and its actual effect on sniffing;
- `LIST_UNBRACED` and nested list parsing;
- bare `LOAD FROM` inference for the corpus-supported scalar, integer-width, decimal, temporal,
  list/array, map, and struct shapes, including cross-file consistency;
- `.gz` and `.gzip`, corrupt streams, compressed-format rejection, and exact error class/message;
- exact DECIMAL and temporal value preservation through ingest and result formatting.

`PARALLEL=true` must use more than one worker on an eligible large or multi-file load;
`PARALLEL=false` must remain serial; both must honor `DatabaseConfig::max_workers`. Prove the
execution-mode distinction with deterministic instrumentation or synchronization in tests, not a
wall-clock assertion. Parallel parsing/insertion must use bounded typed batches, deterministic
observable results/warnings, and no whole-file row materialization.

### C. Parquet

Add a maintained Rust Parquet/Arrow implementation and route it through the same source resolver and
typed `DataChunk` ingestion path. Cover:

- `COPY FROM` and `LOAD FROM` for node and relationship inputs;
- file extension and explicit `file_format` dispatch;
- projection/partial columns, defaults, serial columns, endpoint lookup, multi-file/glob input, and
  schema/header mismatch;
- nulls, integer widths, floating point, decimal scale, strings/blobs, dates/timestamps including
  millisecond/nanosecond and Impala encodings, and the nested array/list/map/struct shapes exercised
  by the corpus;
- gzip, Brotli, and Zstd Parquet compression cases present in the reader corpus;
- correct cast, unsupported-type, malformed-file, and option error channels;
- CSV and Parquet output for `COPY ... TO` and logical export where required below.

Do not transpose complete Parquet inputs through `Vec<Vec<Value>>`. Decode projected columns into
bounded typed batches and submit them through storage batch APIs.

### D. NPY / `BY COLUMN`

Replace the current phase `NotImplemented` path with oracle-compatible NPY behavior:

- parse, bind, and execute one-file-per-column `COPY ... FROM (...) BY COLUMN` sources;
- validate table kind, file count, column count, dtype, dimensionality, equal row counts, options,
  and partial-column mapping in the oracle's error phase/order;
- support the one-, two-, and three-dimensional and large cases required by the corpus, including
  nested destination types where applicable;
- support `LOAD FROM .npy` and dataset bootstrap paths required by the IM3 case set;
- implement `IGNORE_ERRORS` only where the oracle supports it; never hide structural NPY errors;
- reject relationship-table `BY COLUMN` through the required binder contract;
- preserve typed-batch and bounded-memory behavior.

### E. Rust-native Arrow interchange

Define and ship a documented public Rust API that:

- imports maintained Arrow crate `RecordBatch` values into an existing node or relationship table
  with schema/name/type/null validation and batch insertion;
- exports a `QueryResult` as Arrow `RecordBatch` values with exact names, logical types, nulls, and
  supported nested values;
- converts column-wise directly from/to Koko `DataChunk` buffers; use zero-copy sharing where the
  physical representations permit it and one bounded column-wise conversion per batch where they
  do not; never construct a complete row matrix as an intermediate;
- has explicit ownership/lifetime and error contracts and participates in the applicable memory
  accounting.

The Arrow C Data Interface, PyArrow/Polars bindings, external connectors, and stable FFI ABI remain
IM5 and must not be pulled into this goal.

### F. Complete `COPY`

Finish the full IM3 `COPY` surface:

- relationship-group file COPY chooses the concrete FROM/TO member from validated `from=` and `to=`
  options, rejects a missing/invalid/ambiguous pair correctly, and accepts legacy exported per-pair
  names such as `knows_person_person` during logical import without exposing them as ordinary catalog
  tables;
- query-source `COPY table FROM (<query>)`, including nested `LOAD FROM`, consumes typed result
  batches directly instead of calling `ExecResult::into_rows()`; it preserves casts, constraints,
  endpoint lookup, partial columns, defaults, serial values, `IGNORE_ERRORS`, result counts, and
  transaction visibility;
- partial-column/default/serial semantics work consistently for CSV, Parquet, NPY where applicable,
  and query sources;
- `COPY (<query>) TO '<path>'` writes CSV and Parquet with oracle-compatible headers, NULL/nested value
  formatting, delimiters/quotes/escapes/options, empty-result schema, overwrite/error behavior, and
  cleanup after a failed write;
- all supported `COPY` paths process bounded typed batches without retaining a second complete row
  representation.

### G. Logical `EXPORT DATABASE` / `IMPORT DATABASE`

Implement parser, AST, binder, execution, and tests for explicit logical save/restore:

- `EXPORT DATABASE '<new-directory>'` supports the oracle's default, CSV, and Parquet forms and CSV
  options; reject unsupported formats, invalid option types/combinations, and existing destinations
  exactly;
- export a portable directory containing executable `schema.cypher` and `copy.cypher` plus table
  data files; preserve identifier escaping and deterministic object/data order;
- preserve all currently supported logical catalog/data state exercised by the contract: node and
  relationship tables, relationship groups and their pair data, properties and logical types,
  defaults including nested defaults, macros, explicit sequences and current values, SERIAL columns
  and their next values, and all committed rows;
- `IMPORT DATABASE '<directory>'` validates the directory and required scripts, reconstructs a fresh
  in-memory database through ordinary catalog and typed-batch ingestion paths, and reports nested
  script failures through the oracle-compatible error channel;
- import the legacy per-pair relationship-group directory in
  `/Users/dai/code/koko/dataset/import_db_legacy_relgroup`;
- after round-trip, queries and subsequent writes behave identically, including macro calls,
  `currval`/`nextval`, continued SERIAL allocation, relationship-group matches, and defaults;
- export/import remains an explicit user operation. Do not add `Database::open(path)`, restart
  semantics, checkpoint/recovery claims, or any native storage format.

Add hermetic CSV and Parquet round-trip tests that construct a nontrivial in-memory database, export
it, import it into a fresh database, compare schemas/data/catalog behavior, mutate the imported
copy, and verify source/import isolation. Cover missing directories/files and failed import/export
cleanup.

### H. Architecture and resource invariants

- Keep one in-memory storage engine and one typed batch currency; no format-specific row store,
  shadow catalog, dual result representation, or dormant durable-backend abstraction.
- Reuse IM2 `DataChunk`, batch mutation, exact schema, storage handles, and memory-resource contracts.
- Bound reader/writer queues and Koko-owned staging buffers; do not read an entire large source or
  query result merely to ingest/export it. Document unavoidable third-party-buffer accounting.
- Preserve snapshot visibility, single-writer defaults, rollback/constraint behavior, prepared
  catalog invalidation, factorization, and result formatting.
- Keep engine code safe Rust. Do not introduce a general `unsafe` subsystem.
- Fix causes, not corpus symptoms. Do not special-case case IDs, filenames, or expected values.

## Explicit non-goals

Do not implement or preserve seams for:

- native database files, `Database::open(path)`, persistent catalog/statistics/indexes, WAL,
  checkpoint/recovery/crash replay, durable MVCC, native pages/compression, larger-than-RAM native
  table storage, or physical `storage_info`;
- IM4 concurrent query execution, productized multi-writer conflicts, cancellation/deadlines,
  exhaustive query-memory accounting, spill, or the final `<=2x` LSQB target;
- IM5 index DDL, named graphs/GDS, UDFs, extensions, Arrow C ABI, external/remote connectors,
  `ice_disk`, CLI, or language bindings.

If an original IM3 case reaches one of these blockers only after its ingestion/interchange behavior
works, record the newly observed blocker and correct owner with evidence; do not implement the later
phase and do not use reclassification to avoid unfinished IM3 work.

## Test and evidence contract

Every permanent public or semantic contract needs a focused regression that fails on a plausible
implementation bug. Derive oracle-sensitive expectations from the C++ shell/corpus, not intuition.
Use tiny hermetic generated fixtures for unit/integration tests and the real reference datasets for
corpus verification.

At each substantial landing:

1. Run the affected Rust tests and directly run the affected upstream `.test` files/directories.
2. Re-probe changed semantics against C++ and update the current TRIAGE notes immediately.
   If a P0 fixture is new or changed, run `python3 docs/fable-audit/p0_to_probe.py` and require zero
   unledgered C++ differences before committing it.
3. Confirm no newly passing case reopens another verified deviation.
4. Run formatting and strict Clippy before committing.
5. Commit the independently green landing with the required final trailer:
   `Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>`.

Before declaring IM3 complete, all of the following are mandatory:

1. **Frozen-case disposition:** every starting IM3 case ID passes, has one narrow tested/ledgered
   divergence, or has evidence that completed IM3 behavior exposed a genuinely later-phase blocker.
   `docs/TRIAGE.tsv` has zero rows still owned by an IM3 ref family; no row is silently dropped.
2. **Focused surface:** all relevant `copy`, `csv`, `exceptions`, `glob`, `load_from`, `npy_1d`,
   `reader`, `rel_group`, `transfer_demo`, and Parquet-backed demo/nested-type cases pass or have the
   exact disposition above. No supported IM3 syntax ends in a phase `NotImplemented` path.
3. **Logical round-trip:** both CSV and Parquet export/import reconstruct a fresh in-memory database
   with schema, data, macros, sequences/SERIAL state, defaults, and relationship groups verified
   before and after further writes.
4. **Arrow contract:** public Rust Arrow import/export round-trips exact supported schemas and values
   across multiple batches, nulls, nested values, empty results, and type errors without row-matrix
   materialization.
5. **Parallel proof:** tests establish that eligible parallel CSV loads use multiple capped workers,
   serial mode uses one, and both preserve the oracle-visible result/warning contract.
6. **Workspace hygiene:** `cargo fmt --all`,
   `cargo clippy --workspace --all-targets -- -D warnings`, and `cargo test --workspace` are green.
7. **Strict standing gate:** run `python3 scripts/goal_gate.py --strict` with
   `KOKO_ROOT_DIRECTORY=/Users/dai/code/koko`,
   `KOKO_DATASET_DIR=/Users/dai/code/koko/dataset`, and
   `KOKO_CPP_BIN=/Users/dai/code/koko/build/release/tools/shell/koko`; record exact corpus/P0/
   battery/arity/TRIAGE counts and require zero panics, unparsed files, unledgered differences,
   missing/stale/malformed TRIAGE rows, and `fix-m*` rows.
8. **A/B invariants:** compare stdout, stderr, and exit status under default,
   `KOKO_NO_OPTIMIZE=1`, and `KOKO_THREADS=1` for the standing `agg`, `match`, and `lsqb` suites and
   the affected ingestion suites; outputs are byte-identical except for an exact documented control
   whose purpose is parallel-vs-serial scheduling and whose query-visible output remains identical.
9. **Performance correctness:** run `scripts/perf_gate.py`; all nine `lsqb-sf01` answers remain
   correct with no timeout. Record timings and investigate any material regression caused by IM3.
   The universal `<=2x` target remains IM4 and is not an IM3 close gate.
10. **Documentation:** update `ROADMAP.md`, `docs/PROGRESS.md`, `docs/TRIAGE.tsv`, README/API
    examples, crate docs, and `AGENTS.md` so they agree on delivered behavior, exact evidence,
    remaining ownership, and the next phase. Preserve historical evidence as historical rather than
    rewriting it.
11. **Clean closure:** all planned landings are committed, the working tree is clean, no temporary
    probes/generated exports remain, and the final commit is formatter/Clippy/test/gate clean.

## Definition of done

IM3 is done only when the implementation works end to end—not when syntax parses or a reader
scaffold compiles. The central resolver, CSV semantics and parallelism, Parquet, NPY, Rust Arrow
interchange, complete COPY paths, and logical export/import must all be real; every starting IM3
case must have an exact disposition; logical round-trips must reconstruct and remain writable; all
standing gates and evidence must be current; and IM4/IM5/native-durability work must remain outside
the change. Do not close with stubs, placeholder metadata, broad waivers, ignored tests, silent
fallbacks, dual paths, or a list of follow-up work that belongs to IM3.
