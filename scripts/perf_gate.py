#!/usr/bin/env python3
"""Enforcing IM4 LSQB performance gate.

Each engine/query pair gets one warm-up and three measured isolated-process
samples. Engine order alternates per repetition. Ratios within 10% of a
contract boundary get two additional paired samples; the median of all five is
authoritative. Dataset loading happens in each process but is excluded from
the engine-reported query time.

Usage:
    KOKO_ROOT_DIRECTORY=/path/to/koko python3 scripts/perf_gate.py [test_file]

Environment:
    KOKO_ROOT_DIRECTORY  C++ checkout and oracle corpus (required)
    KOKO_DATASET_DIR     dataset root (default: $KOKO_ROOT_DIRECTORY/dataset)
    KOKO_TEST_BIN        prebuilt release Rust runner (default: target/release/koko-test)
    KOKO_CPP_BIN         prebuilt release C++ shell
    PERF_TIMEOUT         per-process timeout in seconds (default: 90)
    PERF_QUERIES         comma-separated diagnostic subset (default: all; subset is not close proof)
"""

from __future__ import annotations

import os
import platform
import re
import statistics
import subprocess
import sys
import tempfile
from pathlib import Path


ROOT = os.environ.get("KOKO_ROOT_DIRECTORY")
if not ROOT:
    sys.exit("KOKO_ROOT_DIRECTORY must point at the koko C++ checkout")

DATASET_DIR = os.environ.get("KOKO_DATASET_DIR", os.path.join(ROOT, "dataset"))
RUST_BIN = os.environ.get("KOKO_TEST_BIN", "target/release/koko-test")
CPP_BIN = os.environ.get(
    "KOKO_CPP_BIN", os.path.join(ROOT, "build/release/tools/shell/koko")
)
TIMEOUT = float(os.environ.get("PERF_TIMEOUT", "90"))
TEST_FILE = (
    sys.argv[1]
    if len(sys.argv) > 1
    else os.path.join(ROOT, "test/test_files/lsqb/lsqb_queries.test")
)
QUERY_FILTER = {
    label.strip().lower()
    for label in os.environ.get("PERF_QUERIES", "").split(",")
    if label.strip()
}

MEASURED_SAMPLES = 3
EXTRA_SAMPLES = 2
RATIO_LIMIT = 2.0
PRESERVED_WINS = {"q4", "q5", "q7"}
ANSI = re.compile(r"\x1b\[[0-9;?]*[A-Za-z]|\[6n")


def parse_queries(path: str) -> list[tuple[str, str, int]]:
    text = Path(path).read_text()
    matches = re.findall(
        r"-LOG (\S+)\s*\n-STATEMENT (.+?)\n---- 1\n(\d+)", text, re.S
    )
    return [
        (label, " ".join(statement.split()), int(expected))
        for label, statement, expected in matches
    ]


def dataset_name(path: str) -> str | None:
    match = re.search(r"-DATASET\s+CSV\s+(\S+)", Path(path).read_text())
    return match.group(1) if match else None


def rust_case_file(
    label: str, statement: str, expected: int, dataset: str
) -> str:
    body = (
        f"-DATASET CSV {dataset}\n"
        "-SKIP_VECTOR_CAPACITY_TESTS\n\n--\n\n"
        f"-CASE P_{label}\n"
        f"-LOG {label}\n"
        f"-STATEMENT {statement}\n"
        f"---- 1\n{expected}\n"
    )
    file = tempfile.NamedTemporaryFile("w", suffix=".test", delete=False)
    file.write(body)
    file.close()
    return file.name


def run_rust(label: str, case_file: str) -> tuple[float | None, str]:
    environment = dict(
        os.environ,
        KOKO_ROOT_DIRECTORY=ROOT,
        KOKO_DATASET_DIR=DATASET_DIR,
        KOKO_TIMING="1",
        LC_ALL="C",
    )
    try:
        process = subprocess.run(
            [RUST_BIN, case_file],
            env=environment,
            capture_output=True,
            text=True,
            timeout=TIMEOUT,
        )
    except subprocess.TimeoutExpired:
        return None, "TIMEOUT"

    passed = process.returncode == 0 and "1 passed, 0 skipped, 0 failed" in process.stdout
    seconds = None
    for line in process.stderr.splitlines():
        if line.startswith("[timing]") and f" {label} " in line:
            seconds = float(line.split()[-1].removesuffix("s"))
    if not passed:
        detail = (process.stdout + "\n" + process.stderr).strip().splitlines()
        return seconds, f"WRONG({detail[-1] if detail else f'exit {process.returncode}'})"
    if seconds is None:
        return None, "NO_TIMING"
    return seconds, "OK"


