# A3 — Write-path corpus + demo_db panic audit

Scope: (1) root-cause the `demo_db/` panic and scan the whole corpus for other
panics; (2) classify every failure in `transaction/`, `dml_node/`, `dml_rel/`,
`ddl/`, `rel_group/`.

Legend for Part 2 categories: **(a)** missing feature · **(b)** WRONG RESULT ·
**(c)** error-wording only · **(d)** harness limitation · **(e)** dataset/file-format gated.
"CONFIRMED" = reproduced against the C++ oracle; "SUSPECTED" = classified from
the error text / test source without a full oracle diff.

---

# PART 1 — the demo_db panic (CONFIRMED, headline deviation)

## Root cause (one line)
An `EXISTS { … }` / `COUNT { … }` subquery placed inside a **recursive-rel
lambda filter** `-[e:Follows* (r, n | WHERE … )]->` is lowered by the planner to
a reference to a lifted per-row *marker column*, but the recursive DFS/BFS
evaluates that predicate against an **empty** `DataChunk`, so the column index is
out of bounds → process abort.

## Panic
```
thread 'main' panicked at crates/koko-expr/src/lib.rs:177:56:
index out of bounds: the len is 0 but the index is 7
```
Backtrace (abbrev): `eval_with_aggs → dfs_all → enumerate_paths → stream_expand`.

## Offending test / statement
- File: `/Users/dai/code/koko/test/test_files/demo_db/demo_db.test`
  (only `demo_db.test` in that dir panics; the runner returns rc=101).
- Case `DemoDBTest`, log `RecursiveSubqueryPredicate` (line 30):
  ```cypher
  MATCH (a:User {name: "Adam"})-[e:Follows* (r, n | WHERE EXISTS {MATCH (n)-[:LivesIn]->(:City {name: "Waterloo"})})]->(b:User)
  RETURN properties(nodes(e), 'name'), b.name;
  ```

## Minimal repro
```cypher
MATCH (a:User)-[e:Follows* (r, n | WHERE EXISTS {MATCH (:City)})]->(b:User) RETURN COUNT(*);
```
(dataset `demo-db/csv`). Panics identically. Also reproduced with:
- `COUNT { … } > 0` in place of `EXISTS { … }` — panics.
- `SHORTEST 1..4` recursive mode — panics (BFS path `enumerate_shortest`).
- The subquery need **not** reference the lambda vars `r`/`n`; `EXISTS {MATCH (:City)}` alone panics.
- Control: a plain predicate `(r, n | WHERE n.age > 30)` works fine (returns 8) — only the subquery form crashes.

## C++ oracle (works, gives correct rows)
Full query in the C++ shell returns the 3 rows the .test expects (order aside):
```
[Karissa]|Zhang
[]|Zhang
[]|Karissa
```
`EXISTS {MATCH (:City)}` variant → `COUNT(*) = 8`. So this is a crash on valid
input that C++ evaluates correctly (it re-evaluates the correlated subquery per
intermediate node during expansion).

## Exact mechanism (code trail)
1. Planner lifts every `EXISTS/COUNT {}` in a part into a per-row result column:
   `crates/koko-planner/src/lib.rs:704-738` — `result_col = layout.alloc(ty)`,
   pushed to `layout.subquery_cols`, computed by a separate `PlanOp::Subquery`
   (or a decorrelated Mark hash-join).
2. `BoundExpr::Subquery` therefore compiles to a plain column read:
   `crates/koko-expr/src/lib.rs:399`
   → `CompiledExpr::Column(resolver.subquery_column(*id)?)` (here index `7`).
3. The recursive rel's `(r,n | WHERE …)` predicate is compiled independently via
   `build_recursive_filter` (`crates/koko-processor/src/lib.rs:1336-1357`), which
   calls the same `compile()`, so the embedded subquery also becomes `Column(7)`.
4. At expansion time the predicate is checked against an **empty** chunk:
   `crates/koko-processor/src/lib.rs:1954-1962`
   ```rust
   fn check(pred: &Option<CompiledExpr>, binds: &[(String, Value)]) -> bool {
       let dummy = DataChunk::new(&[]);            // <-- 0 columns
       matches!(ce.eval_with_bindings(&dummy, 0, binds), Ok(v) if v.as_bool() == Some(true))
   }
   ```
   Only the `(r,n)` lambda values are supplied (via the lambda stack); the lifted
   subquery column is not present. `eval_with_aggs` then hits
   `crates/koko-expr/src/lib.rs:177` → `chunk.columns[7]` on a 0-column chunk → panic.

