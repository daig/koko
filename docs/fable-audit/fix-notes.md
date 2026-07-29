# fix-notes — C++ mechanism notes for still-open gaps

Extracted from the `docs/pi/` gap tree (2026-06-29/30) before its removal on 2026-07-01 — the fold-and-delete
is recorded in `B1-pi-disposition.md`, and the full original tickets remain in git history (`docs/pi/` prior
to this commit). Only *still-open* items with fix-level C++ evidence are kept here; everything else is owned
by `../../fable-audit.md` and the A1–A10 sub-reports. Line numbers are as-of the audit date.

## Engine behavior (each verified in fable-audit §3)

**Repeated `ON CREATE SET`/`ON MATCH SET` (audit W8).** Parser accumulation bug: `merge_clause` overwrites
`on_create`/`on_match` vectors on each repeated clause (`koko-parser/src/parser.rs:1067-1081`) instead of
extending. C++ grammar allows zero-or-more merge actions (`Cypher.g4:356-360`) and the binder iterates all
(`bind_updating_clause.cpp:112-121`). Fix: `extend` instead of assign; preserve source order; check duplicate-
property last-write order against C++.

**MERGE non-suppress duplicate-input cardinality (audit W9).** When `suppress_dup` is gated off because a
non-key variable is carried, Rust emits per-input-row where C++'s factorized output collapses duplicates
(`MATCH (n:Q) UNWIND [1,1] AS i MERGE (p:P {id:i}) RETURN n.qid` → 4 vs 2 rows). The dedup gate itself is
C++-faithful (`MergePlan.suppress_dup` mirrors `logical_merge.cpp:57`; keys per `plan_update.cpp:58-73`);
the divergence is the non-suppress *execution* path.

**UINT128 `range`/`list_product` narrowing (audit V16).** `range` type-checks UINT128 but evaluates via
`as_i64().unwrap_or(...)` returning INT64 elements (`scalarfn.rs:977-993`); `list_product` multiplies
`filter_map(as_i64)`, silently skipping wide values (`scalarfn.rs:475-486`, `:1157-1170`). C++ registers
per-width overloads (`list_range_function.cpp:86-102`) and dispatches product on the child type
(`list_agg_function.cpp:12-27`, `:53-74`). Fix: bind result type from the child type; typed checked
accumulators (u128 path); audit `list_sum` for the same pattern. Related: the SUM aggregate's i128
accumulator wraps for UINT128 (audit V3) — same typed-accumulator fix family.

**PK type eligibility (audit §3.3).** C++ validates PK types in the DDL binder — allows int widths,
INT128/UINT128, STRING, FLOAT, DOUBLE; rejects the rest (`bind_ddl.cpp:116-153`). Rust has *two different
wrong policies*: binder/catalog accept any type (`koko-binder/src/lib.rs:628-665`,
`koko-catalog/src/lib.rs:488-529`), while storage's `PkKey` supports int/STRING/BOOL/UUID/DATE/timestamps
but not FLOAT/DOUBLE (`koko-storage/src/lib.rs:205-230`, runtime error at `:677-686`). Fix: one binder-level
allow-list mirroring C++, plus FLOAT/DOUBLE `PkKey` encoding (explicit NaN/-0.0 canonicalization).

**`storage_direction` option (audit §3.3).** Parsed into the AST (`parser.rs:770-794`) but not forwarded by
`bind_create_rel_table` (`koko-binder/src/lib.rs:668-699`); execution calls `catalog.create_rel_table`
which defaults `Both` (`koko/src/lib.rs:244-263`); in-mem storage always builds both adjacencies
(`koko-storage/src/lib.rs:556-584`). C++ stores it in rel-group metadata and *validates query direction*
against it (`bind_ddl.cpp:198-205`, `bind_graph_pattern.cpp:160-246` — undirected patterns reject on
fwd-only tables). Minimum parity: thread the metadata + add the binder direction check; keeping both
adjacencies physically is a legitimate in-memory implementation detail.

**`regexp_replace` option validation (audit §3.3).** Rust runtime-checks only `contains('g')`
(`scalarfn.rs:930-945`). C++ bind-time requires a literal STRING exactly `"g"`
(`regex_replace_function.cpp:78-101`; overloads `:121-136`). Fix at bind: arity 3-or-4, literal-`"g"`-only.

