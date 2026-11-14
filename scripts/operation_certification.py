"""Versioned, operation-specific acceptance after exact-source lane validation.

The caller MUST validate SDK lane provenance and pinned dependencies with
sdk_evidence.validate_sdk_lane first. JUnit and collection alone cannot certify.
This is trusted-runner evidence accounting, not adversarial artifact attestation.
"""

from __future__ import annotations

CATEGORIES = frozenset(
    {
        "positive",
        "negative",
        "condition",
        "boundary",
        "pages",
        "restart",
        "process-crash",
        "native-auth",
        "native-error",
        "resources",
    }
)
CORE = frozenset(
    {"positive", "negative", "condition", "boundary", "native-error", "resources"}
)


def certify(policy, matrix, manifest, collected, results, validated_lanes):
    if policy.get("schema_version") != 1 or not policy.get("profiles"):
        raise ValueError("Invalid operation certification policy")
    entries = {entry["id"]: entry for entry in matrix["operation_contracts"]["entries"]}
    report = {}
    for profile, definition in policy["profiles"].items():
        operations = {}
        for operation, contract in definition["operations"].items():
            entry = entries.get(operation)
            if entry is None or profile not in entry.get("certified_subsets", []):
                raise ValueError(
                    f"Unlinked certification operation: {profile}: {operation}"
                )
            selectors, gates = contract.get("selectors"), contract.get("gates", {})
            if (
                not isinstance(selectors, dict)
                or not selectors
                or set(gates) != CATEGORIES
            ):
                raise ValueError(f"Incomplete certification boundary: {operation}")
            outcomes = {}
            for category, gate in gates.items():
                if "not_applicable" in gate:
                    if (
                        category in CORE
                        or set(gate) != {"not_applicable"}
                        or not isinstance(gate["not_applicable"], str)
                        or not gate["not_applicable"].strip()
                        or (
                            category == "native-auth"
                            and selectors.get("protocol")
                            != "SMTP/RFC5321-selected-plaintext"
                        )
                    ):
                        raise ValueError(
                            f"Invalid gate exception: {operation}: {category}"
                        )
                    outcomes[category] = {
                        "status": "not-applicable",
                        "reason": gate["not_applicable"],
                    }
                    continue
                if set(gate) - {"sdk", "rust"} or not (
                    gate.get("sdk") or gate.get("rust")
                ):
                    raise ValueError(
                        f"Empty or unknown gate proof: {operation}: {category}"
                    )
                if category == "native-auth" and (
                    not gate.get("sdk") or gate.get("rust")
                ):
                    raise ValueError(
                        "Native authentication requires an authenticated SDK assertion"
                    )
                proofs = []
                for node in gate.get("sdk", []):
                    if (
                        node not in collected
                        or node not in entry["evidence_candidates"].get("sdk", [])
                        or category
                        not in manifest.get("tests", {})
                        .get(node, {})
                        .get("operation_id_checks", {})
                        .get(operation, [])
                    ):
                        raise ValueError(
                            f"Unscoped SDK proof: {operation}: {category}: {node}"
                        )
                    matches = []
                    for lane in validated_lanes:
                        if (
                            lane.get("binary_source_verified") is not True
                            or lane.get("dirty_worktree") is not False
                            or lane.get("upgrade_candidate") is not False
                        ):
                            continue
                        for result in lane["results"]:
                            if result["test"] != node:
                                continue
                            receipt = result.get("properties", {}).get(
                                "operation_certification", {}
                            )
                            assertion = receipt.get("operations", {}).get(operation, {})
                            matches.append(
                                result["outcome"] == "passed"
                                and result.get("qualification_eligible") is True
                                and receipt.get("profile") == profile
                                and assertion.get("selectors") == selectors
                                and category in assertion.get("checks", [])
                                and (
                                    category != "native-auth"
                                    or (
                                        lane.get("storage_auth_enforced") is True
                                        and assertion.get("native_auth") is True
                                    )
                                )
                            )
                    proofs.append(
                        {
                            "test": node,
                            "kind": "sdk",
                            "passed": any(matches) and results.get(node) != "failed",
                        }
                    )
                for assertion in gate.get("rust", []):
                    node = assertion.get("test")
                    if (
                        node not in collected
                        or node not in entry["evidence_candidates"].get("rust", [])
                        or not isinstance(assertion.get("assertion"), str)
                        or not assertion["assertion"].strip()
                    ):
                        raise ValueError(
                            f"Unreviewed Rust proof: {operation}: {category}: {node}"
                        )
                    proofs.append(
                        {
                            "test": node,
                            "kind": "rust",
                            "assertion": assertion["assertion"],
                            "passed": results.get(node) == "passed",
                        }
                    )
                outcomes[category] = {
                    "status": (
                        "passed"
                        if all(proof["passed"] for proof in proofs)
                        else "pending"
                    ),
                    "proofs": proofs,
                }
            operations[operation] = {
                "status": (
                    "certified"
                    if all(
                        gate["status"] in {"passed", "not-applicable"}
                        for gate in outcomes.values()
                    )
                    else "pending"
                ),
                "selectors": selectors,
                "gates": outcomes,
            }
        if not operations:
            raise ValueError(f"Empty certification profile: {profile}")
        report[profile] = {
            "status": (
                "certified"
                if all(op["status"] == "certified" for op in operations.values())
                else "pending"
            ),
            "operations": operations,
        }
    for entry in entries.values():
        for profile in entry.get("certified_subsets", []):
            if entry["id"] not in report.get(profile, {}).get("operations", {}):
                raise ValueError(
                    f'Unknown certification profile link: {entry["id"]}: {profile}'
                )
    return report
