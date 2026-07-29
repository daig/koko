# A10 — Test-Harness Fidelity Audit: Rust runner vs C++ test framework

**Question:** Is the Rust `.test` runner more lenient than the C++ test framework, so
that pass counts are inflated and deviations hidden?

**Headline:** **No systematic pass-inflation was found.** The Rust runner's error and
row-comparison semantics are faithful to C++ (exact error match after rtrim; default
lexicographic row sort; `ok` ignores output; missing/short result blocks are hard
parse-errors, not silent passes). Its divergences overwhelmingly run in the **stricter /
more-conservative** direction — parse-errors, unsupported result forms, no JSON
canonicalization, ignored `-CHECK_COLUMN_NAMES`, un-substituted variables — which produce
FAILs/SKIPs (and *hidden* cases), not false PASSes. Genuine leniency exists only in the
narrow `-CHECK_PRECISION` path (2 files / 3 directives) and two structural "ignored
directive" cases (`-WASM_ONLY`, header-level `-SKIP`) that in this corpus produced FAILs,
not passes. The real fidelity cost is **hidden coverage**, not inflated passes.

Sources:
- C++ parser: `/Users/dai/code/koko/test/test_runner/test_parser.cpp` (+ `include/test_runner/test_parser.h`)
- C++ comparator: `/Users/dai/code/koko/test/test_runner/test_runner.cpp` (+ `test_runner.h`, `test_group.h`)
- C++ driver: `/Users/dai/code/koko/test/runner/e2e_test.cpp`, `test/graph_test/private_graph_test.cpp`, `base_graph_test.{h,cpp}`, `test/test_helper/test_helper.cpp`
- Rust runner: `/Users/dai/code/koko-rs/crates/koko-test-runner/src/lib.rs` (+ `main.rs`)
- Corpus outputs: `scratchpad/corpus/*.txt` (53 buckets); C++ source corpus: `/Users/dai/code/koko/test/test_files/` (477 `.test` files)
- Corpus output totals: **PASS 1257, SKIP 341, FAIL 351, parse-error 28** (parse-errors are per-file lines, not counted in the 1949 case rows)

All probe `.test` files run with `KOKO_DATASET_DIR=/Users/dai/code/koko/dataset target/release/koko-test <file>` live in `scratchpad/probe/`.

---

## 1. Directive comparison table

Legend: **SAME** = semantically equivalent; **DIFF** = handled but diverges; **SKIP-CASE** =
Rust disables the whole case with a reason; **IGNORED** = silently dropped (case still runs);
**PARSE-ERR** = whole file collapses to one FAIL; **N/A** = not a real C++ directive.