**`split_part` empty separator (audit V17).** Rust special-cases `''` as whole-string (`scalarfn.rs:874-889`);
C++ splits per character (`string_utils.cpp:79-100` — empty delimiter = next char boundary;
`split_part('Alice','',5)` → `e`). Rust's `string_split` already has the per-char behavior — reuse it.

**Long sort keywords (audit §2).** `oC_SortItem` accepts `ASCENDING|ASC|DESCENDING|DESC`
(`Cypher.g4:394-406`); Rust checks only the short forms (`parser.rs:1422-1438`). Trivial parser fix.

**`EXISTS{}` in `WITH … WHERE` (audit §2).** `bind_query` binds the WITH-WHERE in the carried scope
(`koko-binder/src/lib.rs:1070-1077`) then rejects if it staged a subquery (`:1078-1084`) — the bound plan's
`input_filter` has no subquery list. Fix options: materialize an explicit filter part after the WITH
projection, or extend `BoundPart` input filters with staged subqueries. (Same guard also blocks sequence
calls there.)

**HINT inside `EXISTS{}`/`COUNT{}` (audit §2).** C++ grammar allows it (`Cypher.g4:642-643`), binds via the
same join-hint path as MATCH (`bind_subquery_expression.cpp:65-67`). Rust's subquery parser demands `}`
right after the optional WHERE (`parser.rs:1766-1782`). Minimum parity = parse-and-discard, like top-level
`match_clause` (`parser.rs:990-995`).

**Graph-value *expressions* through `WITH` (audit §2; issue.2589).** Bare node/rel variables carry; a
node/rel-typed *expression* projection still throws "carrying a node or relationship expression through
WITH is not supported in this phase". Owning seams: `koko-binder` projection binding +
`koko-processor` `materialize_carried`.

**Implicit-cast remaining slices (audit §3.4/§6.1).** The property/SET/list-element paths now share C++'s
`hasImplicitCast` rule (numeric↔numeric + any→STRING except BLOB/INTERNAL_ID/NODE/REL/RECURSIVE_REL —
`vector_cast_functions.cpp`, `built_in_function_utils.cpp:73-136`, `expression_binder.cpp:103-124`). Still
NOT routed through it: (a) `ALTER … ADD … DEFAULT` value binding; (b) per-argument coercion to declared
scalar-function parameter types (blocked on the audit's §6.1 signature catalog); (c) CALL-config values —
C++ *validates option names*, type-checks with a cast, and constant-folds expressions
(`bind_standalone_call.cpp:17-39`): Rust accepts unknown options as no-ops, rejects non-literal values
(`koko/src/lib.rs:707-745`), and `current_setting` lacks C++ defaults.

**Recent-fix over-corrections (B1 bonus residuals, unverified-depth).** The 2026-06-30 coalesce/ifnull
bind-time type-check rejects `coalesce(1, true)` / struct-arg merges that C++ accepts; the duplicate-map-key
rejection (`2692cd1`) covers literal keys at bind but not eval-time duplicates under
`DISABLE_MAP_KEY_CHECK=FALSE`.

**Rel-group self-loop narrowing residual.** A self-loop over a multi-labeled node (`(a)-[:R]->(a)`, `R: X→Y`,
`a` labeled X and Y) keeps the old "Nodes are not connected through relationship table R." message; C++
narrows the node and proceeds (`query_graph_label_analyzer.cpp:76-127`).

## Public API / tooling inventory (audit §2, P5-adjacent)

From feature-gaps §6 — absent vs the C++ `main/` surface: interactive shell (multiline/history/completion/
output modes); C ABI (`koko.h`) + language bindings; Arrow C Data Interface (in-memory `query_as_arrow`,
Arrow-backed tables) distinct from the P4 file readers; QueryResult column-type metadata, query
summary/timing, multi-result chains (public `query()` is single-statement — parser demands EOF); prepared-
statement metadata + typed parameter bind; Connection interrupt/timeout/thread controls; UDF registration;
`StorageDriver` direct scan/count embedding API.

## Runner (complements A10)

- Corpus placeholder expansion unhandled: `-SET`, `${COLS}`-style substitution, REPEAT/ARANGE row builders.
- `-SKIP` case *bodies* are still strict-parsed — one unsupported directive inside a skipped case
  parse-fails the whole file (distinct from the `-SKIP <trailing comment>` bug in audit §5).
