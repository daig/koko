# Koko Query-Result Value Formatting Specification

This spec documents the EXACT string rendering produced by C++ `Value::toString()` (`src/common/types/value/value.cpp`) and its helpers, as consumed by the `.test` comparison corpus. The Rust port must reproduce these byte-for-byte.

## 0. Tuple / column framing (context)

A result tuple is rendered by `FlatTuple::toString()` (`src/processor/result/flat_tuple.cpp:49`): each column's `Value::toString()` is joined with a single `|` (pipe, no surrounding spaces) and the line is terminated with `\n`.

```cpp
for (auto i = 0ul; i < values.size(); i++) {
    if (i != 0) { result += "|"; }
    result += values[i].toString();
}
result += "\n";
```

The header row (`MaterializedQueryResult::toString`, `materialized_query_result.cpp:73`) joins `columnNames` the same way (`|`, then `\n`). The `colsWidth`/`delimiter` overload is only for the pretty-printed shell box; the `.test` corpus uses the plain overload above. All per-value rules below describe one cell.

Top-level dispatch is `Value::toString()` (`value.cpp:591`), keyed on `LogicalTypeID`.

---

## 1. NULL and empty string

- **NULL**: the very first check in `Value::toString()`:
  ```cpp
  if (isNull_) { return ""; }
  ```
  A NULL value of ANY type renders as the **empty string** `""` (not the literal `null`). In a tuple this means the cell is empty (e.g. `Alice||30` for a middle NULL).
- **Empty STRING**: a non-null empty string also renders as `""` (`strVal` is returned directly). NULL and empty-string are therefore indistinguishable in the textual result.

---

## 2. BOOL

`TypeUtils::toString(const bool&)` (`type_utils.cpp:124`):
```cpp
return val ? "True" : "False";
```
- `true` → `True`
- `false` → `False`

Capital first letter, rest lowercase. (NULL bool → `""`.)

---

## 3. Integer types (INT8/16/32/64, UINT8/16/32/64, SERIAL)

These go through the generic template `TypeUtils::toString<T>` (`type_utils.h:46`) which calls `std::to_string(val)`. Plain base-10 decimal, leading `-` for negatives, no thousands separators, no `+`.

| Type | LogicalTypeID | Example value | Rendered |
|------|---------------|---------------|----------|
| INT8 | INT8 | -12 | `-12` |
| INT16 | INT16 | 300 | `300` |
| INT32 | INT32 | 0 | `0` |
| INT64 / SERIAL | INT64 / SERIAL | 9223372036854775807 | `9223372036854775807` |
| UINT8..UINT64 | UINT* | 255 | `255` |

`SERIAL` is dispatched identically to `INT64` (`value.cpp:598-600`).

### INT128 / UINT128

INT128 uses `Int128_t::toString` (`int128_t.cpp:61`): manual base-10 long division, leading `-` for negatives, `INT64_MIN`-style minimum handled specially, zero → `0`. UINT128 uses `UInt128_t::toString`. No separators. Examples: `170141183460469231731687303715884105727`, `-5`.

---

## 4. FLOAT and DOUBLE — fixed 6 decimal places

**This is the load-bearing rule.** Both FLOAT and DOUBLE fall through the generic template (no specialization exists in `type_utils.cpp` — only int128/uint128/bool/internalID/temporal/string/blob/uuid/list/map/struct/union are specialized). So:

```cpp
// type_utils.h:46
template<typename T>
static inline std::string toString(const T& val, void* = nullptr) {
    ...
    return std::to_string(val);   // float & double land here
}
```

`std::to_string(double)` / `std::to_string(float)` in the C++ standard is defined as `sprintf` with `"%f"`, i.e. **always exactly 6 digits after the decimal point**, with trailing zeros preserved (no scientific notation, no trimming).

| Value (double or float) | Rendered |
|-------------------------|----------|
| 37.25 | `37.250000` |
| 1.731 | `1.731000` |
| 0.0 | `0.000000` |
| -2.5 | `-2.500000` |
| 1000000.0 | `1000000.000000` |
| 3.0 | `3.000000` |

This matches the agg-test strings `37.250000` and `1.731000` cited in the task. FLOAT (32-bit) renders the same way (6 decimals) but the underlying value is the float-rounded magnitude before formatting, so e.g. a float storing `1.731` may print whatever `%f` of the promoted float yields — the *format* is identical (6 decimals); only the stored binary value differs. Rust must use `format!("{:.6}", x)` semantics (equivalently `%f`).

