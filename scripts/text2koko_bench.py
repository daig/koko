#!/usr/bin/env python3
"""Executable, provider-neutral Text2Koko benchmark.

The benchmark keeps natural-language intent and semantic oracles stable while a
model, prompt profile, or Koko dialect changes. Model providers are deliberately
outside this script: a generator command receives one JSON request on stdin and
must return {"query": "..."} with optional token-usage metadata on stdout.

Examples:
    cargo build -p koko-cli
    python3 scripts/text2koko_bench.py validate
    python3 scripts/text2koko_bench.py run --generator reference --output /tmp/reference.json
    python3 scripts/text2koko_bench.py run --generator-command 'python3 model.py' \
        --profile guided --model frontier --repetitions 3 --output /tmp/frontier.json
    python3 scripts/text2koko_bench.py compare /tmp/reference.json /tmp/frontier.json
"""

from __future__ import annotations

import argparse
import datetime as dt
import fnmatch
import hashlib
import json
import math
import os
from pathlib import Path
import platform
import re
import shlex
import subprocess
import sys
import tempfile
import time
from typing import Any, Callable, Optional

ROOT = Path(__file__).resolve().parents[1]
DEFAULT_CORPUS = ROOT / "benchmarks" / "text2koko" / "corpus.json"
DEFAULT_KOKO_BIN = ROOT / "target" / "debug" / "koko"
REPORT_VERSION = 1
REQUEST_VERSION = 1
MAX_GENERATOR_OUTPUT_BYTES = 64 * 1024


class BenchError(RuntimeError):
    """An actionable benchmark configuration or infrastructure failure."""


class GenerationError(RuntimeError):
    """One model invocation failed before it produced a query."""

    def __init__(self, message: str, seconds: float = 0.0):
        super().__init__(message)
        self.seconds = seconds


Json = dict[str, Any]
Generator = Callable[[Json, str, int], tuple[str, float, Optional[Json]]]
USAGE_FIELDS = {
    "input_tokens",
    "output_tokens",
    "cached_input_tokens",
    "reasoning_tokens",
}


def canonical_json(value: Any) -> str:
    return json.dumps(value, ensure_ascii=False, separators=(",", ":"), sort_keys=True)


def digest(value: Any) -> str:
    return hashlib.sha256(canonical_json(value).encode("utf-8")).hexdigest()

def validate_usage(value: Any) -> Json | None:
    if value is None:
        return None
    if not isinstance(value, dict) or not value or set(value).difference(USAGE_FIELDS):
        raise GenerationError(
            "generator usage must be a non-empty object containing only "
            "input_tokens, output_tokens, cached_input_tokens, and reasoning_tokens"
        )
    if any(isinstance(count, bool) or not isinstance(count, int) or count < 0 for count in value.values()):
        raise GenerationError("generator usage token counts must be non-negative integers")
    return value


def parse_metadata(value: str) -> Json:
    try:
        metadata = json.loads(value)
    except json.JSONDecodeError as error:
        raise BenchError(f"invalid --metadata-json: {error}") from error
    if not isinstance(metadata, dict):
        raise BenchError("--metadata-json must decode to a JSON object")
    return metadata


def read_json(path: Path) -> Any:
    try:
        with path.open("r", encoding="utf-8") as source:
            return json.load(source)
    except OSError as error:
        raise BenchError(f"cannot read {path}: {error}") from error
    except json.JSONDecodeError as error:
        raise BenchError(f"invalid JSON in {path}: {error}") from error


def require_string(value: Any, field: str) -> str:
    if not isinstance(value, str) or not value.strip():
        raise BenchError(f"{field} must be a non-empty string")
    return value


def require_string_list(value: Any, field: str, *, nonempty: bool = False) -> list[str]:
    if not isinstance(value, list) or any(not isinstance(item, str) or not item.strip() for item in value):
        raise BenchError(f"{field} must be a list of non-empty strings")
    if nonempty and not value:
        raise BenchError(f"{field} must not be empty")
    return value


