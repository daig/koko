# IM3 execution plan — ingestion and interchange

**Authority:** `ROADMAP.md` IM3 and `docs/IM3_GOAL_PROMPT.md`. **Starting inventory:** the
2026-07-19 post-IM2 `docs/TRIAGE.tsv` snapshot. **Oracle:** C++ Koko 0.17.0 at
`/Users/dai/code/koko` and its `.test` corpus. Native storage, production concurrency,
connectors, and the Arrow C Data Interface remain out of scope.

**Status: COMPLETE 2026-07-20.** All 94 frozen cases pass without waiver or reclassification;
`TRIAGE.tsv` has no IM3-owned row; the strict goal gate, workspace hygiene, 94-case default/no-opt/
one-worker A/B matrix, and nine-query LSQB correctness gate are green. The implementation delivered
every contract below without entering IM4, IM5, the Arrow C Data Interface, or native durability.

## Starting point and definition of done

The Rust engine already has typed `DataChunk` buffers, typed batch storage mutation APIs, CSV
reading, query-source COPY, per-pair relationship storage, memory accounting, and snapshot-correct
statistics. IM3 replaces the remaining fragmented and row-oriented ingestion paths rather than
adding a second engine.

IM3 is complete only when:

1. all 94 frozen cases below pass or have one narrow oracle-probed divergence;
2. `docs/TRIAGE.tsv` has no IM3-owned row;
3. local file resolution, CSV, Parquet, NPY, Arrow, complete COPY, and logical export/import work
   end to end through the typed storage/result APIs;
4. mutation is atomic after multi-source preflight, including explicit transactions;
5. bounded-memory parallel CSV produces deterministic row counts and warning order;
6. the prompt's focused, workspace, strict goal, A/B, LSQB, documentation, and clean-tree gates pass.

## Shared contracts

### File resolver

`koko-common::file_resolver` is the only local-source expansion implementation. Its input is an
ordered list of user spellings plus `base_dir`, `home_directory`, and comma-separated
`file_search_path`. Its result contains the original spelling, canonical concrete local path, and
one resolved `FileFormat` (`Csv`, `Parquet`, or `Npy`).

Resolution is deterministic and oracle-shaped:

- reject remote URI schemes with the explicit IM5 unsupported error before filesystem access;
- absolute paths are used directly; `~` expands against the connection's `home_directory`;
- a relative spelling first probes `base_dir`; only an empty match falls through to search-path
  entries in their configured order;
- expand full glob syntax, sort matches within each spelling, preserve spelling order and duplicate
  matches, and support paths longer than 255 bytes;
- an empty expansion is `Binder exception: No file found that matches the pattern: <spelling>.`;
- directories fail as `Provided path is not a file` after existence and before format dispatch;
- explicit `file_format` overrides extension inference; otherwise all concrete paths must infer the
  same format (`.csv` and `.csv.gz` are CSV); mixed types are a Copy exception;
- all paths, formats, schemas/arity, and options are validated before any mutation.

`SessionConfig` carries the statement's `base_dir`, `home_directory`, and `file_search_path` so
`LOAD` and `COPY` call the same resolver even though `LOAD` needs schema information during binding.
No resolver performs network I/O.

### Typed source batches

`koko-loader` owns one non-object-safe source enum rather than a hot-path trait object:

- `ResolvedSource::{Csv, Parquet, Npy}` identifies validated inputs;
- each reader yields owned `DataChunk` values with exact names and `LogicalType`s, at most
  `VECTOR_CAPACITY` rows;
- conversion is column-wise; no complete `Vec<Vec<Value>>` row matrix is allowed;
- readers reserve tracked memory before growth and release it with each consumed batch;
- import routes batches into `insert_node_batch` / `insert_rel_batch`; row-level adapters remain only
  where one row's error must be isolated for `IGNORE_ERRORS`.

All format readers share target-column mapping, defaults/SERIAL application, endpoint-PK
resolution, relationship-group member routing, per-row error classification, deterministic warning
sequencing, and mutation savepoint rollback.

### Arrow public API

Use maintained `arrow-array` and `arrow-schema` crates and expose:

```rust
Connection::import_arrow(&self, table: &str, batches: &[arrow_array::RecordBatch]) -> Result<u64>
QueryResult::to_arrow_record_batches(&self) -> Result<Vec<arrow_array::RecordBatch>>
```

Node import fields must exactly equal stored target columns by name and compatible logical type.
Relationship import fields are `from`, `to`, then the relationship property names; `from` and `to`
use the endpoint tables' primary-key types. A relationship group routes every row from the resolved
endpoint IDs. Field lookup is case-insensitive but duplicates are rejected. Missing nullable/default
columns are not implicit in this API: the schema is exact and deterministic.

Arrow field metadata includes `koko.logical_type=<display type>` to preserve distinctions Arrow
cannot express alone. The direct mapping covers booleans; signed/unsigned integers; FLOAT/DOUBLE;
DECIMAL128; STRING; BLOB; UUID; DATE; TIMESTAMP/TIMESTAMP_TZ; INTERVAL; LIST/ARRAY; MAP; STRUCT; and
UNION. Null bitmaps are preserved. Graph entity/path and INTERNAL_ID result columns return an
explicit unsupported-type error instead of lossy stringification. Physical-compatible primitive,
string/binary, offset, and validity buffers are shared where the current Koko representation
permits; otherwise exactly one bounded column-wise conversion is performed. Imports participate in
the database tracker before allocation and are atomic per call.

The Arrow C Data Interface, FFI ABI, PyArrow, Polars, and connectors remain IM5.

### Logical database interchange

`EXPORT DATABASE '<dir>' [(format='csv'|'parquet', ...csv options...)]` creates a new logical
directory (`parquet` is the default); an existing directory is a runtime error. `SCHEMA_ONLY=true`
forbids every other option and emits no data files. `IMPORT DATABASE '<dir>'` reconstructs a fresh
in-memory database. The portable C++-compatible layout is:

- `schema.cypher`: user types, explicit sequences, node tables, relationship groups (all ordered
  FROM/TO pairs, multiplicity, directions, defaults/comments), exact sequence/SERIAL state restored
  with `CREATE SEQUENCE ... START <current>` plus `RETURN nextval(...)`, and macros in dependency
  order;
- one data file per node table and per physical relationship-group member, named deterministically
  and escaped safely;
- `copy.cypher`: ordinary COPY statements using explicit column lists and FROM/TO routing;
- `index.cypher`: an empty compatibility script because indexes remain IM5.

Import requires `schema.cypher`, parses every script, resolves and validates every referenced data
file, then executes `schema.cypher`, `copy.cypher`, and `index.cypher` through the normal
parser/binder/catalog/typed-batch paths inside one savepoint. It rejects missing/duplicate/unknown
files, a non-empty user catalog, and nested script failures without committed partial state. Paths
are safely quoted; identifiers are backtick-escaped. Empty tables still produce valid files/schemas.
Re-exporting the same snapshot is deterministic. No opaque manifest or direct catalog restoration
path is allowed: every restored object must be represented in executable Cypher.

Logical interchange is not native persistence: no database file, `Database::open`, WAL, checkpoint,
buffer pool, or crash-survival promise is added.

## Initial oracle probes

- C++ 0.17.0 resolved `~/home.csv` through `CALL home_directory=...`; for `pick.csv`, an existing
  process/base-directory file won over both comma-ordered `file_search_path` entries.
- A missing `.parquet` path failed with `No file found...` before format dispatch; an existing
  directory with `file_format='csv'` failed as `Provided path is not a file`; mixed concrete CSV and
  Parquet paths failed as `Copy exception: Loading files with different types...`.
- The NPY corpus establishes rank-1 scalar columns, rank-2 fixed arrays, and rank-3 arrays flattened
  row-major to the declared fixed ARRAY width. `BY COLUMN` rejects relationship targets and non-NPY
  inputs before execution; NPY accepts only `IGNORE_ERRORS`.
