# Koko CLI UX and behavior contract

> **Status (2026-07-26): implemented and current.** This document owns the user-visible behavior,
> interaction model, terminal layout, automation contract, and acceptance criteria for Koko's
> first-party Rust CLI.
> [`CLI_ARCHITECTURE.md`](CLI_ARCHITECTURE.md) owns current code structure and integration
> boundaries; [`CLI_PLAN.md`](CLI_PLAN.md) is the completed historical landing plan.
>
> [`ROADMAP.md`](../ROADMAP.md) owns the product boundary and post-v0 operating rules. This CLI is a
> terminal client for the in-memory product; it does not reactivate native durability, extensions,
> connectors, projected graphs, foreign bindings, or another deferred engine surface.

The key words **MUST**, **MUST NOT**, **SHOULD**, **SHOULD NOT**, and **MAY** are normative. Examples
use `koko` as the executable name.

## 1. Product outcome

The CLI must make the embedded Rust database pleasant for two distinct jobs:

1. **Interactive exploration:** create data, inspect schemas, develop Cypher, diagnose errors, and
   control long-running queries without leaving a terminal.
2. **Reliable automation:** execute a command, file, or stdin stream with deterministic data output,
   diagnostics, and exit status.

The same invocation must not blur those jobs. Interactive mode may be rich and adaptive; batch mode
must be quiet, stable, and composable.

### 1.1 User-visible product boundary

- Every data session starts one in-memory database. Help and version exit before creating it, and
  there is no positional native database path.
- Explicit logical `IMPORT DATABASE` and `EXPORT DATABASE` remain the save/restore mechanism and
  must not be described as crash-safe persistence.
- Named typed and `ANY` graphs, transactions, prepared query behavior, native scalar UDFs, local
  read-only `icebug-disk`, cancellation, deadlines, and memory errors are visible through their
  existing Cypher and result contracts.
- Local `icebug-disk` is a query/catalog feature, not a CLI storage mode or attach flag.
- The CLI must not offer native database-file, WAL, checkpoint, recovery, compression, page,
  buffer-pool, persistent-index, or physical-storage controls.
- The CLI must not add remote paths, connectors, extension installation, authentication,
  multi-database attach, projected graphs, or graph visualization.

### 1.2 Non-goals of this document

This document does not choose crates, terminal libraries, modules, threads, channels, or ownership
structures. It also does not define a second query language or alter Cypher semantics. Current
structural decisions belong to [`CLI_ARCHITECTURE.md`](CLI_ARCHITECTURE.md); the completed landing
sequence remains historical in [`CLI_PLAN.md`](CLI_PLAN.md).

## 2. Design principles

1. **Context is visible.** The selected graph and transaction state are never hidden.
2. **Human by default, exact on demand.** TTY output is readable; redirected output is stable and
   undecorated.
3. **No silent loss.** Truncated rows, omitted columns, partial output, failed statements, and
   cancellation are always explicit.
4. **One authoritative language.** Graph selection, transactions, schema mutation, import/export,
   and query settings retain their Cypher behavior. Shell commands control the shell or inspect the
   current session; they do not create parallel database semantics.
5. **Terminal-native, not terminal-dependent.** Unicode, color, completion, and live progress
   enhance a capable TTY, but every operation remains usable without them.
6. **Safe local defaults.** Starting the CLI never executes an untrusted working-directory file,
   overwrites output, commits a transaction, or records input invisibly.
7. **Predictable state transitions.** Prompts and status derive from observed session state after
   each statement, never from guessing what the statement intended.
8. **Automation is a contract.** stdout, stderr, formats, partial results, and exit codes are public
   interfaces rather than incidental rendering choices.

## 3. Reference behavior and intentional changes

The C++ shell is a behavioral reference, not an automatic compatibility target. The following
choices preserve its productive behavior while correcting its UX problems.

| Original behavior | Rust CLI decision |
|---|---|
| Embedded, low-latency REPL | Retain |
| No argument opens in memory | Retain |
| Parser-aware multiline editor | Retain and add an explicit newline gesture |
| UTF-8 editing and readline-style keys | Retain |
| Persistent, deduplicated history and reverse search | Retain with safer storage and controls |
| Schema/function/property completion | Retain with typed, categorized candidates |
| Unicode box table and head/tail truncation | Retain |
| Many output modes | Keep the useful, nonredundant modes; do not require legacy HTML/LaTeX/list variants |
| `koko>` prompt | Replace with graph/transaction-aware prompt |
| Flat `:help` command list | Replace with topical help and examples |
| `:max_rows 0` means reset to 20 | Replace magic zero with `all` and `default` |
| `:max_width 0` means terminal width | Replace magic zero with `auto` |
| `:mode`, `:stats`, separate multiline commands | Use explicit `:format`, `:timing`, and setting values |
| Startup prose and summaries mixed with data | Separate result data from terminal UI and diagnostics |
| Working-directory `.kokorc` executes automatically | Prohibit implicit current-directory execution |
| Native database/WAL/buffer flags | Omit; outside the Rust product boundary |
| Empty table cell for NULL | Display `NULL` in human formats by default; retain configurable legacy display |
| Two empty-prompt interrupts exit | Retain only when no transaction is active and explain the gesture |

The original implementation and its tests remain the reference for details not overridden here:
`../koko/tools/shell/{shell_runner.cpp,embedded_shell.cpp,linenoise.cpp}` and
`../koko/tools/shell/test/`.

## 4. Interaction surfaces

The CLI has five visible surfaces:

1. **Invocation:** flags, input source, configuration, and process exit.
2. **Session chrome:** greeting, prompt, continuation prompt, and progress.
3. **Input editor:** Cypher, meta commands, completion, history, and search.
4. **Result presentation:** human tables, machine formats, summaries, and multiple result sets.
5. **Diagnostics:** parse/bind/runtime errors, cancellation, warnings, and usage errors.

