# A6 — Type System, Casts & Rendering: C++ (koko) vs Rust (koko-rs)

Audit of the data-type inventory, literal typing, explicit CAST semantics, implicit
coercion, comparison/ordering, and value rendering. Oracle = C++ shell; SUT = Rust CLI.
~521 differential probes run via `diffprobe.py`; probe files in `scratchpad/probes/`,
raw aligned output in `scratchpad/reports/raw/`.

All findings are **CONFIRMED** by live differential probing unless tagged **[SUSPECTED]**
or **[code-only]** (established from source reading, not exercised by a probe).

Severity legend: **[VALUE]** silent wrong result (highest) · **[BEHAVIOR]** error-vs-value
or accept-vs-reject · **[TYPE]** type-tag/inference differs · **[RENDER]** formatting ·
**[ERRSTR]** error-string wording only (lowest).

---

## 1. Type inventory

| Logical type | C++ `LogicalTypeID` | Rust `LogicalType` | Notes |
|---|---|---|---|
| ANY | ✅ ANY=0 | ✅ `Any` | |
| NODE / REL / RECURSIVE_REL | ✅ 10/11/12 | ✅ `Node`/`Rel`/`RecursiveRel` | |
| SERIAL | ✅ 13 | ✅ `Serial` | INT64 surface |
| BOOL | ✅ 22 | ✅ `Bool` | |
| INT8/16/32/64/128 | ✅ 23–26,31 | ✅ `Int(IntKind::*)` | |
| UINT8/16/32/64 | ✅ 27–30 | ✅ `Int(IntKind::U*)` | |
| UINT128 | ✅ 43 | ✅ `UInt128` | separate (exceeds i128) both |
| DOUBLE / FLOAT | ✅ 32/33 | ✅ `Double`/`Float` | |
| DATE | ✅ 34 | ✅ `Date` | |
| TIMESTAMP{,_SEC,_MS,_NS,_TZ} | ✅ 35–39 | ✅ `Timestamp*` | |
| INTERVAL | ✅ 40 | ✅ `Interval` | |
| DECIMAL(p,s) | ✅ 41 | ✅ `Decimal(u8,u8)` | max p=38 both; default `(18,3)` both |
| INTERNAL_ID | ✅ 42 | ✅ `InternalId` | |
| STRING | ✅ 50 | ✅ `String` | |
| BLOB / BYTEA | ✅ 51 (BYTEA=alias) | ✅ `Blob` | |
| LIST / ARRAY | ✅ 52/53 | ✅ `List`/`Array` | ARRAY carries length both |
| STRUCT / MAP / UNION | ✅ 54/55/56 | ✅ `Struct`/`Map`/`Union` | |
| **POINTER** | ✅ 58 (enum only) | ❌ | Not castable-by-name in **either** (C++: "POINTER is neither an internal type…"; Rust: "not supported in this phase"). Not user-facing. |
| **JSON** | ✅ 60 | ❌ **MISSING** | `CAST('{}' AS JSON)` → C++ `{}`; Rust `Not implemented: data type JSON`. `to_json` needs the JSON extension in C++ (not in Rust at all). |
| RDF VARIANT | ❌ (not in this fork) | ❌ | N/A both — Kùzu's RDF types are absent from this C++ base. |

**Inventory verdict:** Rust covers the entire user-facing C++ inventory **except JSON**
(missing) and POINTER (not user-facing in either). Type-alias mismatches — see §5.

---

## 2. Literal typing (probe file `p01`, `p13`)

Integer-literal magnitude ladder INT64 → INT128 → UINT128 **matches** for all positive
boundaries (2^31, 2^63−1, 2^63, 2^64, 2^127−1, 2^127, 2^128−1). Float literals → DOUBLE both.
`1e3` → DOUBLE both. `.5` → DOUBLE both; `5.` → parser error both. `2^128` overflow → both
error (see §8 errstr). Arithmetic result typing, INT64 `+1` overflow error, integer division
`1/2=0`, all **match**.

| # | Probe | C++ | Rust | Sev |
|---|---|---|---|---|
| L1 | `typeof(-9223372036854775808)` (−2^63) | `INT64` | `INT128` | **[TYPE]** |
| L2 | `typeof(2^3)` / `RETURN 2^3` | `DOUBLE` / `8.000000` | Parser error `unexpected character '^'` | [BEHAVIOR] power op unsupported in Rust (affects any `^`) |

