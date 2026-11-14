"""Fail closed when a candidate is mistaken for operation certification."""

import copy
import unittest
from operation_certification import certify, CATEGORIES

NODE = "sdk-tests/test_selected.py::test_head"
OP = "selected.Head.current"


def fixture():
    selectors = {"api_version": "v1", "resource": "current"}
    gates = {key: {"sdk": [NODE]} for key in CATEGORIES}
    gates["pages"] = {
        "not_applicable": "Single-resource HEAD has no paginated response."
    }
    policy = {
        "schema_version": 1,
        "profiles": {
            "v1": {"operations": {OP: {"selectors": selectors, "gates": gates}}}
        },
    }
    matrix = {
        "operation_contracts": {
            "entries": [
                {
                    "id": OP,
                    "support_tier": "partial",
                    "certified_subsets": ["v1"],
                    "evidence_candidates": {"sdk": [NODE], "rust": []},
                }
            ]
        }
    }
    manifest = {"tests": {NODE: {"operation_id_checks": {OP: list(CATEGORIES)}}}}
    result = {
        "test": NODE,
        "outcome": "passed",
        "qualification_eligible": True,
        "properties": {
            "operation_certification": {
                "profile": "v1",
                "operations": {
                    OP: {
                        "selectors": dict(selectors),
                        "checks": list(CATEGORIES),
                        "native_auth": True,
                    }
                },
            }
        },
    }
    lane = {
        "binary_source_verified": True,
        "dirty_worktree": False,
        "upgrade_candidate": False,
        "storage_auth_enforced": True,
        "results": [result],
    }
    return policy, matrix, manifest, {NODE}, {NODE: "passed"}, [lane]


class CertificationTests(unittest.TestCase):
    def test_complete_receipts_certify_only_selected_tuple(self):
        report = certify(*fixture())
        self.assertEqual(report["v1"]["operations"][OP]["status"], "certified")
        self.assertEqual(len(report["v1"]["operations"]), 1)

    def reject_mutation(self, mutate, *, invalid=False):
        args = copy.deepcopy(fixture())
        mutate(args)
        if invalid:
            with self.assertRaises(ValueError):
                certify(*args)
        else:
            self.assertNotEqual(
                certify(*args)["v1"]["operations"][OP]["status"], "certified"
            )

    def test_missing_gate_is_invalid(self):
        self.reject_mutation(
            lambda a: a[0]["profiles"]["v1"]["operations"][OP]["gates"].pop(
                "resources"
            ),
            invalid=True,
        )

    def test_core_gate_cannot_be_waived(self):
        self.reject_mutation(
            lambda a: a[0]["profiles"]["v1"]["operations"][OP]["gates"].update(
                resources={"not_applicable": "Too hard"}
            ),
            invalid=True,
        )

    def test_uncollected_reference_is_invalid(self):
        self.reject_mutation(lambda a: a[3].clear(), invalid=True)

    def test_generic_candidate_does_not_establish_scoped_assertion(self):
        self.reject_mutation(
            lambda a: a[2]["tests"][NODE]["operation_id_checks"].clear(), invalid=True
        )

    def test_skipped_failed_missing_and_ineligible_are_not_proof(self):
        for outcome in ["skipped", "failed", "not-run"]:
            with self.subTest(outcome=outcome):
                self.reject_mutation(
                    lambda a: a[5][0]["results"][0].update(outcome=outcome)
                )
        self.reject_mutation(
            lambda a: a[5][0]["results"][0].update(qualification_eligible=False)
        )
        self.reject_mutation(lambda a: a[5][0].update(results=[]))

    def test_anonymous_auth_is_not_native_auth_proof(self):
        self.reject_mutation(lambda a: a[5][0].update(storage_auth_enforced=False))
        self.reject_mutation(
            lambda a: a[5][0]["results"][0]["properties"]["operation_certification"][
                "operations"
            ][OP].update(native_auth=False)
        )

    def test_wrong_selector_and_profile_are_not_proof(self):
        self.reject_mutation(
            lambda a: a[5][0]["results"][0]["properties"]["operation_certification"][
                "operations"
            ][OP]["selectors"].update(resource="historical")
        )
        self.reject_mutation(
            lambda a: a[5][0]["results"][0]["properties"][
                "operation_certification"
            ].update(profile="v2")
        )

    def test_dirty_unverified_and_upgrade_lanes_are_not_proof(self):
        for key, value in [
            ("dirty_worktree", True),
            ("binary_source_verified", False),
            ("upgrade_candidate", True),
        ]:
            with self.subTest(key=key):
                self.reject_mutation(lambda a: a[5][0].update({key: value}))

    def test_junit_pass_without_receipt_is_not_proof(self):
        self.reject_mutation(lambda a: a[5][0]["results"][0].update(properties={}))

    def test_unknown_operation_is_invalid(self):
        self.reject_mutation(
            lambda a: a[1]["operation_contracts"]["entries"].clear(), invalid=True
        )

    def test_rust_needs_passing_outcome_and_reviewed_assertion(self):
        args = fixture()
        gate = args[0]["profiles"]["v1"]["operations"][OP]["gates"]["resources"]
        gate.clear()
        gate["rust"] = [
            {
                "test": "runtime::bound",
                "assertion": "Counts bytes before accepting work.",
            }
        ]
        args[1]["operation_contracts"]["entries"][0]["evidence_candidates"][
            "rust"
        ].append("runtime::bound")
        args[3].add("runtime::bound")
        self.assertEqual(certify(*args)["v1"]["operations"][OP]["status"], "pending")
        args[4]["runtime::bound"] = "passed"
        self.assertEqual(certify(*args)["v1"]["operations"][OP]["status"], "certified")
        gate["rust"][0]["assertion"] = ""
        with self.assertRaises(ValueError):
            certify(*args)


if __name__ == "__main__":
    unittest.main()
