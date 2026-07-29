# Main-session verified findings (running ledger)

## Baseline (2026-07-01, HEAD 2692cd1, C++ 0.17.0 @129e32a72)
- Full corpus: 1257 passed / 341 skipped / 379 failed (1977 cases) + demo_db dir crashes the runner.
- p0: 198p/16s/0f (skips = `mini` dataset path not set).
- Docs' own numbers stale in the conservative direction (tck actual 260p/45f vs doc 227p/78f; transaction 444p/6f vs 435p/15f).
- No upstream drift: C++ frozen 2026-05-28, port started 05-31.
- At scale: lsqb 9/9 correct; ldbc interactive_short + interactive_complex PASS; ldbc basic 1 fail (see #3).
- Perf (docs, 10b final): q4/q5/q7/q2 faster than C++; q1/q3/q6 ~2x slower; q8 5x; q9 4x.

## Confirmed behavioral deviations (I ran both engines)
1. PANIC family — wrong-arity scalar function calls panic (no bind-time arity validation):
   142 of 390 probes (195 fn names x 0/1-arg) kill the process. e.g. `RETURN sqrt()`, `RETURN left('abc')`,
   `RETURN properties(r)` (scalarfn.rs:1178/1187 args[i] OOB). C++: clean "Function X did not receive
   correct arguments" binder errors. Embedded-DB = host process death.
2. demo_db corpus panic: koko-expr/src/lib.rs:177 index OOB (len 0 idx 7) — A3 root-causing.
3. Bare-internal-id pipeline seam: struct_extract(r,'_src'|'_dst'|'_id') on a rel var errors
   "expects STRUCT.. got INTERNAL_ID"; on rels(p)[i]/nodes(p)[i] silently returns NULL (C++ returns the id).
   Root cause of the ldbc basic.test fail. Silent-NULL cases are the worst flavor.
4. nextval() lifted pre-WHERE: `MATCH (n) WHERE n.id=2 RETURN nextval('t')` -> rust 2 (currval 3),
   cpp 1 (currval 1). Sequence over-advances + shifted values.
5. length(p) on degenerate single-node path (unmatched OPTIONAL var-length): rust 0, cpp NULL.
   (Path VALUE {_NODES:[a],_RELS:[]} matches—the pre-audit claim "we return NULL" was stale.)
6. Non-const LIMIT (`LIMIT 1+1`): rust rejects "Not implemented ... constant integer in this phase"; cpp folds -> 2. (Class B confirmed.)
7. `^` power operator: rust Parser exception; cpp returns 8.000000 (DOUBLE). (Class B confirmed.)
8. EXPLAIN / PROFILE: rust Parser exception (deferred feature; cpp prints plan).
9. COPY duplicate-PK error class: rust `Runtime exception` vs cpp `Copy exception`; COPY done-message
   drops the table name ("copied to table." vs "copied to the person table.").
10. Too-permissive family (rust accepts, cpp Binder-errors): `MATCH (a:person) CREATE (a)`
    ("Cannot resolve any node or relationship to create."), recursive-rel projection item `{n}`,
    PROPERTIES() second-arg-must-be-literal. (exceptions dir; A1 quantifying.)
11. current_setting('x') in scalar position: both error, different wording (cpp: "CURRENT_SETTING is a
    TABLE_FUNCTION_ENTRY..."; rust: "Catalog exception: function CURRENT_SETTING does not exist.").

## p0-vs-C++ differential sweep (port's own green suite diffed against oracle: 18/48 files diff)
12. labels(n): rust `[City]` (LIST) vs cpp `City` (STRING) — Rust invented Neo4j-style list; Koko returns STRING.
    p0 fixture bakes the invented value.
13. SET += (map merge AND numeric prop +=): C++ PARSER-REJECTS all `+=` forms; Rust implements them.
    Invented Neo4j extension baked into p0 dml fixtures. (Verified directly; A9's parity table wrong here.)
14. SUM(INT64) result type: cpp widens to INT128 (sum of 2x ~2^63 = 18446744073709551614);
    rust keeps INT64 + throws invented "Overflow exception: SUM of INT64 values is not within INT64 range."
    C++ overload table: SUM(INT*)->INT128, SUM(UINT*)->UINT128, SUM(FLOAT)->DOUBLE. AVG(INT*)->DOUBLE.
15. SHOW_SEQUENCES 3rd column: cpp = static start value; rust = advanced/current state. Multiple rows diverge.
16. Nested EXISTS subquery: rust "Not implemented: nested subqueries"; cpp evaluates (0 rows).
17. UNION type unification with NULL: `RETURN 1 AS x UNION ALL RETURN null AS x` — cpp Binder error
    ("x has data type ANY but INT64 was expected"); rust succeeds (1, NULL). Too-permissive.
18. CALL show_tables() without RETURN: cpp errors ("Only standalone table functions can be called without
    return statement"); rust returns rows. show_macros() WHERE-filter: rust not-implemented, cpp works.
19. Error-wording family: CREATE-inline unknown property -> cpp "Cannot find property b for ." vs rust
    "Table T does not contain property b."; missing-PK CREATE -> cpp Binder "Create node  expects primary key
    id as input." vs rust Runtime "Null value found for primary key column." (class AND stage differ);
    ALTER ADD FROM..TO on node table: validation-order differs; DROP MACRO IF EXISTS missing -> cpp still
    ERRORS ("Catalog exception: Marco m1 doesn't exist." — upstream typo) vs rust graceful skip message.
    sum/avg type-mismatch errors: cpp dumps full overload table, rust one-liner.
20. Unbound $param: cpp = quirky (NULL-ish in expressions; WHERE with unbound param passes rows =
    likely upstream bug); rust = clean "Parameter X not found." Divergent-on-quirk, rust saner.
21. State-divergence artifacts in sweep to re-verify individually: factorization count(DISTINCT)=2-vs-1 and
    index_scan n.id=2.0 row-vs-empty (earlier stmts diverged); transaction show_tables id 3-vs-2 =
    documented hermetic rel-table-id divergence (accepted).

## Parser coverage (A9 report, headline items spot-verified by me)
22. MISSING EVERYDAY OPERATORS (hard parse errors in rust, work in cpp): `IN` (list membership),
    `STARTS WITH`, `ENDS WITH`, `CONTAINS`, `=~` regex, `^` power, `&`/`|` bitwise, `<<`/`>>` shifts,
    `!` factorial, quantifiers ANY/ALL/NONE/SINGLE(x IN l WHERE p), pattern-predicate expressions,
    positional params $1, WSHORTEST, CALL...YIELD, EXPLAIN/PROFILE, CREATE INDEX/GRAPH/USE/EXPORT/IMPORT/
    ATTACH/DETACH/extension mgmt, COPY TO / COPY FROM (subquery|list|glob|BY COLUMN),
    unicode identifiers, doubled-backtick escape.
23. REVERSE TRAP: Rust implements list comprehension [x IN l WHERE p | e] (C++ grammar has none);
    the no-WHERE form `[x IN l | e]` PARSES IN BOTH with different meanings (cpp: membership|bitwise-or ->
    usually binder error; rust: comprehension result). Same text, different semantics.

## Verified from agent reports (I re-ran each repro)
24. Float->int casts TRUNCATE instead of ROUND: to_int16(1.731) / CAST(1.731 AS INT32) -> rust 1, cpp 2.
    Silent wrong values across all float->int/uint casts. (A1)
25. SUM(UINT64) uses INT64 accumulator -> spurious overflow error; cpp 9223372036854782520.
    Same family as SUM(INT64)->INT128 widening (finding 14). (A1)
26. floor/ceil on DECIMAL keep input scale: rust -10.0, cpp -10 (scale reduced to 0). (A1)
27. make_date(2011,1,32): rust rolls over to 2011-02-01; cpp Conversion exception "Date out of range". (A1)
28. [3,4] > [3,NULL]: rust NULL, cpp False (list comparison with NULL element). (A1)
29. Nested list/map STRING elements inside STRUCT render single-quoted in rust; cpp renders bare
    (loaded data) e.g. {loc: ['van,couver']} vs {loc: [van,couver]}. (A2; also A3 DeleteNodeWithNestedType)
30. Correlated EXISTS in OPTIONAL MATCH ... WHERE loses the correlation -> 0 rows vs cpp 1. WRONG CARDINALITY.
    (A2, issue.4080; verified.)
31. demo_db PANIC root cause (A3): EXISTS{}/COUNT{} inside a recursive-rel lambda filter
    (-[e:F* (r,n | WHERE EXISTS {...})]->) -> planner lifts subquery to a column; per-step filter evaluates
    against an empty DataChunk -> index OOB panic (koko-expr:177). C++ answers correctly. Crash on VALID input.
32. Variable references are CASE-INSENSITIVE in C++ koko; case-sensitive in rust.
    UNWIND [1,2] AS a RETURN A -> cpp rows, rust "Variable A is not in scope." (A3 RC-2; verified.)
33. Bare LOAD FROM header misdetection DROPS the first data row: csv '1,foo/2,bar' -> rust returns only
    2|bar; cpp both rows. DATA LOSS on load. (A3 RC-1; verified.)
34. TABLE_INFO default-expression rendering: FLOAT default 5.4 renders "5.400000" vs cpp "5.4"
    (source text; documented as known-lossy in the then-current gap ledger).
35. tck re-triage (A2): of 45 fails, _ID divergences only 3 (docs said "most"); Class B = 12 (8 zero-label
    CREATE (), 4 ^); SKIP/LIMIT non-literal bind-reject = 11 (cpp constant-folds any expr incl.
    to_int64(ceil(1.7))); error-wording = 10; harness parse = 8; degenerate-path = 1. Docs' "m.x map access
    missing" = STALE (works now); "ORDER BY over aggregate" reclassified error-wording (cpp also rejects).
36. exceptions-dir taxonomy (A1): ~25 error-class/wording (missing `Copy exception:` type; parser errors lack
    (line: N, offset: M) + query echo + caret decoration); ~14 unimplemented COPY features; ~6 missing
    validations; 3 parquet-gated; 4 harness. NOT one systemic cause but error-path fidelity dominates.
37. Missing write-path features (A3): FLOAT/DOUBLE/BLOB PK types; CALL storage_info/fsm_info;
    CREATE HASH/ART INDEX DDL; COPY rel-group per-pair options (from=/to=) blocks all of rel_group corpus;
    single-direction rel storage (storage_direction); COPY FROM subquery.
38. Interval parsing gaps (A1): missing 'h' unit, 'quarter', fractional units.
39. UNION-type cast doesn't convert to target field type (123 vs 123.000000). (A1)
40. CALL threads=4.5 accepted (cpp validates INT). (A1)
41. Runner harness gaps confirmed by A1/A3: -CHECK_COLUMN_NAMES unimplemented (false fails);
    `-SKIP # comment` not honored; ${DEFINE} substitution unsupported; `---- error(regex)` unsupported (abort);
    a bare `CALL fsm_info()` in a .test aborts the file run.

## From A4 (bulk I/O) + my re-verifications
42. CSV nested-string quote retention REVERSED from the pre-audit docs: C++ KEEPS source quote
    chars as data ('new york' size 10), Rust strips (size 8). Value/size/equality diverge on loaded
    nested strings. The then-current gap ledger described it backwards. VERIFIED on tinysnb.
43. CSV escape-strictness: C++ errors on ESCAPE not followed by QUOTE/ESCAPE; Rust silently drops. (A4 B2)
44. COPY options ignored: skip=N ignored entirely; header=false/0 ignored (always auto-skips header).
    LOAD skip+header off-by-one. (A4 E1/E2/E3)
45. COPY error wrapper missing: C++ `Copy exception: Error in file <path> on line <N>: ... Line/record
    containing the error: '<record>'`; Rust bare message. Systematic across COPY errors. (A4+A1)
46. n.id = 2.0 on INT64 PK: hermetically REAL — cpp returns NO row (its PK/index path misses integral
    doubles) though cpp itself says 2 = 2.0 is True; rust returns the row (scalar-consistent).
    Divergence-on-oracle-quirk; a .test on it would fail. count(DISTINCT) p0-sweep diff was an artifact
    (state divergence) — hermetically SAME.
47. Runner-fidelity verdict (A10): comparisons exact/stricter (error strings exact after rtrim; sorts like
    C++; missing result block = hard error). One leniency: -CHECK_PRECISION (2 files) wider tolerance than
    C++ 1-ULP + applies to all numeric cells + no order requirement. Hidden coverage: 45 cases C++ runs
    (-BATCH_STATEMENTS 25, -CREATE_DATASET_SCHEMA 8, -LOOP 7, concurrent 5) + 19 uppercase-EMPTY (case bug
    lib.rs:601) + 28 whole-file parse collapses (error(regex) 13 files, hash 4) + demo_db panic file;
    no per-statement catch_unwind. -WASM_ONLY/-SKIP-header ignored (runs 4 files C++ disables, all FAIL now).

## From A6 (types/casts) — headliners re-verified by me
48. float->int rounding mode: C++ round-half-to-EVEN (nearbyint), Rust truncates toward zero.
    CAST(5.5 AS INT64) -> 6 vs 5; CAST(1.731 AS INT32) -> 2 vs 1. Every width, every cast form. VERIFIED.
49. Invalid DATE/TIMESTAMP strings silently normalize/roll over in Rust: '2020-02-30' -> 2020-03-01,
    '01-01-2020' -> 0006-07-13 (!), '25:00' rolls to next day; C++ Conversion exception. VERIFIED.
50. REAL alias maps to DOUBLE in Rust; FLOAT in C++ (table_info confirms). VERIFIED.
51. Common-type inference: mixed-sign ints (C++ UINT8+INT8->INT16, rust->INT8); DECIMAL+numeric ->
    rust collapses to DOUBLE (C++ preserves DECIMAL(21,2)). (A6, not re-run)
52. String->LIST cast quote retention: CAST('["a","b"]' AS STRING[]) -> cpp keeps quote chars, rust strips.
    Same family as CSV nested-string parsing (42). (A6)
53. NaN DISTINCT: count(DISTINCT) over two NaNs -> cpp 2, rust 1. (A6, not re-run)
54. Permissive comparisons: `1 = true` / DATE-vs-number -> cpp binder Type-Mismatch error, rust False.
    bool<->int CASTs allowed in rust, rejected in cpp. '+5'/'007'/'01.5' string->number accepted (cpp rejects).
    INTEGER alias accepted (cpp rejects). -9223372036854775808 literal typed INT128 (cpp INT64). (A6)
55. JSON type missing (CAST('{}' AS JSON) not supported; cpp core has it). (A6)
56. Interval parsing: negatives ('-3 days'), fractional ('1.5 hours'), decade/millennium/quarter units
    missing (cpp accepts). DATE '/' separator missing. (A6+A1)
57. CREATE () zero-label: with exactly ONE node table, cpp infers it and creates (count=1); rust rejects
    ("must specify exactly one label in this phase"). Empty-DB + multi-table cases: both error (wording
    differs). VERIFIED. (8 tck cases)

## From A8 (clauses) — key items
58. MERGE on a rel pattern matching MULTIPLE existing edges: C++ binds ALL (2 result rows; ON MATCH SET
    updates BOTH); Rust binds ONE (1 row; only one edge updated). Result cardinality AND final DB state
    diverge silently. VERIFIED. (Docs' "single representative match" was about duplicate input rows,
    a different scenario.)
59. ACYCLIC recursion: Rust = true no-repeated-node; C++ ACYCLIC behaves like WALK (quirk/no-op) →
    row counts diverge (cpp 9 vs rust 2 on a cycle; 50 vs 38 on tinysnb). Deviation from the oracle,
    semi-documented in the then-current gap ledger. (A8)
60. Aggregate in WHERE: C++ accepts (quirk); Rust rejects with binder error. (A8)
61. Rust-permissive extras: CREATE referencing a sibling node's property in the same CREATE; UNWIND of a
    scalar -> silent 0 rows (C++ errors); WALK keyword accepted (C++ rejects it as syntax!); list
    comprehensions; SET += (already logged). (A8)
62. properties(node|rel) 1-arg panic ALSO confirmed by A8's independent probing (scalarfn.rs:1178).
63. Large verified agreement surface (A8): self-loops, rel-uniqueness-in-pattern, undirected double-count,
    cross products, zero-length paths, TRAIL/SHORTEST/ALL SHORTEST counts, bounds errors, OPTIONAL null
    propagation/chaining, WITH scoping, DISTINCT, UNWIND edges, correlated EXISTS/COUNT, MERGE node
    ON CREATE/MATCH, SET/DELETE/DETACH DELETE, dup-PK errors, keyword case-insensitivity, unicode strings,
    COMMENT ON, CALL config — byte-identical.

## Non-deviations verified (pre-audit docs wrong/stale in our favor)
- Sequences ARE rollback-transactional in C++ 0.17 too (BEGIN; nextval; ROLLBACK; nextval -> 1 in
  both). The then-current gap ledger claimed a deviation that does not exist.
- Degenerate-path value now matches Koko quirk (single-node path, not NULL).

## Oracle bugs found (C++ crashes; not port deviations)
- `label(rels(p)[1])` SEGFAULTS the C++ shell (rc=139). Rust correctly returns the label.

## Missing-feature confirmations (not started / rejected at parse)
- On-disk storage entirely absent: Database::in_memory() is the only constructor (P4).
- Multi-file COPY `FROM [f1,f2]`/`(f1,f2)`: parse error. IGNORE_ERRORS: "not supported in this phase".
- npy/parquet/gz readers absent (corpus dirs skip/fail).
- Extensions absent (P5) — but extension/ dir is empty in this C++ checkout too; its tests fail there as well.
