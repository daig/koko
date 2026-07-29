# fable-audit — an honest assessment of the C++→Rust migration

*2026-07-01 · Rust HEAD `2692cd1` vs C++ koko 0.17.0 (`129e32a72`, frozen 2026-05-28 — no upstream drift since the port began).*

> **FINAL SCOREBOARD 2026-07-07 — ROADMAP M1–M5 all closed** @`a2d7ed2`. Corpus
> **1622p/384s/145f** (was 1257/341/379 + demo_db panics at the audit baseline), **0 panics**
> (corpus + `arity_sweep.sh`), **0 unparsed files**; p0 re-diff **42 clean / 0 diffs / 6 ledgered
> / 4 skipped**; deviation battery **143 probes / 76 DIFFs / 0 unledgered** (all residual DIFFs
> classified by the frozen machine inventory; the M5-close set covered aggregate placement,
> pattern-comprehension variables, physical `storage_info`, unordered `LIMIT`, NFD substrings,
> recursive-lambda EXISTS). **TRIAGE.tsv: 0 fix-m* rows**; its remaining 129 `p4`,
> 12 `p5`, and 4 divergence rows are frozen historical buckets. They are not current phase
> ownership: most old `p4` rows map to active IM2–IM4 work, physical `storage_info` maps to
> permanently deferred native durability, and `ice_disk`/old `p5` map to active IM5 work.
> A/B byte-identical (opt vs no-opt, threads=1); the in-memory LSQB perf gate held throughout.
> The seven `docs/GOAL.md` §3 completion criteria are green on a fresh
> `scripts/goal_gate.py --strict` run.

> **How to read this document now:** §§1–§7 are the historical 2026-07-01 baseline analysis and
> recommended repair order, not a current backlog. M1–M5 resolved or ledgered those findings.
> Use `ROADMAP.md` for the current product, work and intentional decisions.
> `docs/TRIAGE.tsv` and the machine compatibility inventories retain frozen close evidence only.
> Native durability is permanently deferred.

> **Status 2026-07-02 — ROADMAP M1 (Trust) closed** @`7e86d60`. Scoreboard: corpus
> **1354p/368s/429f** (was 1257/341/379 + demo_db panicking), **0 panics** (was 2 sites/142
> arity probes), **0 unparsed files** (was 28 collapsed), p0 re-diff **0 unledgered** (was
> 18/48), deviation battery honestly measured at **1060 statement-DIFFs** (this doc's ~45
> counted deviation *rows*). All 427 remaining failures triaged in `docs/TRIAGE.tsv`
> (fix-m2 69 · fix-m3 23 · fix-m4 104 · fix-m5 79 · p4 137 · p5 11 · divergence 4);
> intentional differences captured by machine inventories + `tests/p0/divergences.test`.
> Gate: `scripts/goal_gate.py` (checks 1,2,3,5,6 green; check 4 is the M2–M5 drain list).
> §3.1 (C1/C2/C3) retired; §5 harness items fixed; V8/DROP-MACRO/SET+=/numeric-strings fixed.

> **Status 2026-07-02 — ROADMAP M2 (silent wrong values) closed** @`0fa70fd`. Corpus
> **1376p/368s/407f** (0 panics, 0 timeouts); battery **873** unledgered statement-DIFFs
> (was 1060); p0 re-diff 38 clean / 0 diffs / 10 ledgered; TRIAGE 405/405 (fix-m2 19
> residual · m3 23 · m4 104 · m5 79 · p4 165 · p5 11 · divergence 4). Every §3.2 row fixed
> or ledgered — V1–V17, W1–W9, R1–R7 done except: C2 parity retargeted → M5 (entangled
> with nested subqueries), escape-strictness → M4 (Copy-wrapper-coupled), W5's
> IGNORE_ERRORS → P4 (ROADMAP COPY-option scope). ldbc and match corpus dirs fully green;
> A/B + lsqb perf gate held throughout (two would-be regressions caught by the gates).

> **Status 2026-07-05 — ROADMAP M3 (signature catalog) closed** @`8705380`. Corpus
> **1402p/368s/381f** (0 panics); battery **783** unledgered statement-DIFFs (was 873);
> p0 38 clean / 0 diffs / 10 ledgered; TRIAGE 381 rows (fix-m2 18 residual · **m3 0** ·
> m4 102 · m5 79 · p4 165 · p5 11 · divergence 4). The declarative catalog is the oracle
> itself: `catalog_data.rs` (all 1214 `show_functions` overload rows, dump byte-identical)
> feeding a bind-time signature gate that runs C++ `matchFunction` over a faithful
> `getCastCost` port (`koko-common::types::cast_cost`). §6.1 retired; V4 both polarities,
> overload-error format byte-exact, UNION implicit casts + min-cost tags, LIST_CREATION
> mixed-type fallback, coalesce strict fallback, DDL-DEFAULT/CALL-option gates all landed.
> Differential arity sweep (199 names × 0–3 args, both engines): every diff is a ledgered
> pure-superset or probe artifact.

