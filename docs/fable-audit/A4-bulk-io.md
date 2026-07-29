# A4 — Bulk-I/O surface audit (copy / csv / load_from / glob / reader / ice_disk / npy / parquet / extension / md5 / binary_demo / storage_version / explain)

Scope: classify every FAIL on the bulk-I/O surface into
(a) file-format/feature gated (expected — P4 not started),
(b) CSV-path WRONG RESULT (Rust loads/parses but produces different values — most important),
(c) error-wording only,
(d) harness limitation,
(e) COPY/CSV option semantics missing or divergent.

Verification: every category-(b) and category-(e) claim below was reproduced live through **both**
engines (C++ oracle `build/release/tools/shell/koko`, Rust `examples/koko_cli` + `diffprobe.py`).
Repro CSVs live in `scratchpad/b_repro/`.

---

## Executive summary

- **The escape/quote "mis-quoting" gap is CONFIRMED and is the only genuine CSV-parse value bug**,
  but its direction is the **opposite** of what the pre-audit gap ledger recorded: **C++ retains**
  the original quote characters of a nested string element as literal content; **Rust strips**
  them. Observable in value, `size()`, and `=` equality—not just rendering. (§B1)
- A second, cleanly-reproducible CSV-parse divergence exists: **escape-strictness inside quoted fields** — C++
  errors when ESCAPE is not followed by QUOTE/ESCAPE; Rust silently drops the escape. (§B2)
- **Two confirmed COPY option bugs**: `COPY … (skip=N)` is **ignored entirely** (§E1), and `COPY … (header=false /
  0 / FaLsE)` is **ignored** (Rust always auto-skips the header) (§E2). These account for the whole
  `copy_multi_boolean` + `copy_with_skip_lines` FAIL cluster.
- **One systematic error-wording gap**: Rust COPY conversion/parse errors drop the C++
  `Copy exception: Error in file <path> on line <N>: … Line/record containing the error: '<record>'` wrapper. (§C1)
- Everything else is **expected P4/format gating** (Parquet, NPY, gz/gzip, multi-file `[…]`, glob `*`/`?`,
  `COPY FROM (subquery)`, `COPY (cols) FROM`, `COPY (…) TO`, `EXPLAIN`, `INSTALL`, `WITH (storage=…)`,
  `IMPORT DATABASE`, `CALL storage_version()`, unsupported PK types, spill-to-disk, `SAMPLE_SIZE`/`LIST_UNBRACED`),
  **bare-`LOAD` type sniffing not implemented** (§B3, a feature gap that surfaces as wrong values), or **harness
  limitations** (`error(regex)`/`hash` result parsing, `-MULTI_COPY_RANDOM` directive).

Rough disposition of the ~93 FAILs on this surface: **(a) ≈ 60**, **(b) = 4** (2 copy_nested + escape divergence
family), **(c) ≈ 6 distinct**, **(d) ≈ 8**, **(e) ≈ 12**. (Sniffing 10× is counted under (a)/(b) hybrid, §B3.)

---

## (b) CSV-path WRONG RESULT  — MOST IMPORTANT

### B1. CONFIRMED — nested string elements: C++ retains source quote chars as content; Rust strips them
`copy/copy_nested.NodeStruct`, `copy/copy_nested.NodeStructReload` (corpus copy.txt:40-41), and much wider.

The C++ `Value::listToString`/`structToString` (`src/common/types/value/value.cpp:1082-1106`) add **no** quotes —
they just join children with `,` / `, `. So any quote you see in C++ output is **stored as literal content**. When
Kùzu's CSV parser reads a nested LIST/STRUCT/ARRAY/MAP literal, it **keeps the surrounding quote characters of each
string element** (whatever quote the source used — single or double). Rust strips them.

Source (`dataset/tinysnb/vOrganisation.csv`, col `state STRUCT(...)`):
- ID 1: `location: ['toronto', 'montr,eal']`   (single-quoted in CSV)
- ID 4: `location: [\"vanco,uver north area\"]`  (double-quoted in CSV)

Live evidence (`diffprobe.py --dataset tinysnb`):

| query | C++ (oracle) | Rust |
|---|---|---|
| `o.state.location[2]` (ID1) | `'montr,eal'` | `montr,eal` |
| `size(o.state.location[2])` | **11** | **9** |
| `o.state.location[2] = 'montr,eal'` | **False** | **True** |
| `o.state.location[1]` (ID4) | `"vanco,uver north area"` | `vanco,uver north area` |
| `size(…)` (ID4) | **23** | **21** |
| `o.state` (ID4) render | `… location: ["vanco,uver north area"] …` | `… location: ['vanco,uver north area'] …` |
| `o.state.location` (ID1) render | `['toronto','montr,eal']` | `[toronto,montr,eal]` |

