# Koko first-party CLI implementation plan

> **Status (2026-07-26): completed historical plan; L1–L9 closed on 2026-07-23.**
> Do not resume this document as current sequencing or verification policy.
> [`CLI_UX.md`](CLI_UX.md) owns observable behavior,
> [`CLI_ARCHITECTURE.md`](CLI_ARCHITECTURE.md) owns the implemented boundaries, and
> [`ROADMAP.md`](../ROADMAP.md) owns current scope and work.
> The names, commands, counts, toolchain targets, and comparison gates below are preserved as
> landing evidence.
>
> [`CLI_GOAL_PROMPT.md`](CLI_GOAL_PROMPT.md) is likewise a completed historical bootstrap.

## 1. Outcome and fixed boundary

The completed landing shipped a workspace package named `koko-cli` with installed binary `koko`.
Its four externally visible surfaces share one application core:

1. interactive TTY input;
2. `--command`, `--file`, and piped-stdin batch input;
3. explicit `--init` and `:read` source inclusion; and
4. human, delimited, and machine result destinations, including transactional files.

The CLI opens exactly one `Database::new()` and one `Connection`. Cypher uses the normal parser,
binder, planner, processor, storage, transaction, cancellation, deadline, and memory paths. The
current `crates/koko/examples/koko_cli.rs` remains the narrow differential adapter and keeps its
line-oriented historical-tool contract.

The landing baseline was the completed IM5 gate:

- strict corpus **1785 passed / 343 skipped / exactly 23 deferred-or-ledgered failures**;
- zero panic, unparsed, unledgered, arity, or malformed/missing/stale TRIAGE state;
- all 52 corpus directories, P0/oracle/deviation checks, optimizer/no-optimizer, and one-worker
  invariants green; and
- nine correct, timeout-free LSQB answers with every median Rust/C++ ratio at most 2x and the
  preserved q4/q5/q7 wins.

Those values are historical closure evidence, not current universal landing gates. The C++ engine
and original shell were references for inherited Cypher semantics and useful interaction ideas, not
a bug-for-bug CLI oracle. `CLI_UX.md` remains the Koko CLI contract.

## 2. Decisions fixed before implementation

### 2.1 Crate and dependency set

Add external dependencies at workspace scope, pin the following compatible minor lines in
`Cargo.lock`, and prove the resolution with Rust 1.85 before building CLI modules:

| Purpose | Dependency | Selected line and use |
|---|---|---|
| Argument/help parsing | `clap` | `4.6`; construct `Command` from the central option registry rather than duplicate it in derives |
| Interactive editor | `reedline` | `0.49`, default features only; custom Koko validator, keymap, completer, highlighter, prompt, and history adapters |
| Terminal events/control | `crossterm` | `0.29`; raw mode, dimensions, resize, paste, key events, and guarded restoration |
| Terminal styling | `nu-ansi-term` | `0.50`; only behind the capability/style layer and never in machine renderers |
| Typed serialization | `serde`, `serde_json` | `1`; streaming protocol writes and duplicate-aware custom parameter visitors, not generic-map round trips |
| Configuration | `toml_edit` | `0.25`; typed keys plus source spans/provenance |
| Platform directories | `directories` | `6`; user config and state roots only |
| Unicode | workspace `unicode-segmentation`, `unicode-width` | `1` and `0.2`; grapheme movement and terminal display width |
| Signals | `ctrlc` | `3.5`; process signal notification into atomics/wakeup only |
| File transactions | workspace `tempfile` | `3`; securely created sibling files plus narrow platform replacement code |
| Delimited output | workspace `csv` | `1`; RFC 4180 CSV only; TSV keeps its explicit encoder |
| Machine values | `base64` | `0.22`; standard padded RFC 4648 blobs |
| Suggestions | `strsim` | `0.11`; bounded, high-confidence command/keyword suggestions |
| History locking | `fd-lock` | `4`; private, race-aware history updates without a database |
| Errors | workspace `thiserror` | `2`; typed CLI/process errors without replacing engine `Error` |
| PTY verification | `portable-pty` | `0.9`, dev-only; real Unix PTY and Windows ConPTY harness |
| Subprocess/property checks | `assert_cmd`, `predicates`, `proptest` | `2`, `3`, and `1`, dev-only |