| Directive | C++ (file:line) | Rust (lib.rs:line) | Verdict |
|---|---|---|---|
| `-DATASET <TYPE> <name>` | `extractDataset` parser.cpp:63-98 (uses `params[2]`) | header loop 363-368, `dataset=parts[1]` | **DIFF** — Rust ignores `<TYPE>`, name-only; `CSV_TO_PARQUET(x)`/`CSV_TO_JSON(x)` forms not parsed → treated as literal dir name → skipped. `empty` match is **case-sensitive lowercase** (601) → uppercase `EMPTY` skipped (see §4). |
| `-STATEMENT` | parser.cpp:416-427; engine splits multi-stmt via `getNextQueryResult` (runner.cpp:166-174) | `parse_one_statement` 215-345; **runner** splits on `;` (`split_statements` 126-164) | **DIFF (mostly SAME)** — Rust splits `;` itself and requires **exact 1:1** stmt↔`----` count (324-330). C++ chains results and clamps to last block (`std::min`, runner.cpp:180). Rust is *stricter* (extra/short blocks → PARSE-ERR). |
| `[conn] …` prefix | regex `\[(conn.*?)\]` parser.cpp:788 (**must start with "conn"**) | `extract_conn_prefix` 170-190 (**any identifier** `[A-Za-z_]\w*`) | **DIFF (benign)** — Rust accepts any `[ident]`; C++ only `conn*`. Corpus only uses `conn1/2/…`, so no observed effect. |
| `----  ok` | runner.cpp:183-186 `ASSERT_TRUE(isSuccess())` | 263, run_statement 729 | **SAME** — success only, output ignored. Probe `ok_ignores.test` → PASS. |
| `----  error` | runner.cpp:187-192 — `rtrim` both, `ASSERT_EQ` (**exact**); multi-line joined `\n` (`extractTextBeforeNextStatement` 274-289) | 264-289 error branch; compare 731-740 (`trim_end` both, `==`) | **SAME** — exact match after right-trim. Probes: exact→PASS, prefix→FAIL, substring→FAIL, trailing-WS→PASS, internal-WS→FAIL. |
| `----  error(regex)` | runner.cpp:193-198 `std::regex_match` (full match) | none — `spec` falls to numeric parse → `Err` | **PARSE-ERR** — `invalid result count \`error(regex)\``. Whole file → 1 FAIL. **13 files / 24 directives.** Verified via `regex.test`. |
| `----  hash` | parser.cpp:246-252 reads `N tuples hashed to <md5>`; runner.cpp:236-247 MD5 compare | none → numeric parse `Err` | **PARSE-ERR** — `invalid result count \`hash\``. **4 files / 10 directives.** Verified via `hash.test`. |
| `----  N` + tuples | parser.cpp:253-269; runner.cpp checkPlanResult | 290-317; `compare_rows` 752-785 | **DIFF** — see JSON-canonicalization gap §3. Row count mismatch → FAIL both. |
| `----  N` + `<FILE>:name` | parser.cpp:257-260 (answers path, **no trim**) | 296-305 RowsFile; resolved 707-717 (`trim_end` each line) | **DIFF (minor)** — Rust right-trims answer-file lines; C++ keeps them verbatim. |
| `-CHECK_ORDER` | parser.cpp:440-442; sort suppressed runner.cpp:254, 397 | 489-493 / 240-242; `ordered` 769 | **SAME** — default sorts both sides; flag preserves order. Probes `sort_default`→PASS, `sort_ordered`→FAIL. |
| `-CHECK_PRECISION` | parser.cpp:443-445; runner.cpp:248-252 — **requires CHECK_ORDER** (ASSERT 249); 1-ULP on **FLOAT/DOUBLE-typed** cols vs raw double (`checkResultNumeric` 341-385, `precisionEqual` 88-100) | 494-498 / 244-246; `cells_match_precision` 789-804 | **DIFF — LENIENT** (see §3, finding L1). |
| `-CHECK_COLUMN_NAMES` | parser.cpp:446-448; prepends colnames row, `numTuples++` runner.cpp:232-234, 390-392 | in `DIRECTIVE_PREFIXES` (97) but **no handler** | **IGNORED** → column-name row treated as data → row-count mismatch → **FAIL** (`colnames.test`). **1 file / 2 directives.** Not a false pass. |
| `-LOG` | parser.cpp:303-309 (own statement type) | 483-488 / 247-249 (label only) | **SAME (benign)** — informational. |
| `-SKIP` (per-case) | parser.cpp:647-660 → `DISABLED_` prefix | 412-418 → `skip`, reason `-SKIP` | **SAME** — C++ gtest skips `DISABLED_`. 219 corpus skips. |
| `-SKIP` (**in header**) | parser.cpp:190-204 → disables **whole group** | header loop only reads `-DATASET`/`--` → **ignored** | **DIFF — ignored** (see §3, finding L3). **3 files.** |
| `-SKIP_IN_MEM` | parser.cpp:120-127 skip iff `IN_MEM_MODE=true` | 421-428 → skip (Rust is always in-mem) | **SAME (appropriate)** — Rust can't run disk-mode tests; matches C++ in-mem behavior. 34 skips. |
| `-SKIP_WASM`,`-SKIP_MUSL`,`-SKIP_STATIC_LINK`,`-SKIP_*_TESTS`,`-SKIP_COMPRESSION_DISABLED`,`-SKIP_FSM_LEAK_CHECK` | parser.cpp:105-160 conditional on non-standard build | not matched → **IGNORED** (case runs) | **SAME (standard build)** — these do not skip in a normal C++ build either. 24 files. |
| `-WASM_ONLY` | parser.cpp:115-119 skip iff **not** `__WASM__` (i.e. **skips in std build**) | not matched → **IGNORED** (case runs) | **DIFF** — Rust runs a case C++ skips (see §3, finding L2). **1 case.** |
| `-RELOADDB` | parser.cpp:326-330; runner runTest 110-123 (**no-op if inMem**) | 432-435 → dropped | **SAME (in-mem)** — no-op both. |
| `-CREATE_CONNECTION name` | parser.cpp:405-409 | 438-441 (lazy on first `[name]`) | **SAME** — multi-conn cases run. |
| `-DEFINE_STATEMENT_BLOCK` / `]` / `-INSERT_STATEMENT_BLOCK` | parser.cpp:460-474, 677-687 | 444-482 | **SAME** — capture + expand macro blocks. |
| `-PARAMETER name=value` | *not a C++ directive* | 403-411, `parse_param_value` 51-71 | **N/A** — Rust-only (feeds `query_with_params`). C++ inlines params textually via `${}`. |
| `-LOOP` / `-ENDLOOP` | parser.cpp:643-646, 725-785 (unrolls) | `unsupported_directive` 196-207 → **SKIP-CASE** `uses -LOOP` | **SKIP-CASE** — 7 skips. C++ runs. |
| `-BATCH_STATEMENTS` | parser.cpp:428-434; runner runBatchStatements 102-119 | 199-206 → **SKIP-CASE** | **SKIP-CASE** — 25 skips. C++ runs. |
| `-BEGIN/-END_CONCURRENT_EXECUTION` | parser.cpp:311-320; runTest 147-165 | 200-204 → **SKIP-CASE** | **SKIP-CASE** — 5 skips. C++ runs. |
| `-CREATE_DATASET_SCHEMA` / `-INSERT_DATASET_BY_ROW` | parser.cpp:341-354; runTest 166-182 | 203-205 → **SKIP-CASE** | **SKIP-CASE** — 8 skips. C++ runs. |
| `-SET` / `-SET_ENV` | parser.cpp:387-404 (REPEAT/ARANGE/current_timestamp/random, stores vars) | **not in `DIRECTIVE_PREFIXES`** → IGNORED; vars never substituted | **DIFF** — **22 `-SET` uses**; their `${VAR}`s stay literal in queries → FAIL/deflation (see §3, finding L6). |
| `-CHECKPOINT_WAIT_TIMEOUT`,`-BUFFER_POOL_SIZE`,`-TEST_FWD_ONLY_REL`,`-CHECK_STORAGE_VERSION`,`-SET_STORAGE_VERSION`,`-IMPORT_DATABASE`,`-REMOVE_FILE`,`-MULTI_COPY_RANDOM`,`-LOAD_DYNAMIC_EXTENSION` | parser.cpp various | recognized-prefix or unknown → **IGNORED** | **DIFF (mostly benign)** — most are disk-mode/no-op in-mem; `-IMPORT_DATABASE`/`-REMOVE_FILE`/`-MULTI_COPY_RANDOM` change semantics but their datasets are unavailable anyway. |
| `-PARALLELISM` | *not in C++ TOKEN_MAP* | in `DIRECTIVE_PREFIXES` (105) | **N/A** — phantom; 0 corpus uses. |
| `${KOKO_ROOT_DIRECTORY}` | parser.cpp:36, runner.cpp:151 | `expand_corpus_vars` 575-582 | **SAME** — 847 uses. |
| `${DATABASE_PATH}` (65), `${KOKO_EXPORT_DB_DIRECTORY}` (90), `${KOKO_VERSION}` (1), `${STRING_EXCEEDS_PAGE}` etc. | runner.cpp:150-162 + variableMap | **not substituted** | **DIFF** — literal `${…}` remains → FAIL/deflation. Mostly export/import tests Rust can't run anyway. |
| `--` header/body separator | parser.cpp:187-189 | 355-361; **required** (537-539) | **SAME** — both reject a missing separator. |

