# A8 — Clause & Pattern Semantics Differential Audit

Scope: MATCH/OPTIONAL MATCH/WITH/RETURN/UNION/UNWIND, subqueries, var-length rels,
writes (CREATE/MERGE/SET/DELETE), and misc (CALL/COMMENT/EXPLAIN/case/quoting).
Method: black-box differential via `diffprobe.py`; C++ shell (`koko`) is the oracle,
Rust (`koko_cli`) is the port. Rows sorted before compare unless `#ORDER#`.

Volume: **573 probe statements** across 27 probe files in `scratchpad/A8/*.probe`.
Setups hermetic (CREATE NODE/REL TABLE + CREATE) except `tinysnb1/tinysnb2` which use `--dataset tinysnb`.

Classification legend: MISSING FEATURE · CRASH · WRONG CARDINALITY · WRONG VALUE ·
RUST TOO PERMISSIVE · ERROR-WORDING · ID-DIVERGENCE(accepted).

---

## Executive summary

- **1 CRASH**: `RETURN properties(<node|rel>)` panics the Rust engine (aborts the whole session).
- **8 MISSING FEATURES**, one very high-impact: the `IN` list-membership operator is
  entirely unimplemented in the Rust parser, which blocks a large class of WHERE/RETURN
  predicates. Also missing: `STARTS WITH`/`ENDS WITH`/`CONTAINS`; `any/all/none/single`;
  nested subqueries; required-MATCH-after-OPTIONAL; EXPLAIN/PROFILE; most CALL table
  functions; constant-arithmetic LIMIT/SKIP.
- **3 semantic/cardinality divergences**: `ACYCLIC` var-length, `MERGE` on a relationship
  with multiple existing matches, aggregate in `WHERE`.
- **6 "Rust too permissive"** cases where Rust accepts/evaluates input the C++ oracle rejects
  (SET +=, CREATE sibling-prop ref, list comprehensions, `UNWIND <scalar>`, NULL-typed UNION
  column, `WALK` keyword).
- **1 WRONG VALUE**: `labels()` returns a scalar in C++, a list in Rust.
- Many ERROR-WORDING diffs (both reject, text differs) — listed at the end.
- **Large agreement surface** — see "What matches" at the bottom; most clause/pattern
  semantics are byte-identical.

No pure ID-DIVERGENCE diffs were surfaced (properties/counts were projected throughout;
the one `RETURN DISTINCT a` node-dump happened to align on `_ID` and matched).

---

## A. CRASH

### A1. `properties(node|rel)` panics the Rust engine  — CRASH
Minimal repro: `MATCH (a:N) RETURN properties(a)`
```
cpp : Error: Binder exception: Function PROPERTIES did not receive correct arguments:
      Actual:   (NODE)   Expected: (LIST,STRING) -> ANY
rust: thread 'main' panicked at crates/koko-function/src/scalarfn.rs:1178:32:
      index out of bounds: the len is 1 but the index is 1
```
- Also panics for `properties(<rel var>)`.
- The panic aborts the process, so every subsequent statement in the session yields no output.
- Note: `properties(expr).prop` (e.g. `properties(r).w`) fails earlier at bind time
  ("Cannot bind property … type ANY[]") and does NOT crash — only a bare evaluated
  `properties(node|rel)` crashes. (probe: `A8/path.probe`, `A8/nodefns.probe`)

---

## B. MISSING FEATURES (C++ supports; Rust errors)

### B1. `IN` list-membership operator — unimplemented in Rust parser  [HIGH IMPACT]
Every form fails with `Parser exception: expected Eof but found Ident("IN")`.
| probe | cpp | rust |
|---|---|---|
| `RETURN 1 IN [1,2,3] AS b` | `True` | Parser error at `IN` |
| `MATCH (a:P) WHERE a.id IN [1,2] RETURN a.nm` | `A \| B` | Parser error at `IN` |
| `WITH [1,2,3] AS lst RETURN 2 IN lst AS b` | `True` | Parser error at `IN` |
| `UNWIND [1,2,3,4] AS x WITH x WHERE x IN [2,3] RETURN x` | `2 \| 3` | Parser error at `IN` |
| `RETURN NOT 1 IN [2,3] AS b` | `True` | Parser error at `IN` |
Confirmed directly against `koko_cli` (both `IN` and lowercase `in`). Note `IN` *does*
parse inside list-comprehension syntax `[x IN list …]`, so this is specifically the
binary membership operator. (probes: `A8/in_op.probe`, `A8/where_compose.probe`)