Use `std::sync::mpsc::sync_channel` for the one-request/one-outcome worker boundary. Do not add an async
runtime, a general event bus, a second JSON object model, a shell parser, or a terminal UI framework.
Optional `reedline` SQLite, clipboard, shell-expansion, and bashism features remain disabled.

If one selected minor line no longer resolves on Rust 1.85, pin the newest patch or preceding minor
with the required API and record the reason here before CLI code depends on it. Do not raise the
workspace MSRV as a dependency workaround.

### 2.2 Package and module shape

`crates/koko-cli` is a library plus a thin binary:

```text
crates/koko-cli/
  Cargo.toml                 package koko-cli; [[bin]] name = "koko"
  src/
    lib.rs                   application entry and injectable process capabilities
    main.rs                  real process adapters; maps ExitDecision to ExitCode
    bootstrap.rs             validation, activation, precedence, and mode selection
    registry.rs              sole command and option registries
    source.rs                source frames, include stack, and canonical segmentation
    command.rs               typed meta-command parser and dispatcher
    parameter.rs             parameter store and tagged machine-value decoder
    session.rs               application/session state machine and snapshot refresh
    worker.rs                one bounded serial owner of Database and Connection
    presentation.rs          outcome classification and renderer lifecycle
    value_codec.rs           one recursive machine input/output codec
    human.rs                 box/table/markdown/line layout and plan presentation
    machine.rs               CSV/TSV/JSON/JSONL/trash protocol encoders
    output.rs                channel routing, spill gates, and file transactions
    editor.rs                Reedline adapters, keymap, paste, prompts, and display map
    completion.rs            syntax/metadata/parameter/registry completion and highlighting
    history.rs               private persistence, search, and admission policy
    platform.rs              terminal, signal, directory, permissions, and atomic-replace edges
  tests/
    engine.rs                real embedded-engine integration
    batch.rs                 real subprocess, pipes, files, and exit status
    pty.rs                   real terminal interaction
    original_shell.rs        useful retained contracts from CLI_UX.md section 3
    support/                 deterministic PTY/subprocess helpers, never a mock engine replacement
```

Modules may split when a file becomes unwieldy, but ownership above must not move across layers.
`koko-cli` has one internal Koko dependency: the public `koko` facade. It must not depend on
`koko-parser`, catalog, binder, planner, processor, storage, or loader crates directly, including in
product tests.

### 2.3 Additive `koko` tooling API

Land and test the facade below before `koko-cli` imports it. Names are fixed so the CLI is built against
one coherent contract; fields may remain private with borrowing getters. All snapshots own immutable
data and retain no engine lock.

```rust
pub const fn version() -> &'static str;
pub fn analyze_cypher(source: &str, cursor: Option<usize>) -> SyntaxAnalysis;

impl Connection {
    pub fn session_snapshot(&self) -> Result<SessionSnapshot>;
    pub fn catalog_snapshot(&self) -> Result<CatalogSnapshot>;
    pub fn query_with_typed_params(
        &self,
        cypher: &str,
        params: &[QueryParameter<'_>],
    ) -> Result<QueryResult>;
    pub fn execute_with_metadata(
        &self,
        cypher: &str,
        params: &[QueryParameter<'_>],
    ) -> StatementOutcome;
}

impl QueryResult {
    pub fn result_kind(&self) -> QueryResultKind;
    pub fn statement_diagnostics(&self) -> &StatementDiagnostics;
    pub fn type_context(&self) -> &ResultTypeContext;
    pub fn cell(&self, row: usize, column: usize) -> Result<CellRef<'_>>;
    pub fn status_message(&self) -> Option<&str>;
    pub fn plan(&self) -> Option<&PlanPresentation>;
}
```

The facade-owned public types are:

- `GraphIdentity`, `GraphKind`, `TransactionMode`, and `SessionSnapshot` for the exact fields in
  `CLI_ARCHITECTURE.md` section 6.1;
- `CatalogSnapshot` plus owned graph, table, column, endpoint, index, macro, function-signature, and
  setting descriptors for section 6.2; it includes the selected view's revisions and canonical
  `schema_script()` output;
- `SyntaxAnalysis`, `SyntaxStatus`, `TokenSpan`, `StatementAnalysis`, `StatementClass`, `OutputClass`,
  `SourceSpan`, `SyntaxDiagnostic`, and `CursorContext` for section 6.3;