def load_corpus(path: Path) -> Json:
    corpus = read_json(path)
    if not isinstance(corpus, dict) or corpus.get("version") != 1:
        raise BenchError(f"{path}: corpus version must be 1")

    profiles = corpus.get("prompt_profiles")
    if not isinstance(profiles, dict) or set(profiles) != {"prior", "guided"}:
        raise BenchError(f"{path}: prompt_profiles must contain exactly prior and guided")
    for name, profile in profiles.items():
        if not isinstance(profile, dict):
            raise BenchError(f"prompt_profiles.{name} must be an object")
        require_string(profile.get("system"), f"prompt_profiles.{name}.system")

    schemas = corpus.get("schemas")
    if not isinstance(schemas, dict) or not schemas:
        raise BenchError(f"{path}: schemas must be a non-empty object")
    for name, schema in schemas.items():
        if not re.fullmatch(r"[a-z][a-z0-9_]*", name) or not isinstance(schema, dict):
            raise BenchError(f"invalid schema entry {name!r}")
        require_string(schema.get("prompt"), f"schemas.{name}.prompt")
        require_string_list(schema.get("setup"), f"schemas.{name}.setup", nonempty=True)
        require_string_list(
            schema.get("state_queries"),
            f"schemas.{name}.state_queries",
            nonempty=True,
        )

    cases = corpus.get("cases")
    if not isinstance(cases, list) or not cases:
        raise BenchError(f"{path}: cases must be a non-empty list")
    ids: set[str] = set()
    for index, case in enumerate(cases):
        field = f"cases[{index}]"
        if not isinstance(case, dict):
            raise BenchError(f"{field} must be an object")
        case_id = require_string(case.get("id"), f"{field}.id")
        if not re.fullmatch(r"[a-z][a-z0-9_]*(?:\.[a-z][a-z0-9_]*)+", case_id):
            raise BenchError(f"{field}.id has invalid form: {case_id!r}")
        if case_id in ids:
            raise BenchError(f"duplicate case id {case_id!r}")
        ids.add(case_id)
        schema_name = require_string(case.get("schema"), f"{field}.schema")
        if schema_name not in schemas:
            raise BenchError(f"{case_id}: unknown schema {schema_name!r}")
        require_string(case.get("task"), f"{case_id}.task")
        reference_query = require_string(case.get("reference_query"), f"{case_id}.reference_query")
        if count_statements(reference_query) != 1:
            raise BenchError(f"{case_id}: reference_query must contain exactly one statement")
        mode = case.get("mode")
        if mode not in {"read", "write"}:
            raise BenchError(f"{case_id}.mode must be read or write")
        if not isinstance(case.get("parameters", {}), dict):
            raise BenchError(f"{case_id}.parameters must be an object")
        for name in case.get("parameters", {}):
            if re.fullmatch(r"[A-Za-z_][A-Za-z0-9_]*", name) is None:
                raise BenchError(f"{case_id}: invalid parameter name {name!r}")
        require_string_list(case.get("tags"), f"{case_id}.tags", nonempty=True)
        require_string_list(case.get("setup", []), f"{case_id}.setup")
        verification = case.get("verify", [])
        if isinstance(verification, str):
            verification = [verification]
            case["verify"] = verification
        require_string_list(verification, f"{case_id}.verify")
        if mode == "write" and not verification:
            raise BenchError(f"{case_id}: write cases require at least one verification query")
        if mode == "read" and verification:
            raise BenchError(f"{case_id}: read cases must compare their target result directly")
        if not isinstance(case.get("ordered", False), bool):
            raise BenchError(f"{case_id}.ordered must be boolean")
        tolerance = case.get("float_tolerance", 0.0)
        if isinstance(tolerance, bool) or not isinstance(tolerance, (int, float)) or tolerance < 0:
            raise BenchError(f"{case_id}.float_tolerance must be non-negative")
        policy = case.get("policy", {})
        if not isinstance(policy, dict) or set(policy).difference({"require", "forbid"}):
            raise BenchError(f"{case_id}.policy accepts only require and forbid")
        for policy_name in ("require", "forbid"):
            patterns = require_string_list(policy.get(policy_name, []), f"{case_id}.policy.{policy_name}")
            for pattern in patterns:
                try:
                    re.compile(pattern)
                except re.error as error:
                    raise BenchError(f"{case_id}: invalid policy regex {pattern!r}: {error}") from error

    return corpus


def count_statements(query: str) -> int:
    """Count non-empty semicolon-delimited envelopes outside literals/comments.

    This enforces the tool's one-statement contract; Koko remains the only query
    parser. The scanner deliberately makes no claim about Cypher validity.
    """

    state = "normal"
    escaped = False
    segment_has_code = False
    count = 0
    index = 0
    while index < len(query):
        char = query[index]
        ahead = query[index + 1] if index + 1 < len(query) else ""
        if state in {"single", "double", "backtick"}:
            if escaped:
                escaped = False
            elif char == "\\":
                escaped = True
            elif (state == "single" and char == "'") or (state == "double" and char == '"') or (
                state == "backtick" and char == "`"
            ):
                state = "normal"
            segment_has_code = True
        elif state == "line_comment":
            if char in "\r\n":
                state = "normal"
        elif state == "block_comment":
            if char == "*" and ahead == "/":
                state = "normal"
                index += 1
        elif char == "/" and ahead == "/":
            state = "line_comment"
            index += 1
        elif char == "/" and ahead == "*":
            state = "block_comment"
            index += 1
        elif char in "'\"`":
            state = {"'": "single", '"': "double", "`": "backtick"}[char]
            segment_has_code = True
        elif char == ";":
            if segment_has_code:
                count += 1
                segment_has_code = False
        elif not char.isspace():
            segment_has_code = True
        index += 1
    return count + int(segment_has_code)


def mask_non_code(query: str) -> str:
    """Blank literals, quoted identifiers, and comments while preserving offsets."""

    masked = list(query)
    state = "normal"
    escaped = False
    index = 0

    def blank(position: int) -> None:
        if masked[position] not in "\r\n":
            masked[position] = " "

    while index < len(query):
        char = query[index]
        ahead = query[index + 1] if index + 1 < len(query) else ""
        if state in {"single", "double", "backtick"}:
            blank(index)
            if escaped:
                escaped = False
            elif char == "\\":
                escaped = True
            elif (state == "single" and char == "'") or (state == "double" and char == '"') or (
                state == "backtick" and char == "`"
            ):
                state = "normal"
        elif state == "line_comment":
            if char in "\r\n":
                state = "normal"
            else:
                blank(index)
        elif state == "block_comment":
            blank(index)
            if char == "*" and ahead == "/":
                blank(index + 1)
                state = "normal"
                index += 1
        elif char == "/" and ahead == "/":
            blank(index)
            blank(index + 1)
            state = "line_comment"
            index += 1
        elif char == "/" and ahead == "*":
            blank(index)
            blank(index + 1)
            state = "block_comment"
            index += 1
        elif char in "'\"`":
            blank(index)
            state = {"'": "single", '"': "double", "`": "backtick"}[char]
        index += 1
    return "".join(masked)


def referenced_parameters(query: str) -> set[str]:
    """Return parameter names outside literals, identifiers, and comments."""

    return set(re.findall(r"\$([A-Za-z_][A-Za-z0-9_]*)", mask_non_code(query)))