### B2. `STARTS WITH` / `ENDS WITH` / `CONTAINS` — unimplemented in Rust parser
| probe | cpp | rust |
|---|---|---|
| `MATCH (a:P) WHERE a.nm STARTS WITH 'A' RETURN a.nm` | `Alice` | Parser error at `STARTS` |
| `… WHERE a.nm ENDS WITH 'e' …` | `Alice` | Parser error at `ENDS` |
| `… WHERE a.nm CONTAINS 'li' …` | `Alice` | Parser error at `CONTAINS` |
| `RETURN 'Alice' STARTS WITH 'A' AS b` | `True` | Parser error at `STARTS` |
Not WITH-specific; fails in plain WHERE and in RETURN. (probe: `A8/startswith.probe`)

### B3. `any() / all() / none() / single()` list predicates — unimplemented
| probe | cpp | rust |
|---|---|---|
| `RETURN any(x IN [1,2,3] WHERE x > 2) AS b` | `True` | `Parser exception: expected RParen but found Ident("IN")` |
| `RETURN all(x IN [1,2,3] WHERE x > 0) AS b` | `True` | same |
| `RETURN none(x IN [1,2,3] WHERE x > 5) AS b` | `True` | same |
| `RETURN single(x IN [1,2,3] WHERE x = 2) AS b` | `True` | same |
(probe: `A8/in_op.probe`)

### B4. Nested subqueries — "not supported in this phase"
```
MATCH (a:P) WHERE EXISTS { MATCH (a)-[:K]->(b) WHERE EXISTS { MATCH (b)-[:K]->() } } RETURN a.nm
cpp : A
rust: Error: Not implemented exception: nested subqueries are not supported in this phase
```
(probe: `A8/subquery.probe`)

### B5. Required MATCH after OPTIONAL MATCH — "not supported in this phase" (documented)
```
MATCH (a:P) OPTIONAL MATCH (a)-[:K]->(b) MATCH (b)-[:K]->(c) RETURN a.nm, c.nm
cpp : A|C
rust: Error: Not implemented exception: a required MATCH after OPTIONAL MATCH is not supported in this phase
```
Confirms the documented limitation. (probe: `A8/optional.probe`)

### B6. `EXPLAIN` / `PROFILE` — rejected at parse in Rust
```
EXPLAIN MATCH (a:P) RETURN a.nm
cpp : <physical plan box drawing>
rust: Error: Parser exception: expected a MATCH, CREATE, or RETURN clause but found Ident("EXPLAIN")
PROFILE MATCH (a:P) RETURN a.nm   -> same (found Ident("PROFILE"))
```
(Plan output would legitimately diverge anyway; the point is Rust rejects the keywords.) (probe: `A8/misc.probe`)

### B7. Most CALL table functions — "not supported in this phase"
Only `show_tables()` and `table_info()` are implemented (both match C++ exactly, incl. with
WHERE / RETURN / COUNT / ORDER BY). Unimplemented in Rust (all work in C++):
`db_version`, `current_setting`, `show_functions`, `show_connection`, `storage_info`,
`show_indexes`, `show_attached_databases`, `show_warnings`.
```
CALL db_version() RETURN 1 AS ok
cpp : 1
rust: Error: Not implemented exception: CALL db_version(...) table functions are not supported in this phase
```
`CALL current_setting('threads') RETURN 1` fails differently in Rust:
`Parser exception: expected Eof but found Int(1)`.
Standalone config `CALL threads=4;` DOES work in both (empty output). (probe: `A8/callfns.probe`, `A8/misc.probe`)

### B8. LIMIT / SKIP with constant arithmetic — Rust too restrictive
C++ constant-folds arithmetic in LIMIT/SKIP; Rust only accepts a bare integer literal.
| probe | cpp | rust |
|---|---|---|
| `MATCH (a:P) RETURN a.nm LIMIT 1+1` | `A \| B` | `Error: Not implemented exception: LIMIT requires a constant integer in this phase` |
| `… LIMIT 2*2` | `A\|B\|C\|D` | same |
| `… SKIP 1+1` | `C \| D` | `… SKIP requires a constant integer in this phase` |
Truly non-const LIMIT (`LIMIT a.age`) and negative (`LIMIT -1`) are rejected by both — see
error-wording section. (probe: `A8/return.probe`)

---

## C. WRONG CARDINALITY / SEMANTICS