- `QueryParameter<'a> { name, value, declared_type }`, where all three inputs are borrowed and the
  optional declared type is validated without string interpolation;
- `StatementOutcome`, `StatementFailure`, `FailureKind`, `InterruptReason`,
  `StatementDiagnostics`, and `StatementWarning` for sections 6.7-6.8;
- `QueryResultKind`, `ResultTypeContext`, `CellRef`, `CellValueRef`, `PlanPresentation`, and
  `PlanNode` for sections 6.5 and 9.1; and
- the foundational `JsonValue`, `IntKind`, `Interval`, `RecursiveRelValue`, and other types needed to
  exhaustively encode the already-public `Value` enum, re-exported through `koko` rather than
  imported from `koko-common` by the CLI.

`CellRef` carries its declared `LogicalType` and borrows string or generic `Value` payloads from the
column vector; fixed-width scalars copy by value. Nested generic values remain borrowed while the
codec recursively threads the declared child type. Canonical decimal and temporal text is factored
from the engine formatter into a borrowed `Display` helper rather than re-parsed from a rendered row.
No renderer calls `to_result_strings()` or builds `Vec<Vec<Value>>`.

`execute_with_metadata` is the metadata-preserving entry to the same parse/bind/plan/execute
implementation used by `query_with_params`; the existing methods delegate and map the rich failure
back to the unchanged `Error`. It is not a second execution path. Successful results retain only that
statement's warning records and total warning count. Failures preserve `Error` display byte-for-byte,
add a real parser span only when one exists, and distinguish explicit interrupt from deadline expiry
without text matching.

`QueryResultKind` distinguishes rows, engine status, `EXPLAIN`, and `PROFILE` structurally. The plan
payload is produced from the Rust plan/operator tree and available measurements; existing corpus
rendering stays unchanged. It never imitates or parses the C++ plan string. Result type context is
captured at execution so later catalog changes cannot alter graph-value encoding.

The baseline progress implementation adds no processor callback: the CLI displays lifecycle and
elapsed time. A percentage or processed-row count appears only if a later measured engine change
supplies a meaningful monotonic snapshot; invented progress is forbidden and is not a CLI completion
requirement.

### 2.4 Cross-cutting invariants

Every landing preserves these rules:

- one parser, one query engine, one transaction/graph authority, one connection, and one ordered
  session runner;
- one central command registry and one central option registry drive parsing, help, completion,
  suggestions, config validation, and dispatch;
- the worker returns typed outcomes and never bytes; the presentation owner performs every write;
- stdout carries data only, stderr carries UI/diagnostics only, and terminal control targets stderr;
- buffers and channels are bounded; rendering walks materialized columnar results without a second
  complete representation;
- signal handlers touch only atomics/wakeup and the existing lock-free interrupt handle;
- executable paths are explicit and local; config cannot execute content; and
- no landing weakens `CLI_UX.md` or changes an architecture boundary without first amending the owning
  document with rationale.

## 3. Dependency-ordered landings

Complete and commit these landings in order. A gate is evidence for that landing, not permission to
stop before the final close gate.

### L1 - canonical syntax tooling

Add parser-backed tooling analysis for tokens, comments/literals, statement spans, completeness,
structured parser diagnostics, statement/output class, and cursor context. Expose only facade-owned
views through `koko::analyze_cypher`; execution continues to call the ordinary parser.

**Gate:** focused parser/facade tests cover semicolons and nesting inside strings/comments, redundant
separators, empty input, multiple statements, incomplete EOF, invalid complete input, forced-submit
inputs, Unicode byte spans, parameters, and every completion-context category. Existing parser and P0
results are unchanged.

### L2 - engine-authoritative observation

Add session revisions and the immutable `SessionSnapshot`/`CatalogSnapshot` APIs. Factor the existing
interchange schema renderer for snapshot use. Include built-in, macro, and connection-local function
metadata without exposing hidden `ANY` tables or mutable catalog/storage objects.

**Gate:** public facade tests prove typed/`ANY` selection, read-only/read-write/none transaction state,
uncommitted catalog visibility, graph-drop fallback, graph/catalog/UDF revision changes, canonical
schema output, setting values, and snapshot retention after locks are released. Concurrent readers do
not observe torn graph/catalog metadata.

### L3 - structured execution and result traversal