---

## 2. Skip-reason census (from `scratchpad/corpus/*.txt`)

341 total SKIP lines. Bucketed by reason:

| Reason string | Count | C++ would… | Fidelity meaning |
|---|---:|---|---|
| `(-SKIP)` | 219 | also skip (`DISABLED_`) | **Match** — legitimate. |
| `dataset \`…\` is not available` | 43 | run 19, skip/limit 24 | **Mixed** — see breakdown ↓. |
| `(-SKIP_IN_MEM)` | 34 | run (disk mode) | **Appropriate** — Rust is in-mem only. |
| `(uses -BATCH_STATEMENTS)` | 25 | **run** | **Hidden** — Rust can't; C++ executes. |
| `(uses -CREATE_DATASET_SCHEMA)` | 8 | **run** | **Hidden.** |
| `(uses -LOOP)` | 7 | **run** | **Hidden.** |
| `(uses -BEGIN_CONCURRENT_EXECUTION)` | 5 | **run** | **Hidden.** |

"dataset not available" (43) breakdown by dataset name:
- **`EMPTY` (uppercase) → 19** — C++ **runs** these as an empty DB; Rust skips (case-sensitive bug, §4). **Hidden coverage.**
- `binary-demo → 17` — `-DATASET KOKO` binary format; Rust engine can't load. Appropriate skip.
- `CSV_TO_PARQUET(...) → 7` — Rust doesn't parse the conversion form. C++ converts + runs.

