#!/usr/bin/env python3
"""Re-diff Koko's hermetic product fixtures against the historical Ladybug reference.

Each `-CASE` becomes its own probe and runs in a fresh session in both engines, matching the
product runner's fresh-database-per-case semantics. Exact entries in `active_divergences.json` are
recognized differential decisions; historical Markdown prose cannot hide an observed mismatch.
A successful compatibility audit observes every active statement exactly once and no unnamed
differences.

The tool is optional post-v0 evidence for changes that intentionally own inherited compatibility,
not a universal Koko landing gate.

Cases requiring non-empty datasets, multiple connections, environment substitution, or
LOOP/answer-file machinery are skipped. `behavior_decisions.test` is skipped because it is the
fixture and several statements intentionally crash the reference shell.
"""

import glob
import json
import os
import re
import subprocess
import sys

REPO_ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
PRODUCT_FIXTURES = os.path.join(REPO_ROOT, "crates/koko-test-runner/tests/product")
SCRATCH = os.path.dirname(os.path.abspath(__file__))
OUTDIR = os.path.join(SCRATCH, "product_probes")
DECISIONS = os.path.join(REPO_ROOT, "ROADMAP.md")
ACTIVE_DIVERGENCES = os.path.join(SCRATCH, "active_divergences.json")
os.makedirs(OUTDIR, exist_ok=True)


def split_stmts(stmt):
    """Split a -STATEMENT body on ; (quote-aware, simple)."""
    parts, cur, inq = [], [], None
    for ch in stmt:
        if inq:
            cur.append(ch)
            if ch == inq:
                inq = None
        elif ch in ("'", '"'):
            inq = ch
            cur.append(ch)
        elif ch == ";":
            parts.append("".join(cur).strip())
            cur = []
        else:
            cur.append(ch)
    if "".join(cur).strip():
        parts.append("".join(cur).strip())
    return [p for p in parts if p]


def convert(path):
    """Yield (case_name, stmts, skip_reason) per -CASE in a .test file."""
    lines = open(path).read().splitlines()
    file_skip = None
    cases = []  # (name, stmts, skip)
    cur_name, cur_stmts, cur_skip = None, [], None
    i = 0
    while i < len(lines):
        l = lines[i]
        if l.startswith("-DATASET") and not re.search(r"CSV (empty|none)", l, re.I):
            file_skip = f"dataset {l.strip()}"
        if l.startswith("-CASE "):
            if cur_name is not None:
                cases.append((cur_name, cur_stmts, cur_skip))
            cur_name, cur_stmts, cur_skip = l[6:].strip(), [], None
            i += 1
            continue
        if l.startswith("-CREATE_CONNECTION") or l.startswith("-LOOP") or "${" in l:
            cur_skip = cur_skip or f"directive {l.split()[0] if l.split() else l}"
        if l.startswith("-STATEMENT"):
            body = [l[len("-STATEMENT"):].strip()]
            i += 1
            while i < len(lines) and not lines[i].startswith("---- ") and not lines[i].startswith("-"):
                body.append(lines[i].strip())
                i += 1
            stmt = " ".join(b for b in body if b)
            if "${" in stmt:
                cur_skip = cur_skip or "env substitution"
            cur_stmts.extend(split_stmts(stmt))
            continue
        i += 1
    if cur_name is not None:
        cases.append((cur_name, cur_stmts, cur_skip))
    return cases, file_skip


def norm_ws(s):
    """Collapse whitespace runs so line-wrapped ledger citations still match."""
    return re.sub(r"\s+", " ", s).strip()


def active_divergences():
    """Load the explicit active-statement inventory, keyed by decision ID."""
    if not os.path.exists(ACTIVE_DIVERGENCES):
        raise RuntimeError(f"missing active divergence inventory: {ACTIVE_DIVERGENCES}")
    with open(ACTIVE_DIVERGENCES) as source:
        document = json.load(source)
    if document.get("version") != 1 or not isinstance(document.get("divergences"), dict):
        raise RuntimeError("active divergence inventory must have version 1 and a divergences map")
    decision_text = open(DECISIONS).read()
    active = {}
    for divergence_id, entries in document["divergences"].items():
        if divergence_id not in decision_text:
            raise RuntimeError(f"active divergence ID is absent from ROADMAP.md: {divergence_id}")
        if not isinstance(entries, list) or not entries:
            raise RuntimeError(f"active divergence has no statements: {divergence_id}")
        for entry in entries:
            fixture = entry.get("fixture")
            statement = norm_ws(entry.get("statement", ""))
            if not fixture or not statement:
                raise RuntimeError(f"invalid active divergence entry: {divergence_id}")
            if statement in active:
                previous = active[statement][0]
                raise RuntimeError(
                    f"active statement belongs to both {previous} and {divergence_id}: {statement}"
                )
            active[statement] = (divergence_id, fixture)
    return active