Add typed parameters, rich success/failure metadata, statement-local warnings, interrupt causes,
borrowed cells, pinned type context, structural result kinds, and Rust-owned `EXPLAIN`/`PROFILE`
payloads. Migrate existing query methods onto the same internal implementation and preserve their
public behavior.

**Gate:** public API tests cover every `Value`/`LogicalType` family, nested declared types, duplicate
column names, status versus row results, plans, prepared and direct execution parity, warnings without
history mutation, explicit cancellation versus deadline, tracked-memory and transaction errors, and
catalog changes after result capture. Existing error strings, differential output, connection reuse,
and memory accounting remain unchanged.

### L4 - CLI foundation, registries, and bootstrap

Create `koko-cli`, its thin `koko` binary, process capability traits, application state types, central
registries, argument parser, typed configuration, platform paths, precedence resolution, and
validation-before-activation flow. Implement parameter-file and inline parameter admission using the
single machine-value decoder boundary, but do not execute during bootstrap.

**Gate:** pure and subprocess tests prove help/version without database activation; every documented
option and command appears once in its registry; unknown/missing/conflicting values fail with exit 2;
config errors name file/line/key/accepted values; precedence is exact; `--no-config` is narrow; no
implicit current-directory file is read; input mode and output destination resolve independently; and
all explicit paths retain platform bytes/Unicode.

### L5 - typed presentation and output transactions

Implement the presentation lifecycle, status/plan classification, human layouts, every required
renderer, the recursive machine codec, JSON/JSONL protocol states, CSV/TSV topology spill gate,
stdout/stderr routing, broken-writer behavior, and transactional file destinations.

**Gate:** component and real-file tests cover zero/one/many rows, duplicate names, every value and
nested value branch, ordered JSON and raw `$type` collisions, exact large numbers/decimals/nonfinite
floats, temporal values, graph/path values, Unicode/multiline/wide cells, all human truncation modes,
CSV/TSV quoting and NULL/empty distinction, valid success/failure JSON and JSONL, status and plan
results, trash, broken pipes, writer/flush/rename failures, force/collision policy, replace/append,
symlink races, permissions, Unicode paths, and bounded spill. An old destination survives every
failed invocation byte-for-byte.

### L6 - one ordered source and session runner

Implement source frames, include-cycle handling, canonical segmentation, the typed meta-command
router, parameter store, one bounded worker owning the database/connection, snapshot refresh, command/
file/stdin/init execution, meta-command presentation, output switching, error continuation, explicit
transaction EOF policy, and `ExitDecision`. All modes submit through this runner.

**Gate:** real engine and subprocess tests prove command/file/piped/empty/init inputs, multiple
statements and includes, source-relative paths, all meta commands and invalid arguments, typed
parameters and redaction, selected graphs, explicit transactions and auto-abort, catalog refresh,
stop-first/keep-going boundaries, stdout/stderr isolation, every format, active-transaction rollback at
EOF, exact exit 0/1/2/130 policy where applicable, and one connection/query path. The differential
example remains unchanged.

### L7 - interactive editor, history, completion, and highlighting

Integrate Reedline through Koko adapters for prompts, canonical validation, the documented keymap,
singleline/multiline behavior, grapheme/display mapping, bracketed paste, history/search, completion,
highlighting, terminal capabilities, dumb fallback, and resize. Completion consumes immutable syntax,
metadata, parameter, and registry inputs only.

**Gate:** real PTY tests cover greeting/quiet and primary/continuation prompts; typed/`ANY` and
transaction markers; complete/incomplete/forced submit/newline; pasted multiline text and tabs;
Unicode movement/deletion/wrapping; every documented editing key; resize; persistent private history,
deduplication, skip/off/clear, search accept/edit/cancel; command/keyword/graph/label/variable/property/
function/setting/path completion and revision refresh; color on/off/`NO_COLOR`; narrow and dumb
terminals; and redirected stdout with the editor still on stderr.

### L8 - responsive execution, interruption, and terminal safety

Connect the interactive UI timer to the serial worker, install the signal bridge, implement running/
cancelling redraw, explicit interrupt and deadline presentation, the idle two-interrupt state,
Ctrl-D/quit transaction protection, shutdown/join, and fatal internal-panic containment. Reuse the
same failure/output protocol in batch mode.

