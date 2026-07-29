# A7 — Expression-Level Semantics: Differential Audit (Rust port vs C++ oracle)

**Scope:** black-box differential probing of expression semantics through both engines.
**Harness:** `dp.py` (crash-resilient wrapper over `diffprobe.py`; falls back to per-statement
execution when a batch loses blocks to a SIGSEGV/panic). Probe files in scratchpad; raw outputs in `scratchpad/out/*.txt`.
**Volume:** 731 probes across 14 files.
`ops(138) logic(130) case_null(14) composite(66) strings(63) strfunc(23) lists(61) mapstruct(36) temporal(49) params(15) exprpos(35) overflow_str(43) noderel(27) misc2(31)`

Rendering caveat: C++ list mode renders NULL and `''` identically as empty; disambiguated with `typeof()` where it mattered. For booleans/ints there is no empty value so blank == NULL unambiguously.

## Classification legend
- **WRONG VALUE (silent)** — Rust returns a value differing from the C++ oracle with no error. Worst class.
- **MISSING FEATURE** — C++ computes/accepts, Rust rejects (parser/binder).
- **TOO PERMISSIVE** — C++ rejects, Rust accepts and returns a value (silent).
- **ERROR-WORDING** — both error, message text differs.
- **RUST CRASH** — Rust panics (rc=101).
- **CPP BUG** — C++ crashes or throws an internal runtime error; Rust diverges (often Rust is the standard-correct side).

## Executive summary of biggest issues
| # | Issue | Class | Silent? |
|---|-------|-------|---------|
| 1 | `IN` operator entirely unsupported (parser) — incl. WHERE/WITH/RETURN | MISSING | no (errors) |
| 2 | `STARTS WITH` / `ENDS WITH` / `CONTAINS` / `=~` infix operators unsupported | MISSING | no (errors) |
| 3 | Power `^` operator unsupported | MISSING | no (errors) |
| 4 | Quantifiers `ALL/ANY/NONE/SINGLE` unsupported | MISSING | no (errors) |
| 5 | `STRING + <non-string>` silently concatenates (C++ rejects) | TOO PERMISSIVE | **YES** |
| 6 | INT64_MIN overflow undetected (`MIN-1`, `MIN*-1`, `-(-MIN)`, …) | WRONG VALUE | **YES** |
| 7 | NULL-inside-composite equality: `[1,NULL]=[1,NULL]` → Rust False, C++ True | WRONG VALUE | **YES** |
| 8 | Cross-type comparison (`1=true`, `[1]=1`) → Rust False, C++ rejects | TOO PERMISSIVE | **YES** |
| 9 | `properties(<struct>)` → Rust **panics** (rc=101) | RUST CRASH | crash |
| 10 | List comprehension `[x IN l WHERE p | e]` accepted by Rust, rejected by C++ | TOO PERMISSIVE | **YES** |
| 11 | invalid `date('2021-02-29')` → Rust `2021-03-01`, C++ rejects | WRONG VALUE | **YES** |

C++-side defects that Rust (correctly) diverges from: `1 IN NULL` **SIGSEGVs** C++; `CASE <non-null> WHEN NULL` takes the NULL branch in C++; `NULLIF(NULL,_)` and `=~ NULL` throw internal "ANY type" errors in C++.

---

## A. Operators & precedence  (`ops.probe`, `overflow_str.probe`)

### A.1 Power `^` — MISSING FEATURE (13 probes)
Rust: `Error: Parser exception: unexpected character '^' in query` for every `^`. C++ computes a DOUBLE.
| probe | cpp | rust |
|---|---|---|
| `RETURN 2 ^ 3` | `8.000000` | parser error |
| `RETURN 2 ^ 0.5` | `1.414214` | parser error |
| `RETURN 2 ^ -1` | `0.500000` | parser error |
| `RETURN -2 ^ 2` | `4.000000` (binds `(-2)^2`) | parser error |
| `RETURN 2 ^ 3 ^ 2` | `64.000000` (LEFT-assoc: `(2^3)^2`) | parser error |
| `RETURN 2 ^ (3 ^ 2)` | `512.000000` | parser error |
| `RETURN typeof(2 ^ 3)` | `DOUBLE` | parser error |

