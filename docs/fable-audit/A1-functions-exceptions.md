# A1 — Audit: function / exceptions / cast / agg / arithmetic / uint128 / common

Scope: the 7 test dirs assigned. Failures classified as:
(a) missing feature/function, (b) WRONG RESULT (accepts input, semantically wrong output),
(c) error-message wording only, (d) test-harness limitation, (e) dataset/file-format gated.

**CONFIRMED** = ran both engines (C++ shell oracle + Rust CLI/runner) or re-ran the corpus file.
**SUSPECTED** = read-only inference from corpus + source.

Probe files saved in scratchpad: `/tmp/p1.probe … /tmp/p9.probe` (re-runnable via `diffprobe.py`).

---

## 1. Headline counts per dir (by category)

| dir | pass | fail | (a) missing | (b) WRONG | (c) wording | (d) harness | (e) data/fmt |
|-----|-----:|-----:|-----:|-----:|-----:|-----:|-----:|
| function    | 36 | 59 | ~46 | 9  | 3  | 1 (+2 parse) | 0 |
| exceptions  |  5 | 53 | ~14 | 6  | ~25| 4 (parse)    | 3 |
| cast        |  6 | 24 | ~19 | 4  | 1  | 0            | 0 |
| agg         | 10 |  4 |  1  | 2  | 0  | 0            | 1 |
| arithmetic  | 90 |  3 |  1  | 1  | 0  | 1            | 0 |
| uint128     |  6 |  3 |  0  | 1  | 2  | 0            | 0 |
| common      |  3 |  7 |  2  | 1  | 1  | 3            | 0 |

Big picture: **the overwhelming majority of failures are NOT wrong results.** They are
(a) unimplemented functions/operators/table-functions and (c) error-message wording. The genuine
category-(b) semantic deviations are a small, high-value set (~24 across all dirs, ~15 distinct root causes).

---

## 2. Category (b) — WRONG RESULT deviations (the important ones)

### CONFIRMED (ran both engines)

| # | repro (dataset) | expected (C++) | actual (Rust) | suspected root cause | tests hit |
|---|---|---|---|---|---|
| B1 | `RETURN to_int16(1.731), to_int32(1.5), to_uint128(1.50), to_uint128(1.49)` | `2\|2\|2\|1` | `1\|1\|1\|1` | float→int/uint cast **truncates toward zero**; C++ **rounds to nearest** (half away from 0). | function cast #15, cast DataTypeCasting #155 |
| B2 | `MATCH (a) RETURN DISTINCT labels(a)` ; `RETURN typeof(labels(a))` | `movies` / `STRING` | `[movies]` / `STRING[]` | `labels()` must alias `label()` and return a **scalar STRING** in Koko, not `STRING[]`. | function label #2 |
| B3 | `RETURN [3,4] > [3,NULL]` | `False` | `` (NULL) | element-wise list comparison with a NULL element: C++ yields `False`, Rust yields NULL. | function comparison #39 |
| B4a | CSV struct value `[\"vanco,uver north area\"]` → `MATCH (o:organisation) RETURN o.state.location[1]` (tinysnb) | `"vanco,uver north area"` (quote chars **retained as data**) | `vanco,uver north area` (quotes **stripped**) | Rust's CSV struct/list-literal parser strips surrounding quote chars from nested string elements; C++ keeps them. → **different stored values**, breaks equality/grouping. | agg StructHashTest #4 (4 groups → 6), cast #64, cast #25 |
| B4b | `RETURN {x: ['a','b']}` and `RETURN string({x: ['a','b']})` | `{x: [a,b]}` | `{x: ['a','b']}` | Rust adds single-quotes to strings inside a **list-within-struct** when rendering; C++ renders them unquoted. (Top-level list of strings is fine in both.) | cast #64, cast #25, agg #4 |
| B5 | `RETURN FLOOR(CAST(0.2,'decimal')+CAST(-10,'decimal'))` ; `typeof(...)` | `-10` / `DECIMAL(18, 0)` | `-10.0000` / `DECIMAL(18, 3)` | FLOOR/CEIL on DECIMAL must **reduce scale to 0**; Rust keeps the input scale (trailing zeros). | arithmetic add AddDECIMAL #7 |
| B6 | `MATCH (:person)-[s:studyAt]->(:organisation) RETURN SUM(s.code)` (s.code is UINT64) | `9223372036854782520` | `Error: Overflow exception: SUM of INT64 values is not within INT64 range.` | SUM aggregator uses an **INT64 accumulator / INT64 range-check even for UINT64** input (error text literally says "of INT64" for UINT64 data). | agg simple #66, uint128 AggregateFunctions #6 |
| B7 | `RETURN make_date(2011,1,32)` | `Error: Conversion exception: Date out of range: 2011-1-32.` | `2011-02-01` | `make_date` does not range-check day/month; **rolls over** instead of erroring. | exceptions transaction ParsingErrorRollbackTest #4 |
| B8 | `MATCH (a:person) CREATE (a)` (tinysnb) | `Error: Binder exception: Cannot resolve any node or relationship to create.` | (silently succeeds, no-op) | missing binder validation: re-CREATE of an already-bound var with no new pattern must be rejected. | exceptions binder_error #1 |
| B9 | `CALL threads=4.5` | `Error: Binder exception: Expression 4.500000 has data type DOUBLE but expected UINT64. Implicit cast is not supported.` | (accepts; threads=4.5) | CALL config setter does not type-check the value (no implicit-cast rejection). | common arrayimplicitcasting #1 |
| B10 | `COPY person FROM "…/vPerson.csv"` (message) | `5 tuples have been copied to the person table.` | `5 tuples have been copied to table.` | COPY success message template omits the **table name**. | exceptions auto_commit CopyNodeOutputMsg #2 |
| B11 | `RETURN CAST(union_value(f := 123) AS UNION(v STRING, b BOOL, f FLOAT, t INT32))` | `123.000000` | `123` | union→union cast does not **convert the underlying value to the target field's type** (INT stays INT, not promoted to FLOAT). | cast CastBetweenUnion #4 |
| B12 | `RETURN ['a', , []]` | `Error: Binder exception: Expression a has data type STRING but expected INT64[]. Implicit cast is not supported.` | `[a,,[]]` | list literal with a **missing element** is accepted and element types are not unified; C++ rejects. | function list FunctionList #14 |

