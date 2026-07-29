# Koko first-party CLI goal prompt

> **Status (2026-07-23): historical; completed. Do not resume.**

Work in `/Users/dai/code/koko-rs`. Implement and close Koko's first-party Rust `koko` CLI end
to end. [`CLI_UX.md`](CLI_UX.md) owns all observable behavior and acceptance criteria;
[`CLI_ARCHITECTURE.md`](CLI_ARCHITECTURE.md) owns boundaries, state, and data flow;
[`CLI_PLAN.md`](CLI_PLAN.md) owns dependencies, additive facade APIs, exact landing order,
verification, and the stopping rule; [`ROADMAP.md`](../ROADMAP.md) owns engine scope and permanent
deferrals. The C++ checkout at `/Users/dai/code/koko` is the Cypher-semantics oracle and its shell
is only a retained-behavior reference when `CLI_UX.md` agrees.

Before editing, re-read those contracts and the current facade/parser/result/session/interchange/
warning/interrupt/differential seams. Confirm the preserved **1785 passed / 343 skipped / exactly 23
failures** strict corpus gate and the nine-query correct, timeout-free, every-ratio-`<=2x` performance
gate. Execute `CLI_PLAN.md` L1-L9 in exact order, commit every green landing with the repository
trailer, and keep its landing record current.

Do not add a second Cypher implementation, mirror engine state, parse rendered engine output, bypass
the public `koko` facade, use unbounded output or queues, weaken atomic/machine output, or activate
anything deferred by `ROADMAP.md`. A UX or architecture change first amends the owning contract.

Stop only when `python3 scripts/cli_goal_gate.py --strict` passes every PTY-01..12, BAT-01..10, and
REG-01 contract with no required skip or timeout; the strict corpus and repeated performance gates
remain green; Rust 1.85, debug/release workspace, supported-host, fmt, and strict Clippy checks pass;
resource failures leave no partial state or artifacts; documentation and evidence agree; ordered
commits exist; and the working tree is clean. Then mark this prompt historical and report the final
CLI and preserved-engine scorecard.