Both engines split into the **same** element count (`nelem` 2 / 1), so this is **not** comma mis-parsing — purely
quote retention. The value, its length, and equality all diverge.

Why the corpus only flags the double-quote row: Rust's renderer (recently "resolved") adds *single* quotes to
string elements nested inside a STRUCT, which **coincidentally matches** C++'s stored single quotes for IDs 1/6, so
`NodeStruct` looks green there; the ID-4 double-quote element is the visible mismatch. But direct list access
(`o.state.location`), element extraction, `size()`, and `=` all diverge for every quoted element.

Minimal clean repro (`b_repro/struct.csv`, written with exact quotes):
```
1,"{tags: ['alpha', 'be,ta']}","['x','y']"
2,"{tags: [\"dq elem\"]}","[plainA,plainB]"
```
`CREATE NODE TABLE s(id INT64, st STRUCT(tags STRING[]), arr STRING[], PRIMARY KEY(id)); COPY s FROM … (HEADER=false);`

| | C++ `st.tags[1]` / size | Rust | C++ `arr[1]` | Rust |
|---|---|---|---|---|
| id1 | `'alpha'` / 7 | `alpha` / 5 | `'x'` | `x` |
| id2 | `"dq elem"` / 9 | `dq elem` / 7 | `plainA` | `plainA` |

Note it also affects **plain top-level `STRING[]` columns** (`arr`), not just STRUCT-nested lists — any quoted
element in any list/struct/map parsed from CSV. Unquoted elements (id2 `arr`) are identical.

Doc reconciliation: the pre-audit gap ledger recorded this as *Rust* storing doubled quote
characters. That was stale—Rust stored the clean `montr,eal`; C++ carried the quotes. The renderer
fix noted in `docs/pi/correctness-gaps.md:64-90` addressed display but the underlying stored-value
divergence remained. Classification: (b), CONFIRMED. (Arguably Rust was more correct, but that
audit's contract was byte-identical behavior.)

### B2. CONFIRMED — escape-strictness inside quoted fields (Rust lenient, C++ errors)
Surfaced while investigating `copy/copy_special_char.CopySpecialChars` (copy.txt:81; note that `.test` is `-SKIP`
upstream with a TODO acknowledging the ambiguity, so its expected values are aspirational — see §D).

Clean repro `b_repro/esc2.csv` = one line `0|-esc #a here-|q` with `(DELIM="|", ESCAPE="#", QUOTE="-", HEADER=false)`.
The quoted field contains `#a` = ESCAPE followed by an ordinary char.

- **C++**: `Copy exception: Error in file … on line 1: neither QUOTE nor ESCAPE is proceeded by ESCAPE. Line/record
  containing the error: '0|-esc #a...'`
- **Rust**: succeeds → `esc a here` (drops the escape, keeps `a`).

So inside a quoted field, C++ requires ESCAPE to be followed by QUOTE or ESCAPE (else hard error); Rust accepts any
follower and drops the escape. In **unquoted** fields both engines treat the escape char literally (no processing,
no error) — verified in `b_repro/esc.csv`, both produce `unquoted ##hash and #x here` verbatim. The
`CopySpecialChars` corpus FAIL row (`this is a ##plain## #string` vs aspirational `this is a #plain# string`) is
exactly this unquoted case where **neither** current engine processes the escape — i.e. the FAIL is a `-SKIP`/harness
artifact (§D), but the *quoted-field* strictness divergence above is real. Classification: (b)/(e), CONFIRMED.

### B3. Feature-gap surfacing as wrong values — bare `LOAD FROM` type sniffing (all STRING)
`csv/sniffing.{SniffInt,SniffBool,SniffReal,SniffDate,SniffUUID,SniffList,SniffMap,SniffStruct,ExtremeNest,LargeStruct}`
(csv.txt:17-27). Rust registers every column of a schemaless `LOAD FROM` as `STRING`; C++ infers
INT64/BOOL/DOUBLE/DECIMAL/DATE/TIMESTAMP/UUID/LIST/STRUCT/MAP. Confirmed already documented at
`docs/pi/correctness-gaps.md:491-505`.

This is a single missing feature (type inference), but it **does produce wrong values**, not only wrong types.
Live `LOAD FROM real.csv RETURN *`:
- C++: `1.123457 | 12345678.00 | 123.0000 | ` (DOUBLE + DECIMAL(10,2/10,4))
- Rust: `1.123456789012345678901234567890 | 12345678. | 123. | ` (raw STRING text)

Integer/plain-string files render identically (SniffInt block is byte-same), so the impact is confined to
real/decimal/date/uuid/nested. Classification: (a) feature-gated with (b) symptom — flag prominently but it is a
known, systematic P4-adjacent gap, not a parser bug.

---

## (e) COPY / CSV option semantics missing or divergent

### E1. CONFIRMED — `COPY … (skip=N)` is ignored entirely
`copy/copy_with_skip_lines.CopyFromWithSkippedLines` (copy.txt:88).
Controlled repro `b_repro/skip.csv` (header + 5 rows), 1-col INT64 table:

| options | C++ rows | Rust rows |
|---|---|---|
| (none) | 5 | 5 |
| `(skip=2)` | **3** | **5** |
| `(header=true, skip=2)` | **3** | **5** |
| `(header=false, skip=2)` | **4** | **5** |

Rust always loads all rows regardless of `skip`. Classification: (e), CONFIRMED.

### E2. CONFIRMED — `COPY … (header=false / 0 / FaLsE)` is ignored
`copy/copy_multi_boolean.CopyUse0AsFalse`, `CopyUseFaLsEwithdifferentCases` (copy.txt:24-25).
`COPY person FROM dataset/tinysnb/vPerson_less_col.csv (header 0)` — line 1 is the header `id,fname,Gender`; with
header=false it must be parsed as data and fail the INT64 cast of `id`:

- **C++**: errors (all three forms `(header 0)`, `(header FaLSe)`, `(header=false)`): `Copy exception: … Cast failed.
  Could not convert "id" to INT64 …`
- **Rust**: succeeds (loads 5 rows) for all three — it always treats the file as having a header.

Rust's COPY effectively hard-wires header auto-skip. Classification: (e), CONFIRMED. (The `(header)`, `(header 1)`,
`(header true)` positive cases pass, so only the false/0 path is broken.)