**Gate:** deterministic component tests plus real PTY/subprocess scenarios prove delayed/coalesced
progress, no invented percentage, one interrupt per running statement, late-interrupt isolation, a
usable next query, distinct cancellation/deadline/memory diagnostics, no success summary after
cancellation, valid machine protocol closure, documented human partial-output notice, active-
transaction refusal/rollback, second idle Ctrl-C exit 130, terminal restoration after normal exit,
query error, cancellation, output failure, and panic, and no detached worker or leaked temporary file.

### L9 - complete acceptance and closure

Run every focused suite, the full `CLI_UX.md` section 21 matrix, the retained-original-shell suite, the
workspace and engine regression gates, and the repeated performance gate in one fresh run. Fix the
product; do not mark required scenarios ignored or weaken expectations. Only after behavior is green,
synchronize status/evidence, remove generated artifacts, and close the goal prompt as historical.

**Gate:** section 5 below is green in full, the required ordered commits exist with the repository
trailer, and the working tree is clean.

## 4. Verification program

### 4.1 Harness ownership

| Harness | Required evidence |
|---|---|
| Facade public API | `crates/koko/tests/tooling.rs`; only public APIs, real database/connection, transaction and concurrency cases |
| Pure CLI components | Library unit/property tests for registries, precedence, source mapping, command grammar, codec, layout, protocols, completion ranking, editor transitions, and exit policy |
| Engine integration | `crates/koko-cli/tests/session.rs` and `presentation.rs`; real typed and `ANY` graphs, catalog changes, warnings, plans, memory, output, and rollback |
| Subprocess/files | `crates/koko-cli/tests/batch.rs` and `process.rs`; the release-shaped `koko` binary with real pipes, files, environment, stdout/stderr, signals, and statuses |
| Terminal | `crates/koko-cli/tests/interactive.rs`; `portable-pty`/ConPTY, real terminal bytes, resize, keys, paste, redraw, interruption, and restoration |
| Retained shell behavior | `interactive.rs`, pure editor/history tests, and the strict gate's preserved differential-example protocol; only the useful contracts named in `CLI_UX.md` sections 3 and 21.3 |

Mocks may make core transitions deterministic, but every engine behavior uses the real embedded facade
and every process/terminal promise has a real subprocess or PTY scenario. Fault injection is limited
to clocks, writers, rename, terminal events, and worker panic containment; there is no mock database,
general VFS, or product-only testing flag.

### 4.2 Acceptance traceability

The final test names and close report use these IDs so no `CLI_UX.md` section 21 requirement disappears
inside a broad claim:

| ID | Contract | Owning suites |
|---|---|---|
| PTY-01 | greeting, quiet startup, prompts, graph kind, and transaction markers | `pty`, `engine` |
| PTY-02 | complete/incomplete/forced/multistatement entry and result boundaries | `pty`, syntax unit tests |
| PTY-03 | inline/file/interactive parameters, tagged types, errors, and redaction | `pty`, `batch`, codec tests |
| PTY-04 | bracketed multiline paste, literal/nonliteral tabs, and invalid UTF-8 rejection | `pty`, editor tests |
| PTY-05 | grapheme editing, wrapping, resize, and every documented key | `pty`, editor property tests |
| PTY-06 | history permissions, persistence, deduplication, skip/off/clear, and reverse search | `pty`, history tests |
| PTY-07 | every completion family, ranking, scope, and refresh after graph/catalog/UDF/parameter change | `pty`, `engine`, completion tests |
| PTY-08 | highlighting and diagnostics with auto/on/off color and `NO_COLOR` | `pty`, syntax tests |
| PTY-09 | every meta command, help topic, output form, and invalid-argument path | `pty`, registry/command tests |
| PTY-10 | zero/one/truncated/wide/multiline/duplicate/NULL/empty results, DDL, plans, and multiple results | `pty`, presentation tests |
| PTY-11 | graph switches, read-write/read-only transactions, failures, quit/Ctrl-D protection | `pty`, `engine` |
| PTY-12 | cancellation, deadline, memory failure, progress, narrow/dumb TTY, resize, and redirected stdout | `pty`, `engine` |
| BAT-01 | command/file/init/piped/empty input and source/include locations | `batch`, source tests |
| BAT-02 | parameter precedence/tags/duplicates and all malformed or conflicting options | `batch`, bootstrap tests |
| BAT-03 | no prompt/greeting/ANSI/progress/summary contamination; stable channel split and exit codes | `batch` |
| BAT-04 | stop-first/keep-going and explicit-transaction boundaries | `batch`, `engine` |
| BAT-05 | CSV/TSV headers, escaping, NULL/empty, and multi-result rejection before disclosure | `batch`, machine tests |
| BAT-06 | complete/incomplete JSON and JSONL schema/row/summary/status/error protocols | `batch`, protocol tests |
| BAT-07 | every required format and trash | `batch`, renderer tests |
| BAT-08 | output collision/force/atomic commit/rollback/Unicode paths and broken pipes | `batch`, output tests |
| BAT-09 | cancellation, partial-output policy, active transaction at EOF, and exact status | `batch`, `engine` |
| BAT-10 | config precedence, `--no-config`, `--no-history`, permissions, and no implicit init | `batch`, config/history tests |
| REG-01 | multiline, multiple queries, truncation, keys, completion, search, history, output switching, schema, suggestions, cancellation, and Unicode retained from the old shell | `original_shell` |