- A C++ CSV export produced `schema.cypher`, `copy.cypher`, `index.cypher`, one `<table>.csv` per
  node, and one `<rel>_<from>_<to>.csv` per rel-group member. `copy.cypher` used explicit properties
  plus `from=`/`to=` options; `schema.cypher` restored sequence state with `START <current>` followed
  by `RETURN nextval(...)`. This executable-script layout replaces the draft manifest idea.

## Dependency-ordered landings

### Landing A — resolver and CSV closure

1. Add `file_resolver` and `FileFormat` to `koko-common`; remove binder-local wildcard code and
   executor-local path joins.
2. Thread base/home/search settings into binding and execution. Resolve `LOAD` lists/globs and COPY
   lists/globs identically; preflight all inputs before writes.
3. Close BOM, malformed row, unterminated/escaped quote, quoted-newline, header/dialect precedence,
   `SAMPLE_SIZE`, `LIST_UNBRACED`, gzip extension/corruption, bare LOAD nested inference, and special
   numeric cases against focused C++ probes.
4. Replace CSV row-at-a-time storage calls with typed batches. Split seekable files into independent
   record-boundary ranges; gzip and quoted-newline serial mode stay single-worker. Bound in-flight
   batches by worker count and the memory tracker. Assign `(file_index, byte/row sequence)` and merge
   warnings/counts/errors deterministically.
5. Add focused resolver/CSV tests, C++ re-probes, format/Clippy, and commit.

### Landing B — Parquet and NPY

1. Add `parquet` with Arrow reader support and map schemas to Koko logical types, including
   decimals, timestamp units, nested list/array/map/struct values, nullability, and Impala timestamp
   compatibility.
2. Implement streaming projection and partial-column COPY/LOAD, file lists/globs, nodes,
   relationships, relationship groups, `IGNORE_ERRORS`, and codec-transparent reads.
3. Add `npyz`; parse one `.npy` file per target column for `COPY ... FROM (...) BY COLUMN`. Validate
   file count before shape, equal first-dimension row counts, endian marker, dtype-to-logical type
   mapping, node-only restriction, and option precedence. A rank-1 array maps to a scalar target;
   remaining dimensions flatten row-major into the target's fixed-size ARRAY shape (rank ≥2 is
   supported, matching the 2D/3D corpus).
4. Implement NPY `LOAD FROM` and match/query usage with the same batch source.
5. Add focused tests, direct affected corpus runs, C++ probes, format/Clippy, and commit.

### Landing C — Rust Arrow interchange

1. Add Arrow dependencies and a private column conversion module with exact field metadata.
2. Implement `QueryResult` export directly from existing `DataChunk` batches.
3. Implement exact-schema node and relationship import, group routing, tracked allocations, and
   transaction/savepoint rollback.
4. Document supported mappings and ownership on the public methods; add primitive, null, nested,
   multi-batch, schema-error, relationship, and bounded-memory tests.
5. Run focused tests, format/Clippy, and commit.

### Landing D — complete COPY and logical interchange

1. Finish query-result `COPY ... FROM (<query>)` and table-function source paths without row-matrix
   transposition; enforce arity/type/default/SERIAL and transaction semantics.
2. Resolve relationship-group COPY by endpoint PK types and route each row to the matching physical
   member. Implement explicit `FROM`/`TO` option validation and preserve input order/counts.
3. Add a real `CopyTo` AST/bound statement. Stream query batches to CSV or Parquet with exact headers,
   NULL/nested formatting, delimiter/quote/escape validation, overwrite behavior, empty results, and
   cleanup of partial outputs.
4. Add `ExportDatabase` / `ImportDatabase` AST and bound statements plus the versioned logical
   directory implementation above. Validate the entire import before mutation and run it as one
   transaction/savepoint.
5. Add exact round trips for CSV and Parquet with types, defaults, SERIAL/sequence state, macros,
   relationship groups, empty tables, and writes after import. Run focused corpus/oracle gates,
   format/Clippy, and commit.

## Verification after every landing

- run affected Rust unit/integration tests and affected upstream `.test` files/directories;
- reproduce oracle-sensitive behavior with the C++ shell and update TRIAGE immediately;
- if a P0 fixture changes, run `python3 docs/fable-audit/p0_to_probe.py` and accept no unledgered diff;
- confirm previously verified divergences stay closed;
- run `cargo fmt --all -- --check` and strict Clippy for touched targets;
- commit only an independently green landing with
  `Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>`.