Note: C++ `^` is **left-associative** (`2^3^2` = 64, not 512).

### A.2 `STRING + <non-string>` — TOO PERMISSIVE / SILENT WRONG VALUE (16+ probes)
Rust coerces to string concatenation for INT/DOUBLE/BOOL/LIST/STRUCT; C++ binder-rejects (`Cannot match a built-in function for given function +(STRING,…)`).
| probe | cpp | rust |
|---|---|---|
| `RETURN 'a' + 1` | binder error | `a1` |
| `RETURN 1 + 'a'` | binder error | `1a` |
| `RETURN 'a' + 1.5` | binder error | `a1.500000` |
| `RETURN 1.5 + 'a'` | binder error | `1.500000a` |
| `RETURN 'a' + true` | binder error | `aTrue` |
| `RETURN true + 'a'` | binder error | `Truea` |
| `RETURN 'a' + [1,2]` | binder error | `a[1,2]` |
| `RETURN [1,2] + 'a'` | binder error | `[1,2]a` |
| `RETURN 'a' + {b:1}` | binder error | `a{b: 1}` |
| `RETURN {b:1} + 'a'` | binder error | `{b: 1}a` |
| `RETURN 'a' + NULL` | binder error `+(STRING,ANY)` | (NULL/empty) |
| `RETURN 'a' + 2 + 3` | binder error | `a23` |
| `RETURN 1 + 2 + 'a'` | binder error | `3a` |
| `RETURN 'a' + 1 = 'a1'` | binder error | `True` |
| `RETURN typeof('a' + 1)` | binder error | `STRING` |
| `RETURN size('a' + 123)` | binder error | `4` |

Exception — STRING+DATE routes to Rust's temporal path (ERROR-WORDING, both error):
| `RETURN 'a' + date('2020-01-01')` | binder error `+(STRING,DATE)` | `Runtime exception: unsupported temporal arithmetic: STRING + DATE` |
| `RETURN date('2020-01-01') + 'a'` | binder error `+(DATE,STRING)` | `Runtime exception: unsupported temporal arithmetic: DATE + STRING` |

### A.3 INT64_MIN overflow undetected — WRONG VALUE / SILENT (7 probes)
C++ raises `Overflow exception`; Rust silently emits an out-of-range value. (Positive add/mul overflow and INT8 overflow ARE caught identically in both — see A.6.)
| probe | cpp | rust |
|---|---|---|
| `RETURN -9223372036854775808 - 1` | Overflow exception | `-9223372036854775809` |
| `RETURN -9223372036854775808 - 2` | Overflow exception | `-9223372036854775810` |
| `RETURN -9223372036854775808 * -1` | Overflow exception | `9223372036854775808` |
| `RETURN -9223372036854775808 / -1` | Overflow exception | `9223372036854775808` |
| `RETURN 0 - (-9223372036854775808)` | Overflow exception | `9223372036854775808` |
| `RETURN -(-9223372036854775808)` | Overflow exception `cannot be negated` | `9223372036854775808` |
| `RETURN -9223372036854775808 % -1` | Overflow exception | `0` |

### A.4 List/BOOL arithmetic — ERROR-WORDING (both error)
| probe | cpp | rust |
|---|---|---|
| `RETURN [1] + 2` | `Cannot match a built-in function … +(INT64[],INT64)` | `arithmetic requires numeric operands, got INT64[]` |
| `RETURN 1 + [2]` | `… +(INT64,INT64[])` | `arithmetic requires numeric operands, got INT64[]` |
| `RETURN true + 1` | `… +(BOOL,INT64)` | `arithmetic requires numeric operands, got BOOL` |
| `RETURN [1,2] + [3.5]` | `Cannot bind LIST_CONCAT with parameter type INT64[] and DOUBLE[].` | `list concatenation requires matching child types, got INT64 and DOUBLE` |

### A.5 / A.6 What MATCHES (no diff)
Integer `/` and `%` incl. all negative operands (`-7/2`, `7%-3`, `-7%-3`, …); float `/` and `%`; unary minus & double-negation; precedence (`2+3*4`, left-assoc `10-2-3`, `%`/`*` vs `+`); string concat STRING+STRING; list concat `[1,2]+[3,4]`, `[]+[1]`; div/mod by zero (both error identically); positive overflow `MAX+1`/`MAX*2` (both `Overflow exception`, identical text); INT8 overflow via CAST; numeric widening `typeof(1+1.0)=DOUBLE`.