## Fix direction (informational — no code changed)
The recursive-filter path has no facility to evaluate a subquery per candidate
edge. Two layers are wrong: (i) subqueries inside a recursive lambda filter must
**not** be lifted into an outer marker column (they are correlated to the
per-step `n`), and (ii) `dfs_all`/`enumerate_shortest` need to actually run the
subquery for each `(r,n)` rather than read a non-existent column. Minimum to stop
the crash: detect a subquery inside a recursive filter predicate and either
evaluate it inline against `(r,n)` bindings or reject it with a clear error
instead of aborting.

## Corpus-wide panic scan
Grepped all 54 corpus output files under `scratchpad/corpus/*.txt` for
`panicked`: **only `demo_db.txt` contains a panic.** No other panics in the
corpus. (rc column in `_progress.txt`: `demo_db rc=101`, everything else 0/1.)

---

# PART 2 — write-path corpus failures

## Cross-cutting root causes (each explains several fails)

### RC-1 — `LOAD FROM <csv>` misdetects a data first-row as a header (CONFIRMED, WRONG RESULT / data loss)
Rust's CSV auto-header-detection for `LOAD FROM` flags a numeric/data first row as
a header and drops it; C++ correctly treats a headerless CSV as all-data (columns
`column0, column1, …`).
```
LOAD FROM ".../tinysnb/eKnows.csv" RETURN COUNT(*);   -- C++ 6, Rust 5
LOAD FROM ".../eKnows.csv" RETURN column0, column1;   -- C++ ok; Rust: "Variable column0 is not in scope"
```
Explains **ddl.CreateRelTableAs** (6→5 rows, drops first edge `Alice|Bob`) and
**ddl.CreateNodeTableAsDuplicateIntIDs** (dup CSV `10,Guodong`/…/`10,Ziyi`; Rust
drops line 1 → duplicate gone → expected "duplicated primary key" error never
fires). Rust's PK-uniqueness enforcement itself works (`UNWIND [1,2,2] AS i
CREATE (:P{id:i})` errors correctly), so the missing error is purely a downstream
symptom of RC-1. This is a broad correctness risk for any headerless CSV whose
first row looks header-ish to Rust's heuristic.

### RC-2 — variable names are case-insensitive in C++, case-sensitive in Rust (CONFIRMED, WRONG RESULT, broad)
```
UNWIND [1,2] AS a RETURN A;
   C++ : 1, 2
   Rust: Error: Binder exception: Variable A is not in scope.
```
Explains **transaction/create_node.CreateNodeWithCasting** (`CREATE (it:…) RETURN
IT`). Affects any query that refers to a bound variable with different casing —
likely wider than this single corpus hit.

### RC-3 — `CALL storage_info(...)` table function not implemented (missing feature)
`Not implemented exception: CALL storage_info(...) table functions are not
supported in this phase`. Accounts for 11 fails across dml_node/dml_rel (stats +
create_int_max_width cases).

### RC-4 — FLOAT/DOUBLE/BLOB primary keys not supported (missing feature)
`Runtime exception: Unsupported primary key type in this phase`. Accounts for
CreateDoublePK / CreateFloatPK / CreateBlobPK / CreateManyDoubleAfterCheckpoint in
transaction + dml_node (7 fails).

---

## transaction/  (444p / 60s / 6f)

Fails (5 real + 1 harness):
| Case | Cat | Note |
|---|---|---|
| create_empty_checkpoint.CreateDoublePK | (a) | RC-4 FLOAT/DOUBLE/BLOB PKs |
| create_empty_checkpoint.CreateFloatPK | (a) | RC-4 |
| create_empty_checkpoint.CreateBlobPK | (a) | RC-4 |
| create_node.CreateNodeWithCasting | (b) CONFIRMED | RC-2 case-insensitive var `IT`→`it` |
| node_empty.DeleteNodeWithNestedType | (b) CONFIRMED | nested-STRUCT string quoting (see below) |
| copy/copy_node.test (whole file) | (d) | runner rejects `---- error(regex)` result annotation → file aborts, counts as the 6th fail |

**node_empty.DeleteNodeWithNestedType** — rendering deviation. A `STRING[]`
nested inside a `STRUCT` is rendered with **single quotes** by Rust and **no
quotes** by the current C++ oracle:
```
CREATE NODE TABLE org(ID INT64, state STRUCT(revenue INT16, location STRING[]), PRIMARY KEY(ID));
CREATE (:org {ID:1, state:{revenue:138, location:["toronto","montr,eal"]}});
MATCH (o:org) RETURN o.state;
   C++ : {revenue: 138, location: [toronto,montr,eal]}
   Rust: {revenue: 138, location: ['toronto','montr,eal']}