def select_cases(corpus: Json, patterns: list[str], tags: list[str], limit: int | None) -> list[Json]:
    selected = []
    for case in corpus["cases"]:
        if patterns and not any(fnmatch.fnmatchcase(case["id"], pattern) for pattern in patterns):
            continue
        if tags and not all(tag in case["tags"] for tag in tags):
            continue
        selected.append(case)
    if limit is not None:
        selected = selected[:limit]
    if not selected:
        raise BenchError("case filters selected no benchmark cases")
    return selected


def parameter_type(value: Any) -> str:
    if value is None:
        return "ANY"
    if isinstance(value, bool):
        return "BOOL"
    if isinstance(value, int):
        return "INT64"
    if isinstance(value, float):
        return "DOUBLE"
    if isinstance(value, str):
        return "STRING"
    if isinstance(value, list):
        members = {parameter_type(item) for item in value}
        element = members.pop() if len(members) == 1 else "ANY"
        return f"LIST<{element}>"
    if isinstance(value, dict):
        return "OBJECT"
    raise BenchError(f"unsupported benchmark parameter value: {value!r}")


def build_request(
    corpus: Json,
    case: Json,
    profile: str,
    model: str,
    run_metadata: Json,
    prior_attempts: list[Json],
) -> Json:
    schema = corpus["schemas"][case["schema"]]
    parameters = case.get("parameters", {})
    parameter_text = canonical_json(
        {name: parameter_type(value) for name, value in parameters.items()}
    )
    user = (
        f"Schema:\n{schema['prompt'].strip()}\n\n"
        f"Available parameter types (reference every exact $name):\n{parameter_text}\n\n"
        f"Operation class: {case['mode']}\n"
        f"Task:\n{case['task'].strip()}\n\n"
        "Return exactly one Koko statement in the required JSON object. "
        "Return the requested columns with the requested names."
    )
    messages: list[Json] = [
        {"role": "system", "content": corpus["prompt_profiles"][profile]["system"].strip()},
        {"role": "user", "content": user},
    ]
    for attempt in prior_attempts:
        if attempt.get("query"):
            messages.append({"role": "assistant", "content": canonical_json({"query": attempt["query"]})})
        messages.append(
            {
                "role": "user",
                "content": (
                    "The Koko tool rejected that attempt. Correct the query and return only the JSON object.\n"
                    + canonical_json(attempt["diagnostic"])
                ),
            }
        )
    return {
        "version": REQUEST_VERSION,
        "case_id": case["id"],
        "model": model,
        "profile": profile,
        "run_metadata": run_metadata,
        "messages": messages,
        "response_schema": {
            "type": "object",
            "properties": {"query": {"type": "string", "minLength": 1}},
            "required": ["query"],
            "additionalProperties": False,
        },
    }


def reference_generator(corpus: Json) -> Generator:
    cases = {case["id"]: case for case in corpus["cases"]}

    def generate(_request: Json, case_id: str, _attempt: int) -> tuple[str, float, Json | None]:
        return cases[case_id]["reference_query"], 0.0, None

    return generate


def command_generator(command_text: str, timeout: float) -> Generator:
    try:
        command = shlex.split(command_text)
    except ValueError as error:
        raise BenchError(f"invalid --generator-command: {error}") from error
    if not command:
        raise BenchError("--generator-command must not be empty")

    def generate(request: Json, case_id: str, attempt: int) -> tuple[str, float, Json | None]:
        environment = os.environ.copy()
        environment["TEXT2KOKO_CASE_ID"] = case_id
        environment["TEXT2KOKO_ATTEMPT"] = str(attempt)
        started = time.monotonic()
        try:
            result = subprocess.run(
                command,
                input=canonical_json(request),
                text=True,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                timeout=timeout,
                check=False,
                env=environment,
            )
        except (OSError, subprocess.TimeoutExpired) as error:
            elapsed = time.monotonic() - started
            raise GenerationError(f"generator command failed: {error}", elapsed) from error
        elapsed = time.monotonic() - started
        if result.returncode != 0:
            stderr = result.stderr.strip()
            raise GenerationError(
                f"generator exited {result.returncode}: {stderr[:1000] or 'no stderr'}",
                elapsed,
            )
        if len(result.stdout.encode("utf-8")) > MAX_GENERATOR_OUTPUT_BYTES:
            raise GenerationError(
                f"generator output exceeded {MAX_GENERATOR_OUTPUT_BYTES} bytes",
                elapsed,
            )
        try:
            response = json.loads(result.stdout)
        except json.JSONDecodeError as error:
            raise GenerationError(
                f"generator stdout is not one JSON object: {error}",
                elapsed,
            ) from error
        if (
            not isinstance(response, dict)
            or "query" not in response
            or set(response).difference({"query", "usage"})
        ):
            raise GenerationError(
                'generator response accepts only "query" and optional "usage" keys',
                elapsed,
            )
        query = response["query"]
        if not isinstance(query, str) or not query.strip():
            raise GenerationError("generator query must be a non-empty string", elapsed)
        try:
            usage = validate_usage(response.get("usage"))
        except GenerationError as error:
            raise GenerationError(str(error), elapsed) from error
        return query, elapsed, usage

    return generate


def replay_generator(path: Path) -> Generator:
    document = read_json(path)
    if isinstance(document, dict) and isinstance(document.get("results"), list):
        records = document["results"]
    elif isinstance(document, dict) and isinstance(document.get("responses"), list):
        records = document["responses"]
    elif isinstance(document, list):
        records = document
    else:
        raise BenchError(f"{path}: replay input needs a results/responses array")
    queries: dict[tuple[str, int, int], str] = {}
    for index, record in enumerate(records):
        if not isinstance(record, dict):
            raise BenchError(f"{path}: replay record {index} is not an object")
        case_id = record.get("case_id")
        repetition = record.get("repetition", 0)
        if isinstance(record.get("attempts"), list):
            for attempt, value in enumerate(record["attempts"]):
                if isinstance(value, dict) and isinstance(value.get("query"), str):
                    queries[(case_id, repetition, attempt)] = value["query"]
        elif isinstance(record.get("query"), str):
            queries[(case_id, repetition, record.get("attempt", 0))] = record["query"]
    if not queries:
        raise BenchError(f"{path}: replay input contains no queries")

    def generate(request: Json, case_id: str, attempt: int) -> tuple[str, float, Json | None]:
        key = (case_id, int(request.get("repetition", 0)), attempt)
        if key not in queries:
            raise GenerationError(f"replay has no query for case={case_id} repetition={key[1]} attempt={attempt}")
        return queries[key], 0.0, None

    return generate