**Cases hidden that C++ would execute:** `25 + 8 + 7 + 5 (unsupported directives) + 19 (uppercase EMPTY) = 64 cases`, plus the CSV_TO_PARQUET conversions. None of these inflate PASS (they are SKIPs), but they shrink the tested denominator and hide whether those cases would pass or deviate.

**Additional hiding not in the SKIP census:**
- **28 parse-error lines** collapse whole files into a single FAIL each, hiding every case in them (tck 8, exceptions 4, copy 3, function 2, …). Driven mostly by `error(regex)`/`hash` result forms and short/missing `----` blocks. `main.rs:38-45` counts a parse-error as one file-level FAIL and `continue`s.
- **1 file (`demo_db`) aborted by a Rust panic** (`crates/koko-expr/src/lib.rs:177:56 index out of bounds`, rc=101). The runner has **no `catch_unwind`** per statement, so a panicking statement takes down the whole process/file — all its cases hidden.

---

## 3. Leniency findings, ranked by pass-inflation potential

### Genuinely lenient (could turn a C++ FAIL/SKIP into a Rust PASS)

**L1 — `-CHECK_PRECISION` is looser and operates on rendered strings. [CONFIRMED]**
Scope: 2 files / 3 directives. Four sub-divergences, all in the lenient direction:
1. **Tolerance:** C++ `precisionEqual` = 1 ULP scaled by `min(|x|,|y|)` (runner.cpp:88-100). Rust `cells_match_precision` = `4·f64::EPSILON·max(|x|,|y|)` (lib.rs:797-802) — ~4× wider and scaled by the larger magnitude.
2. **Rendered vs raw:** C++ compares the **actual double** (`getValue<double>()`) to `stod(expected)`. Rust compares the **6-decimal-rendered string** parsed back to `f64` — so any difference below ~1e-6 is erased before comparison. Probe: `RETURN 1.0000000000000007` vs `1.0` under CHECK_PRECISION → Rust **PASS**; C++ (7e-16 > 1 ULP) would **FAIL**.
3. **Type-blind:** Rust treats **any** cell that parses as `f64` numerically; C++ only FLOAT/DOUBLE-**typed** columns (INT/STRING cols compared as strings). So `"1"` vs `"1.0"`, or a numeric-looking STRING, matches in Rust but not C++.
4. **No CHECK_ORDER requirement:** C++ `ASSERT_TRUE(checkOutputOrder)` (runner.cpp:249) → a CHECK_PRECISION-without-CHECK_ORDER test **fails** in C++. Rust just treats precision as ordered (lib.rs:769) → probe `prec_noorder.test` → **PASS**.
Note: for FLOAT (f32) columns the direction flips — C++ uses the much larger f32 epsilon, Rust the f64 epsilon, so Rust is *stricter* there. Net impact tiny given the 3-directive scope.

**L2 — `-WASM_ONLY` ignored → Rust runs a case C++ skips. [CONFIRMED]**
`extension.test` `WASMExtensionTest` (parser.cpp:115-119 skips when not `__WASM__`). Rust doesn't recognize the directive → runs it. **In this corpus it FAILs** (error-text mismatch), so no inflation observed — but structurally, had it passed it would be a false PASS. 1 case.

**L3 — Header-level `-SKIP` ignored → Rust runs whole-file-disabled cases. [CONFIRMED]**
3 files place `-SKIP` before `--` (`ddl_concurrent_execution`, `copy_special_char`, `tinysnb_parquet`); C++ disables the entire group (parser.cpp:190-204). Rust's header loop ignores it. **In this corpus:** 2 skip for other reasons (concurrent-exec / unavailable dataset), and `copy_special_char.CopySpecialChars` **FAILs** — so a spurious FAIL, not a false PASS. Structural inflation vector nonetheless.

**L4 — Multi-line error trimming asymmetry. [SUSPECTED, negligible]**
Rust right-trims **each** expected line then joins (lib.rs:284-288) but only end-trims the **whole** actual (735); C++ rtrims only the whole expected/actual once (runner.cpp:188-190). A non-final expected line with trailing whitespace would match in Rust but not C++ → false PASS. Requires mid-message trailing whitespace; none seen in corpus.

**L5 — `<FILE>:` answer lines right-trimmed. [SUSPECTED, negligible]**
Rust trims each answer-file line (lib.rs:715); C++ keeps them verbatim (runner.cpp:219). Slightly lenient if an answer line has significant trailing whitespace.

### Stricter than C++ (false-FAIL / deflation — the opposite of inflation)

