# Milestone progress and evidence log

> **Historical chronological evidence; not a live gate or backlog.** This file records the M1–M5
> correctness campaign, IM1–IM5 product landings, the first-party CLI, the facade decomposition,
> and the pre-user idiomatic Rust architecture/API cutover. Counts, toolchain versions, commands,
> and Ladybug comparisons below are snapshots from their named landings.
>
> Current product scope, work, limitations, intentional decisions, and verification policy live in
> [`../ROADMAP.md`](../ROADMAP.md). Native durability remains permanently deferred. Arrow C,
> extensions/projected graphs, connectors, and foreign bindings remain owner-deferred.
> `TRIAGE.tsv` is the frozen 23-case final-IM5 corpus residual manifest, not a current bug count.

## Historical scorecard

| when | fix-me rows | battery DIFFs | panics | p0-diff files | corpus p/s/f | note |
|---|---|---|---|---|---|---|
| 2026-07-01 (audit baseline) | ~379 untriaged | ~45 | 2 sites; 142/390 arity | 18/48 | 1257/341/379 + demo_db panics | pre-M1 |
| 2026-07-02 (M1 in flight, @bf71406) | untriaged (TRIAGE.tsv next) | not yet re-run | **0 arity; C2 rejected clean; runner contains panics** | **0 unledgered** (37 clean/11 ledgered/4 skip, per-case) | full sweep pending; spot dirs all ≥ baseline | M1 tasks 1–6 landed |
| 2026-07-02 (fresh sweep, @1294726) | TRIAGE fan-out running | **1060 stmt-DIFFs** (statement-level; audit's ~45 counted rows) | **0** (corpus + arity) | **0 unledgered** | **1354/368/429**, 0 parse errors, demo_db runs | goal_gate 1,2,3,5 green |
| **2026-07-02 (M1 closed, @7e86d60)** | **275 fix-m*** (of 427 triaged: m2 69 · m3 23 · m4 104 · m5 79; p4 137 · p5 11 · div 4) | 1060 stmt-DIFFs (M2–M5 drain list: target/battery_unledgered.txt) | **0** | **0 unledgered** | **1354/368/429** | goal_gate 1,2,3,5,6 green; 4 = the fix backlog |
| 2026-07-02 (M2 numerics, @06eade3) | draining (V1 V3 V5 V6 V7 V16 R2 V9 done) | shrinking (re-count at M2 close) | 0 | 0 unledgered | spot: fn 41p tck 292p arith 91p agg 11p uint128 7p (all ≥) | A/B + perf gate verified @V3 |
| 2026-07-02 (M2 temporal+unicode, @f8cdfa2) | draining (+V2, V10) | string_case/regexp2/regex_invalid all 0 | 0 | 0 unledgered | exceptions 11p, issue 46p (all ≥) | V2: strict date/ts + C++ wordings; V10: 1:1 case map, ASCII regex classes, RE2 lenient-invalid + \N rewrites |
| **2026-07-02 (M2 29 items, @379e746)** | draining | **877 unledgered** (was 1060) | 0 | 0 unledgered | **ldbc 3p/0f (green!)**, issue 47p, optional_match 2p, ddl 69p, copy 48p, demo_db 16p — all ≥ | V11 W2 + V12 slice landed |
| **2026-07-02 (M2 closed, @0fa70fd)** | **225 fix-m*** (m2 19 residual · m3 23 · m4 104 · m5 79; p4 165 · p5 11 · div 4) | 873 unledgered | **0** (+0 timeouts) | **0** (38 clean/10 ledgered) | **1376/368/407** | goal_gate 1,2,3,5,6 green |
| **2026-07-05 (M3 closed, @8705380)** | **199 fix-m*** (m2 18 residual · m3 0 · m4 102 · m5 79; p4 165 · p5 11 · div 4) | **783 unledgered** (was 873) | **0** | **0** (38 clean/10 ledgered) | **1402/368/381** | goal_gate 1,2,3,5 green; 6 = fix-m* backlog only |
| **2026-07-05 (M4 in flight, @94e3b0e)** | **149 fix-m*** (m2 16 · m4 58 · m5 75; p4 165 · p5 11 · div 4) | **763 unledgered** | **0** | **0** | **1452/368/331** | M4 stretch: +50 cases (COPY channel, PK family, validations, interval, SKIP/LIMIT) |
| **2026-07-05 (M4 stretch 2)** | **128 fix-m*** (m2 15 · m4 38 · m5 75; p4 152 · p5 11 · div 4) | **761 unledgered** | **0** | **0** | **1469/384/298** | parser decoration, type names, projection/properties/SET/DELETE/UNWIND checks, UTF-8 wrapper, SKIP_IN_MEM |
| **2026-07-06 (M4 stretch 3, @df67714)** | **89 fix-m*** (m2 13 · m4 14 · m5 62; p4 152 · p5 11 · div 4) | **723 unledgered** | **0** | **0** | **1509/384/258** | binder_error 1→80, aggregate gate, unlabeled-CREATE, ^ operator, struct compare, value-struct fields, overflow literals, map-key gating |
| **2026-07-06 (M4 stretch 4, @1d5bb5a)** | **85 fix-m*** (m2 14 · m4 7 · m5 64; p4 152 · p5 11 · div 4) | **721 unledgered** | **0** | **0** | **1513/384/254** | cast_error 172-stmt green, nested range wording, quantifiers, rel-group endpoint check, struct_extract rendering |
| **2026-07-06 (M4 case-work done, @bf2cdd4)** | **80 fix-m*** (m2 14 · m4 2 · m5 64; p4 152 · p5 11 · div 4) | **723 unledgered** | **0** | **0** | **1518/384/249** | hygiene PASS; hint subsystem, storage-direction checks, em-dash, endpoint hints; remaining m4 = parked null_pk + LoadFromCSVTest |
| 2026-07-06 (diffprobe bisection, @9faeb3e) | 80 fix-m* | **429 real** (+15 cpp-crash) | 0 | 0 | 1518/384/249 | resilient prober: C++ -b crash no longer wipes the file's blocks; error blocks kept raw |
| **2026-07-06 (M5 sweep-in-progress, @72b2a51)** | re-count pending | **~200 and falling** (full re-run in flight) | **0** | **0** | **1542/384/225** | operator tiers (IN/STARTS WITH/=~/bitwise/!), fn backfill (hash/random PCG32/md5/sha256/quantifiers/concat_ws/…), CASE parity, unbound-param NULL, comparison Type Mismatch, BOOL-cast strictness, date_part specifier table, split empty-token rule, array_extract/map char-extract, {} rejection, table-func backfill (db_version/show_indexes/…), float→int boundary wrap, node/rel CAST rendering; A/B + perf gates OK |
| **2026-07-06 (M5 battery ~drained, @7d1161d)** | **59 fix-m*** (m5 43 · m2 14 · m4 2; TRIAGE @4eb62ec) | **~10 real** (last full: 109 @e33da1a; since: params/case-insens/comparison/tablefns/datepart/split/pattern-comp/percentileDisc/rowid + ledger citations) | **0** | **0** | **1543/384/224** | remaining battery: CSV numeric sniffing, relsrc2 properties()-Actual (REL); fix-m5 queue: CALL/YIELD in-query, stats_info real counts, storage_info chunk rows, WSHORTEST, PROJECT_GRAPH/GDS, runner ${KOKO_VERSION}, EXPLAIN corpus, COPY FROM subquery, nested subqueries |
| **2026-07-06 (M5 fix-rows draining, @4f165fb+)** | ~38 fix-m* (recount next sweep) | **0 known real** (sniff landed; gate re-run pending on quiesced tree) | **0** | **0** | **1567/384/200** | CSV numeric sniffing + typed headers; in-query CALL/YIELD/multi-CALL/MATCH+CALL; pattern predicates = EXISTS rewrite; list fn inner-type/greatest-flavor/regexp-option errors; negated-u128; SET/struct ANTLR errors; SHOW_OFFICIAL_EXTENSIONS/_CACHE_ARRAY_COLUMN_LOCALLY. Remaining fix-m5: COPY FROM subquery + partial columns, WSHORTEST ×3, stats/storage real counts, OPTIONAL-then-MATCH, WITH node passthrough (issue.2589), runner multi-statement pairing, in-query current_setting, union/struct list combine; fix-m2 12 (path rendering match6/7, casts); fix-m4 2 parked |
| **2026-07-06 (M5 engine features, @dfb4326)** | **33 fix-m*** (m5 19 · m2 12 · m4 2) | 0 known real | **0** | **0** | **1586/384/181** | COPY FROM subquery/TABLE_INFO + IGNORE_ERRORS + warnings registry (show/clear_warnings, warning_limit, 0-based query ids, summary row) + rel SKIP/partial-column defaults; ACYCLIC = distinct intermediates only (recursive_join green); nested subqueries + WITH-WHERE subqueries (subquery dir green); LOAD-subquery rejection. A/B + perf gates OK ×2. Remaining m5: WSHORTEST ×4, OPTIONAL-then-MATCH, issue.2589 WITH-node, exception.MultiStatements runner pairing, call ×2, list ×2, timestamp NS repr, copy legacy relgroup + current_setting source, RelPartial serial |
| **2026-07-06 (M5 battery drained to sniff, @86c718f)** | 59 fix-m* (m5 43 · m2 14 · m4 2) | **~1 real** (sniff.probe CSV numeric typing; rest fixed or cited: percentile alias, is_trail/is_acyclic, START_NODE/END_NODE rewrite + endpoint materialization, rowid, NULL-list lambda error, exprpos clean) | **0** | **0** | **1549/384/218** | gate non-strict: 1,2,3,5,6 PASS; strict needs battery 0 + fix-m* 0; A/B + perf gates OK @421672d |
| **2026-07-07 (M5 CLOSE — cast/quote/list rules, table fns, WSHORTEST, gz/multi-file LOAD, blank-line PK, @a2d7ed2)** | **0 fix-m*** (p4 129 · p5 12 · div 4) | **0 unledgered** (143 probes / 76 DIFFs, all ledgered; §4 added) | **0** | **0** | **1622/384/145** | cast/ dir green (quote/struct/list/static-cast rules); table fns (current_setting/bm_info/show_loaded_extensions); **WSHORTEST Dijkstra + cost()**; struct-spread `a.state.*`; map dup-key; nested-agg alias; runner (threads=2, `----`-marker, continuation delim, multi-stmt); COPY arity/null-PK/header-gating/escape-EOF; optional_match split; issue.2589 WITH-node; degenerate-OPTIONAL null slots; **gzip CSV + multi-file/glob LOAD + LOAD-after-WITH**; interior-blank null-PK records + total-vs-stored warning count. A/B byte-identical; perf OK. **`scripts/goal_gate.py --strict` VERIFIED GREEN @faaaa50 (exit 0): hygiene/sweep/p0/battery/arity/triage all PASS.** |
| **2026-07-19 (IM2 closed)** | **0 fix-m*** (historical buckets: p4 125 · p5 12 · div 4) | **0 unledgered** (143 probes / 76 DIFFs, all ledgered) | **0** | **0** (43 clean / 5 ledgered / 4 skip) | **1626/384/141** | typed/chunked storage + projected batches; concrete `InMemStorage`; direct typed result batches; exact/prepared metadata; config + tracked memory; per-property snapshot stats; 195 workspace tests; strict gate green; A/B byte-identical; all 9 LSQB correct |
| **2026-07-20 (IM3 closed)** | **0 fix-m*** (residual: p4 31 · p5 12 · div 4) | **0 unledgered** (143 probes / 76 DIFFs, all ledgered) | **0** | **0** (43 clean / 5 ledgered / 4 skip) | **1720/384/47** | all 94 frozen cases pass; resolver + complete CSV/parallel/Parquet/NPY/Arrow/COPY/export/import; strict gate green; workspace hygiene green; 94-case A/B byte-identical; all 9 LSQB correct |
| **2026-07-20 (IM4 closed)** | **0 fix-m*** (historical buckets: p4 29 · p5 12 · div 4) | **0 unledgered** (143 probes / 76 DIFFs, all ledgered) | **0** | **0** (43 clean / 5 ledgered / 4 skip) | **1763/343/45** | overlapping snapshots + bounded multi-writer MVCC; interrupt/deadline/thread controls; complete structural harness; tracked memory; strict gate and A/B invariants green; repeated nine-query LSQB ≤2× gate green |
| **2026-07-21 (IM5 provisional close; reopened)** | **0 corpus fix-m*** (residual: durability 15 · owner-deferred 4 · div 4) | **0 corpus-battery unledgered** (143 probes / 75 DIFFs, all ledgered) | **0** | **0** (43 clean / 5 ledgered / 4 skip) | **1785/343/23** | six landings reached strict corpus/focused gates; post-close audit found active `ANY` semantics/snapshot, graph/index interchange, and true file-backed-scan gaps outside that matrix |
| **2026-07-22 (IM5 final close)** | **0 corpus fix-m*** (residual: durability 15 · owner-deferred 4 · div 4) | **0 corpus-battery unledgered** (143 probes / 75 DIFFs, all ledgered) | **0** | **0** (43 clean / 5 ledgered / 4 skip) | **1785/343/23** | hidden-table normal-pipeline `ANY`; atomic database-wide graph/index interchange; pinned projected query-time `icebug-disk`; focused correction/resource gates, A/B invariants, workspace hygiene, and repeated nine-query ≤2× LSQB gate green |
| **2026-07-24 (facade architecture closed)** | **0 corpus fix-m*** (residual unchanged: durability 15 · owner-deferred 4 · div 4) | **0 corpus-battery unledgered** (143 probes / 0 DIFFs) | **0** | **0** | **1785/343/23** | 68-line composition root; private runtime state capsule; explicit COPY/Arrow/interchange contexts; contract tests under `src/tests/`; 396 workspace tests, strict Clippy/fmt/rustdoc, 23-case CLI gate, strict corpus gate, and repeated nine-query ≤2× LSQB gate green |
| **2026-07-25 (idiomatic Rust cutover closed)** | unchanged product ledger | **0 unledgered** (43 clean / 5 ledgered / 4 skip) | **0 observed** | **0** | **1783/343/25 external 0.17 baseline preserved in default and single-worker modes** | `koko-ir`; generated typed function registry; structural catalog/loader/storage/operator ownership; namespaced exclusive facade; 63-line composition root; 401 workspace tests, strict generator/Clippy/fmt/CLI gates, public API contract, and repeated nine-query ≤2× LSQB gate green |

## Milestones (gates in ROADMAP.md; evidence required to check)

- [x] **M1 — Trust** *(closed 2026-07-02 @7e86d60 — exit gates: 0 panics corpus+arity fleets;
      demo_db runs 15p/2s/7f; 0 unparsed .test files; ledger committed w/ divergences.test;
      fresh baseline 1354p/368s/429f recorded; TRIAGE.tsv 427/427 rows validated;
      goal_gate.py checks 1,2,3,5,6 green)*
  - [x] arity guard at bind (142→0 panics; `scripts/arity_sweep.sh` panics=0) @cbf2bc7
  - [x] C2 clean-reject (recursive-lambda subquery; demo_db runs 15p/2s/7f) @01ee0df
  - [x] runner `catch_unwind` per statement/dataset-load @2b0a20a
  - [x] harness fidelity: EMPTY case-insens, -SKIP comment+header, -WASM_ONLY, skipped-body
        skim @95f9676; error(regex), hash(MD5), -CHECK_COLUMN_NAMES, -SET/REPEAT/ARANGE +
        ${var}, -MULTI_COPY_RANDOM, 1-ULP CHECK_PRECISION @a64d547
  - [x] historical compatibility-decision inventory + `tests/p0/divergences.test`; parity fixes: labels()
        scalar, DROP MACRO IF EXISTS "Marco", numeric-string strictness, SET += removed
        @cb18443; property-wording, missing-PK binder check, CALL has_return, catalog class
        @5a18616
  - [x] p0 re-tune: `p0_to_probe.py` per-case + ledger-cited exclusion → **0 unledgered
        diffs** @5a18616
  - [x] fresh full sweep → `docs/TRIAGE.tsv` 427 rows (0 missing/stale/malformed) @7e86d60
  - [x] `scripts/goal_gate.py` (criteria 1–6; battery persists its unledgered list) @68a6715
- [x] **M2 — Silent wrong values** *(closed 2026-07-02 @0fa70fd — every §3.2 audit row fixed
      or ledgered: numerics V1 V3 V5 V6 V7 V9 V14 V16 R2 · temporal V2 · unicode V10 · write
      path W1 W2 W8 W9 + V11 · seam V12 (MaterializeValues op; ldbc + match dirs green) ·
      CSV/LOAD W3 W4 W5-core · rendering R1 R3–R7 V13 V15 V17. C2 ledger-retargeted → M5
      (shares nested-subquery machinery); escape-strictness → M4 (needs the Copy wrapper);
      IGNORE_ERRORS → p4 (ROADMAP COPY-option scope). Gates: a1/a6verify probes fully SAME
      (a7 residue = M3's V4); sweep 1376p/368s/407f 0 panics; A/B byte-identical
      lsqb/agg/match; perf gate all 9 OK q6 2x q9 3x; 19 case-level fix-m2 residuals with
      stale notes stay in TRIAGE for re-triage during M3)*
- [x] **M3 — Signature catalog** *(closed 2026-07-05 @8705380 — the oracle catalog is the
      declarative table: catalog_data.rs (1214 overload rows, generated by
      scripts/gen_fn_catalog.py, show_functions dump byte-identical) + the sigcatalog gate
      (C++ matchFunction over the koko-common cast_cost matrix; REWRITE rows included;
      ledgered supersets exempt only for plausible args). Coercion both polarities:
      →STRING @6dfbcc2, V4 INT64-strictness @a6d16c3; overload-error format byte-exact
      (tails @a6d16c3, range 22-line table @5a6bdce, catalog Expected blocks @906d21a);
      combine lattice + coalesce strict fallback @213cb8a; LIST_CREATION mixed-type
      fallback @21442c3; UNION implicit casts + min-cost tag @576ad55; ALTER/CREATE
      DEFAULT gate @63c25a3; CALL-option registry + float→integral rejection @c130fd8;
      cardinality→SIZE, interval parse wording @906d21a. Gates: differential arity sweep
      199 names × 0..3-arg shapes — all diffs ledgered supersets or probe false-positives;
      arity_sweep panics=0; implicit_cast 21p/1f (residual = fix-m2 LOAD sniff);
      0 fix-m3 TRIAGE rows; sweep 1402p/368s/381f; battery 873→783)*
- [x] **M4 — Validation + error channel** *(closed 2026-07-06 @bf2cdd4 — §3.3 validation family
      (PK allow-list + float PkKey, storage_direction check), error classes/decoration/wrapper,
      stage+order parity, wording sweep; binder_error 1→80, cast_error/cast_to_nested green,
      hint subsystem; hygiene PASS).*
- [x] **M5 — Language surface** *(closed 2026-07-07 @a2d7ed2 — operator tiers · SKIP/LIMIT + $1 ·
      zero-label CREATE · case-insensitive vars · nested + WITH-WHERE subqueries ·
      MATCH-after-OPTIONAL · graph-expr-through-WITH (issue.2589) · function backfill · intervals/
      formats · CALL/YIELD + table fns (current_setting/bm_info/show_loaded_extensions) ·
      EXPLAIN/PROFILE · **WSHORTEST (Dijkstra + cost())** · struct-field spread `a.state.*` ·
      map dup-key check · gzip CSV · multi-file/glob LOAD · chained LOAD-after-WITH · blank-line
      null-PK records · degenerate-OPTIONAL path slots · comprehension retirement (ledgered M1).
      **Exit gate: zero fix-m* rows; deviation battery 0 unledgered; A/B byte-identical; perf OK.*)
- [x] **IM1 — Embedding correctness & state ownership** *(closed 2026-07-19 — explicit
      database/connection/query ownership, context-aware table functions, effective settings,
      authoritative transaction coordinator; strict gate and A/B invariants green).*
- [x] **IM2 — Columnar engine & Rust API** *(closed 2026-07-19 — explicit storage handles;
      typed/chunked property storage, projected/batched reads and mutations, concrete storage API;
      direct typed result batches and exact schemas; checked/named/typed access; prepared metadata;
      configuration, tracked memory, and snapshot-correct per-property statistics; strict gate and
      LSQB correctness green).*
- [x] **IM3 — Ingestion & interchange** *(closed 2026-07-20 — deterministic resolver;
      oracle-faithful CSV and bounded parallel loading; Parquet/NPY/Rust Arrow; complete COPY;
      logical CSV/Parquet export/import; all 94 frozen cases and standing gates green).*
- [x] **IM4 — Concurrency, controls & performance** *(closed 2026-07-20 — overlapping readers and
      reader/writer snapshots; bounded product multi-writer MVCC; interrupt/deadline APIs;
      deterministic tracked-memory exhaustion; all frozen structural harness cases executed;
      both IM4 TRIAGE rows retired; repeated nine-query LSQB ≤2× gate green).*
- [x] **IM5 — Rust-embedded core completion** *(closed 2026-07-22 — typed and `ANY` named graphs
      share the normal Cypher/MVCC pipeline; ordered JSON and graph-scoped HASH/ART indexes;
      atomic database-wide logical interchange; validated query-time local `icebug-disk`; native
      scalar UDFs; exact/focused/workspace/oracle/A/B/LSQB gates green).*

## Completed handoff

M1–M5 and IM1–IM5 are closed. `ROADMAP.md` owns the completed product/deferred boundary,
`IM5_PLAN.md` §7 owns the correction history and final evidence, and both IM5 goal prompts are
historical contracts that must not be resumed.

**IM2 close evidence (2026-07-19):**

- `StorageReadHandle` and `StorageWriteHandle` carry every visibility decision. No storage object
  retains a mutable read view.
- `InMemStorage` owns typed fixed-capacity property chunks, projected scans/property gathers,
  batched adjacency and mutations, MVCC-aware PK lookup, and snapshot statistics. The obsolete
  `StorageBackend` trait was deleted; loader/processor/database call the concrete engine directly.
- Processor projection, aggregation, `UNION`, `DISTINCT`, ordering, skip, and limit emit or compact
  typed chunks column-wise. `ExecResult` and `QueryResult` have one final representation: exact
  schema plus owned typed batches. Row iteration and named/typed column views borrow those buffers.
- Named access rejects missing and duplicate/ambiguous columns. Results preserve exact types across
  empty, NULL-only, aggregate, union, graph, temporal-flavor, and multi-batch outputs.
- Preparation binds against the current catalog and publishes ordered deduplicated parameters,
  inferred/`Any` types, exact result schema, read-only/statement kind, and write metadata. Execution
  refreshes metadata atomically when the committed or transaction-local catalog changes.
- `DatabaseConfig` enforces worker caps and tracked-memory limits; custom `MemoryResource` hooks,
  current/peak/limit usage, storage/result reservations, and RAII release are observable.
  Whole-query exhaustive accounting and spill remain IM4.
- `stats_info` publishes cardinality and every catalog-ordered property distinct count, including
  NULL, with committed and transaction-local snapshot behavior. Four stale TRIAGE rows
  (`clear_warnings` plus three `stats_info` cases) were retired; the manifest now exactly matches
  the 141 remaining corpus failures.
- No index DDL was added because neither an IM2 correctness result nor measurement justified it.
  No durability, IM3 interchange, IM4 concurrency/control/spill, or IM5 ecosystem surface landed.

**Verification:**

- `cargo fmt --all && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace`
  — clean; 195 tests passed across 26 suites.
- Focused public regressions cover multi-batch typed views, final `DISTINCT`/ordering/skip/limit and
  `UNION`, duplicate-name ambiguity, truthful summaries, prepared metadata/catalog refresh,
  configuration, memory enforcement/release, and snapshot statistics.
- `python3 scripts/goal_gate.py --strict` — GREEN: corpus 1626 passed / 384 skipped / 141 exactly
  triaged failures; 0 panic files; 0 unparsed; demo DB ran; P0 43 clean / 5 ledgered / 4 skipped;
  battery 143 probes / 76 ledgered DIFFs / 0 unledgered; arity panics 0; TRIAGE 141 rows /
  0 missing / 0 stale / 0 `fix-m*` / 0 malformed.
- A/B runs are byte-identical (stdout, stderr, and exit code) under default,
  `KOKO_NO_OPTIMIZE=1`, and `KOKO_THREADS=1`: agg 13p/0s/1 known failure; match 8p/1s/0f;
  lsqb 1p/1 expected Parquet skip/0f.
- `python3 scripts/perf_gate.py` — all nine `lsqb-sf01` answers correct, no timeout. The final
  isolated-process run measured q1 7.96 s / 9× C++; q2 73 ms / 2×; q3 380 ms / 3×;
  q4 95 ms / 0.03×; q5 530 ms / 0.16×; q6 806 ms / 6×; q7 968 ms / 0.34×;
  q8 1.20 s / 5×; q9 2.10 s / 3×. Ratios are single-run and baseline-sensitive; IM4 owns
  the final ≤2× per-query gate.

**IM3 close evidence (2026-07-20):**

- `koko-common::file_resolver` is the single deterministic local-source path for literal files,
  ordered lists, globs, `~`, connection `home_directory`, and ordered `file_search_path`; unsupported
  remote sources fail explicitly.
- CSV now covers BOM/dialect/escape/header/skip/autodetect/list/null/gzip/malformed-row semantics,
  deterministic warnings, all-file preflight, and bounded seekable-file parallel parsing. Serial and
  parallel readers produce the same rows/warnings, and tracked batch reservations release on error.
- Parquet and NPY inspect, preflight, project, read, and write bounded typed batches. Parquet
  preserves exact supported scalar/nested metadata and normalizes query-output `ANY`/`UNION`
  deliberately. Native Rust Arrow import/export covers exact schemas, null/nested values,
  multi-batch and empty results, preflighted type errors, memory limits, and atomic rollback.
- COPY now covers relationship-group pair routing, query sources, partial/default/sequence/SERIAL
  columns, multi-file atomicity, CSV/Parquet/NPY input, and CSV/Parquet query output.
  Logical CSV/Parquet export/import reconstructs schema, comments/defaults/macros, sequences/SERIAL
  state, empty tables, relationship groups, and rows into a fresh writable in-memory database.
- All 94 frozen cases pass without waiver or later-phase reclassification. Their exact IDs remain
  frozen in `IM3_PLAN.md`; every corresponding TRIAGE row was retired, leaving 47 residual rows
  (31 `p4`, 12 `p5`, 4 divergences).

**IM3 verification:**

- `python3 scripts/goal_gate.py --strict` — GREEN: corpus 1720 passed / 384 skipped / 47 exactly
  triaged failures; 0 panic files; 0 unparsed; demo DB ran; P0 43 clean / 5 ledgered / 4 skipped;
  battery 143 probes / 76 ledgered DIFFs / 0 unledgered; arity panics 0; TRIAGE 47 rows /
  0 missing / 0 stale / 0 `fix-m*` / 0 malformed.
- `cargo fmt --all --check`, strict workspace Clippy, and `cargo test --workspace` are green.
  Focused regressions cover CSV dialect/diagnostics/parallelism/memory release; Parquet and NPY
  metadata, shape, scale, and round trips; Arrow API atomicity; COPY; and logical interchange.
- Default, `KOKO_NO_OPTIMIZE=1`, and `KOKO_THREADS=1` stdout/stderr/exit status are byte-identical
  for all 94 frozen cases and for the standing `agg`, `match`, and `lsqb` suites.
- `python3 scripts/perf_gate.py` — all nine `lsqb-sf01` answers correct, no timeout. The
  isolated-process run measured q1 8.76 s / 7× C++; q2 109 ms / 1×; q3 419 ms / 2.1×;
  q4 103 ms / 0.02×; q5 442 ms / 0.08×; q6 1.35 s / 6×; q7 2.51 s / 0.38×;
  q8 3.07 s / 8×; q9 5.37 s / 4×. Ratios are single-run and baseline-sensitive; IM4 owns
  the universal ≤2× per-query gate.

**IM4 close evidence (2026-07-20):**

- Database coordination and immutable catalog/macro snapshots are separated from
  `SharedStorage`; query execution no longer holds one database-wide mutex. Deterministic
  rendezvous tests prove reader/reader and reader/writer overlap, pre-commit snapshot stability,
  and post-commit visibility without cloning the database.
- `DatabaseConfig::with_max_concurrent_writers` productizes writer admission with a default of one.
  Shared writer-tagged versions and undo cover disjoint commits, same-row update/delete conflicts,
  duplicate-PK inserts, same/different-name DDL, commit ordering, statement errors, explicit
  rollback, and conflict-triggered whole-transaction rollback.
- `InterruptHandle` uses a query-captured epoch; connection deadlines are set in milliseconds or as
  a `Duration`. Processor loops, recursive frontiers, loaders/interchange, mutations, and parallel
  workers return catchable `Interrupted.` errors, roll back partial auto-commit work, release
  reservations, and leave later queries usable.
- Query-owned accounting now charges carried/output chunks, aggregate/hash/sort/distinct/recursive/
  MERGE collections, adjacency and PK capacity, row versions, and undo payload before growth.
  Repeated aggregate and COPY exhaustion returns the oracle buffer-manager error, preserves state,
  and restores current usage. Profiling did not justify temporary operator spill.
- The runner implements oracle PCG32 dynamic sets, nested loops, batch statement files, manual
  schema/row-wise dataset construction, and connection-group concurrency. All 41 frozen active
  structural cases execute; upstream `-SKIP` and `-SKIP_IN_MEM` remain authoritative.
- Measured execution work added projection/liveness pruning, columnar extend output and adjacency
  visibility/count fast paths, aggregate specialization, and a columnar mark hash join for
  uncorrelated `EXISTS`/`COUNT`. Parallel joins/sort/frontiers, SIP, generalized plan caching, and
  temporary spill did not land because the frozen measurement rule did not justify them.

**IM4 performance verification:**

- `python3 scripts/perf_gate.py` — PASS under the frozen protocol: one warm-up plus three paired,
  alternating isolated-process samples. All nine answers are correct and no run times out. Median
  Rust/C++ ratios: q1 0.462170, q2 0.334706, q3 1.034993, q4 0.001219, q5 0.008620,
  q6 1.162393, q7 0.179342, q8 1.528232, q9 0.833280. Every query is <2×; q4/q5/q7
  remain <1×. No query triggered the boundary-resampling rule.

**IM4 final verification:**

- `python3 scripts/goal_gate.py --strict --reuse-sweep target/goal-sweep` — GREEN: corpus
  1763 passed / 343 skipped / 45 exactly triaged failures; 0 panic files; 0 unparsed; demo DB ran;
  P0 43 clean / 5 ledgered / 4 skipped; battery 143 probes / 76 ledgered DIFFs / 0 unledgered;
  arity panics 0; TRIAGE 45 rows / 0 missing / 0 stale / 0 `fix-m*` / 0 malformed. The gate's
  workspace build, tests, formatting, and strict Clippy stage is green.
- Fresh full-corpus default and `KOKO_THREADS=1` runs have byte-identical stdout, stderr, and exit
  status across all 52 directories. Default and `KOKO_NO_OPTIMIZE=1` are byte-identical for the
  standing `agg`, `match`, and `lsqb` A/B suites; all six runs exit successfully.

## IM5 provisional close evidence

- `python3 scripts/goal_gate.py --strict` — GREEN: **1785 passed / 343 skipped / exactly 23
  deferred-or-ledgered failures**; 0 panic files; 0 unparsed; P0 43 clean / 5 ledgered / 4
  skipped; battery 143 probes / 75 ledgered DIFFs / 0 unledgered; arity panics 0; TRIAGE 23 rows /
  0 missing / 0 stale / 0 malformed.
- Differential probes: 24 graph/JSON/index statements and 7 local `icebug-disk` statements, all
  identical to C++. Focused retained-surface coverage passes 9 IM5 public API/resource tests plus
  every affected upstream graph, DDL, `ice_disk`, and demo case.
- Default, `KOKO_NO_OPTIMIZE=1`, and `KOKO_THREADS=1` output is byte-identical over the standing
  `agg`/`match`/`lsqb` and affected graph/index/`icebug-disk` suites.
- `python3 scripts/perf_gate.py` — PASS: nine correct answers, no timeout, all median Rust/C++
  ratios below 2×; q4/q5/q7 remain wins. Exact samples and ratios are in `PERF_GATE.md`.
- Release workspace build/tests, debug workspace tests (287 passed across 26 suites), all 52
  corpus directories, strict workspace Clippy, formatting, and focused public API tests are green.

**Historical post-close reassessment (2026-07-21):**

- Fresh `ANY` differential probes found 6/10 query-breadth mismatches and 2/5 error-channel
  mismatches; code inspection found an untracked whole-graph clone.
- Direct export probes showed only the selected graph reached logical export, `ANY` was empty, and
  explicit HASH/ART indexes did not reach `index.cypher`.
- A local-source probe proved `icebug-disk` eagerly hydrated rows at DDL.
- These retained non-corpus gaps reopened IM5 without changing `TRIAGE.tsv`.

Final boundary:

- `Database::in_memory()` and fallible `Database::in_memory_with_config` are the product
  constructors. CHECKPOINT remains an intentional no-op.
- Native database files, persistent catalog/indexes, WAL/recovery, buffer management, physical
  storage introspection, and larger-than-RAM native tables remain permanently deferred.
- `TRIAGE.tsv` contains 15 durability deferrals, 4 owner-deferred projected-graph/extension cases,
  and 4 statement-level divergences. It is exact for the corpus.
- No active milestone remained at this snapshot. The broader migration inventory is historical;
  `ROADMAP.md` now owns current work and scope.

### IM5 correction closure

The correction landed in required order:

1. `86014ac` — hidden internal `_nodes`/`_edges` plus binder lowering and normal execution.
2. `969a106` — schemaless clause/error/prepared/transaction/concurrency/control/memory closure.
3. `4e81b01` — versioned deterministic database-wide logical interchange with detached atomic
   import.
4. `1902645` — pinned, projected query-time Parquet/CSR/flat `icebug-disk` scans.
5. `ad866ad` — C++-ordered JSON and legacy import diagnostic corpus compatibility.

**Final fresh close evidence (2026-07-22):**

- Strict goal gate: 1785/343/23; 0 panic/unparsed/unledgered/arity state; TRIAGE 23 rows with
  0 missing/stale/malformed.
- Focused correction contracts pass for `ANY`, atomic database interchange, and query-time
  `icebug-disk`, including ownership, transactions, prepared statements, concurrency, controls,
  memory, rollback, source lifetime, corruption, directions, recursion, joins, aggregates, and
  multi-vector scans.
- Full release corpus and default/no-opt/one-worker comparisons are green across all 52
  directories. Workspace release build/tests, debug tests (298/26 suites), formatting, and strict
  Clippy are green.
- Repeated LSQB: nine correct, timeout-free, every median Rust/C++ ratio ≤2×; exact ratios are in
  `PERF_GATE.md`.

## First-party `koko` CLI closure

The CLI landed in the dependency order recorded in `CLI_PLAN.md`: canonical parser tooling;
engine-authoritative snapshots; structured result/failure traversal; bootstrap and registries;
typed presentation and atomic output; one ordered source/session runner; Reedline editing/history/
completion/highlighting; interruption and terminal containment; then acceptance closure.

**Final fresh close evidence (2026-07-23):**

- `python3 scripts/cli_goal_gate.py --strict` reports all PTY-01..12, BAT-01..10, and REG-01
  contracts green: 23 required cases, 0 skipped, and 0 timed out. It runs the parser/facade/pure/
  real-engine/session/subprocess/file/PTY suites, the production binary in every input class, and
  the retained differential-example protocol.
- The CLI package has 78 passing tests across pure, public-facade, real-engine, file, subprocess,
  failure-injection, and real-PTY suites. Permanent batch cases cover every required format,
  complete/incomplete JSON and JSONL, CSV/TSV losslessness, transaction-safe keep-going,
  cancellation, atomic Unicode destinations, broken pipes, configuration, history boundaries, and
  lexically scoped editor-only continuation normalization that leaves batch Cypher unchanged.
- Rust 1.85 workspace checking and all-target Linux/Windows cross-checks are green on the Darwin
  close host; the permanent CI workflow runs the strict gate on Linux, macOS, and Windows. Debug/
  release workspace tests and build, formatting, and strict workspace Clippy are green.
- The debug close gate caught and fixed a connection-local UDF removal deadlock: the UDF registry's
  function revision now remains its sole refresh signal, so removing a callback retained by a
  running query no longer waits on that query's session-state lock. The existing retained-callback/
  deadline regression passes.
- The first fresh corpus sweep exposed an L3 retained-semantics regression in non-query `EXPLAIN`/
  `PROFILE`. Regular queries keep structural Rust plans; non-query `EXPLAIN` again validates without
  mutation and non-query `PROFILE` executes through the original statement path. The full upstream
  `explain.Explain` case and a public-facade regression pass.
- The preserved engine gates remain 1785/343/23 with every zero-state invariant green, and nine
  correct timeout-free LSQB answers with every median Rust/C++ ratio at most 2×.

## Facade architecture closure

- The former 10,582-line composition root is now a 68-line public index. Runtime ownership is split
  across database, graph, statement context, connection, execution, transaction, prepared, and
  observation modules; `runtime/mod.rs` is declarations and deliberate re-exports only.
- Result, configuration, tooling, macro, COPY, Arrow, and logical-interchange owners are independent
  of runtime state types and locks. Runtime code constructs concrete operation contexts and retains
  writer admission, savepoint, publication, rollback, and panic-recovery authority.
- The former inline facade tests moved without behavioral dilution into six contract modules under
  `src/tests/`. Public root paths and inherent APIs remain unchanged; no compatibility alias,
  workspace crate, dependency, `unsafe`, async runtime, trait framework, durability seam, or deferred
  product surface was added.
- Closure evidence: 396 workspace tests; strict Clippy, formatting, and rustdoc; all 23 CLI
  PTY/BAT/REG cases; fresh 1785/343/23 corpus plus strict ownership/differential/arity gates; and all
  nine repeated LSQB answers correct with a worst median Rust/C++ ratio of 1.342517.

## Idiomatic Rust architecture and API closure

- Added `koko-ir` as the constrained owner of bound semantic records, typed variable IDs, row
  layouts, and logical plans. Binder, expression, planner, and processor consumers now share that
  contract without dependency cycles or producer-to-consumer edges.
- Catalog creation now crosses typed node/relationship/column definitions; entry internals are
  private, serial ownership is structural, and relationship endpoint pairs have one representation.
  Storage exposes explicit shared guards while retaining one concrete `InMemStorage`.
- Parser, binder, expression, loader, storage, processor, and function code is split by Rust
  responsibility. External CSV/Parquet/NPY and scan protocols live in loader; processor pull states,
  relationship visibility, and borrowed operator capabilities have named owners.
- Function binding and evaluation share generated typed identities. Checked-in `builtins.toml` and
  `catalog_rows.tsv` are deterministic generator inputs, and `scripts/gen_fn_catalog.py --check`
  rejects stale output. Lambda identity and neighbor scratch no longer use thread-local state.
- The public API was a clean pre-user cutover: focused namespaces, `Database::new` /
  `Database::with_config`, one owned `Parameter`, internally serialized shared `Connection`
  execution, mutable prepared execution, an exclusively borrowing `Transaction<'_>`, eager
  columnar `QueryResult`, borrowed row/cell/column traversal, and a scalar-function descriptor.
  Every first-party caller migrated; removed names have no shim.
- Closure evidence: `cargo build --workspace`; **401 tests across 40 workspace suites**; generated
  catalog check; all-target strict Clippy and formatting; all **23** CLI cases; public downstream API,
  doctest, and piped CLI smoke gates. Default and `KOKO_THREADS=1` external corpus runs each preserved
  **1783/343/25** exactly, matching their pre-refactor baseline, and P0 differential probing reported
  **43 clean / 5 ledgered / 4 skipped / zero unledgered diffs**.
- The optimizer-disabled external sweep is intentionally tabled: both final attempts completed
  through `dml_rel.copy.CopyRelSetStorageDirection` but made no further progress before the one-hour
  process deadline. The next file is the naive-expensive `dml_rel/create/create_batch.test`, and
  `current_setting('enable_plan_optimizer')` correctly differs from the default oracle. Current
  stopping and activation conditions are recorded in `ROADMAP.md`.
- The final repeated LSQB gate passed all nine correct answers. Median Rust/C++ ratios were q1
  0.465934, q2 0.363108, q3 1.406553, q4 0.005958, q5 0.015916, q6 1.089022, q7 0.325344,
  q8 1.526174, and q9 0.801603; all are below 2× and q4/q5/q7 retain their required Rust wins.

## Bounded optimizer correctness gate (2026-07-26)

- Replaced the unbounded external `KOKO_NO_OPTIMIZE=1` sweep with three complementary layers:
  fixed-oracle P0 execution in both optimizer modes, focused optimizer contract fixtures in both
  modes, and structural plan assertions for activation and safety boundaries.
- Corrected `decorrelated_join.test` so its sparse 1,100-node outer scan actually crosses
  `DECORRELATE_MIN_PROBE_ROWS`; the same fixture retains a selective seeded fallback and stays fast
  with optimization disabled. Added tiny projection-pruning and optimizer-barrier fixtures.
- Structural tests now pin PK lookup eligibility, hash-join and cost choices, projection and
  carry-column pruning, factorization propagation, sequence/materialization/correlation barriers,
  update/no-projection guards, polymorphic-scan safety, recursive-order safety, and eligible versus
  seeded OPTIONAL/EXISTS/COUNT decorrelation.
- Evidence: both full P0 modes passed; all **11** focused optimizer cases passed in both modes;
  `cargo test --workspace` passed **413 tests across 40 suites**; the P0 C++ re-diff reported
  **45 clean / 5 ledgered / 4 skipped / zero unledgered differences**; strict Clippy and formatting
  passed. The exhaustive external naive sweep is now tabled under `ROADMAP.md`.

## Order-independent `UNION` result typing (2026-07-26)

- `BoundRegularQuery` now owns canonical result columns resolved across every operand. A provenance
  lattice distinguishes untyped NULL, unresolved direct parameters, concrete types, and genuinely
  runtime-dynamic `ANY`; concrete conflicts retain the existing deterministic binder diagnostic.
- CTAS, query-backed COPY, `COPY TO`, prepared metadata, direct result metadata, CLI output, and
  execution all consume that one descriptor. The processor moves exact buffers, converts null-only
  `ANY` to typed-null vectors, and promotes concrete values into generic `ANY` vectors only when
  the resolver selected dynamic `ANY`. Impossible conversions return internal errors rather than
  panicking.
- Regression coverage pins both operand orders for `UNION` and `UNION ALL`, duplicate semantics,
  multi-column independence, dynamic `ANY`, direct parameter inference and conflicts, empty
  results, unlabeled graph values over an empty catalog, CTAS/COPY, public metadata, and Arrow
  conversion.
- Replaced the broad Markdown-based differential whitelist with
  `docs/fable-audit/active_divergences.json`. The P0 re-diff reported
  **45 clean / 5 ledgered / 4 skipped**, exactly **6** named active statement differences across
  **5** fixtures, and zero unnamed, missing, or repeated differences.
- Evidence: both **54-fixture** P0 modes passed; workspace build and all **422 tests across 40
  suites** passed; strict Clippy/formatting and the strict **23-case** CLI gate passed. Default and
  `KOKO_THREADS=1` external corpus sweeps each returned
  **1783 passed / 343 skipped / exactly 25 known failures**, with no panic.

## Post-v0 documentation authority reset (2026-07-26)

- Replaced the migration-era gap inventory with a current tracker organized as confirmed bugs,
  documented product limitations, measurement-gated opportunities, and explicit deferrals. The
  frozen 23-row IM5 residual remains in `TRIAGE.tsv`; accepted semantic decisions are summarized
  by `ROADMAP.md`.
- Reconciled `ROADMAP.md`, `FACADE_ARCHITECTURE.md`, the CLI contracts and historical plans,
  `PERF_GATE.md`, this progress ledger, `AGENTS.md`, and `README.md` around one post-v0 rule:
  Koko-owned product contracts and non-regression tests are authoritative; Ladybug and the
  differential corpus are historical or change-specific evidence.
- Removed stale milestone language from crate documentation and from the remote-source diagnostic.
  The in-memory product now rejects remote paths without referring to a completed milestone.
- Evidence: all **422 workspace tests across 40 suites** passed; the focused remote-source and DDL
  rollback regressions passed; the exact README API example ran and printed `Alice is 35`; the
  generated workspace dependency edges matched the documented DAG; strict Clippy and formatting
  passed; `RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps` generated all workspace
  documentation; and a local Markdown audit found no missing target or anchor among **67 local
  links** in the **14** active/status documents checked.

## Historical semantic decisions retained after the parity campaign

- **count-distinct-factorized** (2026-07-02): Ladybug returned 2 for
  `count(DISTINCT a.id)` with one distinct value over a factorized fan-out. Koko retains the correct
  result 1; `ROADMAP.md` records the decision.
- **create-missing-pk** (2026-07-02): the migration confirmed that a primary key with any default,
  including `nextval`, is self-filling and therefore exempt from the missing-PK bind rejection.
- **`-MULTI_COPY_RANDOM`:** Koko uses even slices independent of the seed because split points
  affect storage batch boundaries, never logical state.

## Historical compatibility-tool crib

These tools remain available when a change explicitly owns inherited compatibility; they are not
universal post-v0 landing gates:

- Build the narrow adapter with `cargo build --release --example koko_cli`; it is not built by
  `cargo build --workspace`.
- Run a focused differential with
  `python3 docs/fable-audit/diffprobe.py <probe> [--dataset tinysnb] [--show-all]`.
- Re-diff eligible hermetic product fixtures with
  `python3 docs/fable-audit/product_to_probe.py`. Only exact entries in
  `docs/fable-audit/active_divergences.json` count as ledgered for that audit.
- Run a historical full external sweep with `scripts/goal_sweep.sh [outdir]`, the arity audit with
  `scripts/arity_sweep.sh`, or the paired LSQB comparison with `scripts/perf_gate.py`. Each requires
  its documented external checkout/data environment.
- Known reference crashes must be isolated because buffered shell output can be lost:
  `label(rels(p)[1])`, NULL-argument list functions, `1 IN NULL`, `repeat(s,-n)`, and
  `properties(null, 'a')`-style calls.