**L1 (CONFIRMED):** the minimum INT64 negates a positive literal. C++ types the whole
`-9223372036854775808` token as INT64; Rust parses `9223372036854775808` first (→INT128,
since 2^63 overflows i64) then negates, keeping INT128. The rendered *value* is identical
(`-9223372036854775808`), so it only bites width-sensitive downstream ops (e.g. INT64-range
overflow checks). `-9223372036854775807` and `-9223372036854775809` type identically in both.

---

## 3. Explicit CAST — numeric (probe files `p02`, `p02b`, `p13`, `p14`)

### 3.1 [VALUE] float→int rounding — C++ round-half-to-EVEN, Rust truncate-toward-zero
**The single largest value DIFF.** Applies to every FLOAT/DOUBLE→integer cast, every width
(INT8/16/32/64/128, UINT*), and every cast form (`CAST(x AS T)`, `to_int64(x)`, `cast(x,'T')`).

C++ uses `std::nearbyint` (banker's rounding); Rust truncates. They coincide only when the
fraction is <0.5 in magnitude or when half-to-even happens to round toward zero.

| Probe | C++ | Rust | | Probe | C++ | Rust |
|---|---|---|---|---|---|---|
| `CAST(0.5 AS INT64)` | `0` | `0` ✓ | | `CAST(2.5 AS INT64)` | `2` | `2` ✓ |
| `CAST(1.5 AS INT64)` | `2` | **`1`** | | `CAST(3.5 AS INT64)` | `4` | **`3`** |
| `CAST(4.5 AS INT64)` | `4` | `4` ✓ | | `CAST(5.5 AS INT64)` | `6` | **`5`** |
| `CAST(-1.5 AS INT64)` | `-2` | **`-1`** | | `CAST(11.5 AS INT64)` | `12` | **`11`** |
| `CAST(1.6 AS INT64)` | `2` | **`1`** | | `CAST(3.99 AS INT64)` | `4` | **`3`** |
| `to_int64(1.5)` | `2` | **`1`** | | `to_int32(3.5)` | `4` | **`3`** |
| `cast(1.5,'INT64')` | `2` | **`1`** | | `CAST(1.5 AS UINT8)` | `2` | **`1`** |

Contrast: **float→DECIMAL rounds half-away-from-zero in BOTH** (`CAST(2.5 AS DECIMAL(4,0))`
→ `3` both; `CAST(3.5…)`→`4` both). So only the float→**integer** path diverges. C++ source:
`numeric_cast.h:56-62` ("PG FLOAT => INT casts use statistical rounding" / `nearbyint`).

### 3.2 [BEHAVIOR] bool↔int CAST — Rust permits, C++ rejects
| Probe | C++ | Rust |
|---|---|---|
| `CAST(true AS INT64)` | Error `Unsupported casting function from BOOL to INT64.` | `1` |
| `CAST(false AS INT64)` | Error (same shape) | `0` |
| `CAST(1 AS BOOL)` | Error `Unsupported casting function from INT64 to BOOL.` | `True` |
| `CAST(0 AS BOOL)` / `CAST(2 AS BOOL)` / `CAST(-1 AS BOOL)` | Error | `False` / `True` / `True` |

### 3.3 Numeric casts that MATCH
int→int overflow (`CAST(128 AS INT8)`, `CAST(256 AS UINT8)`, `CAST(-1 AS UINT8)`, `CAST(-5 AS UINT32)`)
— identical errors both. double→float (`CAST(1e300 AS FLOAT)`→`inf`), FLOAT→DOUBLE, `CAST(NULL AS T)`,
int↔DECIMAL, `CAST(5 AS SERIAL/UINT128)`, `CAST(-5 AS UINT128)`, int-width arithmetic
(`INT8+INT8→INT8`, `INT16+INT32→INT32`) — all match. **Numeric→STRING uses fixed 6-dp both**:
`CAST(1.5 AS STRING)`=`1.500000`, `CAST(100.0 AS STRING)`=`100.000000`, `CAST(true AS STRING)`=`True`,
INT128→full digits, `CAST(date/interval AS STRING)` match.

### 3.4 [ERRSTR] double→int / decimal overflow message rendering
C++ renders the offending float with `%f` (6 dp, full non-scientific expansion); Rust renders
it as an integer / shortest form. Both error, wording differs.

| Probe | C++ | Rust |
|---|---|---|
| `CAST(1e19 AS INT64)` | `…Value 10000000000000000000.000000 is not within INT64 range` | `…Value 10000000000000000000 is not within INT64 range` |
| `CAST(1e300 AS INT64)` | `…Value <309-digit expansion>.000000 …` | `…Value 170141183460469231731687303715884105727 …` (i128::MAX — Rust converts via saturating i128 first) |
| `CAST(99.999 AS DECIMAL(4,2))` | `…99.999000 is not in DECIMAL(4, 2) range` | `…99.999 is not in DECIMAL(4, 2) range` |

---

## 4. Explicit CAST — string parsing (probe files `p03`, `p13`, `p14`)

### 4.1 DIFFs

| # | Probe | C++ | Rust | Sev |
|---|---|---|---|---|
| S1 | `CAST('2020-02-30' AS DATE)` | Error (parsing date) | **`2020-03-01`** | **[VALUE]** |
| S1 | `CAST('2020-13-01' AS DATE)` | Error | **`2021-01-01`** | **[VALUE]** |
| S1 | `CAST('2020-00-01' AS DATE)` | Error | **`2019-12-01`** | **[VALUE]** |
| S1 | `CAST('2021-02-29' AS DATE)` | Error | **`2021-03-01`** | **[VALUE]** |
| S1 | `CAST('01-01-2020' AS DATE)` | Error | **`0006-07-13`** | **[VALUE]** |
| S2 | `CAST('2020-01-01 25:00:00' AS TIMESTAMP)` | Error (hour>23) | **`2020-01-02 01:00:00`** | **[VALUE]** |
| S3 | `CAST('2020/01/01' AS DATE)` | `2020-01-01` | Error (`not a valid DATE`) | [BEHAVIOR] |
| S4 | `CAST('+5' AS INT64)` (also INT32/UINT8) | Error | **`5`** | [BEHAVIOR] |
| S4 | `CAST('+1.5' AS DOUBLE)` / `CAST('+1.5' AS DECIMAL(4,2))` | Error | **`1.500000` / `1.50`** | [BEHAVIOR] |
| S5 | `CAST('05' AS INT64)` / `CAST('007' AS INT64)` | Error (leading zero) | **`5` / `7`** | [BEHAVIOR] |
| S5 | `CAST('01.5' AS DOUBLE)` / `CAST('00' AS DOUBLE)` | Error | **`1.500000` / `0.000000`** | [BEHAVIOR] |
| S6 | `CAST('-3 days' AS INTERVAL)` | Error (parsing interval) | **`-3 days`** | [BEHAVIOR] |
| S7 | `CAST('1.5 hours' AS INTERVAL)` / `'1.5 seconds'` | `01:30:00` / `00:00:01.5` | Error | [BEHAVIOR] fractional units |
| S8 | `CAST('2 decades' AS INTERVAL)` | `20 years` | Error | [BEHAVIOR] extended units |
| S8 | `CAST('1 millennium' AS INTERVAL)` | `1000 years` | Error | [BEHAVIOR] |
| S9 | `CAST('["a","b"]' AS STRING[])` | `["a","b"]` (quotes kept in value) | **`[a,b]`** (quotes stripped) | **[VALUE]** |
| S9 | `CAST("['a','b']" AS STRING[])` | `['a','b']` | **`[a,b]`** | **[VALUE]** |

**S1/S2 (VALUE, HIGH):** C++ validates DATE/TIME components (`isValid`: month 1-12, day ≤
days-in-month leap-aware, hour<24) and errors on out-of-range. Rust silently *normalizes*
(chrono-style roll-over) — `2020-02-30`→`2020-03-01`, and pathologically `01-01-2020`
(read as y=1,m=1,d=2020) → `0006-07-13`. Silent wrong results on malformed input.

**S9 (VALUE):** C++ string→LIST/nested parse does **not** treat interior quotes as delimiters —
`"a"` becomes the literal 3-char value `"a"`. Rust strips surrounding quotes → `a`. Element
*count* matches (both split into 2). This also surfaces on ingestion: `tinysnb`
`organisation.state.location` row with a double-quoted CSV cell renders `["vanco,uver north area"]`
in C++ but `['vanco,uver north area']` in Rust (probe `p10`).

### 4.2 String parsing that MATCHES (high confidence)
- **INT/DOUBLE:** leading/trailing whitespace stripped both; `'5.0'`/`'1e3'`→INT rejected both;
  `'0'`, `'-0'`, `'-5.5'`→DOUBLE accepted both; empty/`'abc'`/`'5abc'`/`'0x10'`/`'1_000'` rejected both.
- **DOUBLE specials:** `inf`, `-inf`, `Infinity`→`inf`, `nan`/`NaN`, `1e400`→`inf` (no error), `.5`, `5.` — all match.
- **BOOL:** `t`/`1`→True, `f`/`0`→False, `true`/`false` (any case), `' true '`; `yes`→error
  `Value yes is not a valid boolean` — all match.
- **DATE:** `2020-1-1` (single digit), leap `2020-02-29` accepted both.
- **TIMESTAMP (+_NS/_MS/_SEC/_TZ):** ` `/`T` separator, `.123456`, `+02` offset applied to plain
  TIMESTAMP too, `Z`, date-only — all match. Nanos truncated to micros both.
- **INTERVAL:** `1 year`, `1 year 2 months`, `3 days`, combined `…04:05:06`, `90 minutes`,
  `1 week`→`7 days`, `36 hours`, `100 microseconds`, `12:34:56` — all match.
- **UUID:** case-insensitive, hyphens-anywhere / no-hyphens both accepted → lowercased; `not-a-uuid`
  → `Invalid UUID: not-a-uuid` both.
- **BLOB:** `\xAB` (lexer hex-escape), non-printable→`\x00\x01\xFF` (uppercase), printable ASCII raw.
- **Nested:** `CAST('[1,2,3]' AS INT64[])`, unquoted `[a,b]`, `{a: 1}`→STRUCT, `{a=1,b=2}`→MAP,
  ARRAY `INT64[3]` incl. length-mismatch error — all match.

### 4.3 [ERRSTR] string-parse error-wording DIFFs (both reject, wording differs)
- Invalid date: C++ `Error occurred during parsing date. Given: "…". Expected format: (YYYY-MM-DD)`
  vs Rust `Cast failed. … is not a valid DATE.` (same for TIMESTAMP, `2020-01-01x`, `infinity`, `epoch`).
- Malformed nested: C++ `Cast failed. {a: 1 is not in STRUCT(a INT64) range.` vs Rust `expected {…} but got {a: 1`.
- ISO interval `P1Y2M3D`/`PT1H30M`, `1 year ago` → both reject, wording differs.
- BLOB `\xGG` → both reject (C++ at lex, Rust `invalid hexadecimal escape…`).

---

## 5. DECIMAL (probe file `p04`) — mostly MATCHES

Arithmetic precision/scale inference, cast rounding (half-away), rendering, and the odd
negative-subunit placement all **match** (Rust deliberately mirrors C++):

| Probe | C++ = Rust |
|---|---|
| `typeof(CAST(1.5 AS DECIMAL(4,2)) + 1)` | `DECIMAL(5, 3)` |
| `typeof(dec(4,2) + dec(4,2))` | `DECIMAL(5, 3)` |
| `typeof(dec(4,2) * dec(4,2))` → value | `DECIMAL(9, 4)` → `2.2500` |
| `typeof(dec / dec)` → value | `DOUBLE` → `3.333333` |
| `typeof(dec(4,2) + 1.0)` | `DOUBLE` |
| `CAST(1.5 AS DECIMAL(10,4))` (trailing zeros) | `1.5000` |
| `CAST(1.555 AS DECIMAL(4,2))` / `1.545` | `1.56` / `1.55` (half-away) |
| `CAST(-0.001 AS DECIMAL(5,3))` | `0.0-1` (shared insertDecimalPoint quirk) |
| `CAST(100 AS DECIMAL(4,2))` overflow | both error |
| `CAST(<38 digits> AS DECIMAL(38,0))` | full digits both |
| `CAST('1e2' AS DECIMAL(6,2))` (exponent) | both reject |

DECIMAL DIFFs are the **common-type** cases (§6, D-rows) and the `+` prefix (§4, S4), not the
arithmetic itself.

---

## 6. Implicit coercion & common-type (probe file `p11`) — verified live

Rust's fixed-target gate `assignable` matches C++ `hasImplicitCast` for the numeric↔numeric
catch-all and the →STRING exclusion set `{Blob, InternalId, Node, Rel, RecursiveRel}`. The DIFFs
are in **common-type selection** (lists/CASE/coalesce) and **function-argument coercion**:

| # | Probe | C++ | Rust | Sev |
|---|---|---|---|---|
| I1 | `upper(123)` | `123` (int→STRING) | Error `expected a STRING argument, got INT64` | [BEHAVIOR] |
| I1 | `lower(<uuid>)` | `550e8400-…` | Error `…got UUID` | [BEHAVIOR] |
| I1 | `size(123)` | `3` (→"123"→len) | Error (binder, wrong args) | [BEHAVIOR] |
| I2 | `typeof(['2', 1])` / `RETURN ['2',1]` | Error `…STRING but expected INT64…` | `STRING[]` / `[2,1]` | [BEHAVIOR] |
| I2 | `typeof(['a', 1])` | Error | `STRING[]` | [BEHAVIOR] |
| I3 | `typeof([date(...), timestamp(...)])` | `TIMESTAMP[]` (DATE widens) | Error `TIMESTAMP but expected DATE` | [BEHAVIOR] |
| D1 | `typeof([CAST(1 AS DECIMAL(4,2)), 1])` | `DECIMAL(21, 2)[]` | `DOUBLE[]` | **[TYPE/VALUE]** |
| D1 | value `[CAST(1 AS DECIMAL(4,2)), 1]` | `[1.00,1.00]` | `[1.000000,1.000000]` | **[VALUE]** |
| D2 | `typeof([DECIMAL(4,2), DECIMAL(6,3)])` | `DECIMAL(6, 3)[]` | `DOUBLE[]` | **[TYPE]** |
| M1 | `typeof([CAST(1 AS UINT8), CAST(1 AS INT8)])` | `INT16[]` | `INT8[]` | **[TYPE/VALUE]** |
| M2 | `typeof([CAST(1 AS UINT32), CAST(1 AS INT16)])` | `INT64[]` | `UINT32[]` | **[TYPE/VALUE]** |

**MATCHES:** `concat('x',1)`=`x1`, `concat(1,2)`=`12`, `concat(date,'x')` (concat *does* coerce
in both); `[1,'2']`/`[1,'a']` → both error (leading-numeric rejects trailing string); `[1,date(...)]`
→ both error (errstr: C++ `TO_DATE(…)` vs Rust `date(…)` internal-name only); `typeof(coalesce(1,2.0))`=`DOUBLE`;
`typeof(CASE WHEN true THEN 1 ELSE 2.0 END)`=`INT64` both; mixed-sign **comparison**
(`INT8 < UINT64`, `UINT8=INT8`) correct both.

**M1/M2/D1/D2 root cause:** C++ `tryGetMaxLogicalType` runs a mixed-sign integral join
(UINT8+INT8→INT16, UINT32+INT16→INT64) and a precision-preserving DECIMAL combine; Rust
`common_type` uses `IntKind::combine` (widest byte-width, tie→signed) and collapses any DECIMAL
pairing to DOUBLE. I2 root cause: Rust `common_type` is **order-dependent** (keeps first concrete
type, then coerces the rest via →STRING) whereas C++ list binding coerces to the leading element's
type without a STRING-always-cast, so a leading string + numeric errors in C++ but yields STRING[]
in Rust.

---

## 7. Comparison, ordering, hashing (probe files `p05`, `p05b`, `p07`, `p09`)

### 7.1 [BEHAVIOR] comparison type-checking — C++ strict binder, Rust permissive
C++ rejects incomparable types at bind time; Rust silently compares and returns a bool/NULL.

| Probe | C++ | Rust |
|---|---|---|
| `1 = true` | Error `Type Mismatch: Cannot compare types INT64 and BOOL` | `False` |
| `1.0 = true` | Error `… DOUBLE and BOOL` | `False` |
| `true = 1` / `CAST(1 AS INT8) = true` | Error | `False` |
| `[1] = [true]` | Error `… INT64[] and BOOL[]` | `False` |
| `date('2020-01-01') = 1` | Error `… DATE and INT64` | `False` |
| `1 < true` | Error | `` (NULL) |
| `1 = NULL IS NULL` (→ `1 = (NULL IS NULL)`) | Error `… INT64 and BOOL` | `False` |

(STRING-vs-numeric like `'a' = 1` errors **identically** in both — `Cast failed. Could not convert "a" to INT64.` — because it triggers a string→int cast attempt, not a type-mismatch.)

### 7.2 [VALUE] NaN in DISTINCT / grouping
| Probe | C++ | Rust |
|---|---|---|
| `UNWIND [nan, nan] AS x RETURN count(DISTINCT x)` | `2` | `1` |

C++ treats each NaN as distinct (comparison semantics, NaN≠NaN); Rust collapses them (bitwise
hash equality). List/scalar NaN *comparison* (`nan = nan`→False) matches both.

### 7.3 Comparison / ordering / hashing that MATCHES
- Cross-numeric `=` and `<`: `1 = 1.0`→True, `1 = 1.5`→False, `INT8 1 = INT64 1`, `1 = DECIMAL 1`,
  `1.0 = FLOAT 1`, all `<` cross-type — match.
- NULL: `1 = NULL`→NULL, `NULL = NULL`→NULL, `NULL IS NULL`→True — match.
- `-0.0 = 0.0`→True, `[0.0 < -0.0]`→False, `1.0/0.0`→`inf`, `-1.0/0.0`→`-inf` — match.
- Boolean ordering `False < True`; string collation byte-order (`'a'<'B'`→False, `'Z'<'a'`→True,
  `'A'<'a'`→True); list ordering (`[1]<[1,2]`, `[1,2]<[1,3]`) — match.
- **ORDER BY NULL positioning:** nulls LAST on ASC, FIRST on DESC — match (numeric, string, bool).
- `-0.0`/`0.0` render `-0.000000`/`0.000000` — match.
- **Cross-type equality via `list_contains`** (hash/equality proxy): `list_contains([1.0],1)`→True,
  `list_contains([1],1.0)`→True, `list_contains([1],CAST(1 AS INT8))`→True, `list_contains([-0.0],0.0)`→True,
  `list_contains([1.5],1)`→False — all match. So numeric cross-width/cross-kind value-equality
  agrees; only the **NaN DISTINCT** case (7.2) diverges.
- Within-type DISTINCT/GROUP BY counts (int, float, string incl. `'a'`≠`'A'`), `count(DISTINCT)` with
  nulls (nulls excluded) — match.

### 7.4 [BEHAVIOR] `IN` operator unsupported in Rust
`RETURN 1 IN [1.0]` and `… WHERE x IN [1,3]` → Rust `Parser exception: expected Eof but found
Ident("IN")`; C++ `True`/rows. The list-membership `IN` operator is entirely unparsed in Rust
(not just RETURN position). Not strictly a type issue but blocks `IN`-based coercion; recorded as a
related gap.

---

## 8. Rendering (probe file `p06`, `p08`, `p10`) — near byte-identical

**One rendering DIFF** (inline nested string quoting), everything else matches byte-for-byte.

| # | Probe | C++ | Rust | Sev |
|---|---|---|---|---|
| R1 | `{a: ['x','y']}` | `{a: [x,y]}` | `{a: ['x','y']}` | **[RENDER]** |
| R1 | `{a: {b: ['x','y']}}` | `{a: {b: [x,y]}}` | `{a: {b: ['x','y']}}` | **[RENDER]** |
| R1 | `[{s: ['a','b']}]` | `[{s: [a,b]}]` | `[{s: ['a','b']}]` | **[RENDER]** |

**R1 (CONFIRMED):** for an **inline-constructed** STRUCT literal whose field is a STRING list,
C++ renders the elements **unquoted**; Rust single-quotes them (its rule: "a STRUCT forces Nested
mode → string list/map elements quote"). Note the Rust rule *does* match C++ for **stored** STRUCT
column properties (`tinysnb` `organisation.state` rows 1,3 → `['toronto','montr,eal']` both), so the
divergence is specifically inline literals. Scalar string struct fields (`{a: 'x'}`→`{a: x}`) and
top-level nested string lists (`[['x','y']]`) match.

**Rendering that MATCHES (byte-for-byte):**
- Floats: `%f` 6-dp; `1e300`→full 309-digit `.000000` expansion (identical); `1e-300`/`0.0000001`→`0.000000`;
  `123456789.123456789`→`123456789.123457`; FLOAT `0.1`→`0.100000`, `1e30`→`…040.000000`.
- DATE/TIMESTAMP: `TIMESTAMP_TZ`→`…+00`, `_NS`/`_SEC`/`_MS` render; `.123456` micros.
- INTERVAL: negatives `-1 days`/`-1 months`, mixed `1 day 05:30:00`, `36:00:00`, micros `00:00:00.0001`.
- BLOB: `hello`→`hello`, `\x00\x01\xff`→`\x00\x01\xFF` (uppercase hex, non-printable escaped).
- UUID: uppercase input → lowercase output.
- NULL in containers: `[1,,3]`, `{a: 1, b: }`, `{a: }`, `[null]`→`[]`, `[[null],[]]`→`[[],[]]` — match
  (NB harness renders NULL and `''` identically; corroborated by both engines behaving the same).

---

## 9. Type-alias / target-name DIFFs (probe file `p12`)

| Probe | C++ | Rust | Sev |
|---|---|---|---|
| `CAST(1 AS REAL)` | `FLOAT` | **`DOUBLE`** | **[TYPE/VALUE]** |
| `CAST(1 AS INTEGER)` | Error `INTEGER is neither an internal type…` | **`INT64`** | [BEHAVIOR] |
| `CAST('{}' AS JSON)` | `{}` | Error `data type JSON is not supported` | [BEHAVIOR] (see §1) |

**REAL is the notable value bug:** C++ `REAL`≡`FLOAT` (4-byte), Rust maps `REAL`→`Double`
(`types.rs:589` `"DOUBLE" | "REAL" => Double`). A `REAL` column/cast is single-precision in C++,
double in Rust — different range, precision, and rendering.

**Aliases that MATCH:** `INT`→`INT32` (both), `NUMERIC`→`DECIMAL(18,3)`, `BYTEA`→`BLOB`,
`BOOL`/`BOOLEAN`, `TIMESTAMP_S`≡`TIMESTAMP_SEC`, `SERIAL`. (Rust *also* accepts `TEXT`→STRING and
`FLOAT8`/`FLOAT4` are C++-only — not probed exhaustively; **[SUSPECTED]** minor alias-set drift.)
POINTER → both error (errstr differs).

---

## 10. Implicit-cast rule comparison (C++ binder vs Rust binder) — [code-only + live]

C++ has **three** cast mechanisms; Rust collapses them into two.

| Mechanism | C++ | Rust |
|---|---|---|
| **A. Overload cost model** `getCastCost` (weights 100–160, strictly widening) — ranks function-overload candidates | `built_in_function_utils.cpp:73` | **ABSENT** — one result-type fn per name, no overload ranking |
| **B. Fixed-target gate** (assignment / param / CASE / list element) — permissive, all numeric↔numeric | `hasImplicitCast` `vector_cast_functions.cpp:256` (numeric catch-all `:294`) | `assignable` `koko-binder/src/lib.rs:4101` |
| **C. Common/max type** of N operands (list/CASE/coalesce/comparison) | `tryGetMaxLogicalType` `types.cpp:1810` | split: `common_type` `lib.rs:4456` + `comparison_common_type` `koko-function/src/lib.rs:1198` + `scalar_result_type` `:68` |

### Rules where Rust MATCHES C++ (confirmed)
- Numeric→numeric all-directions incl. narrowing in the fixed-target gate (`assignable` `:4114`).
- →STRING exclusion set `{Blob, InternalId, Node, Rel, RecursiveRel}` identical.
- Integer-literal magnitude typing INT64→INT128→UINT128 (except the −2^63 edge, §2 L1).
- Arithmetic result-type widening (any DOUBLE→DOUBLE, FLOAT→FLOAT, else widest int); DECIMAL
  arithmetic params `+ - * / %` preserved.
- `is_numeric` set (incl. SERIAL, DECIMAL, UINT128) identical.

### Rules Rust MISSES / under-accepts (confirmed live where noted)
1. **Function-argument implicit-cast-to-STRING** — C++ coerces every arg to the overload param
   type (`bind_function_expression.cpp:110`); Rust doesn't route args through `assignable`, so
   `upper(123)`, `lower(uuid)`, `size(123)` fail at runtime (§6 I1, **live**). (concat is special-cased
   in Rust and does coerce.)
2. **No overload cost model** (mechanism A absent) — no candidate ranking / no preference for
   no-cast or string overloads.
3. **DATE→TIMESTAMP not assignable** — C++ `hasImplicitCast(DATE,TIMESTAMP)`=true; Rust
   `assignable(Date,Timestamp)`=false → DATE+TIMESTAMP list errors (§6 I3, **live**). (Comparison
   still works via runtime path — §6 MATCHES.)
4. **UNION implicit casts** — C++ `hasImplicitCast` handles X→UNION / UNION→UNION; Rust
   `assignable` has no UNION arm → rejects. [code-only]
5. **`common_type` STRING-always-cast + order-independence** — C++ `canAlwaysCast(STRING)` gives
   `max(STRING,X)=X` order-independently; Rust `common_type` keeps first concrete type
   (order-dependent) → `['2',1]` diverges (§6 I2, **live**).
6. **DECIMAL common-type precision** — C++ preserves/ widens DECIMAL; Rust collapses to DOUBLE
   (§6 D1/D2, **live**).
7. **Mixed-sign integral join** — C++ UINT8+INT8→INT16 etc.; Rust widest-then-signed (§6 M1/M2, **live**).
8. **Comparison type-mismatch rejection** — C++ binder errors on BOOL/DATE vs numeric; Rust
   compares silently (§7.1, **live**).

### Rules Rust OVER-accepts vs C++ (confirmed / code-only)
9. **bool↔int CAST** allowed in Rust, rejected in C++ (§3.2, **live**).
10. **Comparisons** across incompatible types return a bool instead of erroring (§7.1, **live**).
11. **String→number `+` prefix and leading zeros** accepted in Rust, rejected in C++ (§4 S4/S5, **live**).
12. **DATE/TIMESTAMP component roll-over** accepted in Rust, rejected in C++ (§4 S1/S2, **live**).
13. **`INTEGER` type alias** accepted in Rust, rejected in C++ (§9, **live**).
14. **STRUCT field-name case-insensitivity** in `assignable` (C++ case-sensitive); **ARRAY length
    ignored** in `assignable` (C++ requires equal length). [code-only — not reachable via probe]

Rust docs (`docs/pi/feature-gaps/implicit-cast-matrix.md`) already track item 1; the claim that Rust
aligns with C++ `hasImplicitCast` for numerics + →STRING is **accurate for the fixed-target gate**
but does **not** extend to common-type selection (items 5-7), function-arg coercion (item 1), or
comparison type-checking (item 8), which are the real holes.

---

## 11. Prioritized DIFF summary

**[VALUE] (silent wrong results — fix first)**
1. float→int cast rounding: C++ half-to-even, Rust truncate — all widths/forms (§3.1).
2. Invalid DATE/TIMESTAMP component roll-over: Rust normalizes, C++ rejects (§4 S1/S2).
3. Mixed-sign int & DECIMAL+numeric common-type in collections (§6 M1/M2/D1/D2).
4. String→LIST quote retention: C++ keeps quote chars, Rust strips (§4 S9).
5. `REAL` alias → FLOAT (C++) vs DOUBLE (Rust) (§9).
6. NaN DISTINCT: C++ 2, Rust 1 (§7.2).
7. Inline nested-struct string quoting (§8 R1).

**[BEHAVIOR] (accept-vs-reject / error-vs-value)**
8. Comparison type-mismatch: C++ errors, Rust returns bool (§7.1).
9. bool↔int cast permissiveness (§3.2).
10. Function-arg implicit→STRING: C++ coerces, Rust rejects (§6 I1).
11. String→number `+`/leading-zero acceptance (§4 S4/S5).
12. DATE `/` separator; INTERVAL negatives / fractional / extended units (§4 S3/S6/S7/S8).
13. DATE+TIMESTAMP & STRING-leading list common-type (§6 I2/I3).
14. `INTEGER` alias; JSON type missing; `IN` operator unparsed (§9, §1, §7.4).
15. `-2^63` literal typed INT128 not INT64 (§2 L1); `^` power op unsupported (§2 L2).

**[ERRSTR] (wording only — lowest)** double/decimal overflow float rendering; invalid
date/timestamp/nested-cast wording; 2^128 literal (Conversion vs Parser exception); `to_json`
/ POINTER wording; list internal fn-name (`TO_DATE` vs `date`).

**Overall:** rendering and DECIMAL arithmetic are essentially byte-identical; the type *inventory*
is complete bar JSON. The material gaps are (a) float→int rounding mode, (b) lenient string/date
parsing (Rust accepts inputs C++ rejects, and normalizes invalid dates), (c) the binder being far
more permissive than C++ for cross-type comparisons and bool/int casts, and (d) common-type
selection for mixed-sign integers, DECIMAL, and DATE/TIMESTAMP in collections.