### SUSPECTED (corpus/source only — not independently re-run in both engines)

| # | case | expected → actual | note |
|---|---|---|---|
| B13 | function rec_joins_small.GDSSmall #3 | `…\|[0:0,0:0,0:0]\|[0:1,0:1,0:1]` → `…\|[,,]\|[,,]` | recursive-rel path: lists of internal-IDs render as empty elements `[,,]`. Needs PROJECT_GRAPH-adjacent dataset + HINT to repro. |
| B14 | cast CastToNestedTypes #1 (LOAD WITH HEADERS + `null_strings`) | `{num: 2341, str: }` → `{num: 2341, str: no}` | field mis-alignment / null_strings handling in nested LOAD; a real value ("no") appears where an empty string is expected. |
| B15 | cast implicit_cast.ImplicitCastWithInsertion #35 | success → `Binder exception: Variable column0 is not in scope.` | `LOAD FROM csv (...)` default column naming (`column0…`) not in scope. |
| B16 | exceptions #6/#7/#20/#25 (missing validations) | error → success | `UnsupportedRecursiveRelProjectionItem`, `PathPropertiesErrors`, `InvalidHeader` (header row not rejected as data), `NullPrimaryKeyInNodeFile` (NULL PK not rejected). Unimplemented error paths. |
| B17 | function list_filter.ListFilter #7 | error → success | LIST_FILTER lambda-returns-non-BOOL not validated. |
| B-amb | cast NestTypeImplicitCast #5 `map([NULL,NULL],[1,2])` | corpus expects `Runtime exception: Null value key is not allowed in map.` | **AMBIGUOUS**: the current C++ shell ALSO accepts this (`{=1,=2}`), so the oracle disagrees with the corpus — likely version skew. Do not treat as a clear Rust bug. |

---

## 3. Category (a) — missing feature / function

### Missing scalar/list/util functions (CONFIRMED absent; C++ has them)
`OCTET_LENGTH`, `HASH`, `MD5`, `SHA256`, `INTERNAL_ID`, `IS_ACYCLIC`, `START_NODE`, `END_NODE`,
`CONCAT_WS`, `COUNT_IF`, `ERROR`, `RANDOM`, `LIST_HAS_ALL`, `ARRAY_APPEND`, `ARRAY_PREPEND`,
`PERCENTILEDISC` (agg).  (function dir + arithmetic RANDOM + agg PERCENTILEDISC.)