def cpp_load_script(dataset: str) -> str:
    directory = os.path.join(DATASET_DIR, dataset)
    schema = Path(directory, "schema.cypher").read_text()
    copy = re.sub(
        r'from "([^"]+)"',
        lambda match: f'from "{os.path.join(directory, match.group(1))}"',
        Path(directory, "copy.cypher").read_text(),
    )
    file = tempfile.NamedTemporaryFile("w", suffix=".cypher", delete=False)
    file.write(schema + "\n" + copy)
    file.close()
    return file.name


def run_cpp(
    statement: str, expected: int, load_script: str
) -> tuple[float | None, str]:
    query = statement if statement.endswith(";") else statement + ";"
    try:
        process = subprocess.run(
            [CPP_BIN, ":memory:", "-b", "-i", load_script],
            input=query + "\n",
            capture_output=True,
            text=True,
            timeout=TIMEOUT,
            env=dict(os.environ, LC_ALL="C"),
        )
    except subprocess.TimeoutExpired:
        return None, "TIMEOUT"

    output = ANSI.sub("", process.stdout)
    counts = [int(value) for value in re.findall(r"│\s*(\d+)\s*│", output)]
    timings = [
        float(match.group(1))
        for match in re.finditer(r"([0-9.]+)ms \(executing\)", output)
    ]
    actual = counts[-1] if counts else None
    if process.returncode != 0 or actual != expected:
        return None, f"WRONG({actual})"
    if not timings:
        return None, "NO_TIMING"
    return timings[-1] / 1000.0, "OK"


def command_version(command: list[str]) -> str:
    try:
        process = subprocess.run(
            command, capture_output=True, text=True, timeout=5
        )
    except (OSError, subprocess.TimeoutExpired):
        return "unavailable"
    lines = (process.stdout or process.stderr).strip().splitlines()
    return lines[0] if lines else "unknown"


def cpu_name() -> str:
    if sys.platform == "darwin":
        try:
            return subprocess.run(
                ["sysctl", "-n", "machdep.cpu.brand_string"],
                capture_output=True,
                text=True,
                timeout=5,
                check=True,
            ).stdout.strip()
        except (OSError, subprocess.SubprocessError):
            pass
    return platform.processor() or "unknown"


def print_metadata() -> None:
    print("metadata:")
    print(f"  host={platform.node()}")
    print(f"  os={platform.platform()}")
    print(f"  machine={platform.machine()} cpu={cpu_name()} logical_cpus={os.cpu_count()}")
    print(f"  python={platform.python_version()}")
    print(f"  rustc={command_version(['rustc', '--version'])}")
    print(f"  cargo={command_version(['cargo', '--version'])}")
    print(f"  cxx={command_version(['c++', '--version'])}")
    for name, binary in (("rust_bin", RUST_BIN), ("cpp_bin", CPP_BIN)):
        stat = Path(binary).stat()
        print(
            f"  {name}={Path(binary).resolve()} size={stat.st_size} "
            f"mtime_ns={stat.st_mtime_ns}"
        )


def run_pair(
    order: tuple[str, str],
    label: str,
    case_file: str,
    statement: str,
    expected: int,
    load_script: str,
) -> dict[str, tuple[float | None, str]]:
    result = {}
    for engine in order:
        if engine == "rust":
            result[engine] = run_rust(label, case_file)
        else:
            result[engine] = run_cpp(statement, expected, load_script)
    return result


def sample_text(samples: list[float]) -> str:
    return "[" + ", ".join(f"{sample * 1000:.3f}" for sample in samples) + "]ms"

def timing_text(seconds: float | None) -> str:
    return "-" if seconds is None else f"{seconds * 1000:.3f}ms"