### E3. CONFIRMED — LOAD FROM `skip` + default header interaction (off-by-one vs C++)
`copy/copy_with_skip_lines.LoadWithSkippedLines` (copy.txt:87). `diffprobe.py` on `dataset/tinysnb/vPerson.csv`
(header + 5 data rows):

| options | C++ count | Rust count |
|---|---|---|
| (none) | 5 | 5 |
| `(skip=1)` | **4** | **5** |
| `(skip=2)` | **3** | **4** |
| `(header=true)` | 5 | 5 |
| `(header=false)` | 6 | 6 |
| `(header=false, skip=2)` | 4 | 4 |

Both agree on header detection and on `skip` when header is explicit. The divergence is only **default header +
skip**: C++ applies `skip` *on top of* header auto-detection (rows = data − skip); Rust lets `skip` *subsume* the
header (rows = physical − skip), yielding one extra row. Unlike COPY (§E1), LOAD FROM does honor `skip`.
Classification: (e), CONFIRMED.

### E4. Not-implemented CSV options (explicit gate)
- `SAMPLE_SIZE` → `Not implemented exception: CSV SAMPLE_SIZE is not supported in this phase`
  (`csv/edge_cases.*` ×4 dataset-load, `csv/sniffing.HeaderTest`, csv.txt:9-12,25).
- `LIST_UNBRACED=TRUE` → `Not implemented exception: CSV LIST_UNBRACED=TRUE is not supported in this phase`
  (`csv/unbraced_lists.Unbraced`, csv.txt:29).
Classification: (e)/(a) — gated, expected.

### E5. SUSPECTED — `DELIM="\\t"` rejected in the dataset-loader path
`copy/copy_snap_amazon0601_csv.CopySNAPAmazon0601CSV` (copy.txt:77); also tracked at
`docs/pi/correctness-gaps.md:52`. `dataset/snap/amazon0601/csv/copy.cypher` uses `(DELIM="\\t")`. Rust: `Binder
exception: Copy csv option value must be a single character with an optional escape character`. **Could not
reproduce via CLI** — direct `DELIM="\t"` *and* `DELIM="\\t"` both load correctly in both engines (`b_repro/tab.csv`,
`b_repro/dbl.cypher`). The failure is specific to how the `koko-test` dataset loader passes/unescapes the option
value, so it is a loader-escaping interaction (borderline (e)/(d)). SUSPECTED.

---

## (c) Error-wording only

