# Koko `.test` File Format & Result-Comparison Specification

This is a faithful spec of the C++ test runner (`test/test_runner/test_parser.cpp`, `test/test_runner/test_runner.cpp`, `test/test_helper/test_helper.cpp`, plus headers `test_parser.h`, `test_group.h`, `test_helper.h`) and the runtime driver (`test/runner/e2e_test.cpp`, `test/graph_test/{base,private}_graph_test.cpp`).

## 0. File model, tokenization, and grouping

- A `.test` file is parsed into a `TestGroup` (one group per file). The group name is the file path relative to `test/test_files`, with the extension stripped and `/` and `\` replaced by `~` (`genGroupName`).
- **A test file is invalid (throws) if its filename contains a `-`** (`parseAndRegisterTestGroup`).
- The file has two regions separated by a line that tokenizes to `--` (`TokenType::SEPARATOR`): a **header** (parsed by `parseHeader`) and a **body** (parsed by `parseBody`). The header ends at the first `--` line.
- **Tokenization (`tokenize` + `extractToken`)**: each line is split into whitespace-separated tokens, but single- and double-quoted spans are kept intact (regex: `(?:[^'"\s\\]+|'[^'\\]*(?:\\.[^'\\]*)*'|"[^"\\]*(?:\\.[^"\\]*)*"|\S+)+`). `params[0]` is the directive; `params[1..]` are arguments.
  - A line whose first token starts with `#`, or that is empty, becomes `TokenType::EMPTY` (a comment / blank line — ignored).
  - The directive token is looked up in `TOKEN_MAP`. **Any line whose first token is not a known directive becomes `TokenType::_SKIP_LINE`** — this is how query-continuation lines and result-tuple lines are recognized as "not a directive."
- **`checkMinimumParams(n)`**: requires at least `n` args after the directive, else throws.
- **`paramsToString(startIdx)`**: re-joins `params[startIdx..]` with single spaces (note: this collapses original whitespace runs to single spaces).
- **Variable substitution (`replaceVariables`)**: any `${NAME}` is replaced. Built-in vars set at parser construction: `${KOKO_ROOT_DIRECTORY}`, `${KOKO_VERSION}`, `${KOKO_EXPORT_DB_DIRECTORY}` (a temp export dir). User vars come from `-SET`/`-LOOP`. Applied to queries, error-message expected text, log messages, and tuple result lines (see below).
- At runtime, queries additionally get these literal replacements (`test_runner.cpp::testStatement`): `${DATABASE_PATH}` → parent dir of the db path, `${KOKO_ROOT_DIRECTORY}`, and a fixed set of env vars via `replaceEnv` (`${AZURE_PUBLIC_CONTAINER}`, `${AZURE_ACCOUNT_NAME}`, `${UW_S3_ACCESS_KEY_ID}`, `${UW_S3_SECRET_ACCESS_KEY}`, `${AWS_S3_ACCESS_KEY_ID}`, `${AWS_S3_SECRET_ACCESS_KEY}`, `${GCS_ACCESS_KEY_ID}`, `${GCS_SECRET_ACCESS_KEY}`, `${OLLAMA_URL}`, `${POSTGRES_CONNECTION_STRING}`, `${RUN_ID}`).

---

## 1. Header directives (before the `--` separator)

A header is valid only if it sets a dataset (group invalid otherwise: `isValid() = !group.empty() && !dataset.empty()`). Allowed header tokens (anything else throws "Invalid test header statement"):

### `-DATASET <TYPE> <arg> [...]`
Requires ≥2 params. `params[1]` is the type; dataset string usually `params[2]`. Variants (`extractDataset`):

| Syntax | DatasetType | `dataset` value |
|---|---|---|
| `-DATASET CSV <name>` | `CSV` | `<name>` |
| `-DATASET PARQUET <name>` | `PARQUET` | `<name>` |
| `-DATASET PARQUET CSV_TO_PARQUET(<name>)` | `CSV_TO_PARQUET` | inner `<name>` (parsed by stripping the leading `CSV_TO_PARQUET(` (15 chars) and trailing `)`) |
| `-DATASET NPY <name>` | `NPY` | `<name>` |
| `-DATASET KOKO <name>` | `KOKO` | `<name>` |
| `-DATASET JSON <name>` | `JSON` | `<name>` |
| `-DATASET JSON CSV_TO_JSON(<name>)` | `CSV_TO_JSON` | inner `<name>` (strip `CSV_TO_JSON(` (12 chars) + `)`) |
| `-DATASET ICEBUG-DISK <name>` | `ICEBUG_DISK` | `<name>` |

The arg is reassembled with `paramsToString(2)` for the `CSV_TO_*(...)` detection (so the inner expression may contain spaces). Unknown type → throws "Invalid dataset type". (Note: `DatasetType::TURTLE` exists in the enum but has no parse branch.)

### `-BUFFER_POOL_SIZE <bytes>`
Requires ≥1 param. Sets `testGroup->bufferPoolSize = stoll(...)`. Overrides the env/default buffer pool size for this group.

### `-TEST_FWD_ONLY_REL`
Sets `testGroup->testFwdOnly = true` (also legal in body). Interacts with the `DEFAULT_REL_STORAGE_DIRECTION=fwd` env: when that env is `fwd` and a case is **not** fwd-only, the case is auto-disabled (prefixed `DISABLED_`).

### `-CHECKPOINT_WAIT_TIMEOUT <micros>`
(Defined in `TOKEN_MAP`; handled inside `parseStatement` — usable in header-adjacent statement context.) Sets `testGroup->checkpointWaitTimeout`.

### Skip directives (header form)
`-SKIP`, `-SKIP_MUSL`, `-SKIP_WASM`, `-WASM_ONLY`, `-SKIP_IN_MEM`, `-SKIP_VECTOR_CAPACITY_TESTS`, `-SKIP_NODE_GROUP_SIZE_TESTS`, `-SKIP_PAGE_SIZE_TESTS`, `-SKIP_SEGMENT_SIZE_TESTS`, `-SKIP_COMPRESSION_DISABLED`, `-SKIP_STATIC_LINK`.
If `shouldSkip(type)` is true, the **whole group** name is prefixed `DISABLED_` (gtest ignores `DISABLED_*`). Skip semantics (`shouldSkip`):

| Token | Skips when |
|---|---|
| `-SKIP` | always |
| `-SKIP_MUSL` | compiled with `__MUSL__` |
| `-SKIP_WASM` | compiled with `__WASM__` |
| `-WASM_ONLY` | **not** compiled with `__WASM__` |
| `-SKIP_IN_MEM` | env `IN_MEM_MODE == "true"` |
| `-SKIP_COMPRESSION_DISABLED` | env `ENABLE_COMPRESSION == "false"` |
| `-SKIP_VECTOR_CAPACITY_TESTS` | `VECTOR_CAPACITY_LOG_2 != 11` |
| `-SKIP_NODE_GROUP_SIZE_TESTS` | `NODE_GROUP_SIZE_LOG2 != 17` |
| `-SKIP_PAGE_SIZE_TESTS` | `PAGE_SIZE_LOG2 != 12` |
| `-SKIP_SEGMENT_SIZE_TESTS` | `MAX_SEGMENT_SIZE_LOG2 != 18` |
| `-SKIP_STATIC_LINK` | compiled with `__STATIC_LINK_EXTENSION_TEST__` |

---

## 2. Body directives

The body is a sequence of `-CASE` blocks; each case accumulates `TestStatement`s. A statement is built by `parseStatement`, which loops consuming **non-terminal** directives (flags) until it hits a **terminal** directive that returns the statement.

### Case / structure directives (handled in `parseBody`)

- **`-CASE <name>`** — requires ≥1 param. Begins a new test case (gtest test named `<name>` in the group). Resets `testFwdOnly` to the group default. Statements following it attach to `<name>` until the next `-CASE`. There is no explicit case-end; a case ends at the next `-CASE` or EOF.
- **`-DEFINE_STATEMENT_BLOCK <name> [`** — requires ≥2 params (the `[` is the second). Opens a reusable named block; lines up to a line that is exactly `]` (`END_OF_STATEMENT_BLOCK`) are parsed as statements and stored in `testCasesStatementBlocks[name]`. The block name is used as the "test case name" while parsing block statements. Example:
  ```
  -DEFINE_STATEMENT_BLOCK COPY_LDBC_NODES [
  -STATEMENT COPY ...
  ]
  ```
- **`-INSERT_STATEMENT_BLOCK <name>`** — requires ≥1 param. Expands a previously defined block's statements into the current case (`addStatementBlock`); throws if the block name is unknown. Copies the block's connection-name set too.
- **`-LOOP <var> ...`** — requires ≥3 params; body up to `-ENDLOOP`. Two forms (`parseLoop`):
  - Array: `-LOOP <var> [v1,v2,v3]` (exactly 3 params, last bracketed). Values comma-split and trimmed.
  - Range: `-LOOP <var> <start> <end> [step]` (≥4 params; `step` default 1, must be >0). Iterates inclusive `start..end`.
  For each value, `${var}` is set and the loop body re-parsed; generated statements are appended to the current case. `-ENDLOOP` closes the loop.
- **`-LOAD_DYNAMIC_EXTENSION <name>`** — requires ≥1 param. Injects a synthetic `LOAD EXTENSION '<root>/extension/<name>/build/lib<name>.koko_extension'` statement expecting `---- ok`. No-op under `__STATIC_LINK_EXTENSION_TEST__`.
- **`-TEST_FWD_ONLY_REL`** — marks the current case fwd-only.
- Skip directives (same set as header) — here they prefix the **current case name** with `DISABLED_` (not the whole group).

### Statement-building directives (`parseStatement`)

**Terminal** (each returns the statement immediately):

- **`-STATEMENT <query...>`** — the primary directive. After the directive, `paramsToString(1)` forms the first query line; an optional leading `[connName]` is stripped (see §4 connections). Then **continuation**: `extractTextBeforeNextStatement(ignoreLineBreak=true)` consumes following lines while they tokenize to `_SKIP_LINE` (i.e. not a known directive), joining them with **spaces** into the query. This is how multi-line queries work — keep writing query text on following lines; the statement text ends at the next directive line (e.g. `----`, `-STATEMENT`, `-LOG`, blank line). Then `replaceVariables` is applied. `-STATEMENT` is *not* itself terminal — parsing continues to collect modifier directives (`-CHECK_*`) and the `----` result block before returning.
- **`-BATCH_STATEMENTS <FILE>:<relpath>`** — query string of form `<FILE>:name`; resolves to `test/statements/<name>` under root. At runtime each line of that file is run as a separate query against the same expected result (`runBatchStatements`).
- **`-LOG <message...>`** — requires ≥1 param. Sets `statement.type=LOG`; the message (variables replaced) is logged at runtime, no query run. Must appear before the statement is otherwise populated (`validateStatement`).
- **`---- ...`** (`TokenType::RESULT`) — opens the expected-result block (see §3). Calls `extractExpectedResults`, marks statement VALID, returns. A statement may carry multiple result blocks (for multi-statement queries returning multiple results); consecutive `----` lines (separated by EMPTY lines allowed) push additional `TestQueryResult`s.
- **`-BEGIN_CONCURRENT_EXECUTION` / `-END_CONCURRENT_EXECUTION`** — mark concurrent-block boundaries (`ConcurrentStatusFlag::BEGIN/END`). Between them, statements per connection are queued and executed in parallel threads at `END`.
- **`-SKIP_FSM_LEAK_CHECK`** — (token `SKIP_FSM_LEAK_CHECKER`) sets `skipFSMLeakCheckerFlag`; disables the post-test FSM page-leak check for this case.
- **`-RELOADDB`** — sets `reloadDBFlag`. At runtime (non-in-mem only): drops connections, recreates the `Database` and connections from the same on-disk path. In-mem mode: no-op.
- **`-CHECK_STORAGE_VERSION <n>`** / **`-SET_STORAGE_VERSION <n>`** — read/assert or write the on-disk storage version header (non-in-mem only). `CHECK` reopens DB and `ASSERT_EQ`s the header version; `SET` writes the version then reopens.
- **`-CREATE_DATASET_SCHEMA <name>`** — sets `manualUseDataset=SCHEMA`, `dataset=<name>`. At runtime runs `dataset/<name>/schema.cypher`.
- **`-INSERT_DATASET_BY_ROW <name>`** — sets `manualUseDataset=INSERT`, `dataset=<name>`. At runtime uses `InsertDatasetByRow` to insert the dataset row-by-row.
- **`-MULTI_COPY_RANDOM <splits> <table> [SEED <s0> <s1>] <source...>`** — splits a copy into `<splits>` random chunks. Optional `SEED <s0> <s1>` (two int64 seeds) after the table; the rest is the source path (`paramsToString`, variables replaced).
- **`-SET_ENV <NAME> <VALUE>`** — sets a process env var immediately during parsing (`setenv`).
- **`-SET <var> <expr>`** — requires ≥2 params. Evaluates `<expr>` (`parseAndEvaluateFunction`) and stores into `variableMap[var]`. Supported expressions:
  - `REPEAT <n> "<text with ${count}>"` → concatenates `text` `n` times with `${count}` = 1..n.
  - `ARANGE <start> <end>` → string `[start,...,end]` inclusive.
  - `current_timestamp()` → current timestamp value.
  - `random.set_seed(<v>)`, `random.randInt32(<max>)` → seed / random int.
  - quoted `"literal"` → the literal string.
  - otherwise parsed as an int64 literal (else throws).
  (Note: TOKEN_MAP maps `-SKIP_LINE` → `TokenType::SET` as well.)
- **`-IMPORT_DATABASE <path>`** — sets `importDBFlag`, `importFilePath` (vars replaced). At runtime: creates a fresh DB and records the import path; quotes are stripped from the path.
- **`-REMOVE_FILE <path>`** — sets `removeFileFlag`, `removeFilePath` (vars replaced). At runtime deletes that file (quotes stripped).
- **`-CREATE_CONNECTION <connName>`** — requires ≥1 param. Registers an additional named connection for the case (enables multi-connection / concurrent tests).

**Non-terminal modifier flags** (set a bool on the pending statement, parsing continues):

- **`-CHECK_ORDER`** — `checkOutputOrder = true`. Disables result sorting (see §4).
- **`-CHECK_PRECISION`** — `checkPrecision = true`. Enables ULP float/double comparison. **Requires `-CHECK_ORDER` too** (asserted at runtime).
- **`-CHECK_COLUMN_NAMES`** — `checkColumnNames = true`. Prepends a header row of column names (pipe-joined) to the actual result; the expected `---- N` count must include this extra row, and the first expected line is the column-name row.
- **`-CHECKPOINT_WAIT_TIMEOUT <n>`** — sets group-level checkpoint wait timeout (non-terminal).

Unknown directive in statement context → throws "Invalid statement".

---

## 3. Result block format (after `----`)

Driven by `extractExpectedResultFromToken` (`params[1]` after `----`):

### `---- ok`
`ResultType::OK`. Expects query success. No following lines.

### `---- error`
`ResultType::ERROR_MSG`. The following lines (consumed by `extractTextBeforeNextStatement` with `\n` delimiter — i.e. joined by newlines, stopping at the next directive) form the **expected error message**. Variables replaced.

### `---- error(regex)`
`ResultType::ERROR_REGEX`. Following lines joined by newline form a regex pattern; variables replaced.

### `---- hash` (any token starting with `hash`)
`ResultType::HASH`. The **next single line** has the form:
```
<N> tuples hashed to <md5hex>
```
`params[0]` of that line → `numTuples` (int); `params.back()` → the expected MD5 hex string. Example:
```
---- hash
7 tuples hashed to 11065b8084b9a2b8ebe386d22d287117
```

### `---- N` (a non-negative integer) — tuple results
`ResultType::TUPLES` with `numTuples = N`. Two sub-forms based on the **next line**:
- If it starts with `<FILE>:` → `ResultType::CSV_FILE`: the expected tuples live in `test/answers/<rest-after-<FILE>:>`. At runtime that file is read line-by-line; its line count must equal `N`.
  ```
  ---- 5001
  <FILE>:file_with_answers.txt
  ```
- Otherwise → inline tuples: read exactly `N` following lines as the expected tuples. Each line has `replaceVariables` applied and is stored verbatim (whitespace preserved). Example:
  ```
  ---- 4
  Alice
  Bob
  Carol
  Dan
  ```

### Tuple line format
- **Column separator is `|`** (a literal pipe). A tuple with columns `0` and empty is `0|`; `|0` means empty first column then `0`.
- **NULL and empty string**: both render as an empty field. Multi-column rows show NULL/empty as nothing between pipes (e.g. `0|` for `[0, NULL]`). A single-column NULL/empty result is a blank line. Example (`OptionalMatch`): `---- 1` followed by `0|`.
- **Empty result set**: `---- 0` with no following tuple lines.
- Result blocks are separated from the next statement by EMPTY lines; `extractExpectedResults` skips EMPTY lines and stops (rewinding one line) at the first non-`RESULT`, non-EMPTY line.

### How actual tuples are stringified (`convertResultToString`)
- Each result tuple → `tuple->toString(...)` with `|` between columns.
- If `checkColumnNames`, a first row of `colName|colName|...` is prepended (`convertResultColumnsToString`).
- If **not** `checkOutputOrder`, the actual rows are sorted with `std::ranges::sort` (lexicographic byte sort) — comment warns this sort must not change or it breaks hashed cases.

---

## 4. Comparison rules (`checkLogicalPlan` → `checkPlanResult`)

Dispatch is on the expected `ResultType`:

- **OK**: `ASSERT_TRUE(queryResult->isSuccess())`.
- **ERROR_MSG**: expects failure; compares `rtrim(actualError) == rtrim(expected)` — **exact string equality after right-trim** (trailing whitespace stripped from both).
- **ERROR_REGEX**: `std::regex_match(rtrim(actualError), regex(pattern))` — **full-match** (`regex_match`, not search), ECMAScript regex.
- **TUPLES / HASH / CSV_FILE** (default branch): asserts success, then `checkPlanResult`.

### Ordering (default vs `-CHECK_ORDER`)
- **Default: results are sorted.** Both the actual tuples (`convertResultToString` sorts when `!checkOutputOrder`) and the expected tuples (`std::ranges::sort(testAnswer.expectedResult)` when `!checkOutputOrder`) are lexicographically sorted before comparison.
- With **`-CHECK_ORDER`**, neither side is sorted; order must match exactly.

### Row-count check
`EXPECT_EQ(resultTuples.size(), actualNumTuples)` where `actualNumTuples = getNumTuples() (+1 if checkColumnNames)`. For TUPLES the expected count is `numTuples` (`N`, which must already include the column-names row if `-CHECK_COLUMN_NAMES`).

### JSON canonicalization
Before comparing TUPLES, every field that looks like JSON (starts with `{` or `[`) on both expected and actual is canonicalized (`canonicalizeTupleJsonFields`): object keys sorted, recursively, so JSON object key order is **not** significant. Fields are split on `|` for this. Non-JSON fields are compared as-is.

### TUPLES comparison
After sort + JSON-canonicalization, `normalizedResultTuples == normalizedExpectedResult` (full vector equality). On mismatch it diffs per-index. **String comparison is case-sensitive and byte-exact** otherwise.

### HASH comparison
`convertResultToMD5Hash` = MD5 over the `convertResultToString` output, each line followed by a `\n` (including respecting `checkOutputOrder`/`checkColumnNames`). Compared to the expected hex (`ASSERT_EQ`).

### `-CHECK_PRECISION` (float/double tolerance)
`checkResultNumeric`: iterates tuples (in result order; **requires `-CHECK_ORDER`**), splits each expected row on `|`, and per column:
- `FLOAT` → `precisionEqual<float>(actual, std::stof(expected))`
- `DOUBLE` → `precisionEqual<double>(actual, std::stod(expected))`
- otherwise → exact `toString()` string equality.

`precisionEqual(x,y)` (the **default tolerance = 1 ULP**):
```cpp
const T m = std::min(std::fabs(x), std::fabs(y));
const int exp = m < min() ? min_exponent - 1 : std::ilogb(m);
return std::fabs(x - y) <= std::ldexp(std::numeric_limits<T>::epsilon(), exp);
```
i.e. equal iff `|x−y| ≤ 1 ULP` at the magnitude of the smaller operand (subnormals handled). There is no configurable tolerance.

### Multiple results per statement
A query producing multiple `QueryResult`s (chained via `getNextQueryResult`) is checked against `statement.result[min(resultIdx, size-1)]` — extra results past the last expected block reuse the last block.

---

## 5. `-DATASET` mechanism & dataset layout (what a Rust port must reproduce)

Datasets live under `<KOKO_ROOT_DIRECTORY>/dataset/<name>` (the local `dataset/` dir here is an unpopulated submodule). Path resolution and loading (`e2e_test.cpp::setUpDataset` + `base_graph_test.cpp::initGraph` + `test_helper.cpp::executeScript`):

- The full dataset path is `appendKokoRootPath("dataset/" + <name>)` (or, if env `E2E_OVERRIDE_IMPORT_DIR` is set, that dir is used instead of `dataset/`, and loading switches to `IMPORT DATABASE '<dir>'`).
- **`getInputDir()` returns `<datasetPath>/`** (trailing slash).
- For non-`KOKO`, non-`empty`, non-`ICEBUG_DISK` datasets, `initGraph` runs, in order:
  1. `executeScript(<dir>/schema.cypher)` — file constant `SCHEMA_FILE_NAME = "schema.cypher"`.
  2. `executeScript(<dir>/copy.cypher)` — file constant `COPY_FILE_NAME = "copy.cypher"`.
- `dataset == "empty"` → no schema/copy loaded (start empty).
- `DatasetType::KOKO` → copies a pre-built `db.lbdb` (`TESTING_DB_FILE_NAME`) from `<dataset>/db.lbdb` into the test db path instead of running cypher (unless it's a `binary-demo` regeneration case).
- `DatasetType::ICEBUG_DISK` → runs **only** `schema.cypher` (which contains `WITH storage = "..."` clauses pointing at external parquet); no `copy.cypher`.
- `CSV_TO_PARQUET` / `CSV_TO_JSON` → before loading, a `CSVConverter` reads the CSV dataset's `schema.cypher` + `copy.cypher`, builds node/rel `TableInfo`, converts each CSV to `.parquet`/`.json` into a temp dir, writes a new `copy.cypher`, and that temp dir becomes the dataset (cleaned up in TearDown).

### `executeScript` semantics (important for faithful schema/copy loading)
Each line of `schema.cypher`/`copy.cypher` is run as one query, with transformations:
1. Single quotes `'` → double quotes `"`.
2. For each double-quoted substring whose lowercased text contains `.csv`, `.parquet`, `.npy`, `.ttl`, `.nq`, `.json`, or `.koko_extension`: if it's a **relative** path with no parent dir, it's resolved against the cypher file's own directory; if relative **with** a parent dir, against `KOKO_ROOT_DIRECTORY`; absolute paths unchanged. Backslashes normalized to `/`.
3. For `icebug-disk` tables (`format = "...icebug-disk..."`), `storage = "<path>"` values get the same relative-path resolution (remote `://` URIs passed through).
4. Under `__STATIC_LINK_EXTENSION_TEST__`, lines starting with `load extension` are skipped.
5. Any query that fails throws (dataset load is fatal).

`schema.cypher` therefore is a sequence of `CREATE NODE TABLE ... ` / `CREATE REL TABLE ...` DDL; `copy.cypher` is a sequence of `COPY <Table> FROM "<file>" ...` statements with relative file paths resolved as above. A typical CSV test header is just `-DATASET CSV tinysnb` and the runner loads `dataset/tinysnb/schema.cypher` then `dataset/tinysnb/copy.cypher`.

---

## 6. Connections (`[connName]` prefix)

`-STATEMENT [connName] <query>`: `extractConnName` regex `\[(conn.*?)\]\s*(.*)` pulls a leading `[conn...]` token off the query into `statement.connName` (default `conn_default` = `DEFAULT_CONN_NAME`). Combined with `-CREATE_CONNECTION` and `-BEGIN/-END_CONCURRENT_EXECUTION` for multi-connection and concurrent tests; in concurrent blocks each connection's statements are queued and run on separate threads at `-END_CONCURRENT_EXECUTION`.

---

## 7. Relevant default constants

- Default test buffer pool: `DEFAULT_BUFFER_POOL_SIZE_FOR_TESTING = (1<<26) + HASH_INDEX_MEM` (64 MB + hash-index reserve); overridable via env `BUFFER_POOL_SIZE` or `-BUFFER_POOL_SIZE`.
- Default `maxNumThreads` = 2 (env `MAX_NUM_THREADS`).
- Env toggles read in `getSystemConfigFromEnv`: `ENABLE_COMPRESSION`, `CHECKPOINT_THRESHOLD`, `FORCE_CHECKPOINT_ON_CLOSE`, `ENABLE_CHECKSUMS`, `MAX_DB_SIZE`; plus `IN_MEM_MODE`, `DEFAULT_REL_STORAGE_DIRECTION`, `SPARSE_FRONTIER_THRESHOLD`, `E2E_REWRITE_TESTS`/`REWRITE_TESTS`, `TEST_JOBS`.
- Path constants: `E2E_TEST_FILES_DIRECTORY = "test/test_files"`, `TEST_ANSWERS_PATH = "test/answers"`, `TEST_STATEMENTS_PATH = "test/statements"`, `DEFAULT_CONN_NAME = "conn_default"`, `TESTING_DB_FILE_NAME = "db.lbdb"`.

(REWRITE mode — `E2E_REWRITE_TESTS=1`, single-threaded `TEST_JOBS=1` — regenerates expected outputs in place via `generateOutput`/`rewriteTestFile`; a Rust port need only support a `--rewrite` equivalent if desired, mirroring the `---- ok` / `---- N` / `---- hash` / `---- error[(regex)]` emission formats shown in `generateOutput`.)

Source files: `/Users/dai/code/koko/test/test_runner/test_parser.cpp`, `/Users/dai/code/koko/test/test_runner/test_runner.cpp`, `/Users/dai/code/koko/test/test_helper/test_helper.cpp`, `/Users/dai/code/koko/test/include/test_runner/test_parser.h`, `/Users/dai/code/koko/test/include/test_runner/test_group.h`, `/Users/dai/code/koko/test/include/test_helper/test_helper.h`, `/Users/dai/code/koko/test/include/test_runner/csv_converter.h`, `/Users/dai/code/koko/test/runner/e2e_test.cpp`, `/Users/dai/code/koko/test/graph_test/base_graph_test.cpp`, `/Users/dai/code/koko/test/graph_test/private_graph_test.cpp`.