def koko_version(binary: Path) -> str:
    try:
        result = subprocess.run(
            [str(binary), "--version"],
            cwd=ROOT,
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            timeout=10,
            check=False,
        )
    except OSError as error:
        raise BenchError(f"cannot execute {binary}: {error}") from error
    if result.returncode != 0:
        raise BenchError(f"{binary} --version failed: {(result.stdout + result.stderr).strip()}")
    return result.stdout.strip()


def statement_plan(corpus: Json, case: Json, query: str) -> tuple[list[str], int, list[int]]:
    schema = corpus["schemas"][case["schema"]]
    statements = [*schema["setup"], *case.get("setup", [])]
    if case["mode"] == "read":
        statements.append("BEGIN TRANSACTION READ ONLY")
    target = len(statements) + 1
    statements.append(query)
    verification: list[int] = []
    verification_queries = list(case.get("verify", []))
    if case["mode"] == "write":
        verification_queries.extend(schema["state_queries"])
    for verify in verification_queries:
        verification.append(len(statements) + 1)
        statements.append(verify)
    if case["mode"] == "read":
        statements.append("ROLLBACK")
    return statements, target, verification or [target]


def invoke_koko(
    binary: Path,
    statements: list[str],
    parameters: Json,
    timeout: float,
) -> Json:
    command_text = ";\n".join(statement.strip().rstrip(";") for statement in statements)
    with tempfile.TemporaryDirectory(prefix="text2koko-") as directory:
        params_path = Path(directory) / "params.json"
        params_path.write_text(canonical_json(parameters), encoding="utf-8")
        command = [
            str(binary),
            "--no-config",
            "--color",
            "never",
            "--command",
            command_text,
            "--params-file",
            str(params_path),
            "--format",
            "json",
        ]
        started = time.monotonic()
        try:
            result = subprocess.run(
                command,
                cwd=ROOT,
                text=True,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                timeout=timeout,
                check=False,
            )
        except subprocess.TimeoutExpired as error:
            return {
                "stage": "deadline",
                "message": f"Koko exceeded {timeout:g}s",
                "seconds": time.monotonic() - started,
                "stderr": (error.stderr or "")[-2000:] if isinstance(error.stderr, str) else "",
            }
        except OSError as error:
            return {
                "stage": "infrastructure",
                "message": f"cannot execute Koko: {error}",
                "seconds": time.monotonic() - started,
            }
        elapsed = time.monotonic() - started
    try:
        envelope = json.loads(result.stdout)
    except json.JSONDecodeError as error:
        return {
            "stage": "infrastructure",
            "message": f"Koko did not emit a valid JSON envelope: {error}",
            "stdout": result.stdout[-2000:],
            "stderr": result.stderr[-2000:],
            "returncode": result.returncode,
            "seconds": elapsed,
        }
    if not isinstance(envelope, dict) or envelope.get("version") != 1 or not isinstance(
        envelope.get("results"), list
    ):
        return {
            "stage": "infrastructure",
            "message": "Koko emitted an unsupported JSON envelope",
            "envelope": envelope,
            "returncode": result.returncode,
            "seconds": elapsed,
        }
    return {
        "stage": "complete" if envelope.get("complete") else "engine_error",
        "envelope": envelope,
        "returncode": result.returncode,
        "stderr": result.stderr[-2000:],
        "seconds": elapsed,
    }


def normalize_result(result: Json, ordered: bool) -> Json:
    if "columns" not in result or "rows" not in result:
        return {"status": result.get("status")}
    normalized = {
        "columns": [
            {"name": column.get("name"), "type": column.get("type")} for column in result["columns"]
        ],
        "rows": result["rows"],
    }
    if not ordered:
        normalized["rows"] = sorted(normalized["rows"], key=canonical_json)
    return normalized


def semantic_results(envelope: Json, statement_numbers: list[int], ordered: bool) -> list[Json]:
    by_statement = {result.get("statement"): result for result in envelope["results"]}
    missing = [number for number in statement_numbers if number not in by_statement]
    if missing:
        raise BenchError(f"Koko envelope omitted successful statements {missing}")
    return [normalize_result(by_statement[number], ordered) for number in statement_numbers]


def values_equal(left: Any, right: Any, tolerance: float) -> bool:
    if isinstance(left, bool) or isinstance(right, bool):
        return left is right
    if isinstance(left, (int, float)) and isinstance(right, (int, float)):
        if not math.isfinite(float(left)) or not math.isfinite(float(right)):
            return left == right
        if tolerance == 0:
            return left == right
        return math.isclose(float(left), float(right), rel_tol=tolerance, abs_tol=tolerance)
    if type(left) is not type(right):
        return False
    if isinstance(left, list):
        return len(left) == len(right) and all(
            values_equal(left_item, right_item, tolerance)
            for left_item, right_item in zip(left, right)
        )
    if isinstance(left, dict):
        return left.keys() == right.keys() and all(
            values_equal(left[key], right[key], tolerance) for key in left
        )
    return left == right