### C1. Systematic — COPY errors drop the C++ file/line/record context wrapper
Rust detects the same error but omits C++'s `Copy exception: Error in file <path> on line <N>: … Line/record
containing the error: '<record>'` envelope. Confirmed instances:
- `copy_map_with_duplicate_key.CopyDuplicateKeyError` (copy.txt:19): C++ `Copy exception: Error in file … on line 2:
  Conversion exception: Map does not allow duplicate keys. Line/record containing the error: '3,"{dan=52,…}"'`
  vs Rust `Conversion exception: Map does not allow duplicate keys.` (verified live).
- `copy_multi_boolean.NewKeyWordDELIMITER`, `HybridTABandSpace` (copy.txt:26-27): C++ `… expected 3 values per row,
  but got 1. Line/record containing the error: 'id,fname,Gender'` vs Rust `Runtime exception: Table person8 expects
  3 columns but line 1 of the CSV has 1.`
Classification: (c), systematic across the COPY error surface.

### C2. COPY success message wording
Rust: `N tuples have been copied to table.` vs C++: `N tuples have been copied to the <name> table.` (verified live,
every COPY). Normally masked by the harness (`_is_setup_noise` drops "…tuples…copied…"), but visible in
`CopyFromWithSkippedLines` once the count already differs. Classification: (c), cosmetic/systematic.

### C3 / C4. Parser-vs-binder stage & dialect wording
- `copy_with_skip_lines.CopyFromInvalidSkipNum` (copy.txt:89): `(skip=2.5)` — C++ `Binder exception: The type of csv
  parsing option SKIP must be a INT64.` vs Rust `Parser exception: expected a string/int/bool/list CSV option value,
  found Float(2.5)` (Rust rejects one stage earlier). (c).
- `csv/dialect_detection.DIFFERENT_DELIMITER` (csv.txt:5): both error on the column-count, different wording; tied to
  delimiter dialect detection (`DIFFERENT_QUOTE`/`DIFFERENT_ESCAPE`/`LOAD_FROM_LIST` pass). (c)/(a).
- `csv/compressed_csv.{CorruptGZIP,ReadFromParquetError}` (csv.txt:2-3): expected specific IO wording, got the
  generic gz-extension gate — subsumed by the gz feature gate. (a).

---

## (a) File-format / feature gated — EXPECTED (P4 not started)

Gating feature named per group. All produce a clean parser/binder/"not supported in this phase" gate.

| Gating feature | Tests (corpus refs) |
|---|---|
| **Parquet reader** ("Cannot load from file type parquet … load the extension") | copy_node_parquet ×2, copy_parquet, copy_snap_amazon0601_parquet, copy_snap_twitter_parquet (copy.txt:43-44,53,78,80); reader.compression ×3, reader.timestamp ×2 (reader.txt); parquet.tinysnb (SKIP, dataset n/a) |
| **NPY reader** (`COPY t FROM (f0,f1,…)` → "found LParen") | copy_npy_* ×6 (copy.txt:45-50); npy_1d.MatchNpy_1d (npy_1d.txt) |
| **gz/gzip compression** ("Cannot load from file type gz/gzip") | compressed_csv.{SCAN_COMPRESSED_CSV,CorruptGZIP,ReadFromParquetError,ReadFromGZIPExtension} (csv.txt:1-4) |
| **Multi-file list** `COPY … FROM ["a.csv","b.csv"]` ("found LBracket") | copy_large_serial ×2 (copy.txt:13-14), copy_multiple_files.{CopyMultipleFilesTest,CopyFilesWithWrongPath,CopyFilesWithSearchPath} (copy.txt:29,31-32) |
| **Glob `*` / `?`** (Rust: "No such file or directory", no expansion) | copy_long_string_multiple_files (copy.txt:17), copy_multiple_files.{CopyFilesWithWildcardPattern,CopyFilesWithHomeDir} (copy.txt:30,33), glob.longpath.LongPath (glob.txt) |
| **`COPY … FROM (subquery)`** ("found LParen") | copy_from_table_func, copy_from_tinysnb.CopyFromSubquery (copy.txt:6-7), copy_partial_column.RelPartialColumnsTest (copy.txt:54), segmentation ×3 (copy.txt:102-104) |
| **`COPY tbl (col,…) FROM`** (partial column list; "expected keyword FROM but found LParen") | copy_partial_column ×2 (copy.txt:54-55) |
| **`COPY (query) TO`** (export; "expected an identifier, found LParen") | copy_to_big_results, copy_to_csv.TinySnbCopyToCSV, copy_to_parquet (copy.txt:83-84,86) |
| **`IMPORT DATABASE`** (parser gate) | import_legacy_relgroup_db (copy.txt:96) |
| **`EXPLAIN`** (parser gate) | explain.Explain (explain.txt); export_explain ×4 SKIP |
| **`INSTALL <ext>`** (parser gate) | extension.{InstallUnofficialExtensions,WASMExtensionTest} (extension.txt) |
| **`WITH (storage=…)`** ice_disk ("expected Eof but found Ident WITH") | ice_disk.* ×4 (ice_disk.txt) — incl. RelIceDiskNodeRegular which *succeeds* (storage clause ignored) instead of the expected mix-guard binder error |
| **`CALL storage_version()`** ("not supported in this phase") | storage_version (storage_version.txt) |
| **Unsupported PK type (P4 storage)** ("Unsupported primary key type in this phase") | copy_pk_basic.{CopyBlobPK,CopyDoublePK,CopyFloatPK} (copy.txt:58,69-70) |
| **Buffer-manager / spill-to-disk (P4)** | spill_to_disk.DisableSpillToDisk (copy.txt:105) — expected OOM, Rust has no BM limit |
| **Legacy rel-group table naming** | copy_from_tinysnb.CopyFromLegacyRelGroup (copy.txt:8) |
| **Binary / CSV_TO_PARQUET datasets not present** | binary_demo ×17 SKIP, copy_pk_long_string_parquet SKIP, tinysnb_parquet SKIP |
| **`&` bitwise-AND operator** (parser) | csv/typed_headers.TypedHeaders (csv.txt:28) — fails on `RETURN height & 3` before typed-header parsing is even reached |

---

## (d) Harness limitations

- **`---- error(regex)` result assertions** not parsed by the Rust runner → whole `.test` skipped as "parse error":
  copy_after_error, copy_pk_duplicate, export_import_db (copy.txt:1,71,95), csv/errors, load_from/load_from
  (load_from.txt), ice_disk_invalid_storage (ice_disk.txt:2). Documented `docs/pi/correctness-gaps.md:512`.
- **`---- hash` result assertions** not parsed → md5testing/md5 (md5testing.txt).
- **`-MULTI_COPY_RANDOM` directive** not implemented → the randomized multi-COPY never runs, so the table is empty:
  multi_copy_node.{CopyLargeIntRandom,CopyLargeIntRandomSeeded} report `count(*) = 0` vs 200000 (copy.txt:99-100).
  The non-random `CopyLargeInt` (explicit COPY statements) passes with 200000, and `serialtable_merged.csv` exists
  (199999 lines) — so this is purely the missing directive, **not** a COPY data bug.
- **`-SKIP` not honored for `copy_special_char`** → the runner executed a test that upstream skips and compared
  against aspirational (unimplemented) expected values; the only *real* divergence it exposes is §B2.
- `-SKIP_IN_MEM` / `-RELOADDB` handling accounts for the export_import_db and copy_to_csv.CopyToInvalidCase SKIPs.

---

## Per-file FAIL tally (this surface)

| corpus file | pass | skip | fail | dominant category |
|---|---|---|---|---|
| copy.txt | 43 | 10 | 52 | mostly (a); (b)=2 nested; (e)=5; (c)=4; (d)=5 |
| csv.txt | 6 | 0 | 23 | (a) sniffing×10 + SAMPLE_SIZE + gz; (c)=2; (a) `&` |
| load_from.txt | 0 | 0 | 1 | (d) error(regex) |
| glob.txt | 0 | 0 | 1 | (a) glob |
| reader.txt | 0 | 0 | 5 | (a) Parquet |
| ice_disk.txt | 0 | 0 | 5 | (a) WITH storage + (d) error(regex) |
| npy_1d.txt | 0 | 0 | 1 | (a) NPY |
| parquet.txt | 0 | 1 | 0 | — (dataset n/a) |
| extension.txt | 0 | 3 | 2 | (a) INSTALL |
| md5testing.txt | 0 | 0 | 1 | (d) hash |
| binary_demo.txt | 0 | 17 | 0 | — (dataset n/a) |
| storage_version.txt | 0 | 0 | 1 | (a) storage_version() |
| explain.txt | 0 | 4 | 1 | (a) EXPLAIN |

---

## Bottom line for the port

The implemented CSV COPY / LOAD FROM path is largely correct. The **only genuine value-level CSV-parse deviations**
are:
1. **§B1** nested-string quote retention (C++ keeps source quotes as content, Rust strips) —
   CONFIRMED, affects value/length/equality, and the pre-audit gap note was stale/reversed.
2. **§B2** escape-strictness inside quoted fields (Rust lenient, C++ errors) — CONFIRMED.

Plus **two COPY option regressions** — `skip` ignored (§E1) and `header=false/0` ignored (§E2) — and one systematic
**COPY error-context wrapper** gap (§C1). All remaining FAILs are expected P4/format gating, the known bare-LOAD
sniffing gap, or harness limitations.
