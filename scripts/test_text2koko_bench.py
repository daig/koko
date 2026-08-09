from __future__ import annotations

import json
from pathlib import Path
import shlex
import sys
import tempfile
import unittest

from scripts import text2koko_bench as bench


class CorpusTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.corpus = bench.load_corpus(bench.DEFAULT_CORPUS)

    def test_seed_corpus_is_representative_and_unique(self) -> None:
        cases = self.corpus["cases"]
        self.assertGreaterEqual(len(cases), 100)
        self.assertEqual({case["schema"] for case in cases}, {"people", "code", "tasks", "notes"})
        self.assertEqual(len({case["id"] for case in cases}), len(cases))
        tags = {tag for case in cases for tag in case["tags"]}
        self.assertTrue(
            {
                "aggregation",
                "any_graph",
                "bounded_path",
                "coding",
                "delete",
                "dialect",
                "lists",
                "optional",
                "parameters",
                "relationships",
                "safety",
                "write",
            }.issubset(tags)
        )

    def test_every_write_has_state_verification(self) -> None:
        for case in self.corpus["cases"]:
            if case["mode"] == "write":
                self.assertTrue(case["verify"], case["id"])
            else:
                self.assertFalse(case.get("verify"), case["id"])

    def test_write_plan_adds_schema_wide_state_snapshots(self) -> None:
        case = next(case for case in self.corpus["cases"] if case["id"] == "people.set_age")
        statements, _target, verification = bench.statement_plan(
            self.corpus,
            case,
            case["reference_query"],
        )
        expected = len(case["verify"]) + len(self.corpus["schemas"]["people"]["state_queries"])
        self.assertEqual(len(verification), expected)
        self.assertEqual(len(statements), len(self.corpus["schemas"]["people"]["setup"]) + 1 + expected)


class StatementEnvelopeTests(unittest.TestCase):
    def test_counts_only_real_statement_separators(self) -> None:
        self.assertEqual(bench.count_statements("RETURN 1"), 1)
        self.assertEqual(bench.count_statements("RETURN 1; // trailing\n"), 1)
        self.assertEqual(bench.count_statements("RETURN ';' AS value"), 1)
        self.assertEqual(bench.count_statements("RETURN `semi;colon`"), 1)
        self.assertEqual(bench.count_statements("/* ; */ RETURN 1; RETURN 2"), 2)
        self.assertEqual(bench.count_statements("; ; // empty"), 0)


class ParameterContractTests(unittest.TestCase):
    def test_prompt_exposes_parameter_types_not_oracle_values(self) -> None:
        corpus = bench.load_corpus(bench.DEFAULT_CORPUS)
        case = next(case for case in corpus["cases"] if case["id"] == "people.minimum_age")
        request = bench.build_request(
            corpus,
            case,
            "prior",
            "model",
            {"temperature": 0},
            [],
        )
        prompt = request["messages"][1]["content"]
        self.assertIn('"min_age":"INT64"', prompt)
        self.assertNotIn("46", prompt)
        self.assertEqual(request["run_metadata"], {"temperature": 0})

    def test_hardcoded_values_do_not_satisfy_parameter_contract(self) -> None:
        case = {"parameters": {"min_age": 46}}
        self.assertEqual(
            bench.check_parameters(case, "MATCH (p) WHERE p.age >= 46 RETURN p"),
            (False, ["query does not reference required parameter $min_age"]),
        )
        self.assertEqual(
            bench.check_parameters(case, "MATCH (p) WHERE p.age >= $min_age RETURN p"),
            (True, []),
        )

    def test_parameter_mentions_in_literals_identifiers_and_comments_do_not_count(self) -> None:
        case = {"parameters": {"id": 1, "id2": 2}}
        query = "RETURN '$id' AS literal, `$id2` AS name, $id_extra AS value /* $id */ // $id2"
        self.assertEqual(
            bench.check_parameters(case, query),
            (
                False,
                [
                    "query does not reference required parameter $id",
                    "query does not reference required parameter $id2",
                ],
            ),
        )