---

## B. Three-valued logic  (`logic.probe`, `case_null.probe`, `misc2.probe`)

Truth tables for AND/OR/NOT/XOR with NULL, IS NULL / IS NOT NULL, and non-null comparisons **all MATCH** (XOR is supported; `NULL AND false`→False, `NULL OR true`→True, `NOT NULL`→NULL, etc.). Diffs:

### B.1 `IN` operator — MISSING FEATURE (major, ~15 probes)
Rust rejects **all** `IN` usages (`Error: Parser exception: expected Eof but found Ident("IN")`), verified also inside `WHERE`, `WITH`, and lowercase `in`. No membership operator exists (functions do not cover it). C++ supports fully.
| probe | cpp | rust |
|---|---|---|
| `RETURN 1 IN [1,2,3]` | `True` | parser error (IN) |
| `RETURN 1 IN []` | `False` | parser error |
| `RETURN NULL IN [1,2]` | (NULL) | parser error |
| `RETURN 1 IN [1, NULL]` | `True` | parser error |
| `RETURN 2 IN [1, NULL]` | `False` (C++: NULL swallowed → False, not NULL) | parser error |
| `RETURN 1.0 IN [1,2]` | `True` | parser error |
| `RETURN [1] IN [[1],[2]]` | `True` | parser error |
| `RETURN NOT (1 IN [2,3])` | `True` | parser error `expected RParen but found IN` |

### B.2 `1 IN NULL` — CPP CRASH (SIGSEGV)
`RETURN 1 IN NULL` → **C++ SIGSEGVs (rc=-11)**, which loses buffered stdout for the whole batch. Rust parser-rejects (IN unsupported). C++ defect.

### B.3 `NOT IN` infix & comparison chains — ERROR-WORDING (both reject)
| probe | cpp | rust |
|---|---|---|
| `RETURN 1 NOT IN [2,3]` | `Invalid input < NOT>: expected rule iC_Statements` | `expected Eof but found Ident("NOT")` |
| `RETURN 1 < 2 < 3` | `Non-binary comparison (e.g. a=b=c) is not supported` | `expected Eof but found Lt` |
| `RETURN 1 < 2 = true` | `Non-binary comparison …` | `expected Eof but found Eq` |
| `RETURN 1 + 1 = 2 = true` | `Non-binary comparison …` | `expected Eof but found Eq` |

### B.4 `CASE <non-null> WHEN NULL THEN…` — WRONG VALUE / SILENT (CPP BUG)
C++ **takes the `WHEN NULL` branch** for any non-null subject (a C++ bug: `subject = NULL` should not match); Rust falls through (standard-correct). Consistent across INT/STRING subjects.
| probe | cpp | rust |
|---|---|---|
| `RETURN CASE 1 WHEN NULL THEN 'a' ELSE 'b' END` | `a` | `b` |
| `RETURN CASE 2 WHEN NULL THEN 'a' WHEN 1 THEN 'c' ELSE 'b' END` | `a` | `b` |
| `RETURN CASE 1 WHEN NULL THEN 'a' WHEN 1 THEN 'c' ELSE 'b' END` | `a` | `c` |
| `RETURN CASE 'x' WHEN NULL THEN 'a' ELSE 'b' END` | `a` | `b` |
| `RETURN CASE 1 WHEN NULL THEN 'a' END` | `a` | (NULL) |

(Agreement: `CASE NULL WHEN NULL THEN 'a' ELSE 'b'`→`a` in both; `CASE NULL WHEN 1 …`→`b` in both.)

### B.5 `CASE` with mixed branch types — TOO PERMISSIVE / SILENT
| probe | cpp | rust |
|---|---|---|
| `RETURN CASE WHEN true THEN 1 ELSE 'a' END` | `Binder exception: Expression a has data type STRING but expected INT64. Implicit cast is not supported.` | `1` |

(`CASE WHEN true THEN 1 ELSE 2.5` matches — both widen to DOUBLE.)