**Method.** Fresh full-corpus run of all 52 `test_files` dirs (not just the tracked subset); ~3,500
differential probes through both binaries (C++ shell `-m list` vs a thin Rust CLI over the public API);
the port's own hermetic p0 suite re-diffed statement-by-statement against the C++ oracle; source-level
audits of the parser, function catalog, type system, and test-runner. Every headline finding below was
re-verified in this session against both engines (probe files + the ten full sub-reports are archived
in `docs/fable-audit/` — see the evidence-trail note at the end).

---

## 1. Where we stand

**Full-corpus scoreboard (the honest one — all dirs, `KOKO_DATASET_DIR` set):**

```
1257 passed / 341 skipped / 379 failed   (1977 cases; plus demo_db, which PANICS the runner)
```

| dir (cases) | p/s/f | dir | p/s/f | dir | p/s/f |
|---|---|---|---|---|---|
| tck | 260/202/45 | transaction | 444/60/6 | dml_node | 121/5/19 |
| function | 36/3/59 | exceptions | 5/0/53 | copy | 43/10/52 |
| ddl | 67/0/10 | cast | 6/6/24 | csv | 6/0/23 |
| dml_rel | 40/9/8 | arithmetic | 90/0/3 | issue | 44/3/11 |
| agg | 10/0/4 | graph | 0/0/9 | rel_group | 0/6/6 |
| common | 3/2/7 | projection | 4/0/6 | uint128 | 6/0/3 |
| ldbc | 2/1/1 | lsqb | 1/1/0 | demo_db | **panic** |
| reader/ice_disk/npy/parquet/glob/load_from/extension/explain/md5/binary_demo | ~0 pass (format/feature-gated) | | | others | mostly pass |

- **The read/write core is real.** At-scale correctness is strong: all 9 lsqb-sf01 queries and the LDBC
  interactive short+complex suites pass on ~200k-node data. The clause/pattern engine (self-loops,
  rel-uniqueness, undirected, cross products, OPTIONAL chains, WITH scoping, DISTINCT, SHORTEST/TRAIL
  counts, MERGE node semantics, transactions incl. MVCC rollback/isolation) verified byte-identical
  across ~600 targeted probes. Perf after P3: between 0.01× and 5× of C++ per lsqb query (wins on q2/q4/q5/q7).
- **The tracked dirs were the healthy ones.** Docs track tck/transaction/dml/ddl/p0 (and understate them —
  actual tck 260p/45f vs documented 227p/78f). The never-tracked dirs are where deviations accumulated:
  `function` 59f, `exceptions` 53f, `cast` 24f, `csv` 23f, `copy` 52f.
- **p0 is not a clean oracle.** Re-diffing the port's own green p0 fixtures against the real C++ shell:
  **18 of 48 hermetic files diverge** — several fixtures bake invented behavior in as expected
  (`labels()` returning a list, `SET +=`, a graceful `DROP MACRO IF EXISTS`, an invented SUM-overflow
  error, list comprehensions).
- **Architecture state at the audit baseline:** in-memory only — `Database::in_memory()` was the
  only constructor; no native on-disk format, WAL, or buffer manager existed. Single writer; no
  extensions.

## 2. Intended features not built yet

**At the 2026-07-01 baseline, phase-planned:** durable storage/recovery; Parquet/NPY/Arrow/gzip/
multi-file/glob readers; `COPY TO`/`EXPORT`/`IMPORT DATABASE`; indexes; statistics persistence;
multi-writer; extensions; shell/bindings; and introspection. This is historical classification,
not current ownership. The active IM1–IM5 map splits durability-independent work out of the old P4
bucket; native persistence remains permanently deferred.

**Not phase-planned but missing — everyday Cypher the parser rejects (discovered, mostly undocumented):**