def main() -> int:
    for binary in (RUST_BIN, CPP_BIN):
        if not Path(binary).is_file():
            print(f"missing prebuilt release binary: {binary}", file=sys.stderr)
            return 2

    dataset = dataset_name(TEST_FILE)
    queries = parse_queries(TEST_FILE)
    all_query_count = len(queries)
    if QUERY_FILTER:
        queries = [query for query in queries if query[0].lower() in QUERY_FILTER]
        missing = QUERY_FILTER.difference(query[0].lower() for query in queries)
        if missing:
            print(f"unknown PERF_QUERIES labels: {sorted(missing)}", file=sys.stderr)
            return 2
    if not dataset or not queries:
        print("test file has no dataset or LSQB query blocks", file=sys.stderr)
        return 2

    print(
        f"IM4 perf gate: {Path(TEST_FILE).name} dataset={dataset} "
        f"queries={len(queries)}/{all_query_count} timeout={TIMEOUT:.0f}s"
    )
    print(
        "protocol: 1 warm-up + 3 measured paired samples; "
        "2 extra when within 10% of a boundary; alternating engine order"
    )
    if QUERY_FILTER:
        print("mode: diagnostic subset; a passing result is not an IM4 close gate")
    print_metadata()
    print()

    failures: list[str] = []
    load_script = cpp_load_script(dataset)
    case_files: list[str] = []
    try:
        for query_index, (label, statement, expected) in enumerate(queries):
            case_file = rust_case_file(label, statement, expected, dataset)
            case_files.append(case_file)

            warm_order = ("rust", "cpp") if query_index % 2 == 0 else ("cpp", "rust")
            warm = run_pair(
                warm_order,
                label,
                case_file,
                statement,
                expected,
                load_script,
            )
            print(
                f"{label}: warmup rust={warm['rust'][1]} cpp={warm['cpp'][1]} "
                f"order={'→'.join(warm_order)}"
            )
            if warm["rust"][1] != "OK" or warm["cpp"][1] != "OK":
                failures.append(
                    f"{label}: warm-up failed "
                    f"(rust={warm['rust'][1]}, cpp={warm['cpp'][1]})"
                )
                continue

            samples = {"rust": [], "cpp": []}
            statuses = {"rust": [], "cpp": []}

            def measure(repetition: int) -> None:
                order = (
                    ("rust", "cpp")
                    if (query_index + repetition) % 2 == 0
                    else ("cpp", "rust")
                )
                pair = run_pair(
                    order,
                    label,
                    case_file,
                    statement,
                    expected,
                    load_script,
                )
                for engine in ("rust", "cpp"):
                    seconds, status = pair[engine]
                    statuses[engine].append(status)
                    if seconds is not None:
                        samples[engine].append(seconds)
                print(
                    f"  sample {repetition + 1}: order={'→'.join(order)} "
                    f"rust={pair['rust'][1]} {timing_text(pair['rust'][0])} "
                    f"cpp={pair['cpp'][1]} {timing_text(pair['cpp'][0])}"
                )

            for repetition in range(MEASURED_SAMPLES):
                measure(repetition)

            if any(status != "OK" for values in statuses.values() for status in values):
                failures.append(
                    f"{label}: measured failure "
                    f"(rust={statuses['rust']}, cpp={statuses['cpp']})"
                )
                continue

            rust_median = statistics.median(samples["rust"])
            cpp_median = statistics.median(samples["cpp"])
            ratio = rust_median / cpp_median
            boundary = 1.0 if label.lower() in PRESERVED_WINS else RATIO_LIMIT
            if boundary * 0.9 <= ratio <= boundary * 1.1:
                print(
                    f"  borderline ratio {ratio:.6f} near {boundary:.2f}; "
                    "running 2 extra paired samples"
                )
                for repetition in range(
                    MEASURED_SAMPLES, MEASURED_SAMPLES + EXTRA_SAMPLES
                ):
                    measure(repetition)
                if any(
                    status != "OK"
                    for values in statuses.values()
                    for status in values
                ):
                    failures.append(
                        f"{label}: extra measured failure "
                        f"(rust={statuses['rust']}, cpp={statuses['cpp']})"
                    )
                    continue
                rust_median = statistics.median(samples["rust"])
                cpp_median = statistics.median(samples["cpp"])
                ratio = rust_median / cpp_median

            print(f"  raw rust={sample_text(samples['rust'])}")
            print(f"  raw cpp ={sample_text(samples['cpp'])}")
            print(
                f"  median rust={rust_median * 1000:.3f}ms "
                f"cpp={cpp_median * 1000:.3f}ms ratio={ratio:.6f}"
            )
            if ratio > RATIO_LIMIT:
                failures.append(f"{label}: median ratio {ratio:.6f} > {RATIO_LIMIT:.2f}")
            if label.lower() in PRESERVED_WINS and ratio >= 1.0:
                failures.append(f"{label}: preserved win regressed to {ratio:.6f} >= 1.00")
            print()
    finally:
        Path(load_script).unlink(missing_ok=True)
        for case_file in case_files:
            Path(case_file).unlink(missing_ok=True)

    if failures:
        print("IM4 PERF GATE: FAIL")
        for failure in failures:
            print(f"  - {failure}")
        return 1
    if QUERY_FILTER:
        print("IM4 PERF SUBSET: PASS (not close proof)")
    else:
        print("IM4 PERF GATE: PASS")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