The surfaces share settings, but their output channels and behavior remain distinct.

## 5. Invocation and mode selection

### 5.1 Synopsis

```text
koko [OPTIONS]
koko --command <CYPHER> [OPTIONS]
koko --file <PATH> [OPTIONS]
<CYPHER> | koko [OPTIONS]
```

There is no positional database path. A positional argument must therefore be rejected rather than
silently interpreted as storage.

### 5.2 Required options

| Option | Behavior |
|---|---|
| `-c`, `--command <CYPHER>` | Execute one command string in batch mode |
| `-f`, `--file <PATH>` | Execute one local UTF-8 command file in batch mode |
| `--param <NAME=JSON>` | Bind one repeatable query parameter using JSON syntax |
| `--params-file <PATH>` | Load query parameters from one local JSON object |
| `-i`, `--init <PATH>` | Execute an explicit local initialization file before the main input |
| `--format <auto/FORMAT>` | Select automatic or explicit result format |
| `-o`, `--output <PATH>` | Write result data to a file |
| `--force` | Permit replacement of an existing `--output` file |
| `--header`, `--no-header` | Control CSV/TSV headers |
| `--timing`, `--no-timing` | Control timing summaries |
| `--progress <auto/on/off>` | Control live progress |
| `--color <auto/always/never>` | Control ANSI styling |
| `--null <TOKEN>` | Select the CSV/TSV NULL token |
| `--keep-going` | Continue independent autocommit statements after an error |
| `--no-config` | Ignore the user configuration file |
| `--no-history` | Disable history for this process |
| `-q`, `--quiet` | Suppress the interactive greeting and nonessential status prose |
| `-h`, `--help` | Print invocation help and exit successfully |
| `-V`, `--version` | Print the CLI and engine version and exit successfully |

Option names are case-sensitive in the conventional way. Option values, paths, query text, and
output tokens must retain their original case and bytes.

### 5.3 Input-source rules

- `--command` and `--file` are mutually exclusive.
- With neither option, TTY stdin starts interactive mode.
- With neither option, non-TTY stdin is read as a batch command stream.
- `--init` runs first in the same session. Failure aborts before the main input.
- `--params-file` must contain one top-level JSON object. It is loaded before inline parameters.
- Each repeated `--param` name must be unique; repeating the same inline name is a usage error.
  Inline parameters override a same-named value from `--params-file`.
- Command-line parameters apply to every query in the invocation and seed the interactive parameter
  map when the process enters a REPL. Parameter binding never performs string interpolation.
- An empty piped input succeeds without opening an interactive prompt.
- `--output` affects result data only; it never redirects diagnostics or progress.
- An existing output path is an error unless `--force` is present.
- Output-file replacement must be all-or-nothing: a failed invocation must not replace the prior
  file with partial data.
- Paths may contain spaces and non-ASCII characters. `~` expansion is supported; shell command
  substitution and implicit environment interpolation are not.
- Remote URLs are rejected.

### 5.4 Input-mode and result-destination defaults

Input mode and result destination are detected independently.

| Setting | Interactive input (TTY stdin) | Batch input |
|---|---:|---:|
| Greeting | On | Off |
| Prompt | On | Off |
| History | On | Off |
| Editor highlighting | Auto/on | Not applicable |
| Completion | On | Not applicable |
| Progress | Auto when stderr is a TTY | Off |
| Timing | On | Off |
| Error policy | Continue session | Stop on first failure |

| Setting | TTY result destination | Redirected/file result destination |
|---|---:|---:|
| Effective default format | `box` | `tsv` |
| Row display limit | 20 | Unlimited |
| Result ANSI styling | Auto | Off |
| CSV/TSV header | On | On |

An interactive session with redirected stdout therefore keeps its editor, history, and prompts but
defaults its result payload to complete, undecorated TSV. Explicit flags and meta-command settings
override the corresponding defaults. `--keep-going` changes only batch error continuation and must
not silently continue an explicit transaction whose state after failure is not usable.

## 6. Output-channel contract

- **stdout** contains requested result payloads and explicit informational command output such as
  `:schema` or `:help`.
- **stderr** contains greeting, prompts, progress, warnings, timing summaries, cancellation notices,
  errors, and usage diagnostics.
- Non-TTY and machine-format stdout must never contain a banner, prompt, ANSI escape, progress
  update, hint, or timing footer unless the selected format defines that metadata inside its
  versioned envelope.
- `--quiet` suppresses greeting and nonessential success prose; it never suppresses errors,
  warnings, or requested data.
- A TTY normally combines stdout and stderr visually, so interactive examples below show a single
  stream even though the channel contract remains in force.

This separation allows an interactive user to redirect query data while retaining prompts and
errors on the terminal.

## 7. Startup, prompt, and session status

### 7.1 Greeting

Default startup is compact:

```text
$ koko
Koko 0.1 · in-memory · graph main
Type :help for help; Ctrl-D or :quit to exit.

koko[main]>
```

The exact version comes from the running library. The greeting must not claim that the session is
persistent. `--quiet` removes both greeting lines but not the prompt.

### 7.2 Prompt grammar

```text
koko[<graph>]>
koko[<graph>|tx]>
koko[<graph>|ro-tx]>
```

Examples:

```text
koko[main]>
koko[analytics]>
koko[analytics|tx]>
koko[analytics|ro-tx]>
```

- `<graph>` is the connection's selected graph using its display name.
- `tx` means an explicit read-write transaction is active.
- `ro-tx` means an explicit read-only transaction is active.
- The prompt refreshes after every completed statement, error, commit, rollback, graph change, or
  externally observed graph-registry change.