### C1. `ACYCLIC` var-length semantics diverge
Graph: 3-cycle 1→2→3→1 plus self-loop 1→1.
| probe | cpp | rust |
|---|---|---|
| `…-[e:E*1..3]->` (default) | `9` | `9` (match) |
| `…-[e:E* TRAIL 1..3]->` | `6` | `6` (match) |
| `…-[e:E* ACYCLIC 1..3]->` COUNT | **`9`** | **`2`** |
| `…-[e:E* ACYCLIC 1..3]-> b.nm,COUNT` | `a\|4 b\|3 c\|2` | `b\|1 c\|1` |
| tinysnb `(person)-[:knows* ACYCLIC 1..2]->(person)` | `50` | `38` |
C++ `ACYCLIC` returns **identical results to its default/WALK** (does not exclude
paths with repeated nodes). Rust `ACYCLIC` correctly excludes them (drops the self-loop
and the return-to-start). So Rust is arguably *more correct*, but it **diverges from the
oracle**. Classify WRONG CARDINALITY vs the contract; annotate that C++'s ACYCLIC looks
non-functional. (probes: `A8/walktrail.probe`, `A8/varlen.probe`, `A8/tinysnb2.probe`)

### C2. `MERGE` on a relationship with multiple existing matches
Two parallel edges (w:5, w:7) exist between id:1 and id:2.
| probe | cpp | rust |
|---|---|---|
| `MERGE (a)-[r:K]->(b) RETURN COUNT(*)` | **`2`** | **`1`** |
| `MERGE (a)-[r:K]->(b) ON MATCH SET r.w=100 RETURN COUNT(*)` | **`2`** | **`1`** |
| then `MATCH ()-[r:K]->() RETURN r.w` | `100 \| 100` | `7 \| 100` |
When the MERGE pattern already matches multiple relationships, C++ binds **all** of them
(MATCH-like) so ON MATCH updates every match; Rust binds only **one**. When MERGE must
create (`MERGE (a)-[r:K {w:999}]->(b)` — no existing match) both agree. (probe: `A8/merge_rel.probe`)

### C3. Aggregate in `WHERE` — C++ accepts, Rust rejects
```
MATCH (a:P) WHERE COUNT(*) > 1 RETURN a.nm
cpp : A \n B \n C          (COUNT(*) evaluated globally = 3 > 1, all rows pass)
rust: Error: Binder exception: aggregate function not allowed in this context
```
Rust is stricter (standard Cypher forbids aggregates in WHERE); C++ evaluates it. (probe: `A8/edge2.probe`)

---

## D. RUST TOO PERMISSIVE (Rust accepts; C++ oracle rejects)

### D1. `SET a += {map}` (map-merge) — Rust supports, C++ parser rejects
```
MATCH (a:P {id:1}) SET a += {age: 99, nm: 'AA'} RETURN a.nm, a.age
cpp : Error: Parser exception: Invalid input … SET a +  (rejects '+=')
rust: AA|99
```
(probe: `A8/writes.probe`)

### D2. CREATE inline reference to a sibling node's property
```
CREATE (a:P {id:3, nm:'C', age:40}), (b:P {id:4, nm: a.nm, age: a.age}) RETURN b.nm, b.age
cpp : Error: Cannot evaluate expression with type PROPERTY.
rust: C|40   (b created reading a's freshly-set properties)
```
The separate-clause form `CREATE (a {..}) CREATE (b {nm: a.nm})` works in **both** (→`E`),
and plain expressions in CREATE props (`'x'+'y'`, `1+1`) work in both. Only referencing a
**sibling within the same comma-separated CREATE** diverges. (probe: `A8/create_order.probe`)

### D3. List comprehensions — Rust supports, C++ (this build) rejects
| probe | cpp | rust |
|---|---|---|
| `RETURN [x IN [1,2,3] WHERE x > 1 \| x]` | Parser error at `WHERE` | `[2,3]` |
| `RETURN [x IN [1,2,3] \| x*2]` | Binder: `Variable x is not in scope.` | `[2,4,6]` |
| `RETURN size([x IN [1,2,3] WHERE x > 1])` | Parser error | `2` |
(probe: `A8/in_op.probe`)

### D4. `UNWIND <scalar>` — Rust silently yields 0 rows, C++ errors
```
UNWIND 5 AS x RETURN x
cpp : Error: Binder exception: 5 has data type INT64 but LIST was expected.
rust: <empty>   (no rows, no error)
```
(probe: `A8/unwind.probe`)

### D5. NULL-typed column in UNION — Rust accepts, C++ errors
```
RETURN NULL AS x UNION RETURN 1 AS x
cpp : Error: Binder exception: x has data type INT64 but ANY was expected.
rust:   \n 1      (two rows: NULL and 1)
```
(probe: `A8/union.probe`)

