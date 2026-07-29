#!/usr/bin/env python3
"""Run each first-party CLI regression surface exactly once.

The focused gate covers parser tooling, public-facade tooling, CLI unit/integration
tests, real subprocesses, and supported PTY behavior. `cargo test --workspace`
remains the complete project regression gate.
"""

from __future__ import annotations

import argparse
from pathlib import Path
import re
import subprocess
import sys

ROOT = Path(__file__).resolve().parents[1]
TIMEOUT_SECONDS = 600


class GateFailure(RuntimeError):
    pass


def run(command: list[str]) -> subprocess.CompletedProcess[bytes]:
    try:
        result = subprocess.run(
            command,
            cwd=ROOT,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            timeout=TIMEOUT_SECONDS,
            check=False,
        )
    except subprocess.TimeoutExpired as error:
        raise GateFailure(f"timed out after {TIMEOUT_SECONDS}s: {' '.join(command)}") from error
    if result.returncode != 0:
        output = (result.stdout + result.stderr).decode("utf-8", "replace")
        raise GateFailure(f"failed ({result.returncode}): {' '.join(command)}\n{output}")
    return result


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument(
        "--strict",
        action="store_true",
        help="reject ignored tests in addition to failures and timeouts",
    )
    arguments = parser.parse_args()
    checks = [
        ("parser tooling", ["test", "-p", "koko-parser", "tooling"]),
        ("public facade tooling", ["test", "-p", "koko", "--test", "tooling"]),
        ("CLI library, process, batch, and PTY", ["test", "-p", "koko-cli", "--all-targets"]),
    ]

    try:
        for name, cargo_arguments in checks:
            result = run(["cargo", *cargo_arguments])
            if arguments.strict and re.search(
                rb"\b[1-9][0-9]* ignored\b", result.stdout + result.stderr
            ):
                raise GateFailure(
                    f"required test command reported ignored cases: "
                    f"cargo {' '.join(cargo_arguments)}"
                )
            print(f"suite  PASS  {name}")
    except GateFailure as error:
        print(f"cli_goal_gate: FAIL: {error}", file=sys.stderr)
        return 1

    print(f"cli_goal_gate: GREEN ({len(checks)} suites, 0 failed, 0 timed out)")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