### B.6 `NULLIF(NULL, …)` — CPP BUG
| probe | cpp | rust |
|---|---|---|
| `RETURN NULLIF(NULL, 1)` | `Runtime exception: Trying to a create a vector with ANY type…` | (NULL) |
| `RETURN NULLIF(NULL, NULL)` | same runtime exception | (NULL) |

(COALESCE / IFNULL / `NULLIF(1,1)` / `NULLIF(1,2)` / `NULLIF(1,NULL)` all MATCH.)

---

## C. Composite comparisons  (`composite.probe`)

Lexicographic list ordering (`[1,2]<[1,3]`, `[2]<[1,2]` etc.), nested lists, list `=`/`<>`, struct `=` incl. **field-order sensitivity** (`{a:1,b:2}={b:2,a:1}`→False in both), struct `<`, and `map()` equality **all MATCH**. Diffs:

### C.1 NULL-inside-composite equality — WRONG VALUE / SILENT
C++ uses null-safe element equality (NULL matches NULL → True); Rust returns False.
| probe | cpp | rust |
|---|---|---|
| `RETURN [1,NULL] = [1,NULL]` | `True` | `False` |
| `RETURN [NULL] = [NULL]` | `True` | `False` |
| `RETURN {a:NULL} = {a:NULL}` | `True` | `False` |
| `RETURN {a:1,b:NULL} = {a:1,b:NULL}` | `True` | `False` |
| `RETURN [1,NULL] < [1,2]` | `False` | (NULL) |

### C.2 Cross-type comparison — TOO PERMISSIVE / SILENT
C++ binder-rejects (`Type Mismatch: Cannot compare types …`); Rust returns `False`.
| probe | cpp | rust |
|---|---|---|
| `RETURN 1 = true` | `Cannot compare types INT64 and BOOL` | `False` |
| `RETURN true = 1` | `Cannot compare types BOOL and INT64` | `False` |
| `RETURN [1] = 1` | `Cannot compare types INT64[] and INT64` | `False` |
| `RETURN {a:1} = [1]` | `Cannot compare types STRUCT(a INT64) and INT64[]` | `False` |
| `RETURN {a:1} = {a:1,b:2}` | `Cannot compare types STRUCT(a INT64) and STRUCT(a INT64, b INT64)` | `False` |

(Agreement: `1 = '1'`→True in both (INT/STRING coerce); `'a' = 97`→identical cast-failure error in both.)

---

## D. String predicates  (`strings.probe`, `strfunc.probe`)

### D.1 `STARTS WITH` / `ENDS WITH` / `CONTAINS` infix — MISSING FEATURE (major, ~30 probes)
Rust parser-rejects every infix use (`expected Eof but found Ident("STARTS"/"ENDS"/"CONTAINS")`), verified also in `WHERE`. C++ supports all. Function forms `starts_with/ends_with/contains` DO exist in Rust (see D.3).
| probe | cpp | rust |
|---|---|---|
| `RETURN 'hello' STARTS WITH 'he'` | `True` | parser error |
| `RETURN 'hello' ENDS WITH 'lo'` | `True` | parser error |
| `RETURN 'hello' CONTAINS 'ell'` | `True` | parser error |
| `RETURN '日本語' CONTAINS '本'` | `True` | parser error |
| `RETURN typeof('hello' STARTS WITH 'he')` | `BOOL` | parser error `expected RParen` |

C++-only observed quirk (documents oracle behavior): `'hello' CONTAINS ''`→**False**, `'' CONTAINS ''`→**False**, but `'hello' STARTS WITH ''`→True.

### D.2 Regex `=~` — MISSING FEATURE (major, ~24 probes)
Rust: `Error: Parser exception: unexpected character '~' in query`. C++ supports full-match regex. Function form `regexp_matches` exists in Rust.
| probe | cpp | rust |
|---|---|---|
| `RETURN 'hello' =~ 'h.*o'` | `True` | parser error `~` |
| `RETURN 'hello' =~ 'ell'` | `False` (full-match: not anchored substring) | parser error |
| `RETURN 'hello' =~ '.ell.'` | `True` | parser error |
| `RETURN 'ABC' =~ '(?i)abc'` | `True` (inline flags) | parser error |
| `RETURN 'hello' =~ '('` | `False` (invalid regex → no match, no error) | parser error |
| `RETURN 'hello' =~ NULL` | `Runtime exception: … ANY type…` (CPP BUG) | parser error |
| `RETURN 'a.b' =~ 'a\.b'` | `Parser exception: Invalid input …` (C++ rejects `\.` in literal) | parser error |