- If authoritative state cannot be read, the prompt displays `?` rather than stale state.
- Color may distinguish fields, but the text itself carries all state.
- Volatile values such as elapsed time, memory use, or row count do not belong in the prompt.

The continuation prompt is visually subordinate and preserves indentation:

```text
koko[main]> MATCH (p:Person) \
         ...> WHERE p.age >= 30 \
         ...> RETURN p.name, p.age \
         ...> ORDER BY p.age DESC;
```

### 7.3 `:status`

`:status` reports, in a stable labeled layout:

- product mode (`in-memory`);
- selected graph and graph kind;
- transaction state;
- effective query timeout;
- worker/thread setting;
- effective tracked-memory limit;
- bound parameter count and names, without values;
- result format;
- row limit and width mode;
- timing, progress, color, highlighting, completion, history, and NULL-display settings.

```text
database       in-memory
graph          main (typed)
transaction    none
parameters     2 (min_age, name)
timeout        none
workers        4
memory limit   512 MiB
format         box
rows / width   20 / auto
```

Status is observational. Running it must not switch graphs, begin a transaction, commit, roll back,
or mutate a setting.

## 8. Query entry and statement boundaries

### 8.1 Multiline mode

Multiline mode is the interactive default.

- Enter at the end of a parse-complete buffer submits it.
- Enter in an incomplete buffer inserts a newline and shows the continuation prompt.
- Enter away from the end inserts a newline rather than unexpectedly submitting.
- `Ctrl-J` force-submits the current buffer, preserving the original shell gesture.
- `Alt-Enter` inserts a newline even when the buffer is parse-complete. Terminals that cannot
  distinguish Alt-Enter must support `Esc`, then Enter as the equivalent gesture.