Caveat for very large magnitudes / special values: `%f` of `inf`/`nan` yields `inf`/`-inf`/`nan` (platform libc dependent); not normally exercised by the corpus.

---

## 5. DECIMAL — scale-driven, no `%f`

DECIMAL does NOT use float formatting. `Value::decimalToString()` (`value.cpp:1152`) renders the backing integer (INT16/32/64/128 depending on precision) via `TypeUtils::toString` (plain integer), then inserts a decimal point using `DecimalType::insertDecimalPoint(value, scale)` (`types.cpp:60`):

```cpp
if (positionFromEnd == 0) return value;          // scale 0 → no dot
if (positionFromEnd > value.size()) {            // need leading zeros
    retval = "0.";  // then (scale - len) zeros, then digits
} else {
    retval = value.substr(0, len-scale);         // integral part
    if (retval=="" || retval=="-") retval += '0';// ensure a leading 0
    retval += "."; retval += value.substr(len-scale); // fractional part
}
```

Behavior — `scale` = the declared decimal scale, applied to the stored unscaled integer:

| Backing int | Scale | Rendered |
|-------------|-------|----------|
| 3725 | 2 | `37.25` |
| 1731 | 3 | `1.731` |
| 5 | 3 | `0.005` |
| 50 | 3 | `0.050` |
| -5 | 3 | `-0.005` |
| 1234 | 0 | `1234` |
| 100 | 2 | `1.00` |

Trailing zeros ARE kept (driven purely by scale — `100` with scale 2 → `1.00`). Negative values with magnitude smaller than scale get `-0.00x`. No thousands separators. NULL → `""`.

---

## 6. STRING / JSON

`value.cpp:647-649`: returns `strVal` verbatim — **no surrounding quotes**, no escaping. A string `Alice` renders as `Alice`. JSON physical strings render identically (raw stored text). Empty string → `""` (indistinguishable from NULL).

---

## 7. BLOB — `\xAA` hex escaping

`Blob::toString(bytes, len)` (`blob.cpp:65`). Per byte:
- "Regular" char (`c >= 32 && c <= 126 && c != '\\' && c != '\'' && c != '"'`, i.e. printable ASCII excluding backslash, single-quote, double-quote) → emitted as-is.
- Everything else → `\x` + two uppercase-table hex digits (`HEX_TABLE`).

```cpp
result += '\\'; result += 'x';
result += HEX_TABLE[byte >> 4];
result += HEX_TABLE[byte & 0x0F];
```