def check_policy(case: Json, query: str) -> tuple[bool, list[str]]:
    code = mask_non_code(query)
    failures = []
    policy = case.get("policy", {})
    for pattern in policy.get("require", []):
        if re.search(pattern, code) is None:
            failures.append(f"required query pattern did not match: {pattern}")
    for pattern in policy.get("forbid", []):
        if re.search(pattern, code) is not None:
            failures.append(f"forbidden query pattern matched: {pattern}")
    return not failures, failures

def check_parameters(case: Json, query: str) -> tuple[bool, list[str]]:
    referenced = referenced_parameters(query)
    missing = [name for name in case.get("parameters", {}) if name not in referenced]
    return not missing, [f"query does not reference required parameter ${name}" for name in missing]


def apply_parameter_check(outcome: Json, ok: bool, failures: list[str]) -> Json:
    outcome["checks"]["parameter_correct"] = ok
    if failures:
        outcome["parameter_failures"] = failures
    return outcome


def failed_execution(stage: str, message: str, seconds: float = 0.0) -> Json:
    parse_valid = stage not in {"generation", "envelope", "parser", "infrastructure", "setup"}
    bind_valid = parse_valid and stage != "binder"
    runtime_valid = bind_valid and stage not in {
        "catalog",
        "configuration",
        "deadline",
        "engine",
        "import_export",
        "interrupt",
        "memory",
        "runtime",
        "safety",
        "transaction",
        "verification",
    }
    return {
        "stage": stage,
        "message": message,
        "seconds": seconds,
        "checks": {
            "envelope_valid": stage not in {"generation", "envelope"},
            "parse_valid": parse_valid,
            "bind_valid": bind_valid,
            "runtime_valid": runtime_valid,
            "parameter_correct": False,
            "semantic_correct": False,
            "state_correct": False,
            "safety_correct": stage != "safety",
        },
        "passed": False,
    }


def evaluate_query(
    corpus: Json,
    case: Json,
    query: str,
    binary: Path,
    timeout: float,
    expected: list[Json] | None,
) -> Json:
    parameter_ok, parameter_failures = check_parameters(case, query)
    if count_statements(query) != 1:
        return apply_parameter_check(
            failed_execution("envelope", "query must contain exactly one statement"),
            parameter_ok,
            parameter_failures,
        )
    policy_ok, policy_failures = check_policy(case, query)
    statements, target, semantic_indices = statement_plan(corpus, case, query)
    invocation = invoke_koko(binary, statements, case.get("parameters", {}), timeout)
    if invocation["stage"] in {"deadline", "infrastructure"}:
        return apply_parameter_check(
            failed_execution(invocation["stage"], invocation["message"], invocation["seconds"]),
            parameter_ok,
            parameter_failures,
        )

    envelope = invocation["envelope"]
    if invocation["stage"] == "engine_error":
        error_record = envelope.get("error") if isinstance(envelope.get("error"), dict) else {}
        failure = error_record.get("error") if isinstance(error_record.get("error"), dict) else {}
        kind = failure.get("kind", "engine")
        message = failure.get("message", "Koko rejected the query")
        error_statement = error_record.get("statement")
        if isinstance(error_statement, int) and error_statement < target:
            kind = "setup"
            message = f"benchmark setup failed before target statement: {message}"
        elif isinstance(error_statement, int) and error_statement > target:
            kind = "verification"
            message = f"benchmark verification failed after target statement: {message}"
        elif case["mode"] == "read" and kind == "transaction":
            kind = "safety"
        failure_result = apply_parameter_check(
            failed_execution(kind, message, invocation["seconds"]),
            parameter_ok,
            parameter_failures,
        )
        failure_result["diagnostic"] = {
            "phase": kind,
            "message": message,
            "statement": error_statement,
        }
        return failure_result

    if invocation["returncode"] != 0:
        return apply_parameter_check(
            failed_execution(
                "infrastructure",
                f"Koko returned {invocation['returncode']} with a complete envelope",
                invocation["seconds"],
            ),
            parameter_ok,
            parameter_failures,
        )
    if len(envelope["results"]) != len(statements):
        return apply_parameter_check(
            failed_execution(
                "envelope",
                f"query changed the statement count: expected {len(statements)}, got {len(envelope['results'])}",
                invocation["seconds"],
            ),
            parameter_ok,
            parameter_failures,
        )
    actual = semantic_results(envelope, semantic_indices, case.get("ordered", False))
    tolerance = float(case.get("float_tolerance", 0.0))
    semantic_ok = expected is None or values_equal(expected, actual, tolerance)
    safety_ok = policy_ok
    checks = {
        "envelope_valid": True,
        "parse_valid": True,
        "bind_valid": True,
        "runtime_valid": True,
        "parameter_correct": parameter_ok,
        "semantic_correct": semantic_ok,
        "state_correct": semantic_ok if case["mode"] == "write" else None,
        "safety_correct": safety_ok,
    }
    if semantic_ok and parameter_ok and safety_ok:
        stage = "ok"
    elif not semantic_ok:
        stage = "semantic"
    elif not parameter_ok:
        stage = "parameter"
    else:
        stage = "safety"
    outcome: Json = {
        "stage": stage,
        "seconds": invocation["seconds"],
        "checks": checks,
        "passed": semantic_ok and parameter_ok and safety_ok,
        "semantic": actual,
    }
    if not semantic_ok:
        outcome["message"] = "query result or verified graph state differs from the reference oracle"
        outcome["comparison"] = {"expected": expected, "actual": actual}
    elif not parameter_ok:
        outcome["message"] = "; ".join(parameter_failures)
    elif not safety_ok:
        outcome["message"] = "; ".join(policy_failures)
    if policy_failures:
        outcome["policy_failures"] = policy_failures
    if parameter_failures:
        outcome["parameter_failures"] = parameter_failures
    return outcome