### D6. `WALK` var-length keyword — Rust accepts, C++ parser rejects
```
MATCH (a:N {id:1})-[e:E* WALK 1..3]->(b) RETURN COUNT(*)
cpp : Parser exception: Invalid input … E* WALK
rust: 9
```
(C++ accepts `TRAIL`, `ACYCLIC`, `SHORTEST`, `ALL SHORTEST` but not `WALK`.) (probe: `A8/walktrail.probe`)

---

## E. WRONG VALUE

### E1. `labels()` returns scalar in C++, list in Rust
```
MATCH (a:N {id:1}) RETURN labels(a) AS l
cpp : N       (scalar STRING)
rust: [N]     (STRING[] list)
```
`label(a)` (singular) returns `N` in both. (probe: `A8/nodefns.probe`)

---

## F. ERROR-WORDING (both reject; text differs — affects byte-identical error contract)

| # | probe | cpp | rust |
|---|---|---|---|
| F1 | `CREATE (n:P:Q {…})` multi-label | `Create node n with multiple node labels is not supported.` | `a node pattern must specify exactly one label in this phase` |
| F2 | `RETURN a.nm ORDER BY COUNT(*)` | `Cannot evaluate expression with type AGGREGATE_FUNCTION.` | `aggregate function not allowed in this context` |
| F2b| `RETURN a.age, COUNT(*) AS c ORDER BY COUNT(*) DESC` | `Cannot evaluate expression with type AGGREGATE_FUNCTION.` | `Variable COUNT(*) is not in scope.` |
| F3 | `RETURN a.nm SKIP 2 ORDER BY a.nm` (ORDER after SKIP) | `Invalid input < ORDER>: expected rule iC_Statements …` | `expected Eof but found Ident("ORDER")` |
| F4 | `RETURN a.nm LIMIT -1` / `SKIP -1` | `Runtime exception: The number of rows to skip/limit must be a non-negative integer.` | `Not implemented exception: LIMIT/SKIP requires a constant integer in this phase` |
| F5 | `RETURN a.nm LIMIT a.age` (non-const) | `Binder exception: The number of rows to skip/limit must be a parameter/literal expression.` | `LIMIT requires a constant integer in this phase` |
| F6 | `RETURN *` (no vars) | `RETURN or WITH * is not allowed when there are no variables in scope.` | `RETURN * requires at least one bound variable` |
| F7 | `RETURN SUM(COUNT(*))` nested agg | `Expression s contains nested aggregation.` | `aggregates cannot be nested` |
| F8 | `…-[:E*-1..2]->` negative bound | `Parser exception: Invalid input …E*-` | `expected RBracket but found Minus` |
| F9 | `MATCH (n) WHERE n:P …` label predicate | `Invalid input <… WHERE n:>` | `expected Eof but found Colon` |
| F10| `EXISTS { (a)-[:K]->() }` / `COUNT { (a)… }` (no MATCH kw) | `Invalid input < ( >: expected oC_SingleQuery` | `expected keyword MATCH but found LParen` |
| F11| `EXISTS { MATCH (a)-[:K]->(b) RETURN b }` (RETURN in subq) | `Invalid input … RETURN` | `expected RBrace but found Ident("RETURN")` |
| F12| `CALL cast('5','INT64')` (scalar as table fn) | `CAST is not a table or algorithm function.` | `CALL cast(...) … not supported in this phase` |
| F13| `CALL show_connection('P')` (P is node table) | `Show connection can only be called on a rel table!` | `CALL show_connection(...) … not supported in this phase` |
| F14| "did not receive correct arguments" family (SIZE/ALL/PROPERTIES on wrong type) | trailing ` \n \n ` blank-line padding | Rust trims trailing blank lines |

F14 is a **systematic** formatting difference: C++ multi-line binder errors of the
"did not receive correct arguments" family end with blank/space-padded lines; the Rust
equivalents omit them. Otherwise the argument tables match.

---

## G. CLI / input-handling (koko_cli example)

### G1. Multiple `;`-separated statements on one line
```
input:  RETURN 1 AS x; RETURN 2 AS y;
cpp :   x=1 , y=2    (both execute)
rust:   Error: Parser exception: expected Eof but found Ident("RETURN")
```
The Rust `koko_cli` example reads line-by-line and does not split a line on `;`.
Semicolonless *single* statements work in both. (Lower priority — REPL/harness detail,
not core engine semantics.) (direct CLI test)

---

## What matches (high-agreement surface — spot list)

All of the following produced **identical** output on both engines:

