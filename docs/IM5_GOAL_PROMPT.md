# IM5 goal entry prompt

> **Historical initial bootstrap.** The six original landings reached their provisional scorecard
> on 2026-07-21; the subsequent correction closed on 2026-07-22. Do not resume this prompt or
> `IM5_CORRECTION_GOAL_PROMPT.md`. `ROADMAP.md` now owns the current product boundary and live
> work.

```text
/goal Work in /Users/dai/code/koko-rs. Complete ROADMAP.md “IM5 — Rust-embedded core
completion” end to end. The then-current ROADMAP.md owns scope; docs/IM5_PLAN.md is the executable
contract and stopping rule; docs/TRIAGE.tsv owns exact residual cases; /Users/dai/code/koko and its
.test corpus are the behavioral oracle.

Re-read the current catalog/storage/database/connection/transaction/binder/processor seams and
confirm the frozen baseline before editing. Then land, in docs/IM5_PLAN.md order: (1) graph-state
ownership and connection/query/transaction routing, (2) typed named graphs, (3) minimal ordered JSON
and schemaless ANY graphs, (4) HASH/ART primary-key index DDL, (5) local read-only icebug-disk, and
(6) connection-local native Rust scalar UDFs. Preserve all IM1–IM4 ownership, MVCC, cancellation,
memory, interchange, corpus, and LSQB contracts. Probe affected semantics against C++; commit each
verified landing and keep status evidence current.

Do not implement or scaffold native durability, Arrow C, extension/plugin support or modules,
PROJECT_GRAPH/GDS, the full JSON extension, FTS/vector, remote/connectors, foreign bindings, or
shell/CLI. Rust-native Arrow RecordBatch interchange and logical export/import remain core; the CLI
is a post-IM5 fast follow-up.

Do not stop at a plan, scaffold, narrow test, or the 22 visible cases. Stop only after every retained
surface works end to end and one fresh close run satisfies docs/IM5_PLAN.md §5: strict frozen corpus
1785 passed / 343 skipped / exactly 23 deferred-or-ledgered failures, zero panic/unparsed/untriaged
state; P0/oracle/deviation/arity and optimizer/one-worker invariants green; repeated nine-query LSQB
correct, timeout-free, and ≤2× C++; focused graph/UDF/index/icebug-disk concurrency, rollback,
cancellation, memory, panic/error, prepared-statement, and public-API contracts green; workspace
release tests/build, all 52 corpus dirs, fmt, and strict Clippy green; scope/status docs synchronized;
artifacts removed; required commits present; working tree clean. Then report the final scorecard and
stop.
```