class SafetyPolicyTests(unittest.TestCase):
    def test_required_syntax_in_literals_or_comments_does_not_count(self) -> None:
        case = {"policy": {"require": [r"(?i)\*SHORTEST\s+1\.\.5"]}}
        decoy = (
            "MATCH p = (a)-[:R*WALK 1..5]->(b) "
            "RETURN '*SHORTEST 1..5' AS text // *SHORTEST 1..5"
        )
        self.assertFalse(bench.check_policy(case, decoy)[0])
        self.assertTrue(
            bench.check_policy(case, "MATCH p = (a)-[:R*SHORTEST 1..5]->(b) RETURN p")[0]
        )


class SemanticComparisonTests(unittest.TestCase):
    def test_unordered_rows_are_canonicalized(self) -> None:
        left = {
            "columns": [{"name": "id", "type": "INT64"}],
            "rows": [[2], [1]],
        }
        right = {
            "columns": [{"name": "id", "type": "INT64"}],
            "rows": [[1], [2]],
        }
        self.assertEqual(bench.normalize_result(left, ordered=False), bench.normalize_result(right, ordered=False))
        self.assertNotEqual(bench.normalize_result(left, ordered=True), bench.normalize_result(right, ordered=True))

    def test_numeric_tolerance_is_recursive_but_types_remain_strict(self) -> None:
        self.assertTrue(bench.values_equal({"rows": [[0.1]]}, {"rows": [[0.10000001]]}, 1e-6))
        self.assertFalse(bench.values_equal({"rows": [[0.1]]}, {"rows": [[0.11]]}, 1e-6))
        self.assertFalse(bench.values_equal([True], [1], 1.0))


class ComparisonContractTests(unittest.TestCase):
    def test_rejects_different_intents(self) -> None:
        left = {"corpus": {"intent_sha256": "left"}, "results": []}
        right = {"corpus": {"intent_sha256": "right"}, "results": []}
        with self.assertRaisesRegex(bench.BenchError, "different natural-language intents"):
            bench.comparison(left, right)

    def test_rejects_different_oracles(self) -> None:
        left = {
            "corpus": {"intent_sha256": "same"},
            "results": [{"case_id": "case", "repetition": 0, "oracle_sha256": "left"}],
        }
        right = {
            "corpus": {"intent_sha256": "same"},
            "results": [{"case_id": "case", "repetition": 0, "oracle_sha256": "right"}],
        }
        with self.assertRaisesRegex(bench.BenchError, "different semantic or state oracles"):
            bench.comparison(left, right)


class SummaryTests(unittest.TestCase):
    def test_resources_aggregate_per_sample_latency_and_reported_usage(self) -> None:
        checks = {
            "envelope_valid": True,
            "parse_valid": True,
            "bind_valid": True,
            "runtime_valid": True,
            "parameter_correct": True,
            "semantic_correct": True,
            "state_correct": None,
            "safety_correct": True,
        }
        records = [
            {
                "mode": "read",
                "tags": ["basic"],
                "first_attempt_passed": True,
                "passed": True,
                "repaired": False,
                "attempts": [
                    {
                        "stage": "ok",
                        "passed": True,
                        "checks": checks,
                        "generation_seconds": 1.5,
                        "seconds": 0.25,
                        "usage": {"input_tokens": 100, "output_tokens": 20},
                    }
                ],
            },
            {
                "mode": "read",
                "tags": ["basic"],
                "first_attempt_passed": True,
                "passed": True,
                "repaired": False,
                "attempts": [
                    {
                        "stage": "ok",
                        "passed": True,
                        "checks": checks,
                        "generation_seconds": 0.5,
                        "seconds": 0.75,
                        "usage": {"input_tokens": 80, "output_tokens": 10},
                    }
                ],
            },
        ]
        resources = bench.summarize(records)["resources"]
        self.assertEqual(resources["generation_seconds"]["total"], 2.0)
        self.assertEqual(resources["execution_seconds"]["mean_per_sample"], 0.5)
        self.assertEqual(resources["usage"]["reported_attempts"], 2)
        self.assertEqual(resources["usage"]["input_tokens"], 180)
        self.assertEqual(resources["usage"]["output_tokens"], 30)


