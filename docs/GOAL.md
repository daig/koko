# Archived GOAL charter — completed M1–M5 correctness track

*Written 2026-07-02 for `/goal`-driven sessions. Grounding: [`../fable-audit.md`](../fable-audit.md)
(the then-definitive audit), the historical milestone form of [`../ROADMAP.md`](../ROADMAP.md),
`fable-audit/fix-notes.md` (C++ mechanism notes), and [`PROGRESS.md`](PROGRESS.md) (session state).*

> **COMPLETED 2026-07-07.** All seven §3 criteria passed in the final strict run. This charter is
> retained as correctness-track evidence and must not be resumed. `../ROADMAP.md` now owns current
> product scope and work.

## 1. The goal

Execute **ROADMAP.md milestones M1 through M5** (the correctness track) until the acceptance
criteria in §3 all pass. This historical charter stopped at the then-named P4 boundary: durable
storage, file formats beyond CSV, extensions, bindings, and multi-writer were categorized rather
than built. Current scope split those concerns by actual dependency: native durability remains
permanently deferred, while durability-independent work moved to IM1–IM5.
The C++ engine at `/Users/dai/code/koko` and its `.test` corpus were the historical oracle and
contract for this track.

## 2. Progress metric (report every iteration)

The scorecard tuple, computed by the commands in §3 and recorded in `PROGRESS.md`:

```
(fix-me triage rows, battery DIFFs unexplained, panics, p0-diff files, corpus p/s/f)
```

Baseline (2026-07-01 audit): fix-me rows **≈379 untriaged**, battery DIFFs **~45**, panics
**2 sites / 142 arity probes**, p0-diff files **18/48**, corpus **1257p/341s/379f (+demo_db dir
panics)**. Completion drives the first four numbers to **0** and explains every remaining corpus
failure. The tuple must never regress across a landed commit (a dir's pass count dropping = stop
and fix before anything else).

## 3. Completion = ALL of the following, freshly run, in one session

1. **Hygiene:** `cargo build --workspace --release && cargo test --workspace &&
   cargo clippy --workspace --all-targets && cargo fmt --check` — all clean.
2. **Full sweep, no crashes:** `scripts/goal_sweep.sh` completes with a `TOTALS` line showing
   **0 runner-level failures to parse any .test file, 0 panics** (grep `panicked` over outputs =
   empty; `demo_db` runs to completion).
3. **p0 oracle-clean:** `python3 docs/fable-audit/p0_to_probe.py` → **“… 0 with diffs …”**
   after accounting for the explicit active decisions whose fixtures assert the decided behavior.
4. **Deviation battery clean:** `python3 docs/fable-audit/diffprobe.py` over every probe file in
   `docs/fable-audit/probes/` → **0 DIFF lines**, except lines whose exact probe statement was
   recorded by the M1 compatibility-decision ledger.
5. **No arity/robustness panics:** `scripts/arity_sweep.sh` → `panics=0`.
6. **Triage manifest closed:** `docs/TRIAGE.tsv` (schema in §4, built in M1) satisfies, against a
   fresh sweep: every failing case has exactly one row; no row is stale (listed-but-passing); and
   **zero rows carry a `fix-m*` category**—every remaining failure is `divergence`, `p4`, `p5`, or
   `harness-concurrency`, with a historical audit reference.
7. **Docs current:** `PROGRESS.md` milestone checklist M1–M5 all checked with per-milestone gate
   evidence; `fable-audit.md` header updated with the final scoreboard and a dated completion note.

*(M1 builds `scripts/goal_gate.py` bundling 1–6 into one strict exit-0 command; from then on,
“completion” = `goal_gate.py --strict` exits 0 and #7 holds.)*

## 4. The triage manifest — `docs/TRIAGE.tsv`

Tab-separated, one row per failing corpus case:
`case_id<TAB>category<TAB>ref<TAB>note` where `case_id` = `<dir>/<file-stem>.<case-name>` exactly
as the runner prints it; `category` ∈ `fix-m1..fix-m5 | divergence | p4 | p5 |
harness-concurrency`; `ref` = a historical fable-audit tag or compatibility-decision ID. Build it
in M1 by joining a fresh sweep against the audit taxonomies (`docs/fable-audit/A1–A10`,
`B1-pi-disposition.md`)—subagent-friendly work. **Categorization honesty rule:** `divergence`
requires an explicit decision with a passing probe; anything else is `fix-m*`. New categories or
decisions not derivable from the audit/roadmap defaults → log under "Decisions for review" in
`PROGRESS.md`, choose the parity-leaning default, continue.

## 5. Session protocol (context will fill — assume many sessions)

1. **Resume:** read `PROGRESS.md` (state + next actions), then only the ROADMAP/audit/fix-notes
   sections the current item needs. Trust committed state over memory.
2. **Work loop:** smallest verifiable increment → verify (targeted probes vs the C++ shell +
   affected corpus dirs) → commit (clippy/fmt clean, `Co-Authored-By: Claude <model> <noreply@anthropic.com>`
   trailer) → update the scorecard + next-actions in `PROGRESS.md`. Commits are the durable state;
   `PROGRESS.md` is the resume pointer — keep it accurate *before* context runs low, and keep a
   line-limit discipline (~150 lines; archive detail into commit messages).
3. **Verification discipline:** the corpus `.test` files and the C++ repo are **read-only** —
   when engine and corpus disagree, fix the engine or ledger a divergence, never the test. p0
   fixtures may change only toward oracle/ledger agreement (re-verify via `p0_to_probe.py`).
   Optimizer/exec changes need the A/B invariants (`KOKO_NO_OPTIMIZE=1`, `KOKO_THREADS=1`
   byte-identical) and the lsqb perf gate (`scripts/perf_gate.py`) not regressing. Never
   reproduce a C++ crash/UB (audit §4); those are `divergence` rows.
4. **Subagents/workflows (opus):** fan out for read-only triage, probe-battery authoring/runs,
   corpus-failure classification, and adversarial verification of fixes; large mechanical sweeps
   (e.g. the M4 error-wording pass) suit a Workflow with per-item verify stages. Keep engine-code
   design decisions and final diffs in the main session. Subagents must not mutate either repo;
   give them the tool crib in `PROGRESS.md` §Tools.
5. **Milestone boundaries:** on completing each M-gate, record the evidence (numbers + commands)
   in `PROGRESS.md`, re-run the full scorecard, and append a dated status line to the
   `fable-audit.md` header before starting the next milestone.
