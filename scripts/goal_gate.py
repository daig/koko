#!/usr/bin/env python3
"""Run the historical M1-M5 migration-close audit bundle.

    scripts/goal_gate.py [--strict] [--skip N,N,...] [--reuse-sweep DIR]

Checks preserve the completed GOAL.md §3 protocol:
  1 hygiene   cargo build/test/clippy/fmt all clean
  2 sweep     full Ladybug corpus sweep: no unparsed files or panics; demo_db runs
  3 product   product_to_probe.py -> no unledgered differential statements
  4 battery   historical probe manifest -> no unledgered differences
  5 arity     arity_sweep.sh -> panics=0
  6 triage    frozen TRIAGE.tsv exactly covers the observed historical residual set

The command remains reproducible compatibility tooling, not Koko's universal post-v0 completion
gate. `--strict` retains its original migration meaning. `--reuse-sweep` reuses a prior external
sweep for checks 2/6; a historical close reproduction should use a fresh sweep.
"""

import argparse
import glob
import json
import os
import re
import subprocess
import sys

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
FA = os.path.join(ROOT, "docs", "fable-audit")
PROBES = os.path.join(FA, "probes")
HISTORICAL_DIFFERENCES = os.path.join(FA, "historical_differences.json")
TRIAGE = os.path.join(ROOT, "docs", "TRIAGE.tsv")
CATEGORIES = {"divergence", "p4", "p5", "harness-concurrency"} | {f"fix-m{i}" for i in range(1, 6)}


def sh(cmd, **kw):
    return subprocess.run(cmd, cwd=ROOT, capture_output=True, text=True, **kw)


def norm_ws(s):
    return re.sub(r"\s+", " ", s).strip()


def historical_differences():
    with open(HISTORICAL_DIFFERENCES) as source:
        document = json.load(source)
    entries = document.get("entries")
    if document.get("version") != 1 or not isinstance(entries, list):
        raise RuntimeError("historical difference inventory must have version 1 and an entries list")
    return {norm_ws(entry) for entry in entries}


def check_hygiene():
    # Workspace tests are hermetic. The corpus paths belong to the sweep and
    # differential checks below; leaking them into loader unit tests makes a
    # partial upstream dataset look like an installed fixture family.
    clean_env = os.environ.copy()
    clean_env.pop("KOKO_DATASET_DIR", None)
    clean_env.pop("KOKO_ROOT_DIRECTORY", None)
    for cmd in (
        ["cargo", "build", "--workspace", "--release"],
        ["cargo", "test", "--workspace", "--release"],
        ["cargo", "clippy", "--workspace", "--all-targets"],
        ["cargo", "fmt", "--check"],
    ):
        r = sh(cmd, env=clean_env)
        if r.returncode != 0:
            return False, f"`{' '.join(cmd)}` rc={r.returncode}\n{r.stdout[-800:]}{r.stderr[-800:]}"
        if cmd[1] == "clippy" and re.search(r"^warning", r.stderr, re.M):
            return False, "clippy warnings present"
    return True, "build/test/clippy/fmt clean"


def run_sweep(outdir):
    r = sh(["zsh", os.path.join(ROOT, "scripts", "goal_sweep.sh"), outdir])
    return r


def check_sweep(outdir):
    outs = glob.glob(os.path.join(outdir, "*.txt"))
    if not outs:
        return False, f"no sweep outputs in {outdir}"
    panics, parse_errs = [], []
    totals = [0, 0, 0]
    for f in outs:
        if os.path.basename(f).startswith("_"):
            continue
        text = open(f, errors="replace").read()
        if "panicked" in text:
            panics.append(os.path.basename(f))
        for m in re.finditer(r"^(?:parse error in|error reading) (.*)$", text, re.M):
            parse_errs.append(m.group(1))
        m = re.search(r"^(\d+) passed, (\d+) skipped, (\d+) failed$", text, re.M)
        if m:
            for i in range(3):
                totals[i] += int(m.group(i + 1))
    demo = os.path.join(outdir, "demo_db.txt")
    demo_ok = os.path.exists(demo) and re.search(
        r"passed.*skipped.*failed", open(demo, errors="replace").read()
    )
    msg = (
        f"TOTALS {totals[0]}p/{totals[1]}s/{totals[2]}f; panic-files={len(panics)}"
        f" {panics or ''}; unparsed={len(parse_errs)} {parse_errs[:5] or ''};"
        f" demo_db={'ran' if demo_ok else 'MISSING/DEAD'}"
    )
    return (not panics and not parse_errs and bool(demo_ok)), msg


def check_product_fixtures():
    script = os.path.join(FA, "product_to_probe.py")
    result = sh([sys.executable, script], timeout=3600)
    summary = re.search(
        r"(\d+) clean, (\d+) with diffs, (\d+) ledgered, (\d+) skipped", result.stdout
    )
    if not summary:
        return False, f"product_to_probe.py output not understood: {result.stdout[-300:]}"
    return summary.group(2) == "0", summary.group(0)


