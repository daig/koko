# A2 — Core Read-Query Corpus Audit (tck + read-query dirs)

Date: 2026-07-01. Oracle = C++ Koko shell; Rust = `target/release/{koko-test,examples/koko_cli}`.
Categories: **(a)** missing feature · **(b)** WRONG RESULT (accepted input, semantically different output) ·
**(c)** error-wording only · **(d)** harness limitation · **(e)** dataset/file-format gated ·
**(f)** known-accepted internal-`_ID` divergence.
All probe files under `scratchpad/probes/`. CONFIRMED = reproduced on both engines; SUSPECTED = inferred.

---

## TOP FINDINGS (most important first)

1. **CONFIRMED (a) — Major parser gap: `IN`, `STARTS WITH`, `ENDS WITH`, `CONTAINS`, `=~` are entirely
   unparsed as operators.** `parse_comparison` (crates/koko-parser/src/parser.rs:1477) handles only
   `= <> < <= > >=`. Even `RETURN 1 IN [0,1]` → `Parser exception: expected Eof but found Ident("IN")`.
   The underlying functions (`list_contains`, `starts_with`, `contains`, `ends_with`) all WORK — so this
   is a **parser-only** gap (desugar operator → function). It is masked in tck (the List5-`IN` / String
   scenarios are `-SKIP`) but surfaces in issue/ (5 fails). This is the single highest-impact defect.

2. **CONFIRMED (b) — Nested list/map string rendering diverges (byte-identical contract broken).**
   A STRING element of a LIST or MAP nested *inside a STRUCT/MAP* is single-quoted by Rust; C++ uses
   double-quotes for typed strings and NO quotes for literal/ANY strings. Scalars-in-struct and
   top-level lists render identically — only nested list/map string *elements* diverge.
   - `MATCH (a:organisation) WHERE a.ID=4 RETURN a.state`
     C++ `... location: ["vanco,uver north area"] ...` vs Rust `... location: ['vanco,uver north area'] ...`
   - `RETURN {loc: ['a,b','c']}`  C++ `{loc: [a,b,c]}` vs Rust `{loc: ['a,b','c']}`
   - `RETURN {m: map(['k'],['v,w'])}`  C++ `{m: {k=v,w}}` vs Rust `{m: {'k'='v,w'}}`
   Hits projection/{multi_label,single_label} `RETURN *`. Probe: `probes/render_rule.probe`.

3. **CONFIRMED (b) — Correlated `EXISTS { }` inside an `OPTIONAL MATCH ... WHERE` returns empty.**
   The correlation to the OPTIONAL-bound variable is dropped; a plain `MATCH` with the identical EXISTS
   works. Isolated minimal repro (`probes/`—inline):
   `OPTIONAL MATCH (v1:V)-[:links_to*1..]->(:V) WHERE EXISTS {MATCH (v1)-[:parent]->(:V {id: 2})} RETURN v1.id`
   → C++ `1`, Rust `` (empty). Change `OPTIONAL MATCH`→`MATCH`: both `1`. Hits issue.4080.

4. **CONFIRMED (a) — A node/rel variable passed to a scalar function is a bare `INTERNAL_ID`, not a
   materialized value.** `struct_extract(e, "_src")` on a rel `e` (directed **or** undirected) →
   `Runtime exception: struct_extract expects a STRUCT, MAP, NODE, or REL, got INTERNAL_ID`; C++ returns
   the source id. `id(e)` works (wants the bare id). Same family as the doc's "collect(node/rel)→property"
   gap, but for direct scalar-fn args. Hits match/undirected and ldbc/basic.

5. **VERIFIED tck triage — the doc's headline is empirically wrong for the *current* fail list.**
   Of 37 real fails: **_ID divergences = 3 (8%), NOT "mostly"**; Class-B reject-valid = 12; SKIP/LIMIT
   feature = 11; pure error-wording = 10; Kùzu degenerate-path quirk = 1. Detail + Class-B list below.

6. **Class C "~40 too-permissive" is NOT in the read-query surface.** Zero too-permissive cases in tck;
   exactly ONE in the whole surface (hint.Hint). The ~40 live in write/copy/validation dirs
   (copy/exceptions/function/ddl/common/agg/dml_rel/ice_disk) — outside this audit.