class GeneratorProtocolTests(unittest.TestCase):
    def test_command_generator_receives_request_and_returns_strict_query_object(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            adapter = Path(directory) / "adapter.py"
            adapter.write_text(
                "import json, sys\n"
                "request = json.load(sys.stdin)\n"
                "assert request['case_id'] == 'people.active_names'\n"
                "json.dump({'query': 'RETURN 1 AS answer', 'usage': {'input_tokens': 12, 'output_tokens': 5}}, sys.stdout)\n",
                encoding="utf-8",
            )
            command = f"{shlex.quote(sys.executable)} {shlex.quote(str(adapter))}"
            generator = bench.command_generator(command, timeout=5)
            query, elapsed, usage = generator({"case_id": "people.active_names"}, "people.active_names", 0)
        self.assertEqual(query, "RETURN 1 AS answer")
        self.assertEqual(usage, {"input_tokens": 12, "output_tokens": 5})
        self.assertGreaterEqual(elapsed, 0.0)

    def test_command_generator_rejects_extra_envelope_keys(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            adapter = Path(directory) / "adapter.py"
            adapter.write_text(
                "import json, sys\njson.load(sys.stdin)\n"
                "json.dump({'query': 'RETURN 1', 'comment': 'extra'}, sys.stdout)\n",
                encoding="utf-8",
            )
            command = f"{shlex.quote(sys.executable)} {shlex.quote(str(adapter))}"
            generator = bench.command_generator(command, timeout=5)
            with self.assertRaises(bench.GenerationError):
                generator({"case_id": "people.active_names"}, "people.active_names", 0)


@unittest.skipUnless(bench.DEFAULT_KOKO_BIN.is_file(), "build koko-cli to run integration checks")
class EngineIntegrationTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.corpus = bench.load_corpus(bench.DEFAULT_CORPUS)
        cls.case = next(case for case in cls.corpus["cases"] if case["id"] == "people.active_names")
        cls.oracle = bench.build_oracles(
            cls.corpus,
            [cls.case],
            bench.DEFAULT_KOKO_BIN,
            timeout=15,
            quiet=True,
        )[cls.case["id"]]

    def test_reference_query_matches_semantic_oracle(self) -> None:
        outcome = bench.evaluate_query(
            self.corpus,
            self.case,
            self.case["reference_query"],
            bench.DEFAULT_KOKO_BIN,
            timeout=15,
            expected=self.oracle["semantic"],
        )
        self.assertTrue(outcome["passed"], outcome)
        self.assertEqual(outcome["stage"], "ok")

    def test_parser_failure_is_staged(self) -> None:
        outcome = bench.evaluate_query(
            self.corpus,
            self.case,
            "MATCH (",
            bench.DEFAULT_KOKO_BIN,
            timeout=15,
            expected=self.oracle["semantic"],
        )
        self.assertFalse(outcome["passed"])
        self.assertEqual(outcome["stage"], "parser")
        self.assertFalse(outcome["checks"]["parse_valid"])

    def test_read_case_is_engine_enforced_read_only(self) -> None:
        outcome = bench.evaluate_query(
            self.corpus,
            self.case,
            "CREATE (:Person {id: 99, name: 'unsafe', age: 1, city: 'x', tags: [], active: true})",
            bench.DEFAULT_KOKO_BIN,
            timeout=15,
            expected=self.oracle["semantic"],
        )
        self.assertFalse(outcome["passed"])
        self.assertEqual(outcome["stage"], "safety")
        self.assertFalse(outcome["checks"]["safety_correct"])


if __name__ == "__main__":
    unittest.main()