def main():
    active = active_divergences()
    observed_active = []
    unnamed_differences = 0
    results = []
    for path in sorted(glob.glob(os.path.join(PRODUCT_FIXTURES, "*.test"))):
        name = os.path.basename(path)[:-5]
        if name == "behavior_decisions":
            results.append((name, "SKIP", "intentional-decision fixture (all cited)"))
            continue
        cases, file_skip = convert(path)
        if file_skip:
            results.append((name, "SKIP", file_skip))
            continue
        file_diffs = 0
        file_ledgered = 0
        file_probes = 0
        for case_name, stmts, skip in cases:
            if skip:
                results.append((f"{name}.{case_name}", "SKIP", skip))
                continue
            if not stmts:
                continue
            probe = os.path.join(OUTDIR, f"{name}.{case_name}.probe")
            with open(probe, "w") as f:
                f.write("\n".join(stmts) + "\n")
            r = subprocess.run(
                [sys.executable, os.path.join(SCRATCH, "diffprobe.py"), probe],
                capture_output=True,
                text=True,
                timeout=600,
            )
            tail = r.stdout.strip().splitlines()[-1] if r.stdout.strip() else "?"
            m = re.match(r"(\d+) probes, (\d+) mismatches", tail)
            file_probes += int(m.group(1)) if m else 0
            n_mismatch = int(m.group(2)) if m else -1
            if n_mismatch != 0:
                # Split explicitly active divergences from real ones.
                real = []
                named_diffs = list(re.finditer(r"^=== \[DIFF\] (.*)$", r.stdout, re.M))
                for dm in named_diffs:
                    stmt = norm_ws(dm.group(1))
                    active_entry = active.get(stmt)
                    if active_entry is None:
                        real.append(stmt)
                        continue
                    divergence_id, expected_fixture = active_entry
                    fixture = os.path.basename(path)
                    if fixture != expected_fixture:
                        real.append(
                            f"{stmt} (inventory expects {expected_fixture}, observed in {fixture})"
                        )
                        continue
                    file_ledgered += 1
                    observed_active.append((stmt, divergence_id, fixture, case_name))
                if n_mismatch == -1:
                    real.append(f"(diffprobe failed: {tail})")
                elif len(named_diffs) != n_mismatch:
                    real.append(
                        f"(diffprobe reported {n_mismatch} mismatches but exposed "
                        f"{len(named_diffs)} named statements)"
                    )
                unnamed_differences += len(real)
                file_diffs += len(real)
                if real:
                    print(f"##### {name}.{case_name}")
                    print(r.stdout)
        if file_diffs:
            status, info = "DIFF", f"{file_probes} probes, {file_diffs} unledgered diffs"
        elif file_ledgered:
            status, info = "LEDGD", f"{file_probes} probes, {file_ledgered} ledgered diffs"
        else:
            status, info = "CLEAN", f"{file_probes} probes, 0 mismatches"
        results.append((name, status, info))

    print("\n===== SUMMARY =====")
    for name, status, info in results:
        print(f"{status:5}  {name}  ({info})")
    n_diff = sum(1 for _, status, _ in results if status == "DIFF")
    n_clean = sum(1 for _, status, _ in results if status == "CLEAN")
    n_ledg = sum(1 for _, status, _ in results if status == "LEDGD")
    n_skip = sum(1 for _, status, _ in results if status == "SKIP")
    print(f"\n{n_clean} clean, {n_diff} with diffs, {n_ledg} ledgered, {n_skip} skipped")

    observed_counts = {}
    for statement, divergence_id, fixture, case_name in observed_active:
        observed_counts[statement] = observed_counts.get(statement, 0) + 1
        print(f"ACTIVE  {divergence_id}  {fixture}:{case_name}  {statement}")
    missing = sorted(set(active) - set(observed_counts))
    repeated = sorted(statement for statement, count in observed_counts.items() if count != 1)
    for statement in missing:
        divergence_id, fixture = active[statement]
        print(f"MISSING {divergence_id}  {fixture}  {statement}")
    for statement in repeated:
        divergence_id, fixture = active[statement]
        print(
            f"REPEAT  {divergence_id}  {fixture}  {statement} "
            f"({observed_counts[statement]} observations)"
        )
    fixture_count = len({fixture for _, _, fixture, _ in observed_active})
    print(
        f"\n{len(observed_active)} explicitly named active statement differences across "
        f"{fixture_count} fixture files; {unnamed_differences} unnamed differences"
    )
    return 1 if n_diff or unnamed_differences or missing or repeated else 0


if __name__ == "__main__":
    sys.exit(main())
