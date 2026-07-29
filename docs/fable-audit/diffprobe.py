#!/usr/bin/env python3
"""Differential probe harness: run the same one-statement-per-line Cypher script
through the C++ shell (oracle) and the Rust engine, align per-statement output
blocks, and report mismatches.

Usage: diffprobe.py <probe-file> [--dataset NAME] [--show-all]

Probe file: one Cypher statement per line ('' and '//'-prefixed lines skipped).
Output: for each statement where the two engines differ, print the statement,
the C++ block, and the Rust block. Exit 0 if identical, 1 otherwise.

Comparison: rows are sorted within a block by default (result order is not part
of the contract for most queries). A statement line may be prefixed with
`#ORDER# ` to compare in order.
"""
import subprocess, sys, os, argparse

REPO_ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
REFERENCE_ROOT = os.environ.get(
    "KOKO_REFERENCE_ROOT", os.path.join(os.path.dirname(REPO_ROOT), "ladybug")
)
CPP = os.environ.get(
    "KOKO_CPP_SHELL",
    os.path.join(REFERENCE_ROOT, "build/release/tools/shell/lbug"),
)
RUST = os.environ.get(
    "KOKO_RUST_SHELL",
    os.path.join(REPO_ROOT, "target/release/examples/koko_cli"),
)
DATASET_ROOT = os.environ.get(
    "KOKO_DATASET_DIR", os.path.join(REFERENCE_ROOT, "dataset")
)
SENT = "KOKOSEPQ7F3X"

def read_probes(path):
    probes = []
    for raw in open(path):
        s = raw.strip()
        if not s or s.startswith("//"):
            continue
        ordered = s.startswith("#ORDER# ")
        if ordered:
            s = s[len("#ORDER# "):]
        probes.append((s.rstrip(";"), ordered))
    return probes

def run_cpp(stmts, dataset=None):
    lines = []
    if dataset:
        ddir = os.path.join(DATASET_ROOT, dataset)
        for f in ("schema.cypher", "copy.cypher"):
            p = os.path.join(ddir, f)
            if os.path.exists(p):
                for l in open(p):
                    l = l.strip()
                    if not l:
                        continue
                    if f == "copy.cypher":
                        # make COPY "file.csv" paths absolute
                        l = l.replace('FROM "', f'FROM "{ddir}/').replace("FROM '", f"FROM '{ddir}/")
                    lines.append(l if l.endswith(";") else l + ";")
    n_setup = 0
    if dataset:
        n_setup = len(lines)
    for s, _ in stmts:
        lines.append(s + ";")
        lines.append(f"RETURN '{SENT}' AS {SENT};")
    inp = "\n".join(lines) + "\n"
    r = subprocess.run([CPP, "-m", "list", "--no_stats", "-b"], input=inp,
                       capture_output=True, text=True, timeout=300)
    out = [l for l in r.stdout.splitlines()
           if l not in ("Opening the database under in-memory mode.",
                        'Enter ":help" for usage hints.')]
    # split into blocks on the sentinel pair (header line SENT, row line SENT)
    blocks, cur = [], []
    i = 0
    while i < len(out):
        if out[i] == SENT and i + 1 < len(out) and out[i+1] == SENT:
            blocks.append(cur); cur = []; i += 2
        else:
            cur.append(out[i]); i += 1
    # setup statements produce leading blocks only if dataset was loaded inline —
    # they are not sentinel-separated, so they land inside the first block.
    # Strip setup noise: drop lines before the first probe's output is impossible
    # to distinguish, so instead we count: dataset setup output lines all land in
    # blocks[0] prefix. Simpler: when dataset is used, we emitted no sentinel
    # after setup lines, so their output is prepended to the first probe block.
    # We drop known setup output shapes: 'result' header + '... has been created.'
    # rows and 'N tuples have been copied ...' messages.
    if dataset and blocks:
        blocks[0] = [l for l in blocks[0] if not _is_setup_noise(l)]
    # drop the header line of each block (first line = column names) when present:
    # a block is either empty (no output?), an Error block, or header+rows.
    cleaned = []
    for b in blocks:
        if b and not b[0].startswith("Error:"):
            b = b[1:]  # drop header
            cleaned.append([unquote_row(l) for l in b])
        else:
            # Error blocks are literal message text (decorated parser errors
            # quote the statement themselves) — no CSV unquoting.
            cleaned.append(list(b))
    return cleaned, r.stderr