- **MATCH cardinality**: self-loops `(a)-[r]->(a)`; relationship-uniqueness within one
  MATCH `(a)-[r1]->(b)<-[r2]-(a)` (excludes r1=r2); reusing the same rel var twice →
  identical "Bind relationship … same name is not supported" error; undirected
  double-counting `(a)-[r]-(b)`; disconnected patterns cross product (3×3, 3×3×3);
  chained MATCH cross product; repeated whole-var MATCH; `*0..0` zero-length.
- **multi-type / label patterns**: `-[:K|L]->`, `-[r]->` (any), anonymous `()-[:K]->()`,
  `(a:P|Q)` label disjunction, `--`/`-->`/`<--`, property-in-pattern vs WHERE, shared-var
  multi-pattern. (only `WHERE n:P` label-predicate rejected by both, see F9)
- **OPTIONAL MATCH**: null propagation, `COUNT(b)` vs `COUNT(*)` over optional nulls,
  WHERE inside optional vs after (via WITH), chained optionals, `COLLECT`/`SUM`/`COUNT
  DISTINCT` over nulls, `b IS NULL` filters, all-null rows.
- **WITH**: scoping & out-of-scope errors ("Variable a is not in scope." identical),
  `WITH *`, `WITH *, expr`, alias collisions (`x AS x` dup accepted identically),
  implicit grouping keys, `DISTINCT`, ORDER BY+SKIP/LIMIT then further MATCH, WITH+WHERE.
- **RETURN**: DISTINCT over scalars/composites/nodes, ORDER BY by alias / by ordinal
  (`ORDER BY 2`) / by expression (`a.age+5`), `LIMIT 0`, `SKIP` past end, `RETURN *`
  (with vars).
- **UNION/UNION ALL**: dedup semantics, int↔float↔double coercion, list coercion, DATE,
  chained unions, mixed UNION/UNION ALL, ORDER BY in a branch, column type unification.
- **UNWIND**: empty list (0 rows), NULL (0 rows), list with NULLs, nested lists,
  `range()` incl. step and descending, cartesian double-UNWIND, DISTINCT, re-UNWIND of a
  COLLECT.
- **subqueries**: `EXISTS { MATCH … }`, `NOT EXISTS`, `COUNT { … }` in RETURN & WHERE,
  correlated (outer var), `EXISTS` in RETURN as boolean, combined with AND/OR (when no
  `IN`). (nested EXISTS unsupported — B4)
- **var-length**: default/TRAIL/SHORTEST/ALL SHORTEST counts & length distributions,
  `*0..k`, `*..k`, `*k..`, `*` (caps at 30), undirected var-length, per-step WHERE via
  outer WHERE, bounds validation errors (lower>upper: "Lower bound … greater than
  upperBound."; exceeds 30: "Upper bound … exceeds maximum: 30."), `length(p)`,
  `nodes(p)`/`rels(p)` sizes, `[n IN nodes(p) | n.nm]`, `COUNT(DISTINCT length(p))`.
- **writes**: `SET prop=val`, `SET prop=expr`, `SET prop=NULL` (clears, rendered empty),
  `SET a={map}` PK-in-map rejection (identical text), MERGE node match-or-create,
  ON CREATE/ON MATCH (incl. combined), MERGE rel create path, CREATE inline chained
  (`CREATE (x)-[:K]->(y)-[:K]->(z)`), CREATE path with RETURN, write-then-read
  (`CREATE … WITH … MATCH`), duplicate-PK error (identical), DELETE + re-MATCH,
  DETACH DELETE, delete-already-deleted (idempotent), multi-label CREATE rejected by both
  (F1 wording).
- **misc**: case-insensitivity of keywords (`match`/`MaTcH`), function names
  (`count`/`CoUnT`, `upper`/`UPPER`/`uPpEr`, `Size`), labels (`a:p` matches `P`);
  backtick-quoted identifiers with spaces and Unicode (`` `my col` ``, `` `Ünïcödé` ``);
  `COMMENT ON TABLE … IS …` ("Comment added to table P."); `show_tables()`/`table_info()`
  with WHERE/RETURN/COUNT/ORDER BY; standalone `CALL threads=4`; semicolonless single stmt;
  `label()`, `id()`, `offset(id())`; `type()`/`startNode()`/`endNode()` absent in both
  ("function … does not exist").

---

## Probe file index (scratchpad/A8/)
match_card, patterns2, optional, optional2, with, return, union, union2, unwind,
subquery, where_compose, in_op, startswith, varlen, varlen2, walktrail, path, nodefns,
writes, writes2, create_order, merge_rel, misc, callfns, edge2, final, tinysnb1, tinysnb2.