**L6 — `-SET`/`${}` variables not substituted. [CONFIRMED]** 22 `-SET` uses; `${DATABASE_PATH}` (65), `${KOKO_EXPORT_DB_DIRECTORY}` (90), `${KOKO_VERSION}` (1), and SET-defined vars stay literal → queries fail. Deflation, not inflation.

**L7 — No JSON canonicalization. [CONFIRMED code diff / SUSPECTED impact]** C++ sorts object keys of JSON-valued cells on both sides before comparison (runner.cpp:23-86, applied 257-259). Rust compares rendered strings verbatim (lib.rs:774-783). For genuinely JSON-typed values (quoted keys) whose key order differs from the answer file, Rust FAILs where C++ passes. (Cypher struct rendering with *unquoted* keys is not canonicalized by C++ either — probe `json_canon.test` FAILs in both, so the gap is limited to true JSON output.)

**L8 — `error(regex)`, `hash`, `-CHECK_COLUMN_NAMES` unsupported → FAIL/PARSE-ERR.** Covered above; all conservative.

### Not lenient — audit questions answered directly

- **Error compared exactly (not prefix/substring)?** — **Exact**, after right-trim. Verified (prefix/substring/internal-WS all FAIL).
- **Rows sorted when C++ wouldn't (or vice-versa)?** — **Same**: both sort by default, both honor `-CHECK_ORDER`. Byte-lexicographic on both sides.
- **`ok` treated as pass when rows expected?** — No divergence; `ok` = success in both. Rust never *defaults* to `ok`.
- **Statement PASS if result block missing?** — **No.** Missing `----` → PARSE-ERR (`mm_noresult.test`). Short/extra blocks → PARSE-ERR (`mm_1s2r`, `mm_2s1r`). Stricter than C++.
- **Whitespace normalized?** — Only right-trim (matches C++). Internal whitespace significant on both sides.

---

## 4. Known-gap status (from Rust docs) — verified against current code

| Known gap | Status | Evidence |
|---|---|---|
| `---- hash` unsupported | **STILL PRESENT** | `hash.test` → `parse error … invalid result count \`hash\``. No HASH/MD5 path in lib.rs. |
| `error(regex)` unsupported | **STILL PRESENT** (not in docs list but same class) | `regex.test` → parse error. 13 files affected. |
| `-DATASET CSV EMPTY` uppercase skipped | **STILL PRESENT** | `is_empty` at lib.rs:601 matches only `"empty"|"none"|""` (case-sensitive). C++ `initGraph` lowercases the dataset path (base_graph_test.cpp:57) and `executeScript` no-ops on missing schema/copy files (test_helper.cpp:37-39), so C++ runs `EMPTY` as an empty DB. **19 cases** skipped as `dataset \`EMPTY\` is not available` (e.g. `cast.txt:1`). One-line fix: lowercase-compare the dataset name. |
| Multi-statement blocks | **SUPPORTED** (reimplemented) | `split_statements` (126-164) + 1:1 `----` zip. Diverges from C++ only in requiring exact block count (stricter). |
| `-LOOP` skipped | **STILL SKIPPED** | `unsupported_directive` (196-207) → 7 cases `uses -LOOP`. |

---

## 5. Bottom line for the audit

- **Pass-rate inflation risk: LOW.** No mechanism turns an untested/failed case into a PASS at scale. The only confirmed lenient paths are `-CHECK_PRECISION` (2 files) and two "ignored directive" edges (`-WASM_ONLY`, header `-SKIP`) that produced FAILs, not passes, in this corpus. Estimated inflated passes in the current corpus: **~0** observed; **≤3 cases** structurally at risk.
- **The real cost is hidden coverage, not inflation.** ~64 cases are SKIPped that C++ executes (BATCH/LOOP/concurrent/schema directives + uppercase EMPTY), 28 files collapse to single parse-error FAILs (hiding all their cases, e.g. every `error(regex)`/`hash` file), and 1 file is lost to an un-caught panic. These shrink the denominator and mask deviations — the auditor's stated risk — but they depress, not inflate, the visible pass ratio.
- **Highest-value fidelity fixes** (by cases un-hidden): (1) support `---- error(regex)` and `---- hash` — unblocks 13+4 files and their parse-error census; (2) lowercase the `empty` dataset check — un-hides 19 cases (trivial); (3) `catch_unwind` per statement so one panic doesn't abort a file; (4) honor header-level `-SKIP` and `-WASM_ONLY` to remove the 3-4 structural inflation vectors; (5) `-CHECK_PRECISION` — compare raw values with a 1-ULP, type-aware tolerance.