### D.3 Function forms `contains/regexp_matches` — WRONG VALUE + behavior diff
`starts_with/ends_with` and most `contains/regexp_matches` cases MATCH (incl NULL propagation, unicode, `(?i)`). Diffs:
| probe | cpp | rust | class |
|---|---|---|---|
| `RETURN contains('hello','')` | `False` | `True` | WRONG VALUE (silent) |
| `RETURN contains('','')` | `False` | `True` | WRONG VALUE (silent) |
| `RETURN regexp_matches('hello','(')` | `False` (silently ignores bad regex) | `Runtime exception: invalid regular expression: … unclosed group` | behavior diff |

### D.4 `LIKE` — ERROR-WORDING (both reject)
| `RETURN 'hello' LIKE 'h%'` | `Invalid input < LIKE>: expected rule iC_Statements` | `expected Eof but found Ident("LIKE")` |

---

## E. List machinery  (`lists.probe`)

Indexing & slicing & `range()` **all MATCH** exactly:
- 1-based; `[0]`→`Runtime exception: List extract takes 1-based position` (both); out-of-range `[4]`/`[100]`→`index=N is out of range` (both); negative-from-end `[-1]`→last (both); NULL index→NULL (both).
- Slicing clamps (`[3..100]`→`[3,4,5]`), reversed slice `[4..2]`→`[]`, `[..-1]`, `[-2..]` all match.
- `range(1,5)`→`[1,2,3,4,5]`, `range(5,1)`→`[]`, `range(1,5,2)`, `range(-3,3)` all match. `list_extract/list_transform/list_filter/size` match.

### E.1 List comprehension `[x IN l WHERE p | e]` — TOO PERMISSIVE (Rust-only, ~7 probes)
Rust computes; C++ does **not** support it.
| probe | cpp | rust |
|---|---|---|
| `RETURN [x IN [1,2,3] | x*2]` | `Binder exception: Variable x is not in scope.` | `[2,4,6]` |
| `RETURN [x IN [1,2,3] WHERE x > 1 | x*10]` | `Parser exception: Invalid input …WHERE…` | `[20,30]` |
| `RETURN [x IN [1,2,3] WHERE x > 1]` | parser error | `[2,3]` |
| `RETURN [x IN [1,NULL,3] WHERE x IS NOT NULL | x]` | parser error | `[1,3]` |