### 4.3 CLI gate command

L9 adds `scripts/cli_goal_gate.py --strict`. It runs the pure, facade, engine, batch, PTY, and retained
shell suites; exercises the built `koko` binary in help, version, command, file, piped, machine-output,
and interactive smoke modes; and prints one row for every ID above. Strict mode fails on any missing,
ignored, skipped, timed-out, flaky-retry-only, or failed required scenario. It does not replace the
engine gate in section 5.

The supported-host CI matrix runs the component, engine, and subprocess suites on Linux, macOS, and
Windows with Rust 1.85; PTY contracts run on Unix PTY and Windows ConPTY where available. Platform
exceptions require an explicit UX/architecture decision, not a silent test skip.

## 5. Fresh close gate and stopping rule

CLI work is complete only when all of the following hold in one fresh run:

1. **Behavior:** `python3 scripts/cli_goal_gate.py --strict` passes every PTY-01 through PTY-12,
   BAT-01 through BAT-10, and REG-01 contract with no required skip or timeout. The production binary,
   not only the library core, is exercised.
2. **Facade:** all public tooling tests pass, existing `query`/prepared/result/error behavior remains
   source-compatible, no CLI crate reaches below `koko`, and the differential
   `examples/koko_cli.rs` protocol is byte-identical for its standing probes.
3. **Engine:** `KOKO_ROOT_DIRECTORY=../koko KOKO_DATASET_DIR=../koko/dataset python3
   scripts/goal_gate.py --strict` reports **1785/343/exactly 23**, with the same 15 durability,
   4 owner-deferred, and 4 ledgered cases and every zero-state invariant green.
4. **Performance:** `KOKO_ROOT_DIRECTORY=../koko python3 scripts/perf_gate.py` reports nine correct
   answers, no timeout, every median Rust/C++ ratio at most 2x, and q4/q5/q7 below 1x.
5. **Workspace:** Rust 1.85 dependency resolution/check, debug and release workspace tests, release
   workspace build, `cargo fmt --all --check`, and
   `cargo clippy --workspace --all-targets -- -D warnings` are green. The Linux/macOS/Windows host
   matrix is green for its required suites.
6. **Resource safety:** cancellation, deadline, tracked-memory failure, broken output, failed rename,
   explicit transaction errors, and internal panic leave no successful protocol state, partial
   committed file, leaked temp/spill file, detached worker, poisoned next query, or unrestored
   terminal.
7. **Architecture audit:** there is one command registry, one option registry, one source/session
   runner, one engine execution path, and one machine value codec; no output path reparses rendered
   values or constructs a complete second row matrix; no CLI-owned graph/transaction/catalog truth or
   parser heuristic has appeared.
8. **Closure:** `ROADMAP.md`, `README.md`, `AGENTS.md`, `docs/CLI_UX.md`,
   `docs/CLI_ARCHITECTURE.md`, this plan, `docs/CLI_GOAL_PROMPT.md`, `docs/PROGRESS.md`, and affected
   gap/evidence documents agree. Generated reports, PTY transcripts, outputs, spills, and temp files
   are absent. Every landing and final closure is committed with the required trailer, the goal prompt
   is marked historical, and the working tree is clean.