def build_oracles(
    corpus: Json,
    cases: list[Json],
    binary: Path,
    timeout: float,
    quiet: bool,
) -> dict[str, Json]:
    oracles: dict[str, Json] = {}
    for index, case in enumerate(cases, start=1):
        outcome = evaluate_query(
            corpus,
            case,
            case["reference_query"],
            binary,
            timeout,
            expected=None,
        )
        if not outcome["passed"]:
            raise BenchError(
                f"reference oracle failed for {case['id']}: "
                f"{outcome['stage']}: {outcome.get('message', 'no diagnostic')}"
            )
        oracles[case["id"]] = {
            "semantic": outcome["semantic"],
            "sha256": digest(outcome["semantic"]),
        }
        if not quiet:
            print(f"oracle PASS {index}/{len(cases)} {case['id']}", file=sys.stderr)
    return oracles


def repairable(outcome: Json) -> bool:
    return outcome["stage"] in {
        "binder",
        "catalog",
        "configuration",
        "engine",
        "envelope",
        "parameter",
        "parser",
        "runtime",
        "safety",
        "transaction",
    }


def evaluate_case(
    corpus: Json,
    case: Json,
    oracle: Json,
    generator: Generator,
    profile: str,
    model: str,
    run_metadata: Json,
    repetition: int,
    repair_turns: int,
    binary: Path,
    execution_timeout: float,
) -> Json:
    attempts: list[Json] = []
    prior_attempts: list[Json] = []
    for attempt_number in range(repair_turns + 1):
        request = build_request(corpus, case, profile, model, run_metadata, prior_attempts)
        request["repetition"] = repetition
        try:
            query, generation_seconds, usage = generator(request, case["id"], attempt_number)
        except GenerationError as error:
            outcome = failed_execution("generation", str(error))
            outcome["diagnostic"] = {"phase": "generation", "message": str(error)}
            attempt = {
                "attempt": attempt_number,
                "generation_seconds": error.seconds,
                **outcome,
            }
            attempts.append(attempt)
            if attempt_number < repair_turns:
                prior_attempts.append({"diagnostic": outcome["diagnostic"]})
                continue
            break
        outcome = evaluate_query(
            corpus,
            case,
            query,
            binary,
            execution_timeout,
            expected=oracle["semantic"],
        )
        attempt = {
            "attempt": attempt_number,
            "query": query,
            "generation_seconds": generation_seconds,
            "usage": usage,
            **{key: value for key, value in outcome.items() if key != "semantic"},
        }
        attempts.append(attempt)
        if outcome["passed"] or attempt_number == repair_turns or not repairable(outcome):
            break
        diagnostic = outcome.get("diagnostic", {"phase": outcome["stage"], "message": outcome.get("message")})
        prior_attempts.append({"query": query, "diagnostic": diagnostic})

    final = attempts[-1]
    first = attempts[0]
    return {
        "case_id": case["id"],
        "schema": case["schema"],
        "mode": case["mode"],
        "tags": case["tags"],
        "repetition": repetition,
        "oracle_sha256": oracle["sha256"],
        "first_attempt_passed": first["passed"],
        "passed": final["passed"],
        "repaired": not first["passed"] and final["passed"],
        "attempts": attempts,
    }


def rate(numerator: int, denominator: int) -> float:
    return numerator / denominator if denominator else 0.0

def distribution(values: list[float]) -> Json:
    ordered = sorted(values)
    count = len(ordered)

    def percentile(fraction: float) -> float:
        if count == 1:
            return ordered[0]
        position = fraction * (count - 1)
        lower = math.floor(position)
        upper = math.ceil(position)
        if lower == upper:
            return ordered[lower]
        weight = position - lower
        return ordered[lower] * (1.0 - weight) + ordered[upper] * weight

    return {
        "total": sum(ordered),
        "mean_per_sample": sum(ordered) / count,
        "p50_per_sample": percentile(0.50),
        "p95_per_sample": percentile(0.95),
    }


def summarize(records: list[Json]) -> Json:
    total = len(records)
    if not total:
        raise BenchError("cannot summarize an empty report")
    final_attempts = [record["attempts"][-1] for record in records]
    checks = [attempt["checks"] for attempt in final_attempts]
    counts = {
        "samples": total,
        "first_attempt_correct": sum(record["first_attempt_passed"] for record in records),
        "final_correct": sum(record["passed"] for record in records),
        "repaired": sum(record["repaired"] for record in records),
        "envelope_valid": sum(check["envelope_valid"] for check in checks),
        "parse_valid": sum(check["parse_valid"] for check in checks),
        "bind_valid": sum(check["bind_valid"] for check in checks),
        "parameter_correct": sum(check["parameter_correct"] for check in checks),
        "runtime_valid": sum(check["runtime_valid"] for check in checks),
        "semantic_correct": sum(check["semantic_correct"] for check in checks),
        "state_correct": sum(check["state_correct"] is True for check in checks),
        "write_samples": sum(record["mode"] == "write" for record in records),
        "safety_correct": sum(check["safety_correct"] for check in checks),
    }
    rates = {
        "first_attempt_accuracy": rate(counts["first_attempt_correct"], total),
        "final_accuracy": rate(counts["final_correct"], total),
        "envelope_validity": rate(counts["envelope_valid"], total),
        "parse_validity": rate(counts["parse_valid"], total),
        "parameter_accuracy": rate(counts["parameter_correct"], total),
        "bind_validity": rate(counts["bind_valid"], total),
        "runtime_validity": rate(counts["runtime_valid"], total),
        "semantic_accuracy": rate(counts["semantic_correct"], total),
        "state_accuracy": rate(counts["state_correct"], counts["write_samples"]),
        "safety_accuracy": rate(counts["safety_correct"], total),
    }
    stage_counts: dict[str, int] = {}
    for attempt in final_attempts:
        stage_counts[attempt["stage"]] = stage_counts.get(attempt["stage"], 0) + 1
    tags = sorted({tag for record in records for tag in record["tags"]})
    by_tag = {}
    for tag in tags:
        tagged = [record for record in records if tag in record["tags"]]
        by_tag[tag] = {
            "samples": len(tagged),
            "first_attempt_accuracy": rate(sum(record["first_attempt_passed"] for record in tagged), len(tagged)),
            "final_accuracy": rate(sum(record["passed"] for record in tagged), len(tagged)),
        }
    per_sample_generation = [
        sum(attempt["generation_seconds"] for attempt in record["attempts"]) for record in records
    ]
    per_sample_execution = [
        sum(attempt["seconds"] for attempt in record["attempts"]) for record in records
    ]
    usage_records = [
        attempt["usage"]
        for record in records
        for attempt in record["attempts"]
        if attempt.get("usage") is not None
    ]
    resources = {
        "generation_seconds": distribution(per_sample_generation),
        "execution_seconds": distribution(per_sample_execution),
        "usage": {
            "reported_attempts": len(usage_records),
            **{
                field: sum(usage.get(field, 0) for usage in usage_records)
                for field in sorted(USAGE_FIELDS)
            },
        },
    }
    return {
        "counts": counts,
        "rates": rates,
        "stages": stage_counts,
        "by_tag": by_tag,
        "resources": resources,
    }