7. **graph/ (0p/9f) needs the entire named-graph subsystem** — `CREATE GRAPH`/`USE GRAPH`/`DROP GRAPH`
   with per-graph catalog+storage isolation. C++ prints "Created graph successfully."; Rust can't even
   parse `CREATE GRAPH` ("expected LParen but found Ident(GRAPH)").

8. **SKIP/LIMIT expression handling** is a ~13-case cluster (11 tck + 2 projection): we reject *any*
   non-literal-integer bound at bind ("requires a constant integer in this phase"); C++ constant-folds
   arbitrary expressions and does runtime range validation.

---

## VERIFIED tck triage (260p / 202s / 45f)

`45 failed` = **37 real FAILs + 8 harness parse-errors** (missing `----` block). Cross-checked by fail
shape: 8 "expected success,got error" + 20 "expected error" + 5 "expected rows,got error" + 4 "row
mismatch" = 37. Classification:

| Bucket | Count | Cases | Category |
|---|---|---|---|
| Zero/unlabeled-node in `CREATE` | 8 | Aggregation1.S1, Aggregation8.S2, match3.S6/S21/S22/S25, match4.S2/S3 | **Class B (a)** |
| `^` power operator | 4 | Precedence2.S2/S3/S4, return2.S1 | **Class B (a)** |
| SKIP/LIMIT expression handling | 11 | return_skip_limit1.S5/S7/S9/S10/S11, return_skip_limit2.S6/S9/S12/S13/S16/S17 | **(a)+(c)** |
| Error-wording only | 10 | List11.S4/S5, Map1.S6, Mathematical3.S1, match1.S6, match2.S8, match4.S9&10, return6.S14, return7.S2, return_orderby2.S14 | **(c)** |
| Internal-`_ID` divergence | 3 | match2.S6, match6.S19, match6.S20 | **(f)** |
| Kùzu degenerate-path quirk | 1 | match7.S19 | **(b) / quirk** |
| Harness: missing `----` block | 8 | Aggregation6, Boolean4, Comparison1/2, List6, TypeConversion1/2, match_where1 | **(d)** |

**Verdict on the doc claim** ("mostly `_ID` + ~23 Class B + ~40 Class C"):
- **"mostly _ID": FALSE.** 3/37. The doc's own baseline (227/202/78) has since improved to 260/202/45;
  in the current list `_ID` is the *smallest* semantic bucket.
- **"~23 Class B": OVERSTATED → actually 12** (8 zero-label CREATE + 4 `^`), or 13 counting the
  const-expr LIMIT (skip_limit2.S6). Two doc Class-B items are **stale/wrong**: (i) *map property access*
  `m.x` / `m.x.y` now **works** (verified `WITH {a:{b:2}} AS m RETURN m.a.b` → 2 on both); (ii) *ORDER BY
  over an aggregate* is actually **error-wording** — C++ *also* rejects `RETURN n.num1 ORDER BY max(n.num2)`
  (return_orderby2.S14 expects an error), so it is (c), not reject-valid.
- **"~40 Class C": not represented here** — 0 too-permissive fails in tck.
- **Doc omits the two largest current buckets:** SKIP/LIMIT (11) and error-wording (10), plus the
  CONFIRMED `IN`/`STARTS WITH` parser gap (hidden in tck by `-SKIP`).

### Class B — precise reject-valid construct list (read surface)
1. **Unlabeled / zero-label node pattern in `CREATE`** — `CREATE ()`, `CREATE ({p})`, `CREATE (a {p})`,
   `CREATE ()-[:T]->()`, `CREATE (a)-[:T]->(b)`. C++ infers the sole node table (verified: `CREATE ()`
   then `MATCH (n) RETURN count(*)` = 1). Rust: `Binder exception: a node pattern must specify exactly
   one label in this phase`. (tck ×8, issue.2701.)