def _is_setup_noise(l):
    return (l == "result" or l.endswith("has been created.") or
            ("tuples" in l and "copied" in l) or l.endswith("have been copied."))

def unquote_row(row):
    """The C++ shell's list mode CSV-quotes a cell (with `|` delimiter) when it
    is NULL/empty or contains `|`/`"`. Parse quote-aware and re-join raw so rows
    compare against the Rust `.test`-format rendering. NULL and '' both become
    empty (indistinguishable in list mode — probe with typeof() when it matters)."""
    if row.startswith("Error:"):
        return row
    cells, cur, i, inq = [], [], 0, False
    while i < len(row):
        ch = row[i]
        if inq:
            if ch == '"':
                if i + 1 < len(row) and row[i+1] == '"':
                    cur.append('"'); i += 2; continue
                inq = False; i += 1; continue
            cur.append(ch); i += 1
        else:
            if ch == '"' and not cur:
                inq = True; i += 1
            elif ch == '|':
                cells.append(''.join(cur)); cur = []; i += 1
            else:
                cur.append(ch); i += 1
    cells.append(''.join(cur))
    return '|'.join(cells)

def run_rust(stmts, dataset=None):
    env = dict(os.environ)
    if dataset:
        env["KOKO_LOAD_DATASET"] = os.path.join(DATASET_ROOT, dataset)
    inp = "\n".join(s + ";" for s, _ in stmts) + "\n"
    r = subprocess.run([RUST], input=inp, capture_output=True, text=True,
                       timeout=300, env=env)
    blocks, cur = [], []
    for l in r.stdout.splitlines():
        if l == "--KOKOSEP--":
            blocks.append(cur); cur = []
        else:
            cur.append(l)
    return blocks, r.stderr

def run_cpp_resilient(stmts, dataset=None, prefix=()):
    """run_cpp, recovering from oracle crashes. The shell's batch mode buffers
    stdout, so a crash anywhere (audit §4 list) loses *all* output — bisect to
    isolate crashers, replaying known-good statements (`prefix`, output
    discarded) so stateful probes keep their tables. A crashing statement's
    block is '<cpp-crash>'."""
    got, _err = run_cpp(list(prefix) + stmts, dataset)
    got = got[len(prefix):]
    if len(got) >= len(stmts):
        return got[: len(stmts)]
    if len(stmts) == 1:
        return [["<cpp-crash>"]]
    mid = len(stmts) // 2
    left = run_cpp_resilient(stmts[:mid], dataset, prefix)
    good = tuple(s for i, s in enumerate(stmts[:mid]) if left[i] != ["<cpp-crash>"])
    right = run_cpp_resilient(stmts[mid:], dataset, prefix + good)
    return left + right


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("probe_file")
    ap.add_argument("--dataset")
    ap.add_argument("--show-all", action="store_true")
    args = ap.parse_args()
    stmts = read_probes(args.probe_file)
    cpp_blocks = run_cpp_resilient(stmts, args.dataset)
    rust_blocks, rust_err = run_rust(stmts, args.dataset)
    if rust_err.strip():
        print(f"[rust stderr] {rust_err.strip()[:500]}", file=sys.stderr)
    n = len(stmts)
    if len(cpp_blocks) != n:
        print(f"WARNING: cpp produced {len(cpp_blocks)} blocks for {n} stmts", file=sys.stderr)
    if len(rust_blocks) != n:
        print(f"WARNING: rust produced {len(rust_blocks)} blocks for {n} stmts", file=sys.stderr)
    mismatches = 0
    for i, (stmt, ordered) in enumerate(stmts):
        c = cpp_blocks[i] if i < len(cpp_blocks) else ["<missing>"]
        r = rust_blocks[i] if i < len(rust_blocks) else ["<missing>"]
        cc = c if ordered else sorted(c)
        rr = r if ordered else sorted(r)
        same = cc == rr
        if not same:
            mismatches += 1
        if not same or args.show_all:
            tag = "SAME" if same else "DIFF"
            print(f"=== [{tag}] {stmt}")
            if not same or args.show_all:
                print("  cpp : " + (" \\n ".join(c) if c else "<empty>"))
                print("  rust: " + (" \\n ".join(r) if r else "<empty>"))
    print(f"\n{n} probes, {mismatches} mismatches")
    sys.exit(0 if mismatches == 0 else 1)

if __name__ == "__main__":
    main()