```
Top-level `STRING[]` matches both engines (no quotes); only the struct-nested
list diverges. Caveat: the .test *expected* file has quotes (single for most
rows, double for the 1-element `["vanco,uver north area"]` row), i.e. the stored
expected output disagrees with the current C++ build too — the nested-string
quoting rule appears version/path-dependent. Either way Rust matches neither.

Skip breakdown (60): `-BATCH_STATEMENTS` 24 (d) · `-SKIP` 11 (d/e) ·
`-SKIP_IN_MEM` 9 (e, in-memory run) · `-CREATE_DATASET_SCHEMA` 8 (d) ·
`-BEGIN_CONCURRENT_EXECUTION` 5 (d, concurrency) · `-LOOP` 3 (d).

## dml_node/  (121p / 5s / 19f)

Fails (18 real + 1 harness):
| Case(s) | Cat | Note |
|---|---|---|
| create_empty.CreateEmptyLabel | (c) | both error on `CREATE (a)`; wording differs ("empty node labels not supported" vs "must specify exactly one label in this phase") |
| create_empty.CreateDoublePK / CreateManyDoubleAfterCheckpoint / CreateFloatPK / CreateBlobPK | (a) | RC-4 |
| create_int_max_width.CreateInt8/16/32/64/128MaxWidth (5) | (a) | RC-3 storage_info |
| set_empty.SetLongString | (d) | `${STRING_EXCEEDS_PAGE}` harness placeholder not substituted → parser error |
| set_tinysnb.SetMultiLabelWithPruning / SetByDictMultiLabelWithPruning | (a) SUSPECTED | multi-label `SET b.orgCode` where `orgCode` exists only on a subset of `b`'s possible labels; Rust binder doesn't prune `b` by rel type → "Cannot find property orgCode for b" |
| set_tinysnb.OptionalSet / OptionalSetByDict | (c) CONFIRMED | `SET a.name, a.fName` and `SET a = {name, fName}` are invalid Cypher; **C++ also rejects** them (parser error) — only wording differs; the .test's `----1` annotation is malformed |
| stats.CreateIntegerStats / CreateDoubleStats / DeleteIntegerStats | (a) | RC-3 storage_info |
| delete/delete_empty.test (whole file) | (d) | runner: statement `… CALL fsm_info() RETURN …` "missing a `----` result block" → file aborts, counts as the 19th fail |

Skips (5): `-LOOP` 3 (d, create_random_int ×2 + set_empty.RandUpdateInt) ·
`-SKIP_IN_MEM` 2 (e, stats.Update*).

## dml_rel/  (40p / 9s / 8f)

Fails (8):
| Case(s) | Cat | Note |
|---|---|---|
| copy.CopyRelSetStorageDirection | (a) SUSPECTED | `create rel table … WITH (storage_direction='fwd')` single-direction storage not implemented; Rust always stores BOTH, so the expected `Runtime exception: Failed to get bwd data … set storage direction to BOTH` never fires |
| many_to_many_stats.CreateIntegerStats / CreateDoubleStats / DeleteIntegerStats | (a) | RC-3 storage_info |
| stats.CreateIntegerStats / CreateDoubleStats / DeleteIntegerStats | (a) | RC-3 storage_info |
| set_empty.SetRelStringColumnHighDuplication | (a) | `COPY nodes FROM (UNWIND range(1,200000) AS i RETURN i)` — `COPY <tbl> FROM (<subquery>)` not parsed ("expected a quoted file path after COPY … FROM, found LParen") |

Skips (9): `-LOOP` 1 (d) · dataset `EMPTY` unavailable 1 (e) · `-SKIP` 1 (d) ·
`-SKIP_IN_MEM` 6 (e).

## ddl/  (67p / 0s / 10f)

Fails (10):
| Case | Cat | Note |
|---|---|---|
| ddl.AddNodeProperty | (b) CONFIRMED | `CALL TABLE_INFO`'s **default-expression** column: C++ preserves literal text `5.4`, Rust re-formats the float as `5.400000`. (Plain FLOAT value rendering matches — `CAST(5.4 AS FLOAT)` = `5.400000` in both.) |
| ddl.CaseInsensitiveProperty | (c) | dup column-name error wording ("Duplicated column name: name, column name must be unique." vs "Duplicate column name name in table definition.") |
| ddl.CreateNodeTableAsDuplicateIntIDs | (b) CONFIRMED | downstream of **RC-1**: header-drop hides the duplicate PK, so expected copy error is missing |
| ddl.CreateNodeTableAsEmptyReturn | (c) | `RETURN *` with no vars — wording differs |
| ddl.CreateRelTableAs | (b) CONFIRMED | downstream of **RC-1**: 6→5 rows, drops first CSV edge `Alice|Bob` |
| ddl_empty.CreateHashIndexWithDefaultPKIndex / CreateHashIndex / CreateArtIndex / ArtIndexCopyFrom / ArtIndexUnsupportedCreate (5) | (a) | `CREATE [HASH|ART] INDEX …` DDL not parsed ("expected LParen but found Ident(\"INDEX/HASH/ART\")") |

No skips.

## rel_group/  (0p / 6s / 6f)

All 6 basic.test cases fail identically at **dataset load**:
`Binder exception: Unrecognized csv parsing option: FROM.`
Root cause (a) missing feature: the rel-group dataset's `copy.cypher` uses the
targeted rel-group copy option
```
COPY knows FROM "edge.csv" (from='personA', to='personB');
```
Rust's CSV-option parser doesn't accept `from`/`to` (used to select one FROM/TO
pair of a multi-pair rel group). Because the whole dataset can't load, every case
in the file is blocked — effectively (e) dataset-gated on an (a) feature gap.
Note: rel-group **DDL** itself works (ddl.CreateRelGroup / alter_rel_group pass);
only the targeted `COPY … (from=,to=)` is missing.

Skips (6): `-SKIP_IN_MEM` 2 (e) · `-SKIP` 1 (e) · dataset `EMPTY` unavailable 3 (e).

---

# Tally of real behavioral deviations (excluding harness/skip)

WRONG RESULT (b), CONFIRMED:
1. **PANIC** on subquery-in-recursive-filter (Part 1) — crash on valid input.
2. RC-2 case-insensitive variable names (Rust rejects differently-cased refs).
3. RC-1 `LOAD FROM` header misdetection → data loss (ddl.CreateRelTableAs) and a
   masked copy-error (ddl.CreateNodeTableAsDuplicateIntIDs).
4. Nested-STRUCT `STRING[]` element quoting (single quotes vs none).
5. `TABLE_INFO` default-expression FLOAT rendering (`5.400000` vs `5.4`).

Missing features (a): CALL storage_info; FLOAT/DOUBLE/BLOB PKs; CREATE
[HASH|ART]/INDEX DDL; rel-group `COPY … (from=,to=)`; single-direction rel
storage (`storage_direction`); `COPY <tbl> FROM (<subquery>)`; multi-label SET
label-pruning (SUSPECTED).

Error-wording only (c): CreateEmptyLabel, CaseInsensitiveProperty (dup col),
CreateNodeTableAsEmptyReturn, OptionalSet/OptionalSetByDict.

Harness limitations (d): `---- error(regex)` annotation (copy_node.test);
`CALL fsm_info()` statement without result block (delete_empty.test);
`${VAR}` placeholder substitution (SetLongString); `-BATCH_STATEMENTS`,
`-CREATE_DATASET_SCHEMA`, `-BEGIN_CONCURRENT_EXECUTION`, `-LOOP` (skips).