def report_document(
    corpus_path: Path,
    corpus: Json,
    cases: list[Json],
    records: list[Json],
    args: argparse.Namespace,
    binary_version: str,
    generator_kind: str,
) -> Json:
    selected_payload = {
        "schemas": {
            name: corpus["schemas"][name]
            for name in sorted({case["schema"] for case in cases})
        },
        "cases": cases,
    }
    intent_payload = [
        {
            "id": case["id"],
            "schema": case["schema"],
            "schema_prompt": corpus["schemas"][case["schema"]]["prompt"],
            "task": case["task"],
            "parameters": case.get("parameters", {}),
            "mode": case["mode"],
            "ordered": case.get("ordered", False),
            "float_tolerance": case.get("float_tolerance", 0.0),
            "tags": case["tags"],
        }
        for case in cases
    ]
    return {
        "version": REPORT_VERSION,
        "kind": "text2koko-report",
        "created_at": dt.datetime.now(dt.timezone.utc).isoformat(),
        "corpus": {
            "path": str(corpus_path),
            "sha256": digest(corpus),
            "selected_sha256": digest(selected_payload),
            "intent_sha256": digest(intent_payload),
            "cases": len(cases),
        },
        "run": {
            "generator": generator_kind,
            "model": args.model,
            "profile": args.profile,
            "repetitions": args.repetitions,
            "repair_turns": args.repair_turns,
            "metadata": args.run_metadata,
            "koko_version": binary_version,
            "python_version": platform.python_version(),
            "platform": platform.platform(),
        },
        "summary": summarize(records),
        "results": records,
    }


def write_report(path_text: str, document: Json) -> None:
    text = json.dumps(document, ensure_ascii=False, indent=2, sort_keys=True) + "\n"
    if path_text == "-":
        sys.stdout.write(text)
        return
    path = Path(path_text)
    try:
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text, encoding="utf-8")
    except OSError as error:
        raise BenchError(f"cannot write report {path}: {error}") from error
    print(f"report {path}")


def load_report(path: Path) -> Json:
    report = read_json(path)
    if (
        not isinstance(report, dict)
        or report.get("version") != REPORT_VERSION
        or report.get("kind") != "text2koko-report"
        or not isinstance(report.get("results"), list)
    ):
        raise BenchError(f"{path}: not a Text2Koko report version {REPORT_VERSION}")
    return report


def comparison(left: Json, right: Json) -> Json:
    left_intent = left.get("corpus", {}).get("intent_sha256")
    right_intent = right.get("corpus", {}).get("intent_sha256")
    if not left_intent or left_intent != right_intent:
        raise BenchError("reports use different natural-language intents, schemas, or parameters")
    left_keys = {(record["case_id"], record["repetition"]) for record in left["results"]}
    right_keys = {(record["case_id"], record["repetition"]) for record in right["results"]}
    if left_keys != right_keys:
        missing_left = sorted(right_keys - left_keys)
        missing_right = sorted(left_keys - right_keys)
        raise BenchError(
            f"reports cover different samples; missing from left={missing_left[:5]}, "
            f"missing from right={missing_right[:5]}"
        )
    left_by_key = {(record["case_id"], record["repetition"]): record for record in left["results"]}
    right_by_key = {(record["case_id"], record["repetition"]): record for record in right["results"]}
    oracle_mismatches = [
        key
        for key in sorted(left_keys)
        if left_by_key[key].get("oracle_sha256") != right_by_key[key].get("oracle_sha256")
    ]
    if oracle_mismatches:
        raise BenchError(
            f"reports use different semantic or state oracles for samples {oracle_mismatches[:5]}"
        )
    improved = []
    regressed = []
    for key in sorted(left_keys):
        before = left_by_key[key]["passed"]
        after = right_by_key[key]["passed"]
        if not before and after:
            improved.append({"case_id": key[0], "repetition": key[1]})
        elif before and not after:
            regressed.append({"case_id": key[0], "repetition": key[1]})
    left_summary = summarize(left["results"])
    right_summary = summarize(right["results"])
    metrics = {}
    for name, before in left_summary["rates"].items():
        after = right_summary["rates"][name]
        metrics[name] = {"left": before, "right": after, "delta": after - before}
    resource_metrics = {}
    for group in ("generation_seconds", "execution_seconds", "usage"):
        for name, before in left_summary["resources"][group].items():
            after = right_summary["resources"][group][name]
            resource_metrics[f"{group}.{name}"] = {
                "left": before,
                "right": after,
                "delta": after - before,
            }
    return {
        "version": 1,
        "kind": "text2koko-comparison",
        "intent_sha256": left_intent,
        "left": left.get("run", {}),
        "right": right.get("run", {}),
        "samples": len(left_keys),
        "metrics": metrics,
        "resources": resource_metrics,
        "improved": improved,
        "regressed": regressed,
    }