### Missing operators / syntax (CONFIRMED via parser errors)
- **`STARTS WITH` / `ENDS WITH` / `CONTAINS`** — unsupported in the Rust parser **everywhere, including WHERE** (not just projection). `MATCH (a:person) WHERE a.fName STARTS WITH 'A' RETURN count(*)` → parse error. High-impact. (function string #21, exceptions wrong_header #7.)
- **Factorial postfix `!`** — `RETURN 5!` → C++ `120`, Rust parse error. (function arithmetic #1.)
- **List predicates `any/all/none/single(x IN list …)`** — parser rejects `in`. (function predicate Any/ListPredicate*.)
- **`WSHORTEST(cost)` weighted-shortest-path** syntax — parser rejects. (function weighted_shortest.)

### Missing table functions / CALL forms (whole family)
`CALL <tablefunc>(...)` unimplemented "in this phase": `SHOW_FUNCTIONS`, `SHOW_OFFICIAL_EXTENSIONS`,
`stats_info`, `storage_info`, `PROJECT_GRAPH`, `_CACHE_ARRAY_COLUMN_LOCALLY`, `TABLE_INFO … YIELD`.
Also the **`YIELD` clause** itself is unparsed. (function call/*, stats_info, storage_info, basic/rec_joins GDS.)

### Missing implicit-cast capability (CONFIRMED — one root cause, 16 cast failures)
String functions (`left`, etc.) require a STRING arg and do **not** insert an implicit cast; C++
coerces INT/DOUBLE/DATE/TIMESTAMP/BOOL/UUID/… → STRING first.
`RETURN left(to_int64(134),3)` → C++ `134`, Rust `Runtime exception: expected a STRING argument, got INT64`.
Covers cast implicit_cast.Cast*ToString #19–#34 (all 16).

### Missing UNION casting (CONFIRMED)
`CAST(false AS UNION(a BOOL))` → C++ `False`, Rust `Cannot cast BOOL to UNION(a BOOL)`. Scalar→UNION,
list→UNION[], and implicit INT64→UNION on CREATE all unimplemented. (cast CastToUnion*, CastWithUnionNested.)

### Incomplete INTERVAL string parser (CONFIRMED)
Missing: hour abbrev **`h`**, **`quarter`** unit, **fractional** magnitudes (`1.5 microsecond`).
`interval('34 h')`→C++ `34:00:00`, Rust error; `interval('3 quarter')`→C++ `9 months`, Rust error;
`interval('1.5 microsecond')`→C++ `0.000002`, Rust error. Supported units that DO work: years/months/days/minutes/milliseconds/`us`.
(function interval #5, timestamp #21; common interval DifferentTypes/Fractional.)

### Missing COPY variants (parser-level; CONFIRMED via corpus)
`COPY … FROM (<subquery>)`, `COPY … FROM ["f1","f2"]` (list of files), `COPY … FROM (…) BY COLUMN`,
CSV `IGNORE_ERRORS=TRUE`, npy multi-file. (function uuid #2; exceptions duplicated/null_pk/npy_fault/WrongNumOfNumpyFiles.)

### Other feature gaps
- Property access on **unlabeled / multi-label** node pattern (`MATCH (a)-[:knows]->(b) … a.name`) → "Cannot find property". (function null #1.)
- **Anonymous / label-less node CREATE** (`CREATE ({AGE:1})-[]->(...)`) → "must specify exactly one label in this phase". (function range #3, exceptions multi_label_update.)
- **`.rowid`** pseudo-property. (function rowid #1.)

---

## 4. Category (c) — error-message wording only (engine correctly rejects)

These are correct rejections with a different string. Dominant in `exceptions`. Sub-themes:

1. **Wrong exception CLASS prefix** — C++ `Copy exception:` vs Rust `Runtime exception:` / `Conversion exception:`,
   with otherwise-identical body. (exceptions duplicated #1/#2, rel_multiplicity ×3.)
2. **Missing file/line/record context** — C++ `Copy exception: Error in file … on line 6: … Line/record containing the error: '5a…'`
   vs Rust `Conversion exception: Cannot parse 5a as INT16.`. (exceptions CastError, UnMatchedColumnType, MissingColumnErrors.)
3. **Missing `(line: X, offset: Y)` + query echo + `^^` caret** in parser errors — Rust emits the terse message only.
   (exceptions syntax_error: `!=` operator, EmptyTable, EmptyProperty, ReturnNotAtEnd, ConjunctiveComparison — 5 cases; also uint128 literal-range below.)
4. **Different specific wording** — e.g. multi-label create, `MANY_LOT` multiplicity, INT35/STRUCT/BIGINT/SMALLINT type-name
   errors (`Not implemented … in this phase` vs `Catalog exception: … is neither an internal type nor a user defined type`),
   RECURSIVE_REL property-access message, interval/timestamp parse-error phrasing.
5. **Different rejection STAGE** — uint128 UInt128PK/Overflow: literal `340282366920938463463374607431768211456`
   rejected at **parse** time (`integer literal out of UINT128 range`) vs C++ at **cast** time
   (`Conversion exception: Cast failed. Could not convert "…" to UINT128.`). (uint128 #2/#5, #4/#6.)
   Also list_reduce / list_transform lambda messages (function #45/#46). Also common interval BasicUsageCheck.

---

## 5. Category (d) — test-harness limitations (NOT engine bugs)

- **`-CHECK_COLUMN_NAMES`** not implemented: runner doesn't emit/count the column-name header row, so expected
  count is off-by-one. **The data rows are byte-for-byte correct.** (common columnname ColumnName + ColumnNameOrdered — both.)
  Re-ran: Rust returns exactly `Adam|0, Karissa|1, Noura|1, Zhang|2` etc.
- **`-SKIP` with a trailing `#` comment** is ignored — the runner runs the case anyway.
  `arithmetic/divide.test` `DivideDECIMAL` is `-SKIP # decimal division drops to double division for now`;
  runner FAILs it (`-0.995` vs `-0.995025`). Underlying decimal-division-precision is *known-unimplemented in C++ too* (hence its skip).
- **`${DEFINE}` variable substitution** in .test files not supported: `CREATE NODE TABLE person1(${COLS} …)` → `$` parse error. (function call CallNodeTableWith300ColumnsInfo.)
- **Runner .test parser rejects some files** (counted separately, not in fail totals):
  `error(regex)` result type (function fsm_info, exceptions invalid_utf8/insert_delete),
  statement/result-block count mismatches (function string_empty, exceptions exception/ignore_invalid_row, common comment — trailing `//` on a statement line).

---

## 6. Category (e) — dataset / file-format gated

- **Parquet not supported** ("Cannot load from file type parquet … load the extension"): exceptions
  ParquetHeaderMismatch, NodeUnmatchedNumColumns, RelUnmatchedNumColumns.
- **Buffer-manager OOM test** can't trigger in in-memory mode (expected `Buffer manager exception: Unable to allocate memory!`,
  statement just succeeds): agg hash_leak LargeAggregateLeakTest.
- (cast_bounds / property_cast / type_alias SKIPs are `EMPTY` dataset unavailability — already reported as SKIP, not FAIL.)

---

## 7. Focus: the `exceptions` dir (why 53 of 58 fail)

**It is NOT one systemic cause — it is a mix, but with one dominant theme: error-path fidelity.**
The dir exists to assert exact error strings on rejected input. Breakdown of the 53:

- **~25 (c) wording/class/format** — the single biggest bucket. Two mechanical systemic sub-causes:
  (i) Rust throws a **generic `Runtime`/`Conversion` exception where C++ throws a typed `Copy exception:`**;
  (ii) Rust parser/binder errors **lack the `(line:offset)` + query-echo + caret** decoration and the
  **file/line/record context** that C++ copy errors carry. Fixing these two rendering conventions would flip a large fraction.
- **~14 (a) unimplemented COPY features** — `IGNORE_ERRORS`, `COPY FROM [list]`, `COPY FROM (subquery)`,
  npy `BY COLUMN`, plus `STARTS WITH` in projection. These fail *before* reaching the error path under test.
- **~6 (b) genuinely missing validations / wrong behavior** — B7 (make_date range), B8 (MATCH-CREATE),
  B10 (copy message), and unimplemented error paths B16 (recursive-rel projection, path PROPERTIES literal arg,
  header-row-as-data detection, NULL/duplicate PK on some copy paths).
- **~3 (e) parquet**, **4 (d) harness .test-parse failures**.

So: two mechanical fixes (typed Copy-exception class + parser-error location/context formatting) address
roughly half; the rest need the COPY feature work (a) and a handful of validation/error-path implementations (b).

---

## 8. Quick wins vs deep work (for triage)

- **Highest-value, small (b) fixes:** B1 float→int rounding, B5 FLOOR/CEIL decimal scale, B6 SUM INT64→wide
  accumulator, B2 `labels()`→scalar, B7 make_date range check, B10 COPY message table name. Each is localized and
  each unblocks real result-correctness (and several unblock multiple tests).
- **Biggest single lever (a):** implement **`STARTS WITH`/`ENDS WITH`/`CONTAINS`** operators and the
  **implicit-arg→STRING coercion** (unblocks all 16 cast implicit-string tests + string/where usage broadly).
- **Biggest lever for `exceptions`:** typed `Copy exception:` class + parser-error `(line:offset)`/caret formatting.
- **Harness:** implement `-CHECK_COLUMN_NAMES`, honor `-SKIP # comment`, and `${DEFINE}` substitution — these are
  false failures today.