Do not declare completion after facade scaffolding, a batch-only shell, an interactive demo, a subset
of renderers, mocked terminal checks, or visible happy paths. Do not defer a normative `CLI_UX.md`
behavior because the chosen library makes it awkward: adapt the library, replace it within the fixed
architecture, or amend the owning contract with explicit rationale before proceeding.

## 6. Explicit non-goals

Do not implement or scaffold native database files, WAL/recovery/checkpoint/pages, persistent catalog
or indexes, physical storage introspection, Arrow C, extensions/plugins or extension modules,
projected graphs/GDS, FTS/vector, remote/object/connectors, foreign bindings, authentication,
attached/multiple databases, client/server mode, lazy/async engine APIs, multiple CLI connections,
parallel statement execution, a daemon, a general VFS, a graph visualizer, or an embedded language
server.

The CLI may expose existing logical `IMPORT DATABASE`/`EXPORT DATABASE` and local read-only
`icebug-disk` Cypher behavior through the normal connection. It must not rebrand either as native
persistence or add CLI-owned storage semantics.

## 7. Landing record

Update this table only after each landing's gate passes; completion evidence belongs here rather than
in the compact goal prompt.

| Landing | Status | Commit | Focused evidence |
|---|---|---|---|
| L1 canonical syntax tooling | complete | L1 landing commit | `cargo test -p koko-parser`; public `tooling` facade tests; P0; strict workspace Clippy; fmt |
| L2 engine-authoritative observation | complete | L2 landing commit | 10 public facade tooling tests; transaction-local metadata; graph/UDF revisions; concurrent snapshots; P0; strict Clippy; fmt |
| L3 structured execution/result traversal | complete | L3 landing commit | typed parameters; borrowed cells; pinned result type contexts; structured status/EXPLAIN/PROFILE; warnings/failures/interrupt causes; public facade tests; P0; strict Clippy; fmt |
| L4 CLI foundation/bootstrap | complete | L4 landing commit | central 22-command/26-option registries; validation-only bootstrap; config/CLI precedence; typed parameters and machine decoder; pure/subprocess/path tests; Rust 1.85; P0; strict Clippy; fmt |
| L5 presentation/output transactions | complete | L5 landing commit | streaming box/table/Markdown/line/CSV/TSV/JSON/JSONL/trash renderers; lossless typed codec; bounded delimited spill; stdout/stderr isolation; atomic replace/append/refuse and race checks; failure injection; real-engine and Rust 1.85 tests; P0; strict Clippy; fmt |
| L6 ordered source/session runner | complete | L6 landing commit | one bounded connection-owning worker; canonical semicolon/source segmentation; ordered init/command/file/stdin/include execution; cycle chains; 22 typed commands; graph-scoped structured introspection; parameter persistence; transaction-safe keep-going/quit/EOF; destination switching; pure/real-engine/subprocess/file tests; P0; strict Clippy; fmt |
| L7 editor/history/completion | complete | L7 landing commit | Reedline prompt/validation/keymap; multiline/forced submit; Unicode/grapheme movement and wrapping; bracketed paste/tab normalization; private bounded deduplicated history with skip/off/clear and search; canonical command/keyword/catalog/parameter/path completion with revision refresh; syntax highlighting and runtime toggles; resize/narrow/dumb terminal PTYs; Rust 1.85; strict Clippy; fmt |
| L8 interruption/terminal safety | complete | L8 landing commit | atomic SIGINT bridge; observed serial-worker wait with delayed/coalesced progress and one interrupt per statement; explicit/deadline/memory identities; valid incomplete JSON/JSONL closure; interactive recovery and batch exit 130; late-signal isolation; active-transaction exit protection; bounded shutdown/join; sanitized panic containment; explicit terminal restoration after normal, error, cancellation, and output-failure paths; real PTY/subprocess tests; Rust 1.85; strict Clippy; fmt |
| L9 acceptance/closure | complete | L9 landing commit | strict `cli_goal_gate.py` 23/23 with real binary/PTY/subprocess/file and retained differential protocol; CSV/TSV raw-string correction; recoverable interactive command errors; multi-result headings; retained non-query `EXPLAIN`/`PROFILE` and concurrent UDF removal corrections; Rust 1.85 plus Linux/Windows all-target cross-checks and permanent Linux/macOS/Windows CI; fresh 1785/343/23 corpus and repeated nine-query ≤2× performance gates; debug/release workspace; strict Clippy; fmt |