### E.2 Quantifiers `ALL/ANY/NONE/SINGLE` — MISSING FEATURE (confirmed, 10 probes)
Rust parser-rejects (`expected RParen but found Ident("IN")` — the `x IN list` head can't parse). C++ supports.
| probe | cpp | rust |
|---|---|---|
| `RETURN all(x IN [1,2,3] WHERE x > 0)` | `True` | parser error |
| `RETURN any(x IN [1,2,3] WHERE x > 2)` | `True` | parser error |
| `RETURN none(x IN [1,2,3] WHERE x > 5)` | `True` | parser error |
| `RETURN single(x IN [1,2,3] WHERE x = 2)` | `True` | parser error |
| `RETURN all(x IN [] WHERE x > 0)` | `True` | parser error |
| `RETURN any(x IN [] WHERE x > 0)` | `False` | parser error |

### E.3 `reduce()` — ERROR-WORDING (missing in BOTH)
| `RETURN reduce(acc = 0, x IN [1,2,3] | acc + x)` | `Catalog exception: function REDUCE does not exist.` | `expected RParen but found Ident("IN")` |

---

## F. Map / struct access  (`mapstruct.probe`)

Struct dot access, **case-insensitive field names** (`{Name:5}.name`→5 in both), missing-field error (`{a:1}.b`→`Invalid struct field name: b.` in both), nested access, `element_at/map_extract/struct_extract/cardinality/properties(node)` **all MATCH**. Diffs:

### F.1 `properties(<struct-literal>)` — RUST CRASH (panic)
| probe | cpp | rust |
|---|---|---|
| `RETURN properties({a:1,b:2})` | `Binder exception: Function PROPERTIES did not receive correct arguments…` | **panic rc=101** — `crates/koko-function/src/scalarfn.rs:1178:32: index out of bounds: the len is 1 but the index is 1` |

### F.2 `list[i].field` / chained access after `]` — TOO PERMISSIVE (Rust-only)
C++ parser rejects `.` after `]` (`mismatched input '.'`); Rust computes.
| probe | cpp | rust |
|---|---|---|
| `RETURN [{a:1},{a:2}][1].a` | parser error `mismatched input '.'` | `1` |
| `RETURN [{a:1},{a:2}][2].a` | parser error | `2` |
| `RETURN {arr:[{x:9}]}.arr[1].x` | parser error | `9` |

### F.3 `map[<int-key>]` positional bracket — behavior diff
| probe | cpp | rust |
|---|---|---|
| `RETURN map([1,2],[10,20])[1]` | returns an entry struct (multi-line render) | `Binder exception: Function LIST_EXTRACT did not receive correct arguments: (MAP(INT64,INT64),INT64)…` |

### F.4 Bracket `struct['key']` / `map['key']` / `keys(map)` — ERROR-WORDING (both reject)
Both reject `['stringkey']` (routes to LIST_EXTRACT which wants INT). The message body is identical; **C++ appends trailing whitespace/newlines** (` \n  \n `) that Rust omits.
| `RETURN {a:1,b:2}['a']` | `…LIST_EXTRACT… (LIST,INT64)->ANY … <trailing ws>` | same text, no trailing ws |
| `RETURN map(['a','b'],[1,2])['a']` | same (MAP,STRING) + trailing ws | same, no trailing ws |
| `RETURN keys(map(['a','b'],[1,2]))` | `Function KEYS did not receive correct arguments: Actual: (MAP…) Expected: (NODE) (REL)` | `Function KEYS expects a NODE or REL argument, but got MAP(STRING, INT64).` |

---

## G. Temporal arithmetic  (`temporal.probe`)

`date+int`→DATE, `date-date`→INT64 days, `interval` add/sub, `date+interval` with **month-end clamping** (`2020-01-31 + 1 month`→`2020-02-29` in both), `timestamp-timestamp`→INTERVAL, **cross DATE/TIMESTAMP comparison** (`date < timestamp`→True; `date = timestamp`→True), interval comparison & normalization (`1 year + 13 months`→`2 years 1 month`; `25 hours` stays `25:00:00`), and all `typeof`s **MATCH**. Diffs:

### G.1 Invalid date silently rolled over — WRONG VALUE / SILENT
| probe | cpp | rust |
|---|---|---|
| `RETURN date('2021-02-29')` | `Conversion exception: Error occurred during parsing date. Given: "2021-02-29". Expected format: (YYYY-MM-DD)` | `2021-03-01` |

(`date('2020-02-29')` valid in both — leap year.)

### G.2 `interval * int` — TOO PERMISSIVE / SILENT
| probe | cpp | rust |
|---|---|---|
| `RETURN interval('2 hours') * 3` | `Binder exception: Function * did not receive correct arguments (INTERVAL,INT64)…` | `06:00:00` |

### G.3 SQL `INTERVAL '1' DAY` literal — ERROR-WORDING (both reject)
| `RETURN INTERVAL '1' DAY` | `Invalid input < '1'>: expected rule iC_Statements` | `expected Eof but found Str("1")` |

---

## H. Parameters  (`params.probe`)

Both PARSE `$ident`. Divergence is at bind/runtime and in name lexing. C++ shell binds unknown params to NULL (permissive); Rust errors.
| probe | cpp | rust | class |
|---|---|---|---|
| `RETURN $x` | (NULL); `typeof($x)`→NULL | `Binder exception: Parameter x not found.` | behavior diff |
| `RETURN $x + 1` | (NULL) | `Parameter x not found.` | behavior diff |
| `RETURN $1` | (NULL) — numeric name accepted | `Parser exception: expected an identifier, found Int(1)` | MISSING (rust) |
| `RETURN $名前` | (NULL) — unicode name accepted | `Parser exception: unexpected character 'å' in query` | MISSING (rust) |
| `RETURN $` | `Invalid input <RETURN $ >…` | `expected Eof but found Ident("x")` | ERROR-WORDING |
| `RETURN ${x}` | `Invalid input <RETURN ${>…` | `expected an identifier, found LBrace` | ERROR-WORDING |

---

## I. Expression-position validation  (`exprpos.probe`, tinysnb)

Legal aggregates (`count/sum/min/max/avg/count(DISTINCT)/collect`, `sum(a.age)+count(*)`→306, `CASE WHEN count(*)>5…`, `abs(count(*))`, `count(*)/2`, ORDER-BY-**alias** `count(*) AS c ORDER BY c`, `WITH count(*) AS c WHERE c>1`) **all MATCH** incl. float `avg`=37.250000. Diffs:

### I.1 `WHERE count(*) > 1` — behavior diff (C++ accepts, Rust rejects)
| probe | cpp | rust |
|---|---|---|
| `MATCH (a:person) WHERE count(*) > 1 RETURN a.fName` | returns **all 8 rows** (count(*) folded to scalar 8) | `Binder exception: aggregate function not allowed in this context` |

C++ is internally inconsistent: only bare `count(*)` in WHERE is accepted; other aggregates in WHERE error (see I.2).

### I.2 Aggregate-in-WHERE / nested-aggregate / aggregate-in-ORDER-BY — ERROR-WORDING (both reject)
| probe | cpp | rust |
|---|---|---|
| `… WHERE sum(a.age) > 100 …` | `Cannot evaluate expression with type AGGREGATE_FUNCTION.` | `aggregate function not allowed in this context` |
| `… WHERE a.age > avg(a.age) …` | `Cannot evaluate expression with type AGGREGATE_FUNCTION.` | `aggregate function not allowed in this context` |
| `RETURN sum(count(*))` | `Expression c contains nested aggregation.` | `aggregates cannot be nested` |
| `RETURN count(sum(a.age))` | `Expression c contains nested aggregation.` | `aggregates cannot be nested` |
| `RETURN a.gender AS g ORDER BY count(*)` | `Cannot evaluate expression with type AGGREGATE_FUNCTION.` | `aggregate function not allowed in this context` |
| `RETURN a.gender AS g, count(*) AS c ORDER BY count(*) DESC` | `Cannot evaluate … AGGREGATE_FUNCTION.` | `Variable count(*) is not in scope.` |
| `WITH a.gender AS g WHERE count(*) > 1 …` | `Cannot evaluate … AGGREGATE_FUNCTION.` | `aggregate function not allowed in this context` |

---

## Misc  (`misc2.probe`, `noderel.probe`)

Node/rel equality, unary-minus-on-property, and property arithmetic **all MATCH** (`person=person`→8 self-pairs, `person<>person`→56, `person=organisation`→0 no-error, rel `e1=e2`→14, `-a.age`, `a.age%10`, `a.eyeSight*2`, `a.fName+'!'`). EXISTS/COUNT subqueries match (`EXISTS{…}`→True, count 5; `COUNT{…}`→3). Boolean-literal case variants and NOT/IS-NULL precedence match. Diffs:

| probe | cpp | rust | class |
|---|---|---|---|
| `RETURN 5 != 3` | `Unknown operation '!=' (you probably meant '<>' …)` | `unexpected character '!' in query` | ERROR-WORDING |
| `RETURN 10 mod 3` | `Invalid input < mod>…` | `expected Eof but found Ident("mod")` | ERROR-WORDING |
| `RETURN 10 div 3` | `Invalid input < div>…` | `expected Eof but found Ident("div")` | ERROR-WORDING |
| `… size([(a)-[:knows]->(b) | b.fName])` (pattern comprehension) | `Variable b is not in scope.` | `unexpected token Colon in expression` | ERROR-WORDING (neither supports) |

---

## Appendix — reproduction
- Harness: `python3 scratchpad/dp.py <probe> [--dataset tinysnb] [--show-all]`
- Probe files: `scratchpad/*.probe`; captured outputs: `scratchpad/out/*.txt`.
- Known crashers (isolate before batching): C++ `RETURN 1 IN NULL` (SIGSEGV); Rust `RETURN properties({…})` (panic rc=101).
