# Archived `/goal` entry prompt for the completed M1–M5 track

> **ARCHIVED 2026-07-07:** this prompt completed its M1–M5 purpose. Do not paste it into a new
> session. The former P4 durability phase is permanently deferred; current orientation starts at
> `../ROADMAP.md`.

---

GOAL: Make koko-rs oracle-faithful to the C++ engine by completing the correctness track — milestones M1→M5 of `ROADMAP.md` — as specified by the charter `docs/GOAL.md`. Work the milestones strictly in order.

GROUNDING (read in this order, then start):
1. `docs/PROGRESS.md` — current state, scorecard, next actions, tools crib. This is the resume pointer; trust it + git log over any assumption.
2. `docs/GOAL.md` — the charter: completion criteria (§3), triage-manifest schema (§4), session protocol + guardrails (§5).
3. `ROADMAP.md` — the milestone plan (item tags reference fable-audit rows).
4. `fable-audit.md` + `docs/fable-audit/` (sub-reports A1–A10, fix-notes.md, probes/) — the audited deviation inventory with verified repros; consult per-item as you work.

COMPLETION (verifiable; do not declare done on anything less): all seven criteria of `docs/GOAL.md` §3 pass in one fresh run — build/test/clippy/fmt clean; `scripts/goal_sweep.sh` with zero panics and zero unparsed .test files; `docs/fable-audit/p0_to_probe.py` reports 0 diff files; the deviation battery (`docs/fable-audit/diffprobe.py` over `docs/fable-audit/probes/`) has 0 DIFFs outside the historical machine inventory; `scripts/arity_sweep.sh` prints `panics=0`; `docs/TRIAGE.tsv` has zero `fix-m*` rows, no unlisted failures, no stale rows against a fresh sweep; PROGRESS.md milestones M1–M5 all checked with evidence and fable-audit.md's header carries the dated final scoreboard. From M1 onward, bundle criteria 1–6 into `scripts/goal_gate.py` (spec in GOAL.md §3.6).

PROGRESS METRIC (report the delta every iteration, and record it in PROGRESS.md's scorecard table): `(fix-me TRIAGE rows, unexplained battery DIFFs, panics, p0-diff files, corpus p/s/f)`. Baseline 2026-07-01: (~379 untriaged, ~45, 142 arity-probe panics + 2 sites, 18/48, 1257/341/379 + demo_db panicking). All five must move toward 0/0/0/0/explained monotonically; any regression in a landed commit is a stop-the-line defect.

OPERATING RULES (digest — GOAL.md §5 governs):
- Smallest verifiable increment → verify against the C++ shell + affected corpus dirs → commit (clippy/fmt clean, Co-Authored-By trailer) → update PROGRESS.md. Commits are durable state; keep PROGRESS.md accurate before context runs low so any future session can resume cold.
- The C++ repo and corpus `.test` files are read-only historical evidence. Fix the engine or record an explicit decision with a probe; never edit corpus inputs merely to erase a differential.
- Divergence decisions: apply ROADMAP M1's recommended defaults; for genuinely new calls, choose the parity-leaning default, log it under "Decisions for review" in PROGRESS.md, and continue.
- Optimizer/exec-touching changes: `KOKO_NO_OPTIMIZE=1` and `KOKO_THREADS=1` A/B byte-identical, and the lsqb perf gate must not regress. Never imitate a C++ crash/UB (audit §4 list).
- Use opus subagents for read-only fan-out (corpus triage, probe batteries, adversarial verification of fixes) and Workflows for large mechanical sweeps with per-item verify stages (e.g. M4 error-wording). Engine design decisions and final diffs stay in the main session. Subagents never mutate the repos; hand them the tools crib from PROGRESS.md.
- Scope ends at the P4 boundary: durable storage / file formats beyond CSV / extensions / bindings / multi-writer are categorized in TRIAGE.tsv, not built. When goal_gate is strict-green and M5 is checked, stop and report completion with the final scorecard.