2. **`^` power operator** — `2^3`, `4 ^ 3 * 2 ^ 3`, `-3 ^ 2`. C++ → `^(CAST(a,DOUBLE),CAST(b,DOUBLE))`,
   result DOUBLE (`2^3`=`8.000000`). Rust: lexer `unexpected character '^'`. (tck ×4.)
3. **Constant-expression `LIMIT`/`SKIP`** — `LIMIT 1+1`, `LIMIT to_int64(ceil(1.7))`. C++ folds & accepts;
   Rust rejects. (tck skip_limit2.S6 + projection.ProjectionSkipLimit.)

(Note: multi-label `CREATE (a:X:Y)` is *not* Class B — C++ also rejects it; it is error-wording, see graph/.)

### The 202 tck SKIPs — all inherited `-SKIP` (C++ test files' own directive, not a Rust choice)
Every SKIP is `(-SKIP)` — Koko C++ marks these openCypher scenarios non-runnable (diverge from
strict TCK or unsupported). The Rust runner inherits them verbatim; they are **not Rust-port regressions**.
By theme (from 232 skipped scenario descriptions):

| Theme | ~n | What they test (C++ itself skips) |
|---|---|---|
| List ops / slice / `IN` edge cases | 47 | list lookup/slice by param, negative/invalid/exceeding range, `IN` type-mismatch matrix |
| Pattern/match (advanced) | 37 | named paths, var-length rels-as-lists, undirected self-loops, optional reverse-dir, "Fail when same var" negatives |
| Null / three-valued logic | 31 | Boolean commutative/associative/distributive on null, null-predicate precedence, missing-prop→null |
| Aggregation (edge) | 31 | max/min over mixed & list-typed values, collect() null-filtering, percentileDisc, agg-inside-expression |
| Numeric/arith edge | 18 | division/modulo/precision corner cases |
| Path | 16 | shortest/named-path corner cases |
| ORDER BY / SKIP / LIMIT by param | 15 | param-driven bounds, negative/float "should fail" |
| type/cast, map(dynamic), WITH-forward, string | ~18 | toInteger/toBoolean edge, dynamic map field access, forwarding rel/path vars, split() |

---

## PER-DIRECTORY CLASSIFICATION