- A standalone backslash (`\`) that is the final non-whitespace code token on an interactive
  physical line explicitly requests another line. It works in either multiline setting and may be
  redundant with a parser-incomplete prefix.
- The marker is recognized through the canonical lexical token stream only when it is outside
  quoted strings, backtick identifiers, and line/block comments. Meta-command lines never interpret
  a trailing backslash as continuation.
- Interactive continuation markers, including markers in a bracketed paste, are removed before
  history admission, source mapping, or execution while their newlines are retained. `Ctrl-J`
  still force-submits the normalized buffer.
- Continuation does not define or validate a per-line Cypher fragment. The complete normalized
  buffer remains subject to the one canonical parser and binder.
- Semicolons separate statements. A single parse-complete statement does not require a trailing
  semicolon for interactive submission.
- Blank input does nothing.
- Whitespace-only statements and redundant trailing semicolons do not produce empty results.

With `:multiline off`, Enter submits the current physical line unless Alt-Enter/Escape-then-Enter or
an explicit continuation marker requests another line. Pasted newlines still remain one paste
operation and are not executed piecemeal.

### 8.2 Multiple statements

- Statements in one submission execute in source order.
- Human formats label multiple row-producing results `Result 1`, `Result 2`, and so on.
- The statement that failed is identified by index and source location.
- Interactive mode continues after an error unless the engine/session is no longer usable.
- Batch mode stops at the first error unless `--keep-going` is present.
- `--keep-going` applies only where continuing preserves explicit transaction semantics; it never
  auto-commits or auto-rolls back to make progress.

### 8.3 Meta-command recognition

- A meta command begins with `:` as the first non-whitespace token in a logical input.
- Meta-command names are ASCII case-insensitive; arguments retain case.
- A meta command occupies its own logical line and does not require a semicolon.
- A colon inside Cypher, a string, a comment, or a later line is not a meta command.
- Unknown commands produce a nearest-command suggestion when one is plausibly close.
- Extra or malformed arguments produce command-specific usage, not a generic Cypher parse error.

## 9. Interactive editing

Editing must be Unicode- and grapheme-safe. Cursor movement, deletion, width calculation, wrapping,
and truncation must not split a UTF-8 sequence or rendered grapheme.

### 9.1 Required key bindings

| Keys | Action |
|---|---|
| `Ctrl-A`, Home | Move to start of current logical line |
| `Ctrl-E`, End | Move to end of current logical line |
| `Ctrl-Home` | Move to start of the complete input buffer |
| `Ctrl-End` | Move to end of the complete input buffer |
| `Ctrl-B`, Left | Move left one grapheme |
| `Ctrl-F`, Right | Move right one grapheme |
| `Alt-B`, modified Left | Move left one word |
| `Alt-F`, modified Right | Move right one word |
| Backspace, `Ctrl-H` | Delete previous grapheme |
| Delete, `Ctrl-D` with input | Delete following grapheme |
| `Ctrl-W`, Alt-Backspace | Delete previous word |
| `Ctrl-U` | Clear the complete input buffer |
| `Ctrl-K` | Delete from cursor to end |
| `Ctrl-T` | Transpose adjacent graphemes |
| `Ctrl-L` | Clear and redraw the terminal |
| `Ctrl-P`, Up | Previous history entry, or previous visual line in multiline input |
| `Ctrl-N`, Down | Next history entry, or next visual line in multiline input |
| `Ctrl-R` | Reverse incremental history search |
| Tab | Complete or open completion menu |
| Shift-Tab | Select previous completion |
| `Ctrl-G` | Dismiss completion/search or cancel the edited buffer without exiting |
| `Ctrl-C` | Cancel query or edited input; see §15 |
| `Ctrl-D` on empty input | Request normal exit; see §14 |

Terminal resize must reflow input without changing buffer contents or logical cursor position.

### 9.2 Paste behavior

- Bracketed paste is used when supported.
- A paste is admitted as one edit operation; embedded newlines do not trigger partial execution.
- Pasted tabs outside quoted strings become four spaces in Cypher input.
- Literal tabs and newlines inside quoted strings retain their semantic content according to Cypher
  escaping rules; the shell must not rewrite already escaped text.
- Large paste rendering may be throttled, but input must not be dropped.
- Invalid UTF-8 input is rejected with a diagnostic and does not corrupt the current buffer.

## 10. Completion and highlighting

### 10.1 Completion candidates

Completion covers:

- meta commands and their valid arguments;
- Cypher keywords;
- graph names;
- node labels and relationship labels;
- variables in scope;
- bound parameter names;
- properties valid for resolved variables;
- scalar, aggregate, and table functions;
- function signatures and logical types;
- supported setting names and values;
- local paths for `:read`, `--file`, `--init`, and `--output`.

Ordinary Cypher symbols are not proposed inside comments or string literals. Path completion is
allowed only in a shell path argument or a Cypher path position explicitly understood by the
language.

### 10.2 Completion presentation

When more than one candidate remains, the menu shows candidate, kind, and type/signature where
known:

```text
name       property      STRING        Person
age        property      INT64         Person
```

Candidate kinds include `keyword`, `graph`, `node label`, `relationship label`, `variable`,
`property`, `scalar function`, `aggregate`, `table function`, `setting`, `command`, and `path`.

Ranking order:

1. exact prefix;
2. symbol valid at the current syntactic position;
3. scope-local variable or property;
4. selected-graph catalog entry;
5. function;
6. keyword;
7. fuzzy match.

Tab accepts a sole candidate or opens the menu. Typing narrows it, Tab/Down selects forward,
Shift-Tab/Up selects backward, Enter accepts, and Escape or `Ctrl-G` dismisses it. Catalog-backed
candidates refresh after successful catalog or graph-registry changes.

### 10.3 Highlighting

- Keywords, identifiers, literals, comments, parameters, and punctuation may use distinct styles.
- Syntax errors use an underline or marker in addition to color.
- Highlighting is enabled only when color is allowed and output is a capable TTY.
- `NO_COLOR`, `--color never`, or `:highlight off` removes all styling without removing information.
- Completion ghost text must be visually distinct from committed input and never copied as part of
  the buffer until accepted.

## 11. History and search

### 11.1 Persistence

- Interactive history is enabled by default and batch history is disabled.
- The default file uses the platform-standard per-user state directory, never the current directory:
  `$XDG_STATE_HOME/koko/history` when set or `~/.local/state/koko/history` otherwise on XDG
  systems, `~/Library/Application Support/Koko/history` on macOS, and
  `%LOCALAPPDATA%\Koko\history` on Windows.
- The history directory and file are created with user-only permissions.
- A multiline submission is one history entry.
- Consecutive duplicates are suppressed after newline and trailing-whitespace normalization.
- Meta commands are recorded except history-control commands and `:param`/`:params`, which may
  reveal parameter values.
- `:history skip` omits the next submitted Cypher statement from persistent history.
- `:history off` stops recording until `:history on`; it does not erase existing entries.
- `--no-history` wins for the full process.
- `history_limit` defaults to 10,000 entries. Reaching it evicts the oldest complete entries.
- `:history clear` asks for confirmation in a TTY and is rejected in noninteractive input.

History is private local state, not a secret store. Help must explain how to disable or skip it
before entering sensitive literals.

### 11.2 Search

`Ctrl-R` opens incremental reverse search:

```text
bck-i-search: person_
MATCH (p:Person) RETURN p.name;
```

- Search is Unicode-aware and case-insensitive.
- Multiline entries are searched as logical text while preserving their original newlines when
  accepted.
- Repeated `Ctrl-R` selects older matches; `Ctrl-S` or Down selects newer matches.
- Enter accepts and submits; Right/End accepts for further editing; Escape or `Ctrl-G` cancels and
  restores the pre-search buffer.
- A failing search is labeled textually, not solely by color or a bell.

## 12. Meta-command contract

With no value, setting commands print their current value and valid choices. Values are explicit;
zero is never a magic synonym for `default`, `auto`, or `all`.

| Command | Behavior |
|---|---|
| `:help [topic/command]` | Show topical help or command usage |
| `:quit [--rollback]` | Exit, optionally rolling back an active transaction |
| `:clear` | Clear and redraw an interactive terminal |
| `:status` | Show effective session and display state |
| `:graphs` | List graphs and mark the selected graph |
| `:schema [graph[.object]]` | Print deterministic executable Cypher for visible schema objects |
| `:describe <graph[.object]>` | Show one object's kind, columns, types, keys/endpoints, and source |
| `:functions [pattern]` | List visible functions, kinds, signatures, and return types |
| `:params [--values]` | List bound parameters, hiding values by default |
| `:param <name> <json>` | Bind or replace one session parameter |
| `:param clear <name/all>` | Clear one or all session parameters |
| `:format [auto/name]` | Show or select automatic/explicit output format |
| `:timing [on/off]` | Show or hide compile/execution timing |
| `:progress [auto/on/off]` | Control live progress |
| `:rows [number/all/default]` | Control interactive display row limit |
| `:width [number/auto]` | Control human-table display width |
| `:null [literal/empty]` | Control human NULL display |
| `:multiline [on/off]` | Control parser-aware multiline entry |
| `:highlight [auto/on/off]` | Control syntax/error styling |
| `:completion [on/off]` | Control completion and ghost text |
| `:history [show N/clear/on/off/skip]` | Inspect or control history |
| `:read <path> [--keep-going]` | Execute a local UTF-8 command file in the current session |
| `:output [stdout/<path> [append/replace]]` | Show or change the interactive result destination |

Behavioral constraints:

- `:graphs`, `:schema`, `:describe`, and `:functions` observe the current session without changing
  graph or transaction state.
- `:schema` defaults to the selected graph and emits objects in deterministic dependency order.
- `:status`, `:graphs`, `:describe`, and `:functions` expose logical rows and honor the selected
  format. `:schema` is an executable Cypher script in human formats and a `statement STRING` column
  in machine formats.
- Ambiguous object names produce candidates rather than choosing arbitrarily.
- `:params` shows names, logical types, and sources but no values unless `--values` is explicit.
  `:param` values persist for subsequent submissions until replaced or cleared.
- `:read` uses the current connection, graph selection, settings, and transaction. Relative nested
  paths resolve from the including file; include cycles are rejected with the chain shown.
- `:output <path>` refuses an existing path; `append` or `replace` is the explicit collision
  decision. `append` never inserts terminal prompts or diagnostics.
- `:help`, `:clear`, `:multiline`, `:highlight`, `:completion`, `:history`, and `:rows` are
  interactive-only and fail with command-specific guidance in batch input.
- `:quit` stops the remaining interactive or batch input and follows the transaction protection in
  §14.
- Database state changes remain Cypher: there is no `:use`, `:begin`, `:commit`, or `:rollback`
  shadow command.

### 12.1 Parameters

- Parameter names use the Cypher parameter-name grammar and are written as `$name` in queries.
- `--param name=<json>` and `:param name <json>` parse values as JSON, not shell or Cypher source.
- Tagged non-JSON values use the normative machine-value mapping in §17.3.
- The complete parameter map is supplied to each statement; the normal binder remains responsible
  for missing, unknown, and incompatible parameter errors.
- `:params` reports name, logical type, and whether the value came from a parameters file, the
  command line, or the interactive session. Values require `:params --values`.
- Parameter values never appear in the greeting, prompt, `:status`, completion menu, history, or an
  error unless the user explicitly requests values or the authoritative engine error contains one.
- Completion proposes bound `$name` values by name and type without exposing their contents.

### 12.2 Help topics

Bare `:help` is short and grouped. It links to at least:

```text
:help queries
:help keys
:help completion
:help history
:help formats
:help parameters
:help batch
:help graphs
:help transactions
:help cancellation
```

Help includes examples for multiline entry, parameters where supported, selected graphs,
transactions, redirection, truncation, and cancellation. Command-line `--help` focuses on process
invocation; `:help` focuses on the live session.

## 13. Result presentation

### 13.1 Human table layout

On a capable TTY, the automatic interactive format is `box`:

```text
┌──────────┬───────┐
│ name     │ age   │
│ STRING   │ INT64 │
├──────────┼───────┤
│ Alice    │ 35    │
│ Bob      │ 34    │
│ …        │ …     │
│ Zoe      │ 19    │
└──────────┴───────┘
1000 rows · 20 shown · 2 columns · compile 0.4 ms · execute 3.4 ms
```

The table is result data on stdout. The summary line is terminal metadata on stderr. A user viewing
a TTY sees them together; redirected stdout contains only the selected result payload.

Layout rules:

- Column name and logical type are both visible.
- Duplicate column names remain distinct positional columns.
- Numeric values align right; text and graph values align left.
- Human NULL is the literal `NULL` by default and may be styled dimly.
- Empty string renders as an empty quoted string (`''`) in human tables so it is distinct from NULL.
- Control characters in one-line table cells render as visible escapes such as `\n` and `\t`.
- Other non-NULL scalar and graph values retain canonical engine formatting.
- Terminal width is measured in rendered graphemes, not bytes.
- ANSI styling never contributes to width.
- `table` is the ASCII-border equivalent of `box`.
- A zero-row result still shows its schema and `0 rows`.
- A zero-column status result is rendered as concise status prose, not an empty border.
- `EXPLAIN` and `PROFILE` use their structured plan presentation and do not masquerade as ordinary
  row tables.

### 13.2 Row and column truncation

Human output to a TTY defaults to 20 displayed rows. If more rows exist:

- show the first `ceil(limit / 2)` and last `floor(limit / 2)` rows;
- render a visible omitted-row marker;
- report total and shown counts;
- suggest `:rows all` once per session, not after every result.

This is display truncation, never query `LIMIT`; the wording must not imply otherwise.

Wide output preserves as many complete columns as practical. If columns must be omitted, retain the
leading and trailing columns with an explicit middle-column marker. If cells must be shortened, use
a visible ellipsis and preserve valid graphemes. The summary reports omitted columns.

Batch and redirected result output is never display-truncated; automation must use an explicit
Cypher `LIMIT` when fewer rows are desired. Redirection must never silently lose rows.

### 13.3 Multiple results and status results

Human formats place a heading before each row-producing result when a submission produces more than
one:

```text
Result 1 of 2
...

Result 2 of 2
...
```

Autocommit DDL/status messages remain visible interactively. In batch machine formats, status
messages go to stderr or into the format's metadata envelope; they must not appear as stray CSV/TSV
rows.

## 14. Transactions and graph context

- `USE GRAPH`, `BEGIN`, `COMMIT`, and `ROLLBACK` update the prompt only after the operation's actual
  result is known.
- A failed statement leaves graph and transaction indicators consistent with the session's real
  state.
- Introspection commands never change the selected graph.
- Concurrent graph deletion must not leave a silently stale prompt. After the session observes the
  change, it shows the authoritative replacement or `?` until selection is valid.
- `:quit` or Ctrl-D with no active transaction exits normally.
- `:quit` or Ctrl-D with an active transaction refuses the first exit request and prints:

  ```text
  Transaction is active. COMMIT, ROLLBACK, or use :quit --rollback.
  ```

- `:quit --rollback` rolls back and exits only after rollback succeeds.
- Batch EOF with an active transaction is an error; the process rolls it back and exits nonzero.
- The CLI never commits an active transaction on exit.

## 15. Cancellation, deadlines, and progress

### 15.1 Input and process interrupts

At the prompt:

- `Ctrl-G` cancels search/completion or clears the current edit and never exits.
- `Ctrl-C` clears nonempty input.
- First `Ctrl-C` on empty input prints `Press Ctrl-D or :quit to exit`.
- A second consecutive empty-input `Ctrl-C` within two seconds exits with status 130 only when no
  transaction is active.
- Any intervening input resets the consecutive-interrupt state.
- With an active transaction, repeated Ctrl-C does not bypass the explicit rollback rule in §14.

During execution:

- First `Ctrl-C` requests query cancellation and prints `Cancelling…`.
- Repeated Ctrl-C may repeat the status but must not falsely report successful cancellation or
  corrupt the terminal.
- Once acknowledged, the shell reports `Query cancelled after <duration>.` and returns to a usable
  prompt when the session remains usable.
- Cancellation is distinct from deadline expiration and tracked-memory exhaustion; each retains its
  authoritative engine error class/message.
- A cancelled query never prints a success summary.

### 15.2 Progress layout

Progress appears only on a TTY and only after a short delay, recommended 500 ms, to avoid flicker:

```text
Running… 41% · 2.3 s · Ctrl-C to cancel
```

When no percentage exists, show elapsed time without inventing one. Updates are rate-limited and use
one redrawable stderr line. Progress is cleared before results or diagnostics print. Batch progress
is off unless explicitly enabled, and even when enabled remains on stderr.

## 16. Errors and warnings

### 16.1 Diagnostic layout

The first line preserves the engine's authoritative error text. Source context follows only when a
real span is available:

```text
Parser exception: expected ')' before RETURN
  --> input:1:17
   |
 1 | MATCH (p:Person RETURN p;
   |                 ^^^^^^ expected ')'
```

- Do not fabricate a caret from string matching.
- Multiline diagnostics include line and Unicode-aware column.
- A multi-statement submission identifies the failing statement number.
- Binder, runtime, transaction, interruption, deadline, and memory errors remain distinguishable.
- Warnings appear before the result summary and never inside CSV/TSV row data.
- Suggestions are concrete and limited. A one-token Cypher parse failure may suggest a close meta
  command; an unknown meta command may suggest the closest command.
- Color is supplemental. Plain output retains labels, source lines, and marker characters.
- Internal panic/backtrace text is never presented as a normal user error. A panic is a product bug,
  not an accepted CLI state.

### 16.2 Batch policy and exit status

| Exit | Meaning |
|---:|---|
| `0` | Every requested operation succeeded |
| `1` | Query, init, import/export, result-write, or other runtime operation failed |
| `2` | Invocation, option, configuration, or meta-command usage error |
| `130` | User interruption terminated the invocation |

- Errors and warnings go to stderr.
- `--keep-going` continues independent autocommit statements and exits 1 if any failed.
- It does not continue through an unusable explicit transaction.
- stdout produced before a streaming failure is partial. The nonzero status and stderr diagnostic
  are mandatory; formats with an envelope additionally mark incompleteness.
- A successful process must not leave an error record or partial-output warning.

## 17. Formats and machine-readable output

### 17.1 Required formats

| Format | Purpose |
|---|---|
| `auto` | Selector: `box` on a capable TTY, `table` on a dumb TTY, and `tsv` when redirected |
| `box` | Unicode bordered human table |
| `table` | ASCII bordered human table |
| `csv` | RFC 4180-style row exchange |
| `tsv` | Escaped tab-separated row exchange |
| `json` | One versioned document containing all result sets |
| `jsonl` | Streaming typed records, one JSON object per line |
| `markdown` | Markdown table for documentation |
| `line` | One labeled column value per physical line/record |
| `trash` | Execute and consume results without row output |

`auto` is the default selector, not a renderer or an alias recorded in output. An explicit format
remains selected when the result destination changes.

Legacy `column`, `list`, HTML, and LaTeX renderers are not required by this contract. They may be
added later only with explicit behavior and tests; they are not aliases that silently select another
format.

### 17.2 CSV and TSV

- Headers are enabled by default and controlled by `--header`/`--no-header`.
- CSV quoting follows RFC 4180 behavior, including embedded delimiter, quote, CR, and LF.
- TSV escapes backslash, tab, CR, and LF so one result row remains one physical line.
- NULL uses unquoted `\N` by default; a literal string `\N` remains distinguishable through normal
  string quoting/escaping.
- Empty string is distinct from NULL.
- `--null <TOKEN>` changes the NULL token for CSV/TSV only.
- CSV/TSV batch output supports one row-producing result set per invocation. Scripts containing
  several row-producing results must use JSON/JSONL or separate invocations; the CLI must reject the
  ambiguous form before emitting row data.
- Status-only statements may precede the one row-producing result; their messages stay off stdout.

### 17.3 JSON document

JSON output is always one syntactically valid document, including after a statement error or
cancellation. Its top level is a versioned envelope:

```json
{
  "version": 1,
  "complete": true,
  "results": [
    {
      "statement": 1,
      "columns": [{"name": "answer", "type": "INT64"}],
      "rows": [[42]],
      "summary": {"rows": 1}
    }
  ]
}
```

- Rows are positional arrays so duplicate column names remain lossless.
- The following value mapping is normative:

  | Koko value | JSON representation |
  |---|---|
  | NULL, boolean, string | Native `null`, boolean, or string |
  | Integer within JSON's exact interoperable range | Native JSON number |
  | Wider integer | `{"$type":"INTEGER","logical_type":"INT128","value":"…"}` with the actual logical type and a decimal string |
  | DECIMAL | `{"$type":"DECIMAL","precision":P,"scale":S,"value":"…"}` using canonical decimal text |
  | Finite FLOAT/DOUBLE | Native number using shortest round-trippable text |
  | NaN or infinity | `{"$type":"NONFINITE","logical_type":"DOUBLE","value":"NaN"}` with the actual float type and `NaN`, `+Infinity`, or `-Infinity` |
  | JSON | Native JSON value; object members retain stored order |
  | DATE/TIMESTAMP/TIMESTAMP_TZ/UUID | `{"$type":"<logical type>","value":"<canonical text>"}` |
  | INTERVAL | `{"$type":"INTERVAL","months":M,"days":D,"micros":U}` |
  | BLOB | `{"$type":"BLOB","encoding":"base64","value":"…"}` using standard padded RFC 4648 Base64 |
  | LIST/ARRAY | Native array whose elements use this mapping |
  | STRUCT | `{"$type":"STRUCT","fields":[[name,value],…]}` to retain field order |
  | MAP | `{"$type":"MAP","entries":[[key,value],…]}` to support non-string keys and order |
  | UNION | `{"$type":"UNION","tag":"member","value":…}` |
  | INTERNAL_ID | `{"$type":"INTERNAL_ID","table":"…","offset":"…"}` using decimal strings |
  | NODE | `{"$type":"NODE","id":…,"label":"…","properties":[[name,value],…]}` |
  | REL | `{"$type":"REL","id":…,"src":…,"dst":…,"label":"…","properties":[…]}` |
  | Recursive relationship/path | `{"$type":"RECURSIVE_REL","nodes":[…],"relationships":[…],"cost":…,"degenerate":false,"null_nodes":0}` |

- `$type` is reserved only in CLI wrapper objects. In output, the result schema disambiguates a raw
  user JSON object with a `$type` member; ordered JSON properties and struct fields are never
  collapsed into an unordered map.
- In parameter input, an object with a recognized `$type` shape is decoded as a tagged value. To
  bind colliding raw JSON, use `{"$type":"JSON","value":<raw JSON>}`; that wrapper's `value` is
  never interpreted as another tag.
- Integer values outside $[-(2^{53}-1), 2^{53}-1]$ use the tagged string form so ordinary JSON
  consumers cannot silently round them.
- Materialized relationship endpoint nodes are not duplicated inside a REL wrapper; `src` and `dst`
  carry identity, and a query may return `start_node()`/`end_node()` explicitly when endpoint
  objects are required.
- Timing fields appear only when timing is enabled.
- On failure, `complete` is `false` and the envelope includes a structured error record with the
  statement index and authoritative message. The process still exits nonzero and prints the human
  diagnostic to stderr.
- This mapping must remain consistent for top-level cells, nested values, parameters, and JSONL
  records. Changing it requires an amendment to this UX contract.

### 17.4 JSON Lines

Every physical line is one complete JSON object with:

- `version`;
- `result` index;
- `type`: `schema`, `row`, `summary`, `status`, or `error`;
- the type-specific payload.

A `schema` record precedes rows, every row is positional, and a `summary` closes a successful result.
A failed or cancelled invocation ends with an `error` record and nonzero exit. Consumers can retain
valid prior records without mistaking them for a complete result.

### 17.5 Output destinations

- `--output` writes only selected-format data.
- Existing files require `--force`.
- A successful write atomically publishes the complete file.
- A failure leaves the old destination untouched and removes any temporary output.
- Interactive `:output` changes subsequent result destinations only; prompts and diagnostics remain
  on stderr.
- `trash` still executes statements, consumes results, reports errors, and participates in timing;
  it suppresses row payload only.

## 18. Configuration and initialization

### 18.1 Configuration

The configuration file is TOML and lives in the platform-standard per-user configuration directory:
`$XDG_CONFIG_HOME/koko/config.toml` when set or `~/.config/koko/config.toml` otherwise on XDG
systems, `~/Library/Application Support/Koko/config.toml` on macOS, and
`%APPDATA%\Koko\config.toml` on Windows.

It may contain CLI display/editor settings and comments, but not Cypher statements or query
parameters. Supported keys are explicit; an unknown or invalid key identifies file, line, key, and
accepted values and exits with status 2 before opening a data session. A representative file is:

```toml
format = "auto"
timing = true
progress = "auto"
color = "auto"
rows = 20
width = "auto"
null_display = "literal"
multiline = true
highlight = "auto"
completion = true
history = true
history_limit = 10000
```

Precedence, lowest to highest:

1. built-in defaults;
2. user configuration;
3. explicit `--init` commands;
4. command-line options;
5. interactive meta-command changes.

`--no-config` skips user configuration but does not skip an explicit `--init`.

The CLI must not automatically execute `.kokorc`, another dotfile, or any command file from the
current working directory. This is a deliberate security change from the original shell.

### 18.2 Initialization and `:read`

- `--init` and `:read` accept local UTF-8 files containing Cypher and meta commands.
- The file path is explicit and shown in diagnostics.
- Relative paths inside an included file resolve from that file's directory.
- Include cycles show the complete include chain and fail.
- The default is stop-on-first-error; explicit `--keep-going` changes only eligible autocommit work.
- Execution uses the live session, so deliberate graph or setting changes persist afterward.
- No shell commands, command substitution, remote fetch, or implicit executable file behavior is
  permitted.

## 19. Accessibility and terminal compatibility

- Honor the `NO_COLOR` convention.
- `--color auto` styles only a capable TTY; `always` and `never` are deterministic overrides.
- `TERM=dumb` disables cursor-addressed redraw, live progress, box drawing, and completion menus while
  retaining a functional line-oriented prompt.
- `table`, `line`, CSV, TSV, JSON, and JSONL remain fully usable without Unicode or ANSI support.
- Information is never conveyed by color alone.
- Error markers, selected graph, transaction state, omitted rows, and completion selection all have
  textual or positional indicators.
- Terminal width changes and narrow terminals must not cause invalid UTF-8, cursor drift, or hidden
  diagnostics.
- Screen-reader users can select `line`, TSV, JSON, or JSONL without interactive animation.

## 20. End-to-end examples

### 20.1 Graph and transaction visibility

```text
koko[main]> USE GRAPH analytics;
Using graph analytics.

koko[analytics]> BEGIN TRANSACTION;
Transaction started.

koko[analytics|tx]> CREATE (:Person {name: 'Ada'});
Created 1 node.

koko[analytics|tx]> COMMIT;
Committed transaction.

koko[analytics]>
```

The prose is illustrative; authoritative statement messages retain the engine contract.

### 20.2 Completion

```text
koko[main]> MATCH (p:Person) RETURN p.<Tab>
name       property      STRING        Person
age        property      INT64         Person
```

### 20.3 Interactive redirection without result loss

```text
$ koko >people.tsv
Koko 0.1 · in-memory · graph main
Type :help for help; Ctrl-D or :quit to exit.
koko[main]> MATCH (p:Person) RETURN p.name ORDER BY p.name;
1000 rows · 1 column · compile 0.4 ms · execute 3.4 ms
koko[main]>
```

`people.tsv` receives a header and all 1,000 rows in the redirected-output default format. Greeting,
prompts, and summary remain on the terminal via stderr.

### 20.4 Batch success

```text
$ koko --command "RETURN 42 AS answer" --format csv
answer
42
$ echo $?
0
```

### 20.5 Parameters

```text
$ koko --command 'RETURN $name AS name, $age AS age' \
    --param 'name="Ada"' --param 'age=36' --format csv
name,age
Ada,36
```

### 20.6 Batch error

```text
$ koko --file report.cypher --format json >report.json
Binder exception: Cannot find variable p.
  --> report.cypher:12:8
   |
12 | RETURN p.name;
   |        ^
$ echo $?
1
```

`report.json` remains syntactically valid with `"complete": false`.

### 20.7 Cancellation

```text
koko[main]> MATCH (a), (b), (c) RETURN count(*);
Running… 41% · 2.3 s · Ctrl-C to cancel
^C Cancelling…
Query cancelled after 2.4 s.

koko[main]>
```

## 21. Behavioral acceptance criteria

The UX contract is satisfied only when real terminal and subprocess checks prove the following.

### 21.1 Interactive PTY contracts

- greeting, quiet startup, primary prompt, and continuation prompt;
- selected typed/`ANY` graph and transaction markers;
- parse-complete, incomplete, forced-submit, forced-newline, and lexically scoped explicit
  continuation behavior, including normalization, strings/comments, history, paste, and dumb mode;
- command-line, file, and interactive parameters, including typed errors and value redaction;
- multiple statements and result boundaries;
- bracketed multiline paste and pasted tabs;
- Unicode grapheme movement, deletion, wrapping, and resize;
- every documented editing key;
- history persistence, permissions, deduplication, skip/off/clear, and reverse search;
- command, keyword, graph, label, variable, property, function, setting, and path completion;
- completion refresh after catalog and graph changes;
- syntax highlighting with color on/off and `NO_COLOR`;
- every meta command and invalid-argument path;
- zero, one, truncated, wide, multiline-value, duplicate-name, NULL, and empty-string results;
- multiple result sets, DDL/status results, `EXPLAIN`, and `PROFILE`;
- graph switches, explicit read-write/read-only transactions, failed statements, and exit protection;
- query cancellation, deadline, tracked-memory failure, and progress redraw;
- TTY, narrow TTY, resized TTY, `TERM=dumb`, and redirected stdout behavior.

### 21.2 Batch contracts

- `--command`, `--file`, explicit init, piped stdin, and empty stdin;
- inline and file parameters, precedence, tagged values, and duplicate-name errors;
- mutually exclusive and malformed options;
- no prompt, greeting, ANSI, progress, or summary contamination on stdout;
- stable stdout/stderr split and exit codes;
- stop-first and `--keep-going` behavior, including explicit transactions;
- CSV/TSV headers, escaping, NULL/empty distinction, and multi-result rejection;
- valid complete and incomplete JSON envelopes;
- valid JSONL schema/row/summary/status/error records;
- all required formats and `trash`;
- output-file collision, force, atomic success, failure rollback, and Unicode paths;
- cancellation and partial stdout disclosure;
- active transaction at EOF;
- config precedence, `--no-config`, `--no-history`, and rejection of implicit working-directory init.

### 21.3 Regression expectations from the original shell

Permanent behavior checks must cover the useful original contracts retained in §3: multiline input,
multiple queries, table truncation, all retained key bindings, completion, reverse search, history
deduplication, output switching, schema display, error suggestions, Ctrl-C query cancellation, and
Unicode input. Tests must assert the Rust UX decisions in this document rather than reproduce an
original quirk by accident.

## 22. Architecture and implementation handoff

[`CLI_ARCHITECTURE.md`](CLI_ARCHITECTURE.md) explains how the implementation can satisfy every
normative behavior above, especially:

- authoritative graph/transaction prompt state;
- parser-aware editing without a second Cypher implementation;
- stdout/stderr isolation;
- valid streaming JSON/JSONL failure behavior;
- atomic output files;
- cancellation and progress without terminal corruption;
- catalog-aware completion refresh and parameter-state completion;
- history/config trust boundaries; and
- conformance to the canonical engine value/error contracts and the normative JSON mapping in
  §17.3.

[`CLI_PLAN.md`](CLI_PLAN.md) sequences and verifies that architecture; its compact bootstrap is
[`CLI_GOAL_PROMPT.md`](CLI_GOAL_PROMPT.md). Neither later artifact may silently weaken this UX
contract; a changed behavior must first amend this document with rationale.