## Final gates

1. Directly run all focused directories named by the IM3 prompt and dispose every frozen case.
2. Verify CSV and Parquet logical round trips, Arrow round trip, deterministic CSV results/warnings
   with `threads=1` and `threads>1`, and tracked-memory failure without leaked mutation.
3. Run workspace formatting, strict Clippy, and tests.
4. Run the strict standing goal gate and full direct corpus; re-diff P0 if changed.
5. Run optimizer-disabled and one-worker A/B suites; compare exact output.
6. Run all nine LSQB queries for correctness and no timeout, recording ratios without claiming the
   IM4 performance gate.
7. Synchronize `README.md`, `ROADMAP.md`, `AGENTS.md`, `docs/PROGRESS.md`, `docs/TRIAGE.tsv`, and
   `docs/PERF_GATE.md` to current evidence.
8. Remove probes/generated exports, confirm a clean tree, and commit closure.

## Frozen IM3 case inventory (94)

### CSV semantics (33)
- `csv/compressed_csv.CorruptGZIP`
- `csv/compressed_csv.ReadFromGZIPExtension`
- `csv/compressed_csv.ReadFromParquetError`
- `csv/compressed_csv.SCAN_COMPRESSED_CSV`
- `csv/edge_cases.EdgeCases`
- `csv/edge_cases.EscapedQuoteInList`
- `csv/edge_cases.HeaderNumColumnsSmallerThanActual`
- `csv/edge_cases.SpecialDoubleLiterals`
- `csv/errors.Errors`
- `csv/sniffing.ExtremeNest`
- `csv/sniffing.HeaderTest`
- `csv/sniffing.LargeStruct`
- `csv/sniffing.SniffDate`
- `csv/sniffing.SniffList`
- `csv/sniffing.SniffMap`
- `csv/sniffing.SniffStruct`
- `csv/unbraced_lists.Unbraced`
- `exceptions/duplicated.DuplicateIntIDsIgnoreErrorSpansMultipleBlocks`
- `exceptions/duplicated.DuplicateIntIDsIgnoreErrors`
- `exceptions/duplicated.DuplicateStringIDsIgnoreErrors`
- `exceptions/duplicated.ManyDuplicateIntIDsIgnoreErrors`
- `exceptions/duplicated.SerialManyDuplicateIntIDsIgnoreErrors`
- `exceptions/ignore_invalid_row.ParallelCopyFromCSVWithMultipleBlocksIgnoreErrors`
- `exceptions/ignore_invalid_row.ParallelSkipInvalidLoadFrom`
- `exceptions/ignore_invalid_row.ParallelSkipInvalidNodeTableRowsMixed`
- `exceptions/ignore_invalid_row.ParallelSkipInvalidNodeTableRowsMixedLimitWarnings`
- `exceptions/ignore_invalid_row.ParallelSkipInvalidNodeTableRowsMixedLimitWarningsMultipleQueries`
- `exceptions/ignore_invalid_row.ParallelSkipInvalidRelMixed`
- `exceptions/ignore_invalid_row.SerialSkipInvalidLoadFrom`
- `exceptions/ignore_invalid_row.SerialSkipInvalidNodeTableRowsMixed`
- `exceptions/ignore_invalid_row.SerialSkipInvalidNodeTableRowsMixedLimitWarnings`
- `exceptions/ignore_invalid_row.SerialSkipInvalidRelMixed`
- `exceptions/ignore_invalid_row.SkipInvalidLoadFromCompressedCSV`