| gap | evidence |
|---|---|
| **`IN` list-membership operator** | `RETURN 1 IN [1,2]` → Parser exception (works only inside `[x IN …]`) |
| **`STARTS WITH` / `ENDS WITH` / `CONTAINS`** | all parse errors, everywhere incl. WHERE (function forms exist) |
| `=~` regex, `^` power, `&`/`\|` bitwise, `<<`/`>>`, postfix `!` | whole operator tiers absent (`^ & ! ~` aren't lexer tokens) |
| quantifiers `ANY/ALL/NONE/SINGLE(x IN l WHERE p)` | misparse as function calls |
| `EXPLAIN` / `PROFILE` | parse errors |
| `CALL … YIELD`; generic `CALL <tablefunc>()` | no YIELD production; only 5 of 28 C++ table functions callable |
| non-literal `SKIP`/`LIMIT` | C++ constant-folds any expression (`LIMIT 1+1`, `to_int64(ceil(1.7))`); Rust binds-rejects — 11 tck fails |
| zero-label `CREATE ()` with one node table | C++ infers the sole table and creates; Rust rejects — 8 tck fails |
| nested `EXISTS`/`COUNT` subqueries | "not implemented in this phase"; C++ evaluates |
| **case-insensitive variable references** | `UNWIND [1,2] AS a RETURN A` works in C++; Rust "Variable A is not in scope" |
| implicit arg→STRING coercion | every STRING param of ~25 string functions (`lower(123)`, `left(to_double(1.34),8)`) works in C++, runtime-errors in Rust |
| ~28 functionally-distinct missing functions | `md5/sha256/hash`, `concat_ws`, `array_append/prepend`, `list_has_all`, `current_date/current_timestamp`, `epoch_ms/to_epoch_ms`, `random/setseed`, `octet_length/encode/decode`, `count_if`, `error`, `start_node/end_node`, `is_trail/is_acyclic`, `internal_id`, `percentiledisc`, … |
| interval/date input formats | `'-3 days'`, `'1.5 hours'`, `decade/millennium/quarter`, `2024/07/01` all rejected (C++ accepts) |
| required `MATCH` after `OPTIONAL MATCH` in the same part | documented; confirmed still open |
| `EXISTS{}`/`COUNT{}` in `WITH … WHERE` | phase-rejected (C++ evaluates correlated post-projection filters; distinct from the nested-subqueries row) |
| node/rel-valued *expressions* through `WITH` | "carrying a node or relationship expression through WITH is not supported in this phase" (bare variables carry fine; `issue.2589`) |
| `ORDER BY x ASCENDING/DESCENDING` | long-form sort keywords are parse errors (`ASC`/`DESC` only) |
| `HINT` inside `EXISTS{}`/`COUNT{}` subqueries | parse error (top-level MATCH HINT parses-and-ignores) |
| CALL-config expression values | `CALL timeout=(1+2+3)*10000` rejected — C++ constant-folds config values |
| coalesce/ifnull over-strict type-check | the 2026-06-30 bind-time fix over-corrects: `coalesce(1, true)` and struct-arg merges C++ accepts are rejected |
| misc | heterogeneous list literals (`['a',1]` → C++ coerces to STRING[]), positional params `$1`, pattern-predicate expressions, `WSHORTEST`, unicode identifiers, doubled-backtick escapes, `USE`/`ATTACH`/named `GRAPH`s (`graph/` dir 0p/9f), rel-group `COPY (from=…, to=…)` (blocks all of `rel_group/`) |

**Public API / tooling parity (vs the C++ `main/` surface — P5-adjacent, mostly not phase-planned):**
the Rust crate exposes Database/Connection/QueryResult/PreparedStatement/Transaction only. Missing:
interactive shell; C ABI + language bindings; the in-memory **Arrow C Data Interface** (`query_as_arrow`,
Arrow-backed tables — distinct from the P4 file readers); QueryResult column-type metadata, query
summary/timing, and multi-result chains (the public `query()` is single-statement — the parser demands
EOF, so `;`-separated strings error); prepared-statement result metadata + typed parameter binding
(beyond the documented plan-cache gap); Connection interrupt/timeout/thread-count controls; UDF
registration; the low-level `StorageDriver` scan/count embedding API; CALL-config *semantics* (unknown
options accepted as no-ops, non-literal values rejected where C++ folds, `current_setting` defaults
incomplete — only 3 knobs have real behavior). Fix-level notes: `docs/fable-audit/fix-notes.md`.

## 3. Behavioral deviations (most important)

### 3.1 Crashes on input — worst class

| # | deviation | repro |
|---|---|---|
| C1 | **Wrong-arity scalar calls panic the process.** No bind-time arity validation; eval indexes `args[i]` directly. **142 of 390 probes** (195 names × 0/1-arg) kill the engine — in an embedded DB that's the host process. C++: clean binder errors for all. | `RETURN sqrt()`, `RETURN left('abc')`, `RETURN properties(r)` → panic `scalarfn.rs:1178/1187` |
| C2 | **`EXISTS{}`/`COUNT{}` inside a recursive-rel per-step filter panics** (valid input; C++ answers it). Planner lifts the subquery to a chunk column; the per-step lambda filter evaluates against an empty chunk → OOB `koko-expr:177`. This is what kills the `demo_db` corpus run. | `MATCH (a:User)-[e:Follows* (r,n \| WHERE EXISTS {MATCH (:City)})]->(b:User) RETURN COUNT(*)` |
| C3 | Test-runner has no per-statement `catch_unwind` — one panic burns the whole file/dir run. | `demo_db` dir reports nothing |

### 3.2 Silent wrong values (same input, different answer, no error)

Value semantics:

| # | deviation | repro → C++ / Rust |
|---|---|---|
| V1 | **float→int casts truncate; C++ rounds half-to-even** (`nearbyint`). Every width, every form. | `CAST(5.5 AS INT64)` → 6 / 5; `to_int16(1.731)` → 2 / 1 |
| V2 | **Invalid dates/timestamps silently normalize** instead of erroring — incl. format garbage. | `date('2020-02-30')` → error / `2020-03-01`; `date('01-01-2020')` → error / **`0006-07-13`**; `make_date(2011,1,32)` → error / `2011-02-01` |
| V3 | **`SUM` doesn't widen**: C++ SUM(INT*)→INT128, SUM(UINT*)→UINT128, SUM(FLOAT)→DOUBLE; Rust keeps the arg type (`koko-function/lib.rs:1670`) and throws an *invented* overflow error where C++ returns the value. A p0 fixture bakes the invented error in. Worse: the UINT128 path uses an i128 accumulator that silently **wraps** — SUM of two 2¹²⁷−1 values → **-2** (C++: `340282366920938463463374607431768211454`). | `SUM` of two ~2^63 values → `18446744073709551614` / "Overflow exception" |
| V4 | **Wrong-typed args yield garbage instead of bind errors**: DOUBLE/STRING in INT positions. | `left('hello',2.0)` → bind error / `''`; `substr('hello',1.0,3.0)` → bind error / `hello`; `factorial('5')` → bind error / `1` |
| V5 | **NULL inside composites — equality/comparison diverge.** | `[1,NULL]=[1,NULL]` → True / False; `{a:NULL}={a:NULL}` → True / False; `[3,4]>[3,NULL]` → False / NULL |
| V6 | `'a' + 1` silently concatenates (~16 cases); C++ binder-rejects non-STRING `+`. | `'a'+1` → error / `a1` |
| V7 | **`-9223372036854775808` literal types as INT128** (C++: INT64) → downstream arithmetic silently escapes INT64 range where C++ raises Overflow. | `abs(-9223372036854775808)` → Overflow / `9223372036854775808` |
| V8 | `labels(n)` returns `STRING[]`; C++ returns scalar STRING (`labels` aliases `label`). Baked into a p0 fixture. | `labels(n)` → `City` / `[City]` |
| V9 | NaN DISTINCT: C++ counts NaNs as distinct-from-each-other. | `count(DISTINCT …nan,nan…)` → 2 / 1 |
| V10 | Unicode divergences: case mapping (Rust full-fold, C++ 1:1 — also ﬁ→FI, İ, final-sigma) and regex character classes (`\w`/`\d`/`\s`: C++ RE2 = ASCII, Rust = Unicode). Grapheme indexing is identical. | `upper('straße')` → `STRAẞE` / `STRASSE` |
| V11 | `nextval()` is lifted above WHERE — advances per pre-filter row and returns shifted values. | `MATCH (n) WHERE n.id=2 RETURN nextval('t')` → 1 (currval 1) / 2 (currval 3) |
| V12 | Node/rel **bare-internal-id pipeline seam**: functions consuming node/rel values mid-pipeline see an `INTERNAL_ID`, not the value. `struct_extract(r,'_src')` errors; on path-materialized `rels(p)[i]`/`nodes(p)[i]` the `_src/_dst/_id` fields are **silently NULL** (root cause of the `ldbc/basic` fail). | `struct_extract(rels(p)[1],'_src')` → `0:0` / NULL |
| V13 | `length(p)` on the degenerate single-node path (unmatched OPTIONAL var-length): C++ NULL, Rust 0. (The path *value* matches.) | see repro in ledger |
| V14 | Common-type inference: mixed-sign ints → C++ INT16 vs Rust INT8; DECIMAL+numeric → Rust collapses to DOUBLE (C++ keeps DECIMAL(21,2)). | `[CAST(1 AS UINT8), CAST(1 AS INT8)]` |
| V15 | `current_setting('threads')` via CALL returns empty (C++ returns the value). | → `10` / empty |
| V16 | `range()`/`list_product()` silently narrow UINT128 through `as_i64` — wide values become defaults or are skipped. | `list_product([CAST(3 AS UINT128), CAST(5 AS UINT128)])` → 15 / **1** |
| V17 | `split_part(s,'',i)`: C++ treats an empty separator as per-character split; Rust returns `''` beyond index 1. | `split_part('Alice','',5)` → `e` / `''` |

Write-path/state:

| # | deviation | repro → C++ / Rust |
|---|---|---|
| W1 | **MERGE on a pattern matching multiple existing rels binds ONE; C++ binds ALL** — result cardinality *and* final DB state diverge (ON MATCH SET updates 1 vs all). | two `(a)-[:R]->(b)` edges; `MERGE (a)-[r:R]->(b) ON MATCH SET r.w=99 RETURN r.w` → 2 rows, both updated / 1 row, one updated |
| W2 | **Correlated `EXISTS{}` in an `OPTIONAL MATCH … WHERE` drops the correlation** → 0 rows vs 1. | `issue/4080`; verified minimal repro in ledger |
| W3 | **Bare `LOAD FROM` header sniffing drops the first data row** (data loss) when it merely *looks* header-ish. | CSV `1,foo↵2,bar` → 2 rows / 1 row |
| W4 | **CSV nested-string quoting: C++ retains quote chars as data; Rust strips them** → different stored values, sizes, equality, grouping (`agg` StructHashTest 4→6 groups). The pre-audit gap ledger described this backwards. | tinysnb `o.state.location[2]`: `size` 10 / 8 |
| W5 | COPY options ignored: `skip=N` (all rows loaded), `header=false/0` (header always auto-skipped); `LOAD … skip` off-by-one; CSV escape-strictness (C++ errors on bad escape, Rust accepts). | `copy_with_skip_lines`, `copy_multi_boolean` clusters |
| W6 | `ACYCLIC` recursion: Rust = true no-repeated-node; C++'s ACYCLIC behaves like WALK. Counts diverge (9 vs 2 on a synthetic cycle). Semi-documented. | `-[:R* ACYCLIC]-` |
| W7 | `UNWIND <scalar>` → silent 0 rows (C++ errors). | `UNWIND 1 AS x` |
| W8 | **Repeated `ON CREATE SET`/`ON MATCH SET` clauses: only the last is applied** — the parser overwrites instead of accumulating (C++ applies all). Silent write loss. | `MERGE (n:N {id:1}) ON CREATE SET n.a=1 ON CREATE SET n.b=2` → a,b = `1\|2` / `\|2` |
| W9 | MERGE duplicate-input rows when dedup is gated off (a non-key variable carried): Rust emits per-input-row; C++'s factorized output collapses them. | `MATCH (n:Q) UNWIND [1,1] AS i MERGE (p:P {id:i}) RETURN n.qid` → 2 rows / 4 rows |

Rendering/introspection (byte-contract violations):

| # | deviation |
|---|---|
| R1 | **Nested list/map STRING elements inside STRUCT/MAP render single-quoted**; C++ renders bare. `{a:['x','y']}` → C++ `{a: [x,y]}` / Rust `{a: ['x','y']}`. (Top-level structs/lists match.) |
| R2 | `floor/ceil(DECIMAL)` keeps input scale; C++ reduces to scale 0 (`-10.0` vs `-10`, and `typeof` DECIMAL(18,3) vs (18,0)). |
| R3 | `REAL` alias maps to DOUBLE; C++ REAL≡FLOAT (visible in `table_info`, rendering). |
| R4 | `SHOW_SEQUENCES` start column shows *mutated* state; C++ shows the defined start value. |
| R5 | `TABLE_INFO` default column prints the folded value (`5.400000`) not source text (`5.4`) — documented, confirmed. |
| R6 | String→nested cast quote handling (`CAST('["a","b"]' AS STRING[])`): C++ keeps quote chars, Rust strips (same family as W4). |
| R7 | COPY success message drops the table name ("…copied to table." vs "…copied to the person table."). |

### 3.3 Rust accepts what C++ rejects (incl. invented extensions — two are semantic traps)

- **List comprehension `[x IN l WHERE p | e]`** — not in the C++ grammar at all. **Trap:** the no-WHERE
  form `[x IN l | e]` *parses in both* — C++ reads it as membership + bitwise-OR (usually "x not in
  scope"), Rust evaluates a comprehension. Same text, different meaning.
- **`SET n += {…}` / `SET n.p += v`** — C++ parser-rejects all `+=`; Rust implements Neo4j map-merge.
  Baked into p0 fixtures. (docs/AGENTS claim it as a shipped feature traced to C++ — it isn't in C++.)
- `WALK` keyword in recursive rels (C++ rejects the keyword; it's the C++ default *behavior* but not
  its syntax); empty struct literal `{}`; `exp()`/`power()`/1-arg `round()`/`INTERVAL*INT`/
  `date(TIMESTAMP)`/`date_part('dow'|'weekday')` (functions C++ lacks); `INTEGER` type alias;
  `'+5'`/`'007'` numeric-string casts; bool↔int CASTs; cross-type `=` returning False where C++
  binder-errors (`1 = true`, `1 = [1]`, DATE vs number); mixed-type CASE branches.
- **Missing validations** (C++ errors, Rust silently proceeds): re-`CREATE` of a bound variable
  (`MATCH (a) CREATE (a)`); CREATE reading a sibling node's property in the same clause; list literal
  with a hole `['a', , []]`; `CALL threads=4.5` and unknown `CALL` option names (accepted as no-ops);
  recursive-rel projection-item validations; `list_filter` lambda type; NULL-key map (ambiguous — current
  C++ shell also accepts; the bind-time literal dup-key check also misses eval-time duplicates);
  `UNION` column typed only by NULL (C++: "x has data type ANY" error); missing struct/union field via
  `struct_extract`/`union_extract` → NULL instead of C++ bind error; `show_tables()` without RETURN;
  `regexp_replace` 4th-arg options other than `'g'` silently degrade to single-replacement (C++
  bind-rejects); rel `storage_direction='fwd'` is parsed/recorded but the C++ query-direction validation
  is skipped (undirected patterns on a fwd-only rel return rows; C++ binder-rejects).
- **PK type eligibility is inverted vs C++** (which allows STRING + all numerics incl. FLOAT/DOUBLE and
  rejects the rest at DDL bind): Rust accepts a BOOL PK (C++: "Primary keys must be either STRING or a
  numeric type") yet rejects FLOAT/DOUBLE PKs — at *insert* time ("Unsupported primary key type in this
  phase"), because binder/catalog validate nothing and the storage `PkKey` has no float encoding.

### 3.4 Error-channel divergences (right rejection, wrong message/class/stage)

The single biggest corpus bucket (~60+ cases across `exceptions`/`copy`/`ddl`/tck) and the reason
`exceptions/` is 5p/53f:

- **Missing exception classes**: C++ `Copy exception:` (duplicate PK in COPY, malformed rows) surfaces
  as Rust `Runtime exception:`; out-of-u128 literals are `Conversion` in C++, `Parser` in Rust.
- **No source decoration on parser errors**: C++ appends `(line: N, offset: M)` + query echo + caret;
  COPY errors carry `Error in file <path> on line <N> … Line/record containing the error: '<…>'`.
- **Function-signature errors**: C++ dumps the full overload table ("Function SUM did not receive
  correct arguments: Actual/Expected…"); Rust one-liners. (Unreproducible without a signature catalog —
  see §6/R1.)
- **Stage differences**: missing-PK `CREATE (:t)` is a *binder* error in C++ ("Create node  expects
  primary key id as input.") vs Rust *runtime* ("Null value found for primary key column."); validation
  order differs (`ALTER … ADD FROM/TO` on a node table reports pair-exists in C++ before
  not-a-rel-table in Rust).
- Wording families: unknown property in CREATE inline map — C++ "Cannot find property b for ." vs Rust
  "Table T does not contain property b."; several invented Rust strings ("…in this phase") that can
  never match the oracle; C++ signature errors end with two blank lines (Rust trims).

### 3.5 Intentional/accepted divergences (fine, but keep them explicit)

Internal `_ID` numbering (clean-room policy — but note: only **3 of 45** current tck fails are _ID
divergences, not "most" as docs claim); hermetic rel-table-id numbering; TABLE_INFO default text (R5 —
documented as lossy).

## 4. Oracle defects found (C++ bugs — do NOT chase parity blindly)

The differential work also caught the *oracle* misbehaving. Recommend the `_ID`-style treatment:
document each as "accepted divergence — C++ defect":

- **C++ SIGSEGVs**: `label(rels(p)[1])`; `1 IN NULL`; `repeat(s, -n)`; `list_sum/list_product/
  list_reverse_sort/list_any_value/list_contains/struct_extract` on NULL. (Rust returns the label /
  NULL — correct.)
- C++ `UNREACHABLE` asserts: `list_position([1,null,3], null)`, `date_part('week', INTERVAL)`.
- C++ wrong values: `array_extract([10,20,30],2)` → `1` (stringifies!); `array_cross_product` on 2-D →
  bogus zeros; `factorial(21)` silently wraps; `CASE <non-null> WHEN NULL` matches; `to_string(null)`
  internal ANY-vector error; unbound `$param` in WHERE passes rows while `$param IS NULL` is NULL.
- C++ quirks Rust chose not to copy: `n.id = 2.0` on an INT64 PK returns nothing (its own `2 = 2.0` is
  True; Rust returns the row); aggregate allowed in WHERE; `DROP MACRO IF EXISTS` still errors (with
  typo "Marco"); cryptic `Error: vector` on a `ddl.test` ALTER sequence.

## 5. Harness fidelity (can we trust the numbers?)

Audit of `koko-test-runner` vs the C++ framework: **no systematic pass-inflation** — error strings
compare exactly (after rtrim), rows sort like C++ honoring `-CHECK_ORDER`, missing result blocks are
hard errors. One narrow leniency: `-CHECK_PRECISION` (2 files) uses a wider-than-C++ tolerance on
re-parsed rendered strings. The real distortion is **hidden coverage**, all on the fail side:
- 45 cases C++ runs but Rust skips (`-BATCH_STATEMENTS` 25, `-CREATE_DATASET_SCHEMA` 8, `-LOOP` 7,
  concurrent-exec 5) + 19 uppercase `-DATASET CSV EMPTY` cases (case-sensitive match, `lib.rs:601`);
- 28 files collapse to a single FAIL on directive parse errors (`---- error(regex)` 13 files,
  `---- hash` 4) hiding their case counts; `-CHECK_COLUMN_NAMES` ignored (false fails); `-SKIP` with a
  trailing comment not honored; `-SKIP` case *bodies* are still strict-parsed (one unsupported directive
  inside a skipped case parse-fails the whole file); corpus placeholder expansion (`-SET`,
  `${COLS}`-style, REPEAT/ARANGE row builders) unhandled; no per-statement `catch_unwind` (one panic =
  whole dir lost); `-MULTI_COPY_RANDOM` unimplemented (its "0 rows" fails are harness, not engine).
- The 202 tck skips are inherited C++ `-SKIP` directives (C++ doesn't run them either) — not hiding
  Rust-specific gaps.

**Docs-vs-reality corrections** (the gap ledger and AGENTS as they stood pre-audit): pass counts stale-low; the
tck triage was wrong in both directions (Class B is 12 not ~23; `_ID` bucket is 3 not "most"; `m.x` map
access works now; "ORDER BY over aggregate" is error-wording, C++ also rejects; the two dominant buckets
— SKIP/LIMIT folding and error-wording — were unlisted); the claimed sequences-transactionality deviation
**does not exist** (C++ 0.17 also rolls back `nextval`); the degenerate-path claim was stale (values match
now; only `length()` differs); the CSV quote bug was described backwards; `SET +=`/MERGE-multi-match
descriptions didn't match the C++ engine. *Following this audit (2026-07-01) the doc tree was
consolidated: the stale status blocks and the `docs/pi/` gap tree were folded into this document and
removed (per-file disposition in `docs/fable-audit/B1-pi-disposition.md`; retained C++ fix notes in
`docs/fable-audit/fix-notes.md`), and the then-current gap ledger was reduced to perf/deferrals.*

## 6. Structural root causes ("what boxed us in")

1. **No typed function catalog.** C++ binds against per-function signature lists (overloads, arity,
   param types, implicit-cast costs, result types). Rust dispatches on name at eval with ad-hoc
   `args[i]` access and a separate name→result-type map. This one gap produces: the 142-panic arity
   family (C1), garbage-on-wrong-arg-type (V4), no implicit arg coercion (~25 functions), wrong SUM
   result types (V3), unreproducible overload-table error messages (§3.4), and `show_functions()`
   being impossible. **Highest-leverage fix in the codebase**: a declarative signature table would fix
   five deviation families at once.
2. **Bare `InternalId` as the pipeline currency for node/rel values** (C++ carries struct-backed
   values). Every value-consumer needs bespoke inflation (`label()`, `keys()`, `deep_materialize` at
   projection seams) and each miss is a silent NULL or type error (V12, ldbc fail). This will keep
   generating bugs as the function surface grows — consider inflating at *expression-eval* boundaries,
   not just projection output.
3. **Subquery lifting assumes a flat pipeline context.** Lifted `EXISTS`/`COUNT`/`nextval` columns
   break inside recursive-lambda scopes (C2 panic) and evaluate too early relative to WHERE (V11);
   nested subqueries are unimplemented for the same reason. The lifting design needs a scoped story.
4. **No semantic-validation layer in the binder** — the "Class C" accepts-invalid family (§3.3) is
   structural, not incidental: validations live scattered at eval time or nowhere.
5. **Error taxonomy narrower than C++** (no `Copy exception`, no parser source decoration, invented
   wordings). Cheap to fix mechanically; large corpus payoff (~60 cases).
6. **Neo4j-isms imported into a Kùzu-dialect engine** (list comprehension, `SET +=`, `labels()` list,
   graceful `IF EXISTS`, case-sensitive variables): each is defensible in isolation, but two are
   active semantic traps (§3.3) and all are baked into p0 fixtures, which made the port's own test
   suite assert the deviations. **Decide the dialect contract explicitly** (pure-oracle vs
   oracle+documented-extensions), then re-tune p0 with a C++-diff gate (the p0-vs-C++ sweep from this
   audit is reusable: `docs/fable-audit/p0_to_probe.py`).
7. Minor: params substituted as bind-time literals (plan-cache latency only); macros as pre-bind AST
   rewrite (DEFAULT-position + rendering residue); `-2^63` literal-typing path (V7).

## 7. Recommended priority order

*(Realized as the milestone plan in [`ROADMAP.md`](ROADMAP.md): M1 ≈ items 1+7+8, M2 ≈ 2+5, M3 ≈ 3,
M4 ≈ 6 + the validation family, M5 ≈ 4 + the function backfill — then P4/P5.)*

1. **Stop the crashes** (C1/C2): arity guard at bind (mechanical even without full signatures) +
   `catch_unwind` in the runner; fix or reject-cleanly the recursive-lambda subquery.
2. **Silent-wrong-value fixes with trivial scope**: float→int `nearbyint` (V1); temporal range
   validation (V2); SUM widening (V3); INT64_MIN literal (V7); composite-NULL equality (V5); `'a'+1`
   (V6); LOAD header-drop (W3); CSV quote retention (W4); COPY `skip`/`header` options (W5).
3. **The function-signature catalog** (§6.1) — then implicit coercion, overload errors, and the
   missing-function backfill ride on it.
4. **Parser surface**: `IN`/`STARTS WITH`/`ENDS WITH`/`CONTAINS` first (they block real workloads),
   then SKIP/LIMIT folding, quantifiers, `^`, zero-label CREATE, case-insensitive vars.
5. **Write-path semantics**: MERGE multi-match (W1), correlated-EXISTS-under-OPTIONAL (W2),
   `nextval` post-WHERE (V11).
6. **Error-channel parity** (classes, decoration, top-20 wordings) — biggest corpus-count win per hour.
7. **Dialect decisions**: list-comprehension/`+=`/`labels()`/ACYCLIC/oracle-defect list (§4) — decide,
   document, and encode each in a test the way `_ID` was decided.
8. Runner hygiene: `error(regex)`, `hash`, uppercase-EMPTY, `-CHECK_COLUMN_NAMES`, `-SKIP` comment,
   per-statement panic isolation — ~90 cases of hidden signal.

*Full evidence trail in `docs/fable-audit/`: the ten sub-reports (`A1`…`A10`), the verified-findings
ledger (`main-session-findings.md`), the raw per-dir corpus outputs (`corpus-run/`), re-runnable probe
files (`probes/`), the two harnesses — `diffprobe.py` (differential prober; pair it with
`crates/koko/examples/koko_cli.rs`, added for this audit) and `p0_to_probe.py` (re-diffs the p0
suite against the C++ shell) — plus `B1-pi-disposition.md` (per-file disposition of the folded
`docs/pi/` gap tree: 39 covered / 13 fixed-since / 17 folded here / 10 stale) and `fix-notes.md`
(C++ mechanism notes with source line refs for the still-open items, extracted from that tree before
its removal; the original tickets remain in git history).*