def add_case_filters(parser: argparse.ArgumentParser) -> None:
    parser.add_argument(
        "--case",
        action="append",
        default=[],
        metavar="GLOB",
        help="select case IDs with a shell-style glob; repeatable",
    )
    parser.add_argument(
        "--tag",
        action="append",
        default=[],
        help="require a case tag; repeatable and conjunctive",
    )
    parser.add_argument("--limit", type=int, help="run only the first N selected cases")


def parser() -> argparse.ArgumentParser:
    root = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    root.add_argument("--corpus", type=Path, default=DEFAULT_CORPUS)
    commands = root.add_subparsers(dest="subcommand", required=True)

    validate = commands.add_parser("validate", help="validate corpus structure and execute every reference oracle")
    validate.add_argument("--koko-bin", type=Path, default=DEFAULT_KOKO_BIN)
    validate.add_argument("--execution-timeout", type=float, default=15.0)
    validate.add_argument("--quiet", action="store_true")
    add_case_filters(validate)

    run = commands.add_parser("run", help="generate, execute, score, and report model queries")
    source = run.add_mutually_exclusive_group(required=True)
    source.add_argument("--generator", choices=["reference"])
    source.add_argument("--generator-command", metavar="COMMAND")
    source.add_argument("--replay", type=Path, metavar="REPORT_OR_RESPONSES")
    run.add_argument("--profile", choices=["prior", "guided"], default="prior")
    run.add_argument("--model", default="unspecified")
    run.add_argument(
        "--metadata-json",
        default="{}",
        help="JSON object recorded in the report and sent to the generator command",
    )
    run.add_argument("--repetitions", type=int, default=1)
    run.add_argument("--repair-turns", type=int, choices=[0, 1], default=0)
    run.add_argument("--generation-timeout", type=float, default=120.0)
    run.add_argument("--execution-timeout", type=float, default=15.0)
    run.add_argument("--koko-bin", type=Path, default=DEFAULT_KOKO_BIN)
    run.add_argument("--output", required=True, help="report JSON path, or - for stdout")
    run.add_argument("--quiet", action="store_true")
    add_case_filters(run)

    compare = commands.add_parser("compare", help="compare two reports over exactly the same samples")
    compare.add_argument("left", type=Path)
    compare.add_argument("right", type=Path)
    compare.add_argument("--output", default="-", help="comparison JSON path, or - for stdout")
    return root


def require_positive(value: int | float | None, name: str) -> None:
    if value is not None and value <= 0:
        raise BenchError(f"{name} must be greater than zero")


def main(argv: list[str] | None = None) -> int:
    arguments = parser().parse_args(argv)
    try:
        if arguments.subcommand == "compare":
            document = comparison(load_report(arguments.left), load_report(arguments.right))
            write_report(arguments.output, document)
            return 0

        require_positive(arguments.limit, "--limit")
        require_positive(arguments.execution_timeout, "--execution-timeout")
        corpus_path = arguments.corpus.resolve()
        corpus = load_corpus(corpus_path)
        cases = select_cases(corpus, arguments.case, arguments.tag, arguments.limit)
        binary = arguments.koko_bin.resolve()
        if not binary.is_file():
            raise BenchError(f"Koko binary not found at {binary}; run cargo build -p koko-cli")
        version = koko_version(binary)

        if arguments.subcommand == "validate":
            build_oracles(corpus, cases, binary, arguments.execution_timeout, arguments.quiet)
            print(f"text2koko: GREEN ({len(cases)} reference oracles, {version})")
            return 0

        require_positive(arguments.repetitions, "--repetitions")
        require_positive(arguments.generation_timeout, "--generation-timeout")
        arguments.run_metadata = parse_metadata(arguments.metadata_json)
        if arguments.generator == "reference":
            generator = reference_generator(corpus)
            generator_kind = "reference"
        elif arguments.generator_command:
            generator = command_generator(arguments.generator_command, arguments.generation_timeout)
            generator_kind = "command"
        else:
            generator = replay_generator(arguments.replay)
            generator_kind = "replay"
        oracles = build_oracles(corpus, cases, binary, arguments.execution_timeout, arguments.quiet)
        records = []
        total = len(cases) * arguments.repetitions
        current = 0
        for repetition in range(arguments.repetitions):
            for case in cases:
                current += 1
                record = evaluate_case(
                    corpus,
                    case,
                    oracles[case["id"]],
                    generator,
                    arguments.profile,
                    arguments.model,
                    arguments.run_metadata,
                    repetition,
                    arguments.repair_turns,
                    binary,
                    arguments.execution_timeout,
                )
                records.append(record)
                if not arguments.quiet:
                    status = "PASS" if record["passed"] else "FAIL"
                    stage = record["attempts"][-1]["stage"]
                    print(f"sample {status} {current}/{total} {case['id']} stage={stage}", file=sys.stderr)
        report = report_document(
            corpus_path,
            corpus,
            cases,
            records,
            arguments,
            version,
            generator_kind,
        )
        write_report(arguments.output, report)
        summary = report["summary"]["counts"]
        print(
            f"text2koko: {summary['final_correct']}/{summary['samples']} correct; "
            f"{summary['first_attempt_correct']} first-attempt; {summary['repaired']} repaired"
        )
        return 0
    except BenchError as error:
        print(f"text2koko: FAIL: {error}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
