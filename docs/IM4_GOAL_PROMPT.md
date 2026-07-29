# IM4 goal entry prompt

> **Completed historical prompt (2026-07-20).** IM4 is closed; do not execute this prompt.
> `../ROADMAP.md` makes IM5 the next phase.

```text
/goal Work in /Users/dai/code/koko-rs. Execute ROADMAP.md “IM4 — Concurrency, controls &
performance” end to end. Treat the then-current ROADMAP.md as phase authority; use docs/TRIAGE.tsv
for the exact residual failures, docs/PERF_GATE.md for the LSQB contract, docs/PROGRESS.md for the
IM3 handoff, and the C++ checkout at /Users/dai/code/koko plus its .test corpus as the behavioral
oracle.

First re-read the current database/connection/MVCC/storage/processor/runner seams, re-measure the
baseline, and create docs/IM4_PLAN.md. Freeze exact affected case IDs, structural skips, stress
scenarios, public API and synchronization contracts, memory-accounting matrix, benchmark protocol,
dependency-ordered landings, and a close gate for each landing. Then execute that plan completely in
the same goal; do not stop after planning, scaffolding, or one subsystem.

IM4 is complete only when:
1. Separate connections demonstrably overlap reader/reader and reader/writer execution under tested
   snapshots, without a query-duration global database lock or O(database) snapshot copies.
2. A product configuration exposes multi-writer MVCC while preserving the single-writer default;
   row, PK, and catalog conflicts, commits, cancellation, errors, and rollback are atomic,
   deterministic, leak-free, and stress-tested.
3. Public interrupt/cancellation and millisecond timeout/deadline controls stop every long-running
   execution path promptly; per-connection worker limits remain effective. No IM4 control is a
   silent no-op.
4. The runner genuinely executes the in-memory-relevant LOOP/dynamic-SET, concurrent-block,
   batch-statement, and row-wise dataset directives. Preserve upstream SKIP/SKIP_IN_MEM decisions;
   do not fake concurrency by serial execution.
5. Every substantial engine-owned allocation, including operator intermediates, is covered by a
   documented database memory budget. OOM is catchable and mutation-safe with no leaked
   reservations. Add temporary spill only where measurement proves it is needed; spill is not
   native durability. Retire both IM4-owned TRIAGE rows.
6. Under a documented repeatable same-machine gate, all nine LSQB queries return the oracle answer,
   complete without timeout, and each Rust/C++ ratio is <=2x while current Rust wins remain <1x.
   Land only profile-justified optimizer/vectorization/parallel changes.
7. Focused concurrency/control/memory/perf tests, the full workspace, all 52 corpus directories,
   P0 re-diff, deviation battery, arity sweep, default/no-opt/one-worker A/B checks, formatting, and
   strict Clippy are green with zero panics, unparsed files, unledgered differences, stale/malformed
   TRIAGE rows, or new silent skips. Record exact counts/timings, synchronize status docs, remove
   temporary artifacts, and commit a clean IM4 closure.

Preserve the IM1–IM3 typed-batch, MVCC, transaction, prepared/result, ingestion, and interchange
contracts. Do not implement IM5 graph/index/extension/connector/binding surface or native database
files, WAL/checkpoint/recovery, durable pages/indexes, or physical storage introspection.
```