def check_battery():
    manifest = os.path.join(PROBES, "MANIFEST.tsv")
    pairs = []
    for line in open(manifest):
        if line.startswith("#") or not line.strip():
            continue
        probe, ds = line.rstrip("\n").split("\t")
        pairs.append((probe, None if ds == "-" else ds))
    ledger = historical_differences()
    unledgered = []
    n_diff_total = 0
    for probe, ds in pairs:
        cmd = [sys.executable, os.path.join(FA, "diffprobe.py"), os.path.join(PROBES, probe)]
        if ds:
            cmd += ["--dataset", ds]
        r = sh(cmd, timeout=900)
        for m in re.finditer(r"^=== \[DIFF\] (.*)$", r.stdout, re.M):
            n_diff_total += 1
            stmt = norm_ws(m.group(1))
            if stmt not in ledger:
                unledgered.append(f"{probe}: {stmt}")
    msg = f"{len(pairs)} probes; {n_diff_total} DIFFs, {len(unledgered)} unledgered"
    if unledgered:
        # Persist the full list for the fix milestones to drain.
        listing = os.path.join(ROOT, "target", "battery_unledgered.txt")
        os.makedirs(os.path.dirname(listing), exist_ok=True)
        with open(listing, "w") as f:
            f.write("\n".join(unledgered) + "\n")
        msg += f" (full list: {listing})"
        msg += "\n  " + "\n  ".join(unledgered[:20])
        if len(unledgered) > 20:
            msg += f"\n  ... and {len(unledgered) - 20} more"
    return not unledgered, msg


def check_arity():
    r = sh(["zsh", os.path.join(ROOT, "scripts", "arity_sweep.sh")], timeout=3600)
    m = re.search(r"panics=(\d+)", r.stdout)
    if not m:
        return False, f"arity_sweep output not understood: {r.stdout[-200:]}"
    return m.group(1) == "0", m.group(0).strip()


def sweep_failures(outdir):
    """case_id set ('<dir>/<stem>.<case>') from a goal_sweep output dir."""
    fails = set()
    for f in glob.glob(os.path.join(outdir, "*.txt")):
        base = os.path.basename(f)[:-4]
        if base.startswith("_"):
            continue
        # Case names may contain spaces — capture up to the ` — ` separator.
        for m in re.finditer(r"^FAIL {2}(.+?) — ", open(f, errors="replace").read(), re.M):
            fails.add(f"{base}/{m.group(1)}")
    return fails


def check_triage(outdir, strict):
    if not os.path.exists(TRIAGE):
        return False, "docs/TRIAGE.tsv missing"
    rows = {}
    bad = []
    for i, line in enumerate(open(TRIAGE), 1):
        if line.startswith("#") or not line.strip():
            continue
        parts = line.rstrip("\n").split("\t")
        if len(parts) < 4:
            bad.append(f"line {i}: expected 4 tab-separated fields")
            continue
        case_id, cat = parts[0], parts[1]
        if cat not in CATEGORIES:
            bad.append(f"line {i}: unknown category `{cat}`")
        if case_id in rows:
            bad.append(f"line {i}: duplicate row for {case_id}")
        rows[case_id] = cat
    fails = sweep_failures(outdir)
    missing = sorted(fails - rows.keys())
    stale = sorted(rows.keys() - fails)
    fixme = sorted(c for c, cat in rows.items() if cat.startswith("fix-m"))
    msg = (
        f"{len(rows)} rows; sweep fails={len(fails)}; missing={len(missing)};"
        f" stale={len(stale)}; fix-m*={len(fixme)}; malformed={len(bad)}"
    )
    for label, items in (("missing", missing), ("stale", stale), ("malformed", bad)):
        if items:
            msg += f"\n  {label}: " + ", ".join(str(x) for x in items[:10])
            if len(items) > 10:
                msg += f" ... +{len(items) - 10}"
    ok = not missing and not stale and not bad and (not strict or not fixme)
    if strict and fixme:
        msg += f"\n  fix-m* (strict): " + ", ".join(fixme[:10]) + ("..." if len(fixme) > 10 else "")
    return ok, msg


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--strict", action="store_true", help="completion mode: fix-m* rows fail check 6")
    ap.add_argument("--skip", default="", help="comma-separated check numbers to skip")
    ap.add_argument("--reuse-sweep", default=None, help="existing goal_sweep output dir (skips re-running)")
    args = ap.parse_args()
    skip = {int(x) for x in args.skip.split(",") if x}

    outdir = args.reuse_sweep or os.path.join(ROOT, "target", "goal-sweep")
    results = []

    def run(n, name, fn, *a):
        if n in skip:
            results.append((n, name, None, "skipped"))
            return
        ok, msg = fn(*a)
        results.append((n, name, ok, msg))
        print(f"[{n}] {name}: {'PASS' if ok else 'FAIL'} — {msg}", flush=True)

    run(1, "hygiene", check_hygiene)
    if 2 not in skip and 6 not in skip and not args.reuse_sweep:
        print("[2] sweep: running full corpus sweep...", flush=True)
        run_sweep(outdir)
    run(2, "sweep", check_sweep, outdir)
    run(3, "product", check_product_fixtures)
    run(4, "battery", check_battery)
    run(5, "arity", check_arity)
    run(6, "triage", check_triage, outdir, args.strict)

    failed = [n for n, _, ok, _ in results if ok is False]
    print(f"\ngoal_gate: {'GREEN' if not failed else 'RED (failed: ' + ','.join(map(str, failed)) + ')'}"
          f"{' [strict]' if args.strict else ''}")
    sys.exit(1 if failed else 0)


if __name__ == "__main__":
    main()
