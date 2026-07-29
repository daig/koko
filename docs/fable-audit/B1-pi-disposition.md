# B1 — Cross-reference: docs/pi/feature-gaps/ (79 files, snapshot 2026-06-29) vs fable-audit.md (2026-07-01, Rust HEAD 2692cd1)

Method: every file read; every COVERED verdict anchored to a specific fable-audit.md section/row; every
FIXED-SINCE verified by a Rust-CLI probe (C++ agreeing) and/or the fixing commit; every UNTRACKED verdict
re-reproduced through BOTH engines (or, for API/runner-surface gaps with no query form, verified against
the Rust source + confirmed absent from fable-audit.md). Probes ran against
`/Users/dai/code/koko-rs/target/release/examples/koko_cli` and
`/Users/dai/code/koko/build/release/tools/shell/koko` (tinysnb where needed, via diffprobe.py).

**Counts: COVERED 39 · FIXED-SINCE 13 · UNTRACKED 17 · WRONG/STALE 10 — total 79.**

| File | Verdict | Justification |
|---|---|---|
| acyclic-endpoint-return-semantics.md | COVERED | W6 (ACYCLIC recursion). Probe: C++ ACYCLIC=158 vs Rust=62; Rust WALK=158=C++ ACYCLIC — "C++'s ACYCLIC behaves like WALK" confirmed. |
| anonymous-unlabeled-create.md | COVERED | §2 row "zero-label `CREATE ()` with one node table". Rust rejects "must specify exactly one label in this phase"; C++ infers the sole table. |
| array-vector-functions.md | FIXED-SINCE | Commit ae9ec2f (array/vector family) + d8d4c76/b7e98c3; probe: `array_value(1,2,3)`→`INT64[3]`, `array_cross_product`→`[-3,6,-3]`, matching C++. |
| arrow-c-data-interface.md | UNTRACKED | Audit §2 names Arrow *file* readers only; the in-memory Arrow C Data Interface (Arrow-backed tables, `query_as_arrow` export) is a distinct un-named capability. Verified absent from crates/koko (no Arrow symbols/dep). |
| bitwise-shift-power-factorial-operators.md | COVERED | §2 row "`=~` regex, `^` power, `&`/`\|` bitwise, `<<`/`>>`, postfix `!` — whole operator tiers absent". All six probed: C++ evaluates, Rust parser exceptions. |
| blob-utilities.md | COVERED | §2 ~28-missing-functions row: "octet_length/encode/decode". Probe: Rust "function does not exist"; C++ has them. |
| c-abi-ffi.md | COVERED | §2 phase-planned "shell/CLI + **non-Rust bindings**" — the C ABI is the non-Rust binding surface. (Judgment call: if "bindings" is read as only the language wrappers, this flips to UNTRACKED.) |
| call-settings-semantics.md | COVERED | V15 (`current_setting('threads')` via CALL → `10` / empty, probe-confirmed) + §3.3 "CALL threads=4.5" accepts-invalid family. |
| coalesce-validation.md | FIXED-SINCE | Commit 40e9924 bind-time type-checks coalesce/ifnull; `coalesce([1],['x'])` now bind-errors. (Note: slight over-correction — Rust now rejects `coalesce(1,true)`/struct-merge cases C++ accepts; see UNTRACKED notes.) |
| collect-distinct-null.md | FIXED-SINCE | Commit 65b012c; `collect(DISTINCT null)` over all-null group now returns NULL, matching C++. |
| concurrent-runner-directives.md | COVERED | §5: skipped "concurrent-exec 5" + `-BATCH_STATEMENTS`/`-LOOP`/`-CREATE_DATASET_SCHEMA`; still in runner `unsupported_directive` list (source-confirmed). |
| constant-or-null-validation.md | WRONG/STALE | `constant_or_null(1)` bind-errors identically to C++; the arity guard predates the snapshot (commit 2096def, 2026-06-20). Observable gap never reproduced. |
| create-index-show-indexes.md | COVERED | §2 phase-planned: "disk indexes + `CREATE [HASH\|ART\|FULLTEXT] INDEX`" — explicit deferral. |
| create-user-role-syntax.md | WRONG/STALE | Premise false: C++ also fails (`Failed parse… load the extension?` — extension not loaded in this checkout); no functional capability in either engine, only error wording differs. |
| decimal-division-skipped-mismatch.md | WRONG/STALE | No divergence — both engines evaluate `DECIMAL/DECIMAL` as DOUBLE (`0.500000`); the file itself flags it as a non-required caveat. |
| decimal-floor-ceil.md | COVERED | R2. Probe: `floor(DECIMAL(18,1))` → Rust `1.0` / C++ `1`; typeof DECIMAL(18,3) vs (18,0). |
| explain-profile-plan-printer.md | COVERED | §2 row "`EXPLAIN` / `PROFILE` — parse errors". Probe: C++ prints plan; Rust parse error. |
| fixed-length-array-types.md | FIXED-SINCE | Commits d8d4c76 + b7e98c3; TABLE_INFO renders `INT64[3]`, wrong-length insert errors identically to C++. |
| graph-ddl-namespaces.md | COVERED | §2 misc row: "`USE`/`ATTACH`/named `GRAPH`s (`graph/` dir 0p/9f)". Probe: C++ accepts CREATE/USE GRAPH + dotted label; Rust parse-rejects. |
| hash-functions.md | COVERED | §2 missing-functions row: "md5/sha256/hash". Probe: Rust "MD5 does not exist"; C++ returns digest. |
| implicit-cast-matrix.md | COVERED | §2 "implicit arg→STRING coercion" row (its own example `lower(123)`) + V4 + §6.1 root cause. Probe: `lower(123)` → C++ `123` / Rust runtime error. |
| interactive-shell.md | COVERED | §2 phase-planned: "shell/CLI + non-Rust bindings". |
| internal-id-ordering.md | FIXED-SINCE | Commit 0fb6900 (order-key type validation). Re-probed: Rust now rejects `ORDER BY id(n)` with the byte-identical C++ binder error "Cannot order by n._ID. Order by INTERNAL_ID is not supported." |
| list-membership-in-operator.md | COVERED | §2 headline row "`IN` list-membership operator". Probe: `RETURN 1 IN [1,2]` → C++ True / Rust parser exception. |
| long-sort-direction-keywords.md | UNTRACKED | `ORDER BY x ASCENDING/DESCENDING`: C++ accepts, Rust "expected Eof but found Ident(DESCENDING)". ASCENDING/DESCENDING appear nowhere in fable-audit.md. Reproduced: yes. |
| map-struct-property-access.md | WRONG/STALE | Audit §5 already corrects this ("m.x map access works now"); probe: dot access works in both, bracket subscript rejected identically in both; the file's residual claim (`map(...).name`) is inverted (Rust accepts, C++ rejects). |
| merge-repeated-action-clauses.md | UNTRACKED | Repeated same-kind MERGE action clauses: Rust keeps only the last `ON CREATE SET`/`ON MATCH SET`, C++ accumulates all. Re-probed: C++ `1\|2` vs Rust `\|2` (a.x lost). Neither W1 nor the MERGE-dedup fix. Reproduced: yes. |
| multi-label-create-diagnostics.md | COVERED | Residual node/empty-label diagnostic divergence is the §3.4 invented-"…in this phase" wording family. (The rel-label pruning half was fixed since — commit 9f55d2d — `CREATE ()-[:R\|S]->()` now prunes/errors like C++.) |
| multi-statement-query-api.md | UNTRACKED | Multi-statement query strings via the public API: `query("RETURN 1; RETURN 2;")` errors at the 2nd stmt (`parse_statement` expects Eof); C++ Connection::query handles multi-statement. Not named in the audit. Verified: yes. |
| multi-writer-conflict-detection.md | FIXED-SINCE | Commit 0fb6900 ("Multi-writer MVCC + write-write conflict detection; debug_enable_multi_writes now functional"); 3 unit tests pass. (§2's "multi-writer P5" refers to production/default multi-writer, a different scope.) |
| named-optional-function-arguments.md | FIXED-SINCE | Commit da53a9b; `struct_pack(x:=2,y:=3)` and `union_value(a:=1)` parse+bind, matching C++. |
| nested-subqueries.md | COVERED | §2 row "nested `EXISTS`/`COUNT` subqueries — 'not implemented in this phase'; C++ evaluates". Probe: C++ 8 / Rust phase error. |
| nested-type-validation.md | FIXED-SINCE | Commits f34feac + 2692cd1: duplicate STRUCT/UNION fields and string→map duplicate keys now error like C++. Residual: `map()` *function* duplicate-key check not threaded into eval under `DISABLE_MAP_KEY_CHECK=FALSE` (Rust `{1=2, 1=3}` / C++ runtime error) — untracked residual, see notes. |
| node-rel-path-functions.md | COVERED | §2 missing-functions row names "start_node/end_node", "is_trail/is_acyclic", "internal_id". Probed missing in Rust, present in C++. (Sub-members `rowid`/`cost` not individually named in the audit — granularity note.) |
| numeric-positional-parameters.md | COVERED | §2 misc row: "positional params `$1`". Probe: C++ parses; Rust parse error. |
| optional-match-continuation.md | COVERED | §2 row "required `MATCH` after `OPTIONAL MATCH` in the same part — confirmed still open". Probe: C++ 8 rows / Rust phase error. |
| optional-named-path-materialization.md | WRONG/STALE | Audit §5 already marks the claim stale: path value now materializes identically in both (`{_NODES:[a],_RELS:[]}`); only `length(p)` differs, which is V13. |
| order-by-aggregate-expressions.md | WRONG/STALE | Premise inverted: C++ also rejects `ORDER BY COUNT(b)` ("Cannot evaluate expression with type AGGREGATE_FUNCTION"); both accept alias/group-key forms. Audit §5 documents the correction ("error-wording, C++ also rejects"). |
| path-pattern-predicates.md | COVERED | §2 misc row: "pattern-predicate expressions". Probe: `WHERE (a)-[:Knows]->(b)` → C++ row / Rust parse error. |
| pattern-variable-rebinding-validation.md | WRONG/STALE | Both engines now reject with byte-identical errors (node-as-rel; duplicate rel var). The file itself predicted "likely resolved". |
| percentile-disc-aggregate.md | COVERED | §2 missing-functions row: "percentiledisc". Probe: Rust "does not exist" / C++ `2`. |
| prepared-statements-api.md | UNTRACKED | Prepared-statement metadata/typed-bind API (is_read_only, statement type, output types, known/unknown params) absent; audit §6.7 only notes "params substituted as bind-time literals (plan-cache latency only)". Verified in source: `PreparedStatement` = `{conn, stmt}` only. |
| primary-key-type-rules.md | UNTRACKED | PK *type eligibility*: Rust accepts C++-invalid `BOOL` PK and rejects C++-valid FLOAT/DOUBLE PK inserts ("Unsupported primary key type in this phase"). Audit only covers missing-PK-*value* stage diff (§3.4). Reproduced: yes. |
| public-connection-api-surface.md | UNTRACKED | Connection API surface: thread control, interrupt/timeout, UDF registration, `query_as_arrow` — all absent (grep-confirmed) and none named in the audit. Verified: yes (API-surface; no query probe applicable). |
| quantified-predicates.md | COVERED | §2 row "quantifiers `ANY/ALL/NONE/SINGLE(x IN l WHERE p)` — misparse as function calls". Probe: C++ True / Rust parse error. |
| query-result-metadata-arrow-multichain.md | UNTRACKED | `QueryResult` metadata API (column logical types, QuerySummary timing, statement type, multi-result chains, Arrow export) absent; audit never names it. Verified in source: QueryResult exposes only names/columns/rows. |
| random-functions.md | COVERED | §2 missing-functions row: "random/setseed". Probe: Rust "does not exist" / C++ works. |
| recursive-lambda-mixed-dependency-validation.md | WRONG/STALE | Both engines reject mixed `r`+`n` conjuncts ("depends on both n and r"); the guard predates the snapshot. Gap does not reproduce. |
| regexp-replace-option-validation.md | UNTRACKED | `regexp_replace('abcabc','a','x','l')` → Rust silently single-replaces `xbcabc`; C++ "Binder exception: regex_replace can only support global replace option: g." regexp_replace is never mentioned in the audit. Reproduced: yes. |
| rel-group-semantics.md | FIXED-SINCE | Commits 9cfb5b0 + 9f55d2d (2026-06-30): duplicate FROM-TO pair rejection, ALTER-ADD dup, wrong-side endpoints, shared-offset materialization now match C++. (The rel_group `COPY (from=…,to=…)` blocker remains open but is a separate gap the file excludes; audit §2 covers it.) |
| relationship-id-path-output-order.md | COVERED | §3.5 accepted divergence: internal `_ID`/rel-table-id numbering. Probe: rel `_ID` values now match (3:0–3:11 both); only non-contractual enumeration order differs (runner sorts). |
| relationship-storage-direction-options.md | UNTRACKED | Rust accepts `WITH (storage_direction='fwd')` but skips C++'s query-direction validation: undirected `MATCH` on a fwd-only table → C++ binder-rejects / Rust returns a row. storage_direction is absent from the audit. Reproduced: yes. |
| relationship-values-in-scalar-expressions.md | COVERED | V12 bare-internal-id pipeline seam. Probe: `struct_extract(e,'_src')` → Rust "got INTERNAL_ID" error / C++ `0:0`. |
| runner-corpus-placeholders.md | UNTRACKED | Corpus placeholder expansion (`-SET`, `${COLS}`, `${STRUCT_VAL}`, REPEAT/ARANGE): `expand_corpus_vars` handles only `${KOKO_ROOT_DIRECTORY}`; §5 lists many runner gaps but not this one. Verified in runner source: yes. |
| runner-error-regex.md | COVERED | §5: "`---- error(regex)` 13 files" collapse to single FAIL. Source-confirmed still unhandled (commit 77535cf fixed multi-line *exact* error capture, not regex). |
| runner-hash-results.md | COVERED | §5: "`---- hash` 4" files hidden. Source-confirmed `spec=="hash"` still falls through to "invalid result count". |
| runner-skip-body-parsing.md | UNTRACKED | `-SKIP` case *bodies* are still strict-parsed, so an unsupported directive (e.g. error(regex)) inside a skipped case fails the whole file; §5's "-SKIP with a trailing comment" is a different bug. Verified in runner source (skip flag set at lib.rs:412-415, parsing continues). |
| runner-structural-directives.md | COVERED | §5 names `-BATCH_STATEMENTS` 25 / `-CREATE_DATASET_SCHEMA` 8 / `-LOOP` 7 / concurrent-exec 5 as skipped; still in `unsupported_directive` (source). (`-INSERT_DATASET_BY_ROW` not individually named but same family.) |
| runner-uppercase-empty-dataset.md | COVERED | §5: "19 uppercase `-DATASET CSV EMPTY` cases (case-sensitive match, `lib.rs:601`)". Source line 601 confirmed case-sensitive. |
| shortest-path-bfs-performance.md | WRONG/STALE | Does not reproduce post-P3: on the corpus dataset Rust SHORTEST all-source is *faster* (0.59s vs 0.85s) and Rust *completes* ALL SHORTEST (37s) where C++ OOMs (~20s). No "Rust times out where C++ succeeds" case remains. |
| skip-limit-expressions-parameters.md | COVERED | §2 row "non-literal `SKIP`/`LIMIT` … 11 tck fails". Probe: `LIMIT 1+1` → C++ 1 row / Rust "constant integer in this phase". |
| split-part-empty-separator.md | UNTRACKED | `split_part('Alice','',5)` → Rust `''` (empty sep = one whole part) / C++ `e` (character-split). split_part is never mentioned in the audit. Reproduced: yes. |
| storage-driver-api.md | UNTRACKED | Low-level `StorageDriver` embedding API (name-based scan-by-offset, node/rel counts) absent; audit §2 names only `CALL storage_info/stats_info` introspection. Verified: no public StorageDriver/scan symbols in source. |
| string-aliases-edge-functions.md | COVERED | §2 missing-functions row names "concat_ws" + "array_append/prepend". Probed missing in Rust / present in C++. (Sub-members toLower/toUpper, array_push_back/front, list_has/array_has not individually named — granularity note.) |
| string-predicates-regex-operators.md | COVERED | §2 rows "`STARTS WITH` / `ENDS WITH` / `CONTAINS`" + "`=~` regex". All four probed: C++ True / Rust parse errors. |
| struct-map-functions.md | FIXED-SINCE | Commits da53a9b + c66d7fc: `struct_pack(x:=2,y:=3)` works; `keys(n)` byte-matches C++. Residual (`struct_extract` missing field → NULL vs bind error) is already audit §3.3. |
| subquery-hints.md | UNTRACKED | Join hints inside subqueries: C++ binds `EXISTS { MATCH … HINT a JOIN e JOIN b }` (returns 5); Rust parse-rejects "expected RBrace but found HINT". HINT is absent from the audit. Reproduced: yes. |
| table-functions-yield.md | COVERED | §2 row "`CALL … YIELD`; generic `CALL <tablefunc>()` — only 5 of 28 table functions callable". Probe: YIELD parse error; `CALL db_version()` phase error. |
| timestamp-current-epoch-functions.md | COVERED | §2 missing-functions row: "current_date/current_timestamp", "epoch_ms/to_epoch_ms". Probed missing in Rust / present in C++. |
| type-alias-interval-aliases.md | COVERED | R3 (REAL→DOUBLE vs C++ REAL≡FLOAT) + §2 interval-format row ('1.5 hours', decade/quarter). Probes confirm. (FLOAT8/FLOAT4/DURATION aliases not individually enumerated — granularity note.) |
| typeof-null.md | FIXED-SINCE | Commit 0fb6900 (`typeof_type_name` maps Any→"NULL"); `typeof(NULL)` → `NULL` in both. |
| uint128-aggregate-overflow.md | COVERED | V3 (SUM doesn't widen; invented overflow error). Probe: SUM of two 2^126 UINT128 → C++ 2^127 / Rust "Overflow exception". |
| uint128-range-list-product.md | UNTRACKED | `range(UINT128,…)` → Rust `[0]` typed INT64[] / C++ correct UINT128[]; `list_product(UINT128s)` → Rust `1` / C++ `6`. Audit only records the C++ list_product NULL-SIGSEGV (§4), not this narrowing. Reproduced: yes. |
| union-functions.md | FIXED-SINCE | Commit da53a9b (UNION runtime): `union_value`/`union_tag`/`union_extract` + `typeof`→`UNION(a INT64)` all match C++. |
| union-runtime-tag-semantics.md | FIXED-SINCE | Commits da53a9b + f34feac: UNION runtime works, duplicate-tag `CAST(… AS UNION(a INT64, a STRING))` now rejects "Duplicate field 'a'" like C++. |
| utility-functions.md | COVERED | §2 missing-functions row: "count_if", "error". Probe: Rust "does not exist" / C++ `1` and "Runtime exception: boom". |
| uuid-generator.md | WRONG/STALE | Claim false: Rust `gen_random_uuid()` works (returns UUIDs, typeof UUID, non-constant), matching C++. (Aside: Rust output lacks RFC-4122 v4 version/variant nibbles — cosmetic, not the file's claim.) |
| weighted-shortest-path-syntax.md | COVERED | §2 misc row: "`WSHORTEST`". Probe: C++ `WSHORTEST(w)`/`ALL WSHORTEST(w)` work / Rust parse-rejects. |
| with-where-subqueries.md | COVERED-ADJACENT → UNTRACKED | `WITH a WHERE EXISTS {…}` → C++ 5 rows / Rust "subquery … in a WITH … WHERE is not supported in this phase". Distinct position from §2's nested-subquery row and W2 (OPTIONAL…WHERE). Reproduced: yes. |

## UNTRACKED (17) — gaps real today but missing from fable-audit.md

Engine-behavior gaps (all re-reproduced through both engines this session):
1. **long-sort-direction-keywords** — `ORDER BY x ASCENDING/DESCENDING` long forms parse-rejected by Rust, accepted by C++; not in the §2 parser table.
2. **merge-repeated-action-clauses** — repeated `ON CREATE SET`/`ON MATCH SET` clauses: Rust keeps only the last, C++ accumulates all (C++ `1|2` vs Rust `|2`); distinct from W1 and from the MERGE-dedup fix.
3. **regexp-replace-option-validation** — bad `regexp_replace` option silently single-replaces in Rust; C++ bind-rejects non-`g`.
4. **split-part-empty-separator** — empty separator: Rust treats string as one part (`''` beyond idx 1); C++ splits per character.
5. **primary-key-type-rules** — PK type eligibility: Rust accepts BOOL PK (C++ rejects) and rejects FLOAT/DOUBLE PK inserts (C++ accepts); audit only has the missing-PK-value stage diff.
6. **uint128-range-list-product** — `range()`/`list_product()` silently narrow UINT128 to INT64 (wrong values `[0]`/`1`); audit only has the C++ NULL-SIGSEGV defect.
7. **relationship-storage-direction-options** — `storage_direction='fwd'` accepted but undirected-pattern validation skipped (C++ binder-rejects, Rust returns rows).
8. **subquery-hints** — `HINT a JOIN e JOIN b` inside `EXISTS{}`/`COUNT{}` parse-rejected by Rust; C++ binds it.
9. **with-where-subqueries** — `EXISTS{}` in `WITH … WHERE` phase-rejected by Rust; C++ evaluates (distinct from §2 nested row and W2).

API/runner-surface gaps (verified against Rust source; capability absent and un-named in the audit):
10. **arrow-c-data-interface** — in-memory Arrow C Data Interface (Arrow-backed tables, `query_as_arrow`); audit names Arrow *file readers* only.
11. **multi-statement-query-api** — public `query()` is single-statement (`parse_statement` expects Eof); C++ handles `;`-separated strings.
12. **prepared-statements-api** — prepared-statement metadata/typed-bind surface (is_read_only, types, params); audit §6.7 only notes plan-cache latency.
13. **public-connection-api-surface** — Connection thread control, interrupt/timeout, UDF registration, Arrow query.
14. **query-result-metadata-arrow-multichain** — QueryResult column-type/summary/multi-chain/Arrow metadata API.
15. **storage-driver-api** — low-level `StorageDriver` scan/count embedding API (audit names only `CALL storage_info/stats_info`).
16. **runner-corpus-placeholders** — test-runner corpus placeholder expansion (`-SET`, `${COLS}`, REPEAT/ARANGE) unhandled; §5 omits it.
17. **runner-skip-body-parsing** — `-SKIP` case bodies still strict-parsed (unsupported directive inside a skipped case fails the whole file); §5's "-SKIP trailing comment" is a different bug.

Residual sub-gaps noted while classifying (parent files verdicted elsewhere):
- nested-type-validation (FIXED-SINCE) residual: `map()` function duplicate-key check not enforced at eval under `DISABLE_MAP_KEY_CHECK=FALSE` (Rust `{1=2, 1=3}` / C++ runtime error).
- coalesce-validation (FIXED-SINCE) over-correction: Rust now rejects `coalesce(1,true)` / `coalesce({a:1},{b:2})` which C++ accepts.
- Granularity: a few §2 bundle rows don't individually name every sub-function verified missing (rowid/cost; toLower/toUpper, array_push_back/front, list_has/array_has; FLOAT8/FLOAT4/DURATION aliases).

## FIXED-SINCE (13)
array-vector-functions, coalesce-validation, collect-distinct-null, fixed-length-array-types,
internal-id-ordering, multi-writer-conflict-detection, named-optional-function-arguments,
nested-type-validation, rel-group-semantics, struct-map-functions, typeof-null,
union-functions, union-runtime-tag-semantics

## WRONG/STALE (10)
constant-or-null-validation, create-user-role-syntax, decimal-division-skipped-mismatch,
map-struct-property-access, optional-named-path-materialization, order-by-aggregate-expressions,
pattern-variable-rebinding-validation, recursive-lambda-mixed-dependency-validation,
shortest-path-bfs-performance, uuid-generator

## Notes
- File count is 79, not 80 (79 `.md` files exist in docs/pi/feature-gaps/).
- No file was classified "UNTRACKED — in sub-report AX only": every UNTRACKED item above is absent from
  the main doc *and* was independently re-verified; the sub-report-only clause was checked but did not
  end up the deciding factor for any file.
- c-abi-ffi.md is the one contestable COVERED (C ABI ⊂ "non-Rust bindings" reading).