### File resolution (12)
- `copy/copy_long_string_multiple_files.CopyLongStringMultipleFilesTest`
- `copy/copy_multiple_files.CopyFilesWithHomeDir`
- `copy/copy_multiple_files.CopyFilesWithSearchPath`
- `copy/copy_multiple_files.CopyFilesWithWildcardPattern`
- `exceptions/duplicated.ManyDuplicateIntIDsMultipleFilesIgnoreErrors`
- `exceptions/empty_db_binder_error.WrongNumOfNumpyFiles`
- `exceptions/ignore_invalid_row.ParallelCopyFromMultipleLargeFilesSkipInvalid`
- `exceptions/ignore_invalid_row.ParallelInvalidNodeTableRowsMultipleFiles`
- `exceptions/ignore_invalid_row.ParallelSkipInvalidNodeTableRowsMultipleFiles`
- `exceptions/ignore_invalid_row.ParallelSkipInvalidNodeTableRowsMultipleFilesSomeEmptyHasHeaderMismatch`
- `exceptions/ignore_invalid_row.SerialSkipInvalidNodeTableRowsMultipleFiles`
- `glob/longpath.LongPath`

### Parquet (19)
- `copy/copy_node_parquet.CopyNodeTest`
- `copy/copy_node_parquet.IgnoreErrorsTest`
- `copy/copy_parquet.CopyParquet`
- `copy/copy_partial_column.NodePartialColumnsTest`
- `copy/copy_snap_twitter_parquet.CopySNAPTwitterParquet`
- `copy/mismatch_from_to.MismatchFromToType`
- `demo_db/demo_db_copy.DemoDBCopyFromParquet`
- `demo_db/demo_db_order_parquet.DemoDBOrderedTestFromParquet`
- `demo_db/demo_db_parquet.DemoDBTestFromParquet`
- `exceptions/wrong_header.NodeUnmatchedNumColumns`
- `exceptions/wrong_header.ParquetHeaderMismatch`
- `exceptions/wrong_header.RelUnmatchedNumColumns`
- `load_from/load_from.LoadFromParquetTest`
- `nested_types/large_array.CopyLargeArray`
- `reader/compression.BROTLIDecompress`
- `reader/compression.GZIPDecompress`
- `reader/compression.ZSTDDecompress`
- `reader/timestamp.impalaTimestamp`
- `reader/timestamp.timestampMSNS`

### NPY/BY COLUMN (10)
- `copy/copy_npy_large.CopyLargeNpyTest`
- `copy/copy_npy_one_dimensional.CopyOneDimensionalNpyTest`
- `copy/copy_npy_one_dimensional.CopyOneDimensionalNpyTestIgnoreErrors`
- `copy/copy_npy_one_dimensional.CopyOneDimensionalNpyTestInvalidOption`
- `copy/copy_npy_three_dimensional.CopyThreeDimensionalNpyTest`
- `copy/copy_npy_two_dimensional.CopyTwoDimensionalNpyTest`
- `exceptions/npy_fault.CopyNpyToRelTable`
- `explain/explain.Explain`
- `load_from/load_from.LoadFromNpyTest`
- `npy_1d/match.MatchNpy_1d`

### Logical export/import (12)
- `copy/copy_large_serial.ExportLargeSerial`
- `copy/export_import_db.DocExportImportExampleCSV`
- `copy/export_import_db.DocExportImportExampleParquet`
- `copy/export_import_db.ExportDBWithOptions`
- `copy/export_import_db.ExportDatabaseWithSerialTable`
- `copy/export_import_db.ExportImportDatabaseDefault`
- `copy/export_import_db.ExportImportDatabaseError`
- `copy/export_import_db.ExportImportDatabaseRelGroup`
- `copy/export_import_db.ExportImportDatabaseWithCSVOption`
- `copy/export_import_db.ExportImportDatabaseWithPARQUET`
- `copy/import_legacy_relgroup_db.ImportDBWithLegacyRelGroup`
- `issue/issue2.3416`

### Relationship-group COPY (7)
- `copy/copy_from_tinysnb.CopyFromLegacyRelGroup`
- `rel_group/basic.Alter`
- `rel_group/basic.BulkInsertManyFromTo`
- `rel_group/basic.COPYWithFROMTO`
- `rel_group/basic.DMLCreateWithSRCandDEST`
- `rel_group/basic.Drop`
- `rel_group/basic.SimpleMatch`

### Query-source COPY (1)
- `transfer_demo/transfer_demo.TransferDemo`