Examples:
- bytes `{0xAA}` → `\xAA`
- bytes `{0x00, 0x41}` (`\0`,`A`) → `\x00A`
- byte `0x5C` (`\`) → `\x5C` (backslash is escaped, not literal)
- byte `0x27` (`'`) → `\x27`; `0x22` (`"`) → `\x22`
- `Hello` (all printable) → `Hello`

Hex digits come from `HEX_TABLE` — confirm case in the header (the parse map accepts both cases; emission uses the table). Standard koko/koko output is uppercase `\xAA`.

---

## 8. UUID

`UUID::toString(int128)` (`uuid.cpp:80`). Canonical 36-char lowercase form `8-4-4-4-12`:
```
xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx
```
Hex digits from `HEX_DIGITS` (lowercase). Note the internal storage flips the high bit (`high ^ (1<<63)`) for sort-ordering; `toString` flips it back before formatting, so the rendered value is the natural UUID. Example: `a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11`.

---

## 9. DATE — `YYYY-MM-DD`

`Date::toString` → `DateToStringCast` (`cast_helpers.h:79`). Format: year (minimum 4 digits, zero-padded; more digits if year ≥ 10000), `-`, 2-digit month, `-`, 2-digit day. If year ≤ 0, the year is converted to `(-year)+1` and a BC suffix (`Date::BC_SUFFIX`, ` (BC)`) is appended.

| date | Rendered |
|------|----------|
| 2023-11-14 | `2023-11-14` |
| year 5, Jan 3 | `0005-01-03` |
| year 0 → | `0001-01-01 (BC)` |

Always zero-padded month/day to 2 digits.

---

## 10. TIME component — `HH:MM:SS[.ffffff-trimmed]`

`Time::toString` → `TimeToStringCast` (`cast_helpers.h:133`). Format `HH:MM:SS` (each 2-digit zero-padded). If microseconds == 0, stops at 8 chars (`HH:MM:SS`). Otherwise appends `.` + microseconds written as 6 digits **with trailing zeros trimmed** (e.g. micros `900000` → `.9`, micros `24000` → `.024`, micros `123456` → `.123456`). This is used inside TIMESTAMP and INTERVAL.

---

## 11. TIMESTAMP variants

All via `Timestamp::toString` (`timestamp_t.cpp:220`):
```cpp
return Date::toString(date) + " " + Time::toString(time);
```
→ `YYYY-MM-DD HH:MM:SS[.fff]` (space separator, fractional seconds trimmed as in §10).

| LogicalTypeID | Conversion before format | Suffix |
|---------------|--------------------------|--------|
| TIMESTAMP | raw micros | — |
| TIMESTAMP_NS | `fromEpochNanoSeconds` | — |
| TIMESTAMP_MS | `fromEpochMilliSeconds` | — |
| TIMESTAMP_SEC | `fromEpochSeconds` | — |
| TIMESTAMP_TZ | base render **+ `+00`** | `+00` |

`TIMESTAMP_TZ` (`type_utils.cpp:154`): `toString(timestamp) + "+00"` — always the literal `+00` suffix (UTC). Example: `2023-11-14 12:30:00+00`. Plain `TIMESTAMP` example: `2023-11-14 12:30:00` or with fraction `2023-11-14 12:30:00.123`.

---

## 12. INTERVAL

`Interval::toString` → `IntervalToStringCast::Format` (`cast_helpers.h:199`). Components are emitted in order **years, months, days, time**, space-separated, omitting zero components:

1. If `months != 0`: split into `years = months/12`, `months%12`. Each non-zero emitted as `<n> year`/`<n> month` with a trailing `s` when value ≠ 1 (so `1 year`, `3 years`, `2 months`). Leading space before each component if buffer non-empty.
2. If `days != 0`: `<n> day`/`<n> days` (same pluralization).
3. If `micros != 0`: a space (if anything precedes), then time as `HH:MM:SS[.ffffff-trimmed]`:
   - Negative micros → leading `-` then absolute time.
   - Hour is zero-padded to ≥2 digits (`if (hour < 10) buffer += '0'`), but hour can exceed 2 digits (e.g. large intervals).
   - Minutes, seconds always 2 digits.
   - Fractional: if leftover micros ≠ 0, `.` + 6-digit micros with trailing zeros trimmed (§10 `FormatMicros`).
4. If everything is zero (`length == 0` and no micros branch): the literal default `00:00:00`.

Examples (matching task):
- months=38, days=2, micros=13h2m → `3 years 2 months 2 days 13:02:00`  *(38 months = 3 years 2 months)*
- task's `3 years 2 days 13:02:00` ⇒ months=36, days=2, micros=46920000000 (13:02:00) → `3 years 2 days 13:02:00` (months%12==0 so no "months" component)
- micros for 18m0.024s → `00:18:00.024`
- empty interval → `00:00:00`
- 1 month → `1 month`; 2 months → `2 months`; 1 day → `1 day`.

Note plural rule keys off `value != 1`, so `-1 day` would be `-1 day` (value is -1, ≠ 1 → gets `s` → `-1 days`; the `value != 1` check is true for -1, so it DOES pluralize: `-1 days`).

---

## 13. INTERNAL_ID — `tableID:offset`

`TypeUtils::toString(const internalID_t&)` (`type_utils.cpp:129`):
```cpp
return std::to_string(val.tableID) + ":" + std::to_string(val.offset);
```

**Format is `tableID:offset`, NOT `offset:tableID`.** The task's `_ID: 0:0` example is ambiguous (both 0); the actual ordering is **table-id first, then offset**. So a node in table 3 at offset 7 → `3:7`. This is used both for standalone INTERNAL_ID columns and inside NODE/REL `_ID`, `_SRC`, `_DST` fields.

---

## 14. LIST / ARRAY — `[a,b,c]`

`Value::listToString()` (`value.cpp:1082`):
```cpp
result = "[";
for each child: result += child->toString(); if not last: result += ",";
result += "]";
```
- `[` + children joined by **`,` with NO space** + `]`.
- Empty list → `[]`.
- NULL element → empty between commas (e.g. `[1,,3]` for a NULL middle element, since child NULL → `""`).
- ARRAY (fixed-size) uses the same `listToString`.
- Nested: `[[1,2],[3,4]]`.

(There is also a vector-backed `TypeUtils::toString(list_entry_t)` at `type_utils.cpp:185` used in column layout; it produces the identical `[a,b,...]` form, empty `[]`.)

> **⚠️ Nested-string quoting (empirical — the corpus, not this `value.cpp`, is the oracle).**
> The `value.cpp` shown here renders *all* string children unquoted, but the committed
> `.test` expected results were generated by a renderer that **single-quotes strings once
> they are inside a `STRUCT`**, and that "nested" context propagates through contained
> `LIST`/`MAP`/`STRUCT`. The corpus is self-consistent within one value — e.g. a single rel
> renders `comments: [rnme,m8s…]` (a list that is a *direct property* — unquoted) yet
> `summary: {locations: ['london','toronto']}` (a list nested in a struct — quoted). The
> precise rule we reproduce (see `value.rs::Value::render`):
> - **Top** context = a result column **or** a node/rel direct property: strings raw; `LIST`
>   propagates Top; `MAP` propagates Top; **`STRUCT` forces Nested onto its field values**.
> - **Nested** context (inside a `STRUCT`): strings `'quoted'`; `LIST`/`MAP`/`STRUCT` propagate Nested.
> - Node/rel properties always render in Top. So `RETURN collect(p.fName)` → `[Alice,Bob]`
>   (unquoted) but `RETURN {a: 'hello'}` → `{a: 'hello'}` (quoted).

---

## 15. STRUCT — `{k: v, ...}`

`Value::structToString()` (`value.cpp:1094`):
```cpp
result = "{";
for i in children: result += fieldNames[i] + ": " + children[i]->toString();
                    if not last: result += ", ";
result += "}";
```
- `{` + `key: value` pairs joined by **`, ` (comma-space)** + `}`.
- Key/value separator is **`: ` (colon-space)**.
- Example: `{a: 1, b: 'hello'}` — **string field values are single-quoted** in the corpus
  (see the nested-string-quoting note under §14; a struct forces its children into the
  "Nested" context).
- Empty struct → `{}` (the vector-backed `structToString` returns `{}` when no fields; the Value-based one yields `{}` when childrenSize==0).
- NULL field values render as `key: ` (empty after colon) — the value-based `structToString` does NOT skip nulls (unlike node/rel).

`RECURSIVE_REL` also dispatches to `structToString()` at the Value level (`value.cpp:662`) — see §18.

---

## 16. MAP — `{k=v, ...}`

`Value::mapToString()` (`value.cpp:1069`):
```cpp
result = "{";
for each entry (a struct with children[0]=key, children[1]=val):
    result += key->toString() + "=" + val->toString();
    if not last: result += ", ";
result += "}";
```
- `{` + `key=value` entries joined by **`, ` (comma-space)** + `}`.
- Key/value separator is **`=`** (no spaces around it).
- Example: `{1=one, 2=two}`.
- Empty map → `{}` (vector-backed variant returns `{}` for size 0).

Note the distinction: STRUCT uses `key: value`, MAP uses `key=value`.

---

## 17. UNION

`value.cpp:657-660`: only the active member is stored at child index 0; rendered as `children[0]->toString()`. So a UNION renders exactly as its active member value (no wrapper, no tag).

---

## 18. NODE — `{_ID: t:o, _LABEL: lbl, prop: val, ...}`

`Value::nodeToString()` (`value.cpp:1108`). Field order is fixed by binding (`bind_graph_pattern.cpp:249`): **field[0] = `_ID` (INTERNAL_ID), field[1] = `_LABEL` (STRING), then user properties in schema order.**

```cpp
if (children[0]->isNull_) return "";            // NULL node (no internal id)
result = "{";
for (i=0..childrenSize):
    if (children[i]->isNull_) continue;          // skip null props
    if (i != 0) result += ", ";
    result += fieldNames[i] + ": " + children[i]->toString();
result += "}";
```

Rules:
- A node whose `_ID` (child 0) is NULL renders as `""` (the whole node is treated as NULL).
- `{` ... `}` wrapper.
- Pairs are `name: value` (`: ` colon-space), joined by `, ` (comma-space).
- **NULL property values are skipped entirely** (no `key: ` emitted), unlike plain STRUCT.
- The `, ` separator is gated on `i != 0`. Subtle edge case: if `_ID` (i=0) is somehow non-null but a later field is the first emitted, the gate `i != 0` can produce a **leading `, `** because the separator condition checks the field index, not whether anything was emitted yet. In practice `_ID` is always present and non-null (it's the nullness sentinel), so the first emitted pair is `_ID` at i=0 and no leading comma occurs. Match this exact gating (`i != 0`) rather than "is-first-emitted".

Field name strings are the literal `_ID`, `_LABEL` (from `InternalKeyword`).

Example:
```
{_ID: 0:0, _LABEL: person, ID: 0, fName: Alice, age: 35}
```
Here `_ID` is `tableID:offset` = `0:0`, `_LABEL` is the table name `person`, then the user-visible properties (including a user property literally named `ID`).

(The column/vector path `TypeUtils::nodeToString` at `type_utils.cpp:269` mirrors this: checks field-vector 0 for null → `""`, else `structToString<true>` which skips null fields and uses the same `{name: value, ...}` formatting.)

---

## 19. REL — `(src)-{_LABEL: lbl, prop: val, ...}->(dst)`

`Value::relToString()` (`value.cpp:1130`). Field order fixed by `getBaseRelStructFields()` (`bind_graph_pattern.cpp:256`): **field[0] = `_SRC` (INTERNAL_ID), field[1] = `_DST` (INTERNAL_ID), field[2] = `_LABEL` (STRING), field[3] = `_ID` (INTERNAL_ID), then properties.**

```cpp
if (children[3]->isNull_) return "";             // NULL rel (no internal _ID)
result = "(" + children[0]->toString() + ")-{";  // children[0] = _SRC
for (i=2; i<childrenSize; ++i):                  // start at _LABEL
    if (children[i]->isNull_) continue;
    if (i != 2) result += ", ";
    result += fieldNames[i] + ": " + children[i]->toString();
result += "}->(" + children[1]->toString() + ")"; // children[1] = _DST
```

Rules:
- Nullness sentinel is **child index 3 (`_ID`)**, not 0. If `_ID` is NULL the rel renders `""`.
- Layout: `(` + `_SRC` internalID + `)-{` + body + `}->(` + `_DST` internalID + `)`.
- Body iterates from **index 2** (so `_LABEL` is the first body field, then `_ID` at index 3, then properties). `_SRC`/`_DST` (indices 0,1) are NOT in the body — they form the endpoints.
- Separator `, ` gated on `i != 2` (mirror exactly; same leading-separator subtlety as NODE — first body field is `_LABEL` at i=2, no leading comma).
- NULL property/body values skipped.
- Endpoint internalIDs use `tableID:offset`.

Example (from task, reconciled with actual field order):
```
(0:0)-{_LABEL: knows, _ID: 3:0, since: 2020}->(0:1)
```
Here `_SRC`=`0:0`, body starts with `_LABEL: knows`, then `_ID: 3:0` (rel's own internal id, `tableID:offset`), then properties, and `_DST`=`0:1` after the arrow.

(Vector path `TypeUtils::relToString` at `type_utils.cpp:277` checks field-vector **3** for null → `""`, then `structToString<true>`. Note: the vector path's generic `structToString` renders ALL fields in order including `_SRC`/`_DST` as plain struct fields — it does NOT build the `(src)-{...}->(dst)` arrow syntax. The arrow syntax comes only from the **Value-level** `relToString`. The `.test` result formatting goes through the Value path (`FlatTuple` holds `Value`s), so the arrow form is authoritative for result comparison.)

---

## 20. RECURSIVE_REL / PATH

At the Value level, `RECURSIVE_REL` dispatches to `structToString()` (`value.cpp:662`). A recursive-rel value is a STRUCT with two children: `children[0]` = `_NODES` (a LIST of NODE values) and `children[1]` = `_RELS` (a LIST of REL values) — see `recursive_rel.cpp` (`getNodes`/`getRels`). So it renders with the generic struct formatter:

```
{_NODES: [<node>,<node>,...], _RELS: [<rel>,<rel>,...]}
```

- Outer `{` ... `}`, fields `_NODES`/`_RELS` (whatever the bound field names are) separated by `, `, `key: value` form.
- `_NODES` value is a LIST → `[` node-strings joined by `,` (no space) `]`, each node rendered per §18.
- `_RELS` value is a LIST → `[` rel-strings joined by `,` `]`, each rel rendered per §19 (with the `(src)-{...}->(dst)` arrow form).
- Because the Value `structToString` does NOT skip nulls, both fields always appear.

Concrete shape:
```
{_NODES: [{_ID: 0:0, _LABEL: person, name: Alice},{_ID: 0:1, _LABEL: person, name: Bob}], _RELS: [(0:0)-{_LABEL: knows, _ID: 3:0}->(0:1)]}
```

---

## 21. POINTER (internal, rarely in results)

`value.cpp:623`: rendered as `TypeUtils::toString((uint64_t)val.pointer)` → plain unsigned decimal of the pointer address. Not normally part of the `.test` corpus.

---

## Quick reference table

| Type | Format | Example |
|------|--------|---------|
| NULL (any) | empty | `` |
| BOOL | `True`/`False` | `True` |
| INT*/UINT*/SERIAL/INT128 | base-10 | `-12`, `300` |
| FLOAT/DOUBLE | `%f`, exactly 6 decimals | `37.250000`, `1.731000` |
| DECIMAL | unscaled int + dot at `scale` from end, scale-driven trailing zeros | `37.25`, `0.005`, `1.00` |
| STRING/JSON | raw, no quotes | `Alice` |
| BLOB | printable ASCII as-is, else `\xHH` | `\xAA`, `Hello` |
| UUID | lowercase `8-4-4-4-12` | `a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11` |
| DATE | `YYYY-MM-DD` (≥4-digit yr, ` (BC)` if ≤0) | `2023-11-14` |
| TIMESTAMP[_NS/_MS/_SEC] | `YYYY-MM-DD HH:MM:SS[.fff]` (fraction trimmed) | `2023-11-14 12:30:00` |
| TIMESTAMP_TZ | above + `+00` | `2023-11-14 12:30:00+00` |
| INTERVAL | `<n> years <n> months <n> days HH:MM:SS[.fff]` (zero parts dropped, plural if ≠1), empty→`00:00:00` | `3 years 2 days 13:02:00`, `00:18:00.024` |
| INTERNAL_ID | `tableID:offset` | `0:0`, `3:7` |
| LIST/ARRAY | `[a,b,c]` (comma, no space) | `[1,2,3]`, `[]` |
| STRUCT | `{k: v, ...}` (`, ` sep, `: ` kv) | `{a: 1, b: hello}` |
| MAP | `{k=v, ...}` (`, ` sep, `=` kv) | `{1=one, 2=two}` |
| UNION | active member only | `42` |
| NODE | `{_ID: t:o, _LABEL: l, prop: v, ...}`, null props skipped | `{_ID: 0:0, _LABEL: person, fName: Alice}` |
| REL | `(src)-{_LABEL: l, _ID: t:o, ...}->(dst)` | `(0:0)-{_LABEL: knows, _ID: 3:0}->(0:1)` |
| RECURSIVE_REL | `{_NODES: [..], _RELS: [..]}` | see §20 |

## Key files
- `/Users/dai/code/koko/src/common/types/value/value.cpp` — `toString` dispatch (591), `mapToString` (1069), `listToString` (1082), `structToString` (1094), `nodeToString` (1108), `relToString` (1130), `decimalToString` (1152).
- `/Users/dai/code/koko/src/common/type_utils.cpp` — type-specific specializations; bool→`True/False` (124), internalID→`tableID:offset` (129), TZ `+00` (154); float/double have NO specialization → generic `std::to_string` (6-decimal `%f`).
- `/Users/dai/code/koko/src/include/common/type_utils.h:46` — generic `toString` template (`std::to_string`).
- `/Users/dai/code/koko/src/include/common/types/cast_helpers.h` — `DateToStringCast` (79), `TimeToStringCast` (133, micros trimming), `IntervalToStringCast::Format` (242).
- `/Users/dai/code/koko/src/common/types/blob.cpp:65` — BLOB `\xHH` escaping (`isRegularChar` at 22).
- `/Users/dai/code/koko/src/common/types/uuid.cpp:80` — UUID formatting.
- `/Users/dai/code/koko/src/common/types/types.cpp:60` — `DecimalType::insertDecimalPoint`.
- `/Users/dai/code/koko/src/common/types/int128_t.cpp:61` — INT128 toString.
- `/Users/dai/code/koko/src/processor/result/flat_tuple.cpp:49` and `/Users/dai/code/koko/src/main/query_result/materialized_query_result.cpp:66` — `|`-joined columns, `\n`-terminated rows.
- `/Users/dai/code/koko/src/binder/bind/bind_graph_pattern.cpp:249-262` — NODE field order (`_ID`,`_LABEL`,…) and REL field order (`_SRC`,`_DST`,`_LABEL`,`_ID`,…).
- `/Users/dai/code/koko/src/include/common/constants.h:28-33` — `InternalKeyword` literals `_ID`/`_LABEL`/`_SRC`/`_DST`.