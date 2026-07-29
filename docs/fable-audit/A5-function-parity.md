# A5 — Function Library Parity Audit (koko C++ oracle vs koko-rs)

Date: 2026-07-01. Method: enumerate the C++ catalog from source + the authoritative
`CALL show_functions()` oracle; cross-reference the Rust registry
(`crates/koko-function/src/scalarfn.rs::SCALAR_NAMES`, `AggOp`, binder specials,
parser `TableFunc`); differentially probe implemented functions through the harness
(`diffprobe.py`) and a single-statement comparator (`runboth.sh`, immune to the
harness's multi-line-error misalignment). CONFIRMED = both engines actually executed
the probe. Binaries: C++ `build/release/tools/shell/koko`; Rust
`target/release/examples/koko_cli`.

## Executive summary

- **Catalog:** C++ exposes **273 distinct function names** (220 scalar, 8 aggregate, 10
  rewrite, 28 table, 5 standalone-table, 2 copy/export). Rust recognizes ~175 scalar +
  7 aggregate + 6 rewrite + 5 table.
- **Missing (CONFIRMED):** **43** scalar/agg/rewrite/predicate names error in Rust while
  C++ returns a value — of which ~28 are functionally distinct (hash `md5`/`sha256`/`hash`,
  blob `octet_length`/`encode`/`decode`, `count_if`, `error`, `concat_ws`, `internal_id`,
  `current_date`/`current_timestamp`/`epoch_ms`/`to_epoch_ms`/`to_interval`, `random`/`setseed`,
  list `any`/`all`/`none`/`single` predicates, `list_has_all`, node/rel `start_node`/`end_node`/
  `cost`/`rowid`/`is_trail`/`is_acyclic`, agg `percentiledisc`), plus ~9 aliases
  (`tolower`,`toupper`,`array_append`/`prepend`/`push_back`/`push_front`,`list_has`,`array_has`)
  and 6 operator-by-name forms. **Table functions: 28 of 33 missing** (`CALL` rejects all
  but current_setting/show_tables/table_info/show_sequences/show_macros); `COPY (query) TO`
  export is a parser error.
- **Coercion gap (task 4):** BROAD in one direction — Rust performs **no implicit cast of
  scalar args to declared param types**, so **every STRING-typed parameter of every string
  function** rejects a non-string arg at runtime (`left(to_double(1.34),8)`, `lower(123)`,
  …) where C++ auto-inserts `CAST(x,STRING)`. ~25+ functions. Reverse polarity: Rust is
  over-permissive on DOUBLE/STRING in INT positions (silent wrong values).
- **Semantics (~1270 probes):** parity is high on trig/bitwise/list-slice/list-sort/map/
  interval-rendering/date_trunc/indexing. Highest-impact confirmed DIFFs: **float→int cast
  is round-half-to-even in C++ but truncation in Rust**; **BOOL casts forbidden in C++ but
  allowed in Rust**; **out-of-range dates error in C++ but silently normalize in Rust**;
  **Unicode differs** in both case-folding (ß/ﬁ/İ/final-sigma) and regex `\w`/`\d`/`\s`
  classes (C++ RE2 ASCII vs Rust unicode); **crashes on both sides** — C++ has 6 confirmed
  SIGSEGVs on NULL list/struct args + a SIGSEGV on `repeat(-n)` + an `UNREACHABLE_CODE`
  assert, while **Rust panics on 2-arg `lpad`/`rpad`**; `array_extract` on ARRAY is a C++
  bug (stringifies); `^` operator and single-arg `round` differ; and a **systematic
  error-string mismatch** (C++ appends trailing blank lines to arg-mismatch errors; Rust
  does not). Note many DIFFs are Rust being *more* correct/permissive than the oracle.
- **CONFIRMED vs SUSPECTED:** every DIFF and missing-function row was executed on both
  engines (CONFIRMED). Probe files saved under `scratchpad/probes/`.

## 0. Catalog size (distinct names, from `show_functions()`)

| Category | C++ distinct names | Rust recognized | Notes |
|---|---|---|---|
| SCALAR | 220 | ~175 | incl. operators & aliases |
| AGGREGATE | 8 | 7 | missing `percentiledisc` |
| REWRITE | 10 | 6 | id/label/labels/length/keys/nullif present; cost/rowid/start_node/end_node missing |
| TABLE | 28 | 5 | huge gap |
| STANDALONE TABLE | 5 | 0 | |
| COPY (export) | 2 | 0 | `COPY (query) TO` unsupported (parser error) |
| **Total distinct** | **273** | | |

C++ `show_functions()` returns 1214 rows = per-overload signatures. The Rust engine
rejects `CALL show_functions()` entirely ("table functions are not supported in this
phase"), so there is no self-describing catalog on the Rust side.

## 1. Missing functions by family (CONFIRMED — C++ returns a value, Rust errors)

Every row below was run on both engines. Rust error is `Catalog exception: function X
does not exist` unless noted.

| Family | Missing in Rust | Count | Evidence (C++ result) |
|---|---|---|---|
| Numeric/math | `random` (rand), `setseed` | 2 | `random()`→0.x; `setseed(0.5)`→ok |
| String | `concat_ws`, `tolower`*, `toupper`* | 3 | `concat_ws('-','a','b')`→`a-b`; `tolower('AB')`→`ab` |
| List/array | `array_append`, `array_prepend`, `array_push_back`, `array_push_front`, `list_has`*, `array_has`*, `list_has_all` | 7 | `array_append([1,2],3)`→`[1,2,3]`; `list_has_all([1,2,3],[1,2])`→True |
| List predicates | `any`, `all`, `none`, `single` | 4 | `any(x IN [1,2,3] WHERE x>2)`→True; Rust = **Parser error** (`expected RParen but found IN`) |
| Blob | `octet_length`, `encode`, `decode` | 3 | `octet_length(BLOB('abc'))`→3 |
| Hash | `hash`, `md5`, `sha256` | 3 | `md5('abc')`→`900150983cd2...` |
| Temporal | `current_date`, `current_timestamp`, `epoch_ms`, `to_epoch_ms`, `to_interval` | 5 | `current_date()`→`2026-07-02`; `epoch_ms(1000)`→`1970-01-01 00:00:01` |
| Internal-id | `internal_id` | 1 | `internal_id(0,0)`→`0:0` |
| Utility | `count_if`, `error` | 2 | `count_if(true)`→1; `error('boom')`→Runtime exception |
| Aggregate | `percentiledisc` | 1 | `percentiledisc(x,0.5)`→3 (note: name is literally `percentiledisc`; `percentile_disc`/`percentile_cont` exist in NEITHER engine) |
| Node/rel/path | `start_node`, `end_node`, `cost`, `rowid`, `is_trail`, `is_acyclic` | 6 | `start_node(e).ID`→0; `is_trail(p)`→True |
| Comparison (by-name) | `equals`, `not_equals`, `greater_than`, `greater_than_equals`, `less_than`, `less_than_equals` | 6 | `equals(1,1)`→True. Niche: normally used as operators `= <> > >= < <=` which work in both. |

`*` = alias of an implemented function (`lower`/`upper`/`list_contains`/`array_contains`).

**Total confirmed-missing scalar/agg/rewrite/predicate names: 43** (of which ~9 are
pure aliases and 6 are operator-by-name forms; ~28 are functionally-distinct gaps).

### Present in Rust despite absence from `SCALAR_NAMES` (verified working):
`struct_pack`, `cast`, `union_value`, `keys`, `nextval`, `currval` (sequences work end
to end), `list_transform`/`list_filter`/`list_reduce` (lambdas), `to_years…to_microseconds`.

## 2. Table / COPY function gap (CONFIRMED)

Rust parser recognizes only **5** table functions: `current_setting`, `show_tables`,
`table_info`, `show_sequences`, `show_macros`. All other `CALL fn()` return
`Not implemented exception: CALL fn(...) table functions are not supported in this phase`.

Missing (23 TABLE): `bm_info`, `catalog_version`, `db_version`, `disk_size_info`,
`file_info`, `fsm_info`, `projected_graph_info`, `read_csv_parallel`, `read_csv_serial`,
`read_npy`, `read_parquet`, `show_attached_databases`, `show_connection`,
`show_functions`, `show_graphs`, `show_indexes`, `show_loaded_extensions`,
`show_official_extensions`, `show_projected_graphs`, `show_warnings`, `stats_info`,
`storage_info`, `storage_version`.

Missing (5 STANDALONE): `_cache_array_column_locally`, `clear_warnings`,
`drop_projected_graph`, `project_graph`, `project_graph_cypher`.

Missing (2 COPY/export): `COPY (query) TO 'file.csv'|'.parquet'` → Rust **Parser error**
`expected an identifier, found LParen` (C++ parses it). Scan table functions
(`read_parquet`/`read_csv`/`read_npy`) partly exist via `COPY FROM` / `LOAD FROM`
(binder handles parquet/npy/csv) but not as callable table functions.

`current_setting('threads')`: C++→`10`; Rust→empty (partial — recognizes the call but
returns no value).

## 3. Coercion-gap quantification (CONFIRMED)

The Rust binder types a scalar call directly from its actual argument types
(`scalar_func_result_type(name, arg_types)`) and performs **no implicit cast of
arguments to the function's declared parameter types**. C++ `getCastCost`
(built_in_function_utils.cpp:73) inserts an implicit `CAST` whenever a finite cast cost
exists; crucially `targetType == STRING ⇒ castFromString(input)` returns a finite cost
for essentially every scalar type, so **any value is implicitly castable to STRING**.

### Polarity A — missing implicit cast (the documented gap): BROAD
Non-STRING argument in a STRING parameter position: C++ inserts `CAST(x, STRING)` and
succeeds; Rust binds, then throws at eval: `Runtime exception: expected a STRING
argument, got <T>` (or a bind-time signature error for `size`). Confirmed on:

`lower, upper, trim, ltrim, rtrim, initcap, reverse, left, right, lpad, rpad, substr,
repeat, contains, starts_with, ends_with, prefix, suffix, levenshtein, replace,
regexp_matches, regexp_replace, string_split, split_part, size` — i.e. **every
STRING-typed parameter of every string function (~25+ functions, all STRING arg
positions)**. Examples (C++ | Rust):
- `left(to_double(1.34), 8)` → `1.340000` | Runtime error (the documented case)
- `lower(123)` → `123` | Runtime error
- `substr(12345,1,3)` → `123` | Runtime error
- `lpad(12,5,0)` → `00012` | Runtime error
- `upper(date('2024-01-05'))` → cast DATE→STRING succeeds | Runtime error

Not affected (work in BOTH): `concat` (variadic ANY, Rust stringifies itself);
comparison/arithmetic operators (`1 = '1'`→True, `date = 'str'`→True — Rust coerces to a
common type); numeric widening INT→DOUBLE for math (`sin(1)`→0.841471 both).

### Polarity B — Rust over-permissive (missing arg validation): correctness bug, narrower
A DOUBLE or STRING in an INT parameter position: C++ **binder-rejects** (`Function … did
not receive correct arguments (STRING,DOUBLE)…`); Rust **silently accepts and returns a
wrong/degenerate value** (no error). Confirmed:
- `left('hello', 2.0)` → C++ bind error | Rust `''` (should be `he`)
- `right('hello', 2.0)` → bind error | Rust `''`
- `substr('hello', 1.0, 3.0)` → bind error | Rust `hello` (returns whole string!)
- `repeat('ab', 2.0)` → bind error | Rust `''`
- `lpad('x', 4.0, 'y')` → bind error | Rust `''`
- `factorial('5')` → bind error | Rust `1` (string→0, factorial(0)=1)
- `list_contains([1,2,3], '2')` → C++ bind error (Implicit cast not supported) | Rust `False`

## 4. Confirmed semantic DIFFs of implemented functions

~1200 probes run across numeric/cast (331), string (pending), list/array/struct/map/union
(288), temporal (267), plus ~90 cross-cutting spot probes. All rows below are
harness-CONFIRMED on both engines. Severity: HIGH = wrong value / crash-vs-value /
value-vs-error; MED = type/precision/rendering; LOW = cosmetic or error-string text.

### 4A. Cross-cutting systematic themes (the important ones)

1. **NULL / untyped-ANY through scalar functions — C++ crashes or errors inconsistently;
   Rust uniformly returns NULL. (HIGH — includes 6 confirmed C++ SIGSEGVs.)**
   Confirmed C++ **SIGSEGV (exit 139)** on: `list_sum(NULL)`, `list_product(NULL)`,
   `list_reverse_sort(NULL)`, `list_any_value(NULL)`, `list_contains(NULL,1)`,
   `struct_extract(NULL,'a')`. C++ runtime/assert/binder error (no value) on:
   `list_reverse/distinct/unique/position(NULL)`, `list_sort(NULL)`,
   `list_transform/filter/reduce(NULL,…)`, `to_string(null)` (C++ "Trying to create a
   vector with ANY type"), `list_position([1,null,3], null)` (C++ `UNREACHABLE_CODE`
   assert). Rust returns NULL for every one. **A C++ crash wipes the whole probe batch**
   (sentinels lost), which is itself a robustness gap.

2. **float→int CAST rounding — C++ round-half-to-even; Rust truncates toward zero.
   (HIGH, systematic.)** `CAST(3.9 AS INT64)`→C++ 4 / Rust 3; `-3.9`→-4/-3; `0.9`→1/0;
   `-0.9`→-1/0. Diverges for any value whose fractional part isn't a tie landing on the
   truncated value. Also overflow-after-round: `CAST(127.9 AS INT8)`→C++ wraps to -128 /
   Rust 127; `CAST(-128.9 AS INT8)`→C++ Overflow error / Rust -128.

3. **BOOL casts — C++ forbids int/float↔bool; Rust permits. (HIGH.)**
   `CAST(1 AS BOOL)`→C++ "Unsupported casting function from INT64 to BOOL" / Rust True;
   `CAST(true AS INT64)`→C++ error / Rust 1; `CAST(true AS DOUBLE)`→C++ error / Rust
   1.000000; `to_bool(2)`→C++ "not a valid boolean" / Rust True.

4. **Out-of-range temporal inputs — C++ strictly validates & errors; Rust silently
   normalizes (chrono-style overflow). (HIGH, ~13 probes.)**
   `make_date(2024,13,1)`→C++ "Date out of range" / Rust `2025-01-01`;
   `date('2024-13-01')`→C++ parse error / Rust `2025-01-01`;
   `timestamp('2024-01-05 25:00:00')`→C++ error / Rust `2024-01-06 01:00:00`;
   `make_date(2023,2,29)`, `(2024,2,30)`, `(2024,0,1)`, `(2024,1,0)`, `(2024,-1,1)` all
   normalize in Rust, error in C++.

5. **Missing-field / by-name extract — C++ bind error; Rust NULL. (HIGH.)**
   `struct_extract({a:1,b:2},'c')`→C++ "Invalid struct field name: c" / Rust NULL;
   `union_extract(union_value(x:=1),'y')`→C++ error / Rust NULL. (The `.field` accessor
   form agrees — both error.)

6. **Rust is a superset in places (C++ errors / lacks; Rust returns a value):**
   `exp`, `power` (C++: "function does not exist"), single-arg `round(x)` (C++ requires
   `(DOUBLE,INT64)`), `INTERVAL * INT` (no C++ overload), `date(TIMESTAMP)` (Rust
   truncates; C++ errors), `date_part('dow'|'weekday')` (Rust 0–6; C++ "Unrecognized
   specifier"), heterogeneous list literals `['a',1]`→C++ coerces to STRING[] / Rust
   binder error. **Reverse:** the `^` power operator — C++ `2^10`=1024.0 / Rust **Parser
   error** (unsupported); and empty struct `{}` — Rust `{}` / C++ Parser error.

### 4B. Numeric / cast DIFFs (331 probes, 258 SAME)
| PROBE | CPP | RUST | SEV |
|---|---|---|---|
| `CAST(3.9 AS INT64)` (+ family) | 4 | 3 | HIGH (theme 2) |
| `CAST(1 AS BOOL)` / `CAST(true AS INT64)` (+ family) | error | True / 1 | HIGH (theme 3) |
| `2 ^ 10` | 1024.000000 | Parser error (`^` unsupported) | HIGH |
| `exp(1)` / `power(3,3)` | function does not exist | 2.718282 / 27.000000 | HIGH (Rust extra) |
| `round(0.5)` (1-arg) | Binder error, needs (DOUBLE,INT64) | 1.000000 | HIGH (Rust extra) |
| `abs(-9223372036854775808)` | Overflow error | 9223372036854775808 (INT128) | HIGH |
| `typeof(-9223372036854775808)` | INT64 | INT128 | MED (root cause of above) |
| `gamma(-1)` / `lgamma(-1)` | nan / inf | -2.5e16 / 37.78 | HIGH |
| `sign(-0.0)` | -1 | 0 | MED |
| `factorial(21)` | -4249290049419214848 (silent wrap) | Overflow error | HIGH |
| `to_string(null)` | Runtime error (ANY-vector bug) | NULL | HIGH (C++ bug) |
| `greatest(null,null)` | NULL | Binder error (ANY,ANY) | MED |
| trig (23), bitwise+shifts (19), div/mod-by-zero, `sqrt(-1)`=nan, `ln(0)`=-inf, INT overflow +/*, INT8..128 range/parse cast errors, INT128, inf/nan parse | — | all SAME | — |

### 4C. List / array / struct / map / union DIFFs (288 probes, 226 SAME)
| PROBE | CPP | RUST | SEV |
|---|---|---|---|
| `array_extract([10,20,30], 2)` | `1` (stringifies array, returns Nth char!) | `20` | HIGH (C++ bug) |
| `list_sum/product/reverse_sort/any_value/contains(NULL)`, `struct_extract(NULL,..)` | **SIGSEGV** | NULL | HIGH (theme 1) |
| `list_position([1,null,3], null)` | `UNREACHABLE_CODE` assert | NULL | HIGH |
| `list_reverse/distinct/sort/transform(NULL)` | runtime/binder error | NULL | HIGH (theme 1) |
| `struct_extract({a:1,b:2},'c')` / `union_extract(..,'y')` | Invalid struct field name | NULL | HIGH (theme 5) |
| `{}` (empty struct) / `typeof({})` | Parser error | `{}` / `STRUCT()` | HIGH |
| `['a',1]` / `[1,'a',true]` | coerces → STRING[] | Binder error | HIGH |
| `array_cross_product(2-D, 2-D)` | `[0.000000,0.000000]` (bogus) | Conversion error (needs 3-D) | HIGH (C++ bug) |
| `list_any_value([])` / `([null,null])` | 0 | NULL | MED |
| `range(1,5,0)` / `list_reduce([],..)` | error text A | error text B | LOW |
| `list_slice`/`[a:b]` all bounds, `list_sort` (NULLS/DESC/mixed), `map()` entirely (incl. **duplicate keys `{a=1,a=2}` allowed in BOTH**, missing key→`[]`, empty map), 1-based indexing, field-order, lambdas | — | all SAME | — |

### 4D. Temporal DIFFs (267 probes, 222 SAME)
| PROBE | CPP | RUST | SEV |
|---|---|---|---|
| `make_date`/`date`/`timestamp` out-of-range (13 probes) | error | silently normalizes | HIGH (theme 4) |
| `date('2024/07/01')` | `2024-07-01` (`/` ok) | Cast failed (rejects `/`) | HIGH |
| `DATE('-0001-01-01')` | parse error | `0002-01-01 (BC)` | HIGH |
| `INTERVAL('1.5 hours')` | `01:30:00` | Cast failed (no fractional) | HIGH |
| `INTERVAL('-1 year')` / `interval('')` | parse error | `-1 years` / `00:00:00` | HIGH |
| `date(TIMESTAMP(..))` | parse error | truncates → date | HIGH |
| `INTERVAL('1 day') * 3` | no overload | `3 days` | HIGH |
| `date_part('weekday'|'dow', ..)` | Unrecognized specifier | 0–6 | HIGH |
| `date_part('week', INTERVAL(..))` | **UNREACHABLE_CODE assert (crash)** | Unrecognized date part | HIGH (C++ bug) |
| `date_part('doy'|'epoch'|'isodow'|..)` unrecognized (14) | "Unrecognized interval specifier string: X" | "Unrecognized date part: X" | LOW (err text) |
| `<T>('garbage')` parse errors (4) | "Error parsing <T>. Expected format:" | "Cast failed. <s> is not a valid <T>" | LOW (err text) |
| `date_trunc` (all parts/types), interval normalization/rendering (incl. neg), `to_timestamp`, supported `date_part` keywords, `last_day`, `century`, `dayname`/`monthname`, `greatest`/`least` on DATE/TS, **`date_part('week')`=0 in BOTH** | — | all SAME | — |

### 4E. Error-string differences (systematic — affects the byte-identical contract)
- **Trailing whitespace (pervasive):** every C++ "did not receive correct arguments"
  binder error appends two extra blank lines (`… -> T\n\n\n` + padding); Rust ends at the
  last signature line. Confirmed via `od -c` on `list_to_string([1,2,3],0)` (both error).
  Affects every arg-mismatch error in every family. LOW severity, but ubiquitous.
- **Parse-error wording:** temporal/number parse — C++ "Error occurred during parsing
  <T>. Given: … Expected format: …" vs Rust "Cast failed. <s> is not a valid <T>."
- **Cast wording:** `CAST(1.9 AS BOOL)` — C++ "Unsupported casting function from DOUBLE to
  BOOL" vs Rust "Cannot cast DOUBLE to BOOL".

### 4F. String / unicode DIFFs (258-probe agent battery + ~70 self-run; 212 SAME)
| PROBE | CPP | RUST | SEV |
|---|---|---|---|
| `upper('ß')`/`upper('straße')`/`upper('ﬁ')`/`upper('ﬀ')` | `ẞ`/`STRAẞE`/`ﬁ`/`ﬀ` (simple 1:1) | `SS`/`STRASSE`/`FI`/`FF` (full fold) | HIGH |
| `lower('ΣΟΣ')` / `lower('İ')` | `σοσ` / `i` | `σος` (final-sigma) / `i̇` | HIGH |
| `regexp_extract('café','\\w+')` / `regexp_matches('日本語','\\w')` | `caf` / `False` (RE2 ASCII `\w`) | `café` / `True` (unicode `\w`) | HIGH |
| `regexp_replace('a1b2','([0-9])','<\\1>')` | `a<1>b2` (`\1` backref) | `a<\1>b2` (literal; no backref) | HIGH |
| `lpad('hello',8)` / `rpad('hello',8)` (2-arg) | Binder error | **Rust PANIC** `index out of bounds` (scalarfn.rs:1196/1202) | HIGH |
| `repeat('ab',-2)` | **C++ SIGSEGV** (aborts session) | `''` | HIGH |
| `length('hello')` / `length([1,2,3])` (STRING/LIST arg) | Binder error (path-only) | `5` / `3` | HIGH |
| non-STRING arg in STRING position (coercion, §3) | value (auto-CAST) | Runtime "expected a STRING argument" | HIGH |
| `string_split('a,,b',',')` | `[a,b]` (drops empties) | `[a,,b]` (keeps) | MED |
| `regexp_matches('x','(')` / `regexp_replace('xyz','[','Q')` (invalid regex) | `False` / `xyz` (RE2 lenient) | Runtime error (unclosed group/class) | MED |
| `regexp_matches('abab','(ab)\\1')` (backref in pattern) | `False` | Error: backreferences not supported | MED |
| `trim('xxhixx','x')` (2-arg custom chars) | Binder error | `xxhixx` (accepts, ignores char arg) | MED |
| `substr('hello',3)` (2-arg) | Binder error (needs 3) | `llo` | MED |
| `contains('','')` | `False` | `True` | MED |
| `regexp_extract('2020','(?<y>[0-9]+)')` (.NET named group) | Error (group index oor) | `2020` | MED |
| `string_split(null,',')` | Runtime error (ANY vector) | NULL | MED |
| `<T>` parse err text; invalid-regex err text; trailing-blank-lines (§4E) | text A | text B | LOW |
| **grapheme indexing** (size/left/right/substr/reverse on `café`/`日本語`/`👍`/ZWJ/combining/flags), lpad/rpad 3-arg (all edges), split_part, levenshtein (incl. multibyte), NULL propagation, `regexp_extract`/`extract_all`/`(?i)`/`\b`/anchors, `[[:alpha:]]` (ASCII in both), initcap | — | **all SAME** | — |

**String verdict:** grapheme/codepoint indexing, padding (3-arg), split_part, levenshtein,
NULL propagation, and POSIX-class/anchor/flag regex are byte-identical. Real divergences:
(a) **Unicode case-folding** (C++ simple 1:1 vs Rust full folding — broad); (b) **regex
`\w`/`\d`/`\s` unicode-awareness** (C++ RE2 ASCII vs Rust `regex` crate unicode — broad);
(c) **replacement backreference `\1`** (C++ substitutes, Rust doesn't); (d) **invalid-regex
leniency** (C++ RE2 returns a value, Rust raises); (e) **Rust PANIC on 2-arg lpad/rpad**
and **C++ SIGSEGV on repeat(-n)** — crashes on both sides; (f) `string_split` empty tokens;
(g) Rust-extra overloads (`length`/`substr`/`trim`); (h) `contains('','')`; (i) §3 coercion.