### match/ (8p/1f)
| Case | Cat | Root cause |
|---|---|---|
| undirected.MatchUndirected (stmt#1) | **(a)** | `struct_extract(e,"_src")` — rel `e` is a bare INTERNAL_ID, not a REL value (directed too). CONFIRMED. |

### optional_match/ (2p/1f)
| Case | Cat | Root cause |
|---|---|---|
| optional_match.OptionalMatch (#2) | **(a)** | "a required MATCH after OPTIONAL MATCH is not supported in this phase" — known Wave-C gap (ordered-join model). |

### order_by/ (3p/1s/1f)
| Case | Cat | Root cause |
|---|---|---|
| order_by.test (load) | **(d)** | `invalid result count 'hash'` — runner can't parse `---- hash` result form. |
| order_by_parquet (SKIP) | **(e)** | dataset `CSV_TO_PARQUET(order-by-tests)` unavailable. |

### filter/ (13p/1f)
| Case | Cat | Root cause |
|---|---|---|
| node.ZoneMapUpdateThenQueryWithoutCheckpoint (#23) | **(a)** | `SET a.state={...,stock:{price:[1],volumn:1}}` — the SET-literal field is misspelled `volumn` but the column is `volume`; C++ coerces the STRUCT **positionally** (name mismatch tolerated), we require field-name match → "Implicit cast is not supported." (Doc: "STRUCT/ANY-field coercion" deferral.) |

### subquery/ (2p/2f)
| Case | Cat | Root cause |
|---|---|---|
| correlated.CorrelatedSubquery (#2) | **(a)** | subquery in `WITH … WHERE` — known Wave-C gap. |
| exists.ExistsSubquery (#1) | **(a)** | `WHERE (a)-[:knows]->(c:person)` — **pattern-expression predicate** in WHERE unparsed ("unexpected token Colon"). `EXISTS { }` works; the bare-pattern shorthand does not. CONFIRMED. |

### unwind/ (2p/1s/1f)
| Case | Cat | Root cause |
|---|---|---|
| unwind.Unwind (#2) | **(c)** | expected `Cannot set expression n with type VARIABLE. Expect node or rel pattern.`; got `Cannot set a property on n: it is not a node or relationship.` |
| unwind_mixed (SKIP) | **(d)** | `-DATASET EMPTY` (uppercase) skipped — runner only treats lowercase empty/none as the empty dataset. |

### projection/ (4p/6f)
| Case | Cat | Root cause |
|---|---|---|
| multi_label.ProjectionMultiLabel / single_label.ProjectionSingleLabel | **(b)** | Nested list-of-string quoting: `location: ['…']` vs C++ `["…"]`. CONFIRMED (finding #2). |
| single_label.LargeListOfStruct (#1) | **(d)** | uses runner `-DEFINE`d var `${STRUCT_VAL}`; unexpanded → `Parser exception: expected an identifier, found LBrace` (macro/`-DEFINE` substitution unsupported). |
| skip_limit.ProjectionSkipLimit (#6, `LIMIT 1+1`) | **(a)** | const-expr LIMIT — C++ folds to 2; we reject. CONFIRMED (finding #8). |
| skip_limit.LoadWithExpressionSkip (#1, `SKIP 4.1`) | **(a)+(c)** | C++ evals → Runtime "must be a non-negative integer"; we bind-reject "requires a constant integer". |
| escape.test (load) | **(d)** | `invalid result count 'hash'`. |

### recursive_join/ (10p/2s/1f)
| Case | Cat | Root cause |
|---|---|---|
| multi_label.VarLengthMultiLabel (#12) | **(c)** | expr rendered `STRUCT_EXTRACT(n,ID)` vs C++ `n.ID` inside the "depends on both n and r" error. Known-gap wording. |
| semantic_empty.TwoCycle/ThreeCycle (SKIP) | **(d)** | `-DATASET EMPTY` (uppercase) skip. |

### shortest_path/ (4p/1s/1f)
| Case | Cat | Root cause |
|---|---|---|
| all_shortest_path.test (load) | **(d)** | `invalid result count 'hash'`. |
| bfs_sssp_parquet (SKIP) | **(e)** | `CSV_TO_PARQUET(shortest-path-tests)` unavailable. |

### nested_types/ (6p/1s/1f)
| Case | Cat | Root cause |
|---|---|---|
| large_array.CopyLargeArray | **(e)** | dataset load needs a **parquet** reader — unimplemented. |
| nested_types_errors.SizeError (SKIP) | **(d)** | uses `-BATCH_STATEMENTS`. |

### user_defined_types/ (5p/1f)
| Case | Cat | Root cause |
|---|---|---|
| user_defined_types.UDTPrimitiveType (#8) | **(a)** SUSPECTED | `LOAD FROM "…udt/vMovies.csv" (header=true) RETURN … height …` → "Variable height is not in scope." Bare `LOAD FROM (header=true)` isn't naming columns from the header for the UDT-typed file (header/column-naming path for this form). |

### ldbc/ (2p/1s/1f)
| Case | Cat | Root cause |
|---|---|---|
| basic.LDBCBasic (#2) | **(a)** | `… struct_extract(rels(p)[1],'_src') …` on an undirected named path → empty vs `1`; same rel-not-materialized family as finding #4 (rel from `rels(p)` is a bare id). |
| interactive_short_parquet (SKIP) | **(e)** | `CSV_TO_PARQUET(ldbc-sf01)` unavailable. |

### graph/ (0p/9f)  — needs the named-graph subsystem
| Case | Cat | Root cause |
|---|---|---|
| any.CreateAnyGraphAndQuery, any.AnyGraphMultiNodeLabels, any_graph.CreateAnyGraph, graph.CreateGraphAndUseGraph, graph.VerifyCatalogIsolation, graph.ShowTablesWithIsolation, graph.ErrorCases, graph.DropGraph (8) | **(a)** | `CREATE GRAPH <name> [ANY]` / `USE GRAPH` / `DROP GRAPH` unparsed. Requires **per-graph catalog+storage isolation** (multiple StorageManagers). CONFIRMED C++ feature ("Created graph successfully."). |
| any.MainGraphCreateMultiNodeLabelsFails (#3) | **(c)** | expected `Create node a with multiple node labels is not supported.`; got `a node pattern must specify exactly one label in this phase` (both reject multi-label CREATE — wording only). |

### hint/ (0p/1f)
| Case | Cat | Root cause |
|---|---|---|
| hint.Hint (#1) | **(c) / too-permissive** | We parse-and-ignore HINT, so we accept a hint C++ rejects with `Hint join pattern has correlation with previous patterns…`. **The only Class-C case in the read surface.** Honoring the hint is a P4 planner feature. |

### issue/ (44p/3s/11f)
| Case | Cat | Root cause |
|---|---|---|
| issue.4080 (#6) | **(b)** | Correlated `EXISTS` in `OPTIONAL MATCH … WHERE` → empty vs `1`. CONFIRMED (finding #3). |
| issue.listContainsCast (#3) | **(a)** | `WHERE a.id IN [0]` — `IN` operator unparsed. CONFIRMED (finding #1). |
| issue.3653 (#4) | **(a)** | `WHERE e.p in ["foo","bar"]` — `IN` unparsed. |
| issue.2588 (#9) | **(a)** | `WHERE "aa" STARTS WITH "a"` — `STARTS WITH` unparsed. |
| issue7.FlatSelect (#6) | **(a)** | contains an `IN` predicate — unparsed. |
| issue7.issue4716 (#58) | **(a)** | contains `STARTS WITH` — unparsed. |
| issue.3488 (#14) | **(a)** | `WHERE NOT (n1)-[]->()` — pattern-expression predicate unparsed ("unexpected token Gt"). |
| issue.2589 (#4) | **(a)** | "carrying a node or relationship expression through WITH is not supported in this phase" — known Wave-C gap. |
| issue.2701 (#3) | **Class B (a)** | `CREATE ({id:1})-[:R]->({id:2})` — zero-label CREATE. |
| issue2.3416 (#1) | **(a)** | `Export Database '…'` — EXPORT DATABASE unsupported (bulk-I/O, P4). |
| issue.5676 (#4) | **(c)** | rel-CTAS with a deleting AS-query: expected `Copy exception: Unable to find primary key value 1.`; got `Runtime exception: Cannot create relationship: no person node with primary key 1.` |
| issue.2376 (SKIP) | — | inherited `-SKIP`. |
| issue3.3246, issue3.BUG (SKIP) | **(d)/(e)** | `-DATASET EMPTY` unavailable. |

### cypherlogic/ (1p) · generic_hash_join/ (1p)
All pass — no action.

---

## APPENDIX — parser gap detail (finding #1)

`crates/koko-parser/src/parser.rs` `parse_comparison` (≈L1477) matches only `Eq/Neq/Lt/Le/Gt/Ge`.
`IN` appears only inside list-comprehension parsing (L1664/1916); no `STARTS WITH`/`ENDS WITH`/
`CONTAINS`/`=~` anywhere. Functions exist and pass (`list_contains`, `starts_with`, `contains`,
`ends_with` all verified SAME vs C++). Fix is a parser desugar: insert a predicate tier between
`parse_not` and `parse_comparison` that recognizes these operators and emits the equivalent function /
`list_contains` / regex call. Affects ≥7 corpus cases directly and is under-counted because the tck
scenarios that would exercise it (List5-`IN`, String STARTS-WITH) are `-SKIP`.

## Probe inventory (scratchpad/probes/)
- `parser_in_starts.probe` — IN/STARTS/ENDS/CONTAINS all fail to parse (9/9 DIFF).
- `regex_and_funcs.probe` — `=~` fails; underlying functions all SAME.
- `render_rel_limit.probe` / `render_rule.probe` — nested-string quoting + rel struct_extract + SKIP/LIMIT.
- `pattern_pred.probe` — pattern-predicate-in-WHERE fails; `EXISTS { }` works.
- `map_access.probe` — `m.a`, `m.a.b`, `a.state.revenue` all SAME (doc Class-B "map access" is stale).
