"""Adversarial checks for proof accounting, rather than source-text name matching."""
import tempfile
import unittest
from pathlib import Path

from validate_contract_evidence import evaluate, rust_records, sdk_records


def matrix():
    return {"providers": {"example": {"crud": {"verified_by": ["interop::works"], "sdk_verified_by": ["sdk-tests/test_example.py::test_works"]}}},
            "operation_contracts": {"entries": [{"id": "example.Get.current", "method": "GET", "path": "/object", "variant": "current", "boundary": "local", "support_tier": "partial", "evidence_candidates": {"rust": ["interop::works"], "sdk": ["sdk-tests/test_example.py::test_works"]}, "acceptance_gates": {"restart": "pending"}}]}}


class EvidenceTests(unittest.TestCase):
    def test_should_reject_unknown_exact_test_id(self):
        with self.assertRaisesRegex(ValueError, "Uncollected exact"):
            evaluate(matrix(), {}, {"other::works"}, {})

    def test_should_keep_skipped_and_absent_tests_unproven(self):
        ids = {"interop::works", "sdk-tests/test_example.py::test_works"}
        report = evaluate(matrix(), {}, ids, {"interop::works": "skipped"})
        self.assertEqual(report["passed_reference_count"], 0)
        self.assertEqual(len(report["unproven_references"]), 2)

    def test_should_reject_failed_evidence(self):
        ids = {"interop::works", "sdk-tests/test_example.py::test_works"}
        with self.assertRaisesRegex(ValueError, "Referenced tests failed"):
            evaluate(matrix(), {}, ids, {"interop::works": "failed"})

    def test_should_preserve_operation_gaps_after_smoke_passes(self):
        ids = {"interop::works", "sdk-tests/test_example.py::test_works"}
        report = evaluate(matrix(), {}, ids, dict.fromkeys(ids, "passed"))
        self.assertEqual(report["operations"][0]["support_tier"], "partial")
        self.assertEqual(report["operations"][0]["acceptance_gates"]["restart"], "pending")

    def test_should_distinguish_duplicate_bare_names_across_rust_binaries(self):
        log = "Running unittests src/lib.rs (target/lib)\ntest works ... ok\nRunning tests/interop.rs (target/test)\ntest works ... ignored\nRunning benches/load.rs (target/bench)\ntest works ... ok\n"
        self.assertEqual(rust_records(log, listing=False), {"works": "passed", "interop::works": "skipped"})

    def test_should_read_exact_parametrized_sdk_ids_and_errors(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "junit.xml"
            path.write_text('<testsuites><testsuite><testcase classname="sdk-tests.test_example" name="test_works[empty]"><error/></testcase></testsuite></testsuites>')
            self.assertEqual(sdk_records(path), {"sdk-tests/test_example.py::test_works[empty]": "failed"})


if __name__ == "__main__":
    unittest.main()
