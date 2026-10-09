#!/usr/bin/env python3
"""Collect exact test IDs and report exact-source contract evidence without tier promotion."""

from __future__ import annotations

import argparse
import datetime as dt
import json
import os
import re
import subprocess
import sys
import tempfile
import xml.etree.ElementTree as ET
from pathlib import Path

from campaign_evidence import campaign_rejection_reason
from sdk_evidence import validate_campaign_processes, validate_sdk_lane

ROOT = Path(__file__).resolve().parents[1]


def command(args: list[str], *, env: dict | None = None) -> str:
    result = subprocess.run(
        args,
        cwd=ROOT,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
        check=True,
        env=env,
    )
    return result.stdout


def rust_records(text: str, *, listing: bool) -> dict[str, str]:
    prefix = ""
    skip = False
    records: dict[str, str] = {}
    for line in text.splitlines():
        if re.search(r"Running benches/", line):
            skip = True
        binary = re.search(r"Running (?:unittests )?(\S+\.rs)", line)
        if binary:
            skip = False
            path = Path(binary[1])
            prefix = f"{path.stem}::" if path.parts[0] == "tests" else ""
        match = (
            re.fullmatch(r"(\S+): test", line.strip())
            if listing
            else re.fullmatch(
                r"test (\S+) \.\.\. (ok|FAILED|ignored)(?: .*)?", line.strip()
            )
        )
        if match and not skip:
            node = prefix + match[1]
            outcome = (
                "collected"
                if listing
                else {"ok": "passed", "FAILED": "failed", "ignored": "skipped"}[
                    match[2]
                ]
            )
            if node in records and records[node] != outcome:
                raise ValueError(f"Conflicting outcomes for {node}")
            records[node] = outcome
    return records


def sdk_records(path: Path) -> dict[str, str]:
    records = {}
    for case in ET.parse(path).getroot().iter("testcase"):
        # pytest's JUnit classname is the module path. Keep parameter IDs in name.
        module = case.attrib["classname"].replace(".", "/")
        node = module + ".py::" + case.attrib["name"]
        outcome = (
            "failed"
            if case.find("failure") is not None or case.find("error") is not None
            else "skipped" if case.find("skipped") is not None else "passed"
        )
        if node in records:
            raise ValueError(f"Duplicate SDK result {node}")
        records[node] = outcome
    return records


def references(matrix: dict, manifest: dict) -> set[str]:
    result = set(manifest.get("tests", {}))
    for families in matrix["providers"].values():
        for family in families.values():
            result.update(family["verified_by"])
            result.update(family["sdk_verified_by"])
    for entry in matrix["operation_contracts"]["entries"]:
        for ids in entry["evidence_candidates"].values():
            result.update(ids)
    return result


def sdk_result_outcome(result: dict) -> str:
    """Keep successful preflights distinct from the declared resource scope."""
    outcome = result["outcome"]
    if outcome != "passed":
        return outcome
    if result.get("qualification_eligible") is False:
        return "scope-unqualified"
    if not result["test"].startswith("sdk-tests/test_large_upload_qualification.py::"):
        return outcome
    if result.get("qualification_eligible") is not True or result.get(
        "qualification_rejection_reason"
    ):
        return "scope-unqualified"
    campaign = result.get("properties", {}).get("large_upload_campaign", {})
    if campaign_rejection_reason(result["test"], campaign) is not None:
        return "scope-unqualified"
    return outcome


def sdk_junit_outcome(node: str, outcome: str, verified: dict[str, str]) -> str:
    """An XML pass contains no measurements and cannot override rejected JSON."""
    if outcome == "passed" and node.startswith(
        "sdk-tests/test_large_upload_qualification.py::"
    ):
        return "passed" if verified.get(node) == "passed" else "scope-unqualified"
    return outcome


def evaluate(
    matrix: dict, manifest: dict, collected: set[str], results: dict[str, str]
) -> dict:
    refs = references(matrix, manifest)
    unknown = sorted(refs - collected)
    if unknown:
        raise ValueError("Uncollected exact test IDs: " + ", ".join(unknown))
    uncollected_results = sorted(set(results) - collected)
    if uncollected_results:
        raise ValueError(
            "Results do not match collection: " + ", ".join(uncollected_results)
        )
    failed = sorted(node for node in refs if results.get(node) == "failed")
    if failed:
        raise ValueError("Referenced tests failed: " + ", ".join(failed))
    ids = set()
    entries = []
    for entry in matrix["operation_contracts"]["entries"]:
        if entry["id"] in ids:
            raise ValueError(f"Duplicate operation ID {entry['id']}")
        ids.add(entry["id"])
        if not all(
            entry.get(field) for field in ("method", "path", "variant", "boundary")
        ):
            raise ValueError(f"Incomplete operation boundary {entry['id']}")
        if entry["support_tier"] == "certified":
            raise ValueError(
                "Tier promotion requires reviewed operation-specific acceptance; this audit runner cannot certify a family"
            )
        candidates = set(sum(entry["evidence_candidates"].values(), []))
        entries.append(
            {
                "id": entry["id"],
                "support_tier": entry["support_tier"],
                "passed_candidates": sorted(
                    node for node in candidates if results.get(node) == "passed"
                ),
                "unproven_candidates": sorted(
                    node for node in candidates if results.get(node) != "passed"
                ),
                "acceptance_gates": entry["acceptance_gates"],
            }
        )
    by_id = {entry["id"]: entry for entry in matrix["operation_contracts"]["entries"]}
    for node, scope in manifest.get("tests", {}).items():
        if "operation_ids" not in scope:
            continue
        operation_ids = scope["operation_ids"]
        if (
            not isinstance(operation_ids, list)
            or not operation_ids
            or any(not isinstance(operation, str) for operation in operation_ids)
            or len(operation_ids) != len(set(operation_ids))
            or set(operation_ids) - by_id.keys()
        ):
            raise ValueError(f"Unknown or invalid SDK operation IDs for {node}")
        for operation in operation_ids:
            if node not in by_id[operation]["evidence_candidates"].get("sdk", []):
                raise ValueError(f"Missing SDK operation evidence link: {operation}: {node}")
    scoped_sdk = {
        node: {**scope, "result": results.get(node, "not-run")}
        for node, scope in manifest.get("tests", {}).items()
    }
    return {
        "collected_test_count": len(collected),
        "reference_count": len(refs),
        "passed_reference_count": sum(results.get(node) == "passed" for node in refs),
        "unproven_references": {
            node: results.get(node, "not-run")
            for node in sorted(refs)
            if results.get(node) != "passed"
        },
        "operations": entries,
        "scoped_sdk_assertions": scoped_sdk,
        "sdk_scope_policy": manifest.get(
            "scope_policy", "No acceptance manifest supplied"
        ),
    }


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--rust-results", type=Path, help="Unedited cargo test stdout/stderr log"
    )
    parser.add_argument(
        "--sdk-results",
        type=Path,
        action="append",
        default=[],
        help="pytest JUnit XML; repeat for independent lanes",
    )
    parser.add_argument(
        "--sdk-evidence",
        type=Path,
        action="append",
        default=[],
        help="SDK lane JSON with source, auth scope, versions and outcomes",
    )
    parser.add_argument(
        "--results-source-sha",
        help="Required with results; must match the checked out HEAD",
    )
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    head = command(["git", "rev-parse", "HEAD"]).strip()
    if (
        args.rust_results or args.sdk_results or args.sdk_evidence
    ) and args.results_source_sha != head:
        raise ValueError("Results source SHA must equal current HEAD")
    if command(["git", "status", "--porcelain"]).strip():
        raise ValueError(
            "Commit or isolate all source changes before collecting exact-source evidence"
        )
    rust = rust_records(
        command(
            ["cargo", "test", "--lib", "--tests", "--all-features", "--", "--list"]
        ),
        listing=True,
    )
    # Collection hooks also emit artifacts. Never overwrite a completed lane.
    with tempfile.TemporaryDirectory(prefix="sqrzl-contract-collection-") as directory:
        sdk_text = command(
            [sys.executable, "-m", "pytest", "sdk-tests", "--collect-only", "-q"],
            env={
                **os.environ,
                "SQRZL_SDK_EVIDENCE": str(Path(directory) / "collection.json"),
            },
        )
    sdk = {
        line.strip()
        for line in sdk_text.splitlines()
        if line.startswith("sdk-tests/") and "::" in line
    }
    if not rust or not sdk:
        raise ValueError("Both Rust and SDK collection must succeed and be nonempty")
    results = (
        rust_records(args.rust_results.read_text(), listing=False)
        if args.rust_results
        else {}
    )
    lane_evidence = []
    verified_sdk_results = {}
    sdk_lane_results = {}
    sdk_rejection_reasons = {}
    source_tree = command(["git", "rev-parse", "HEAD^{tree}"]).strip()

    def tracked_file(path):
        return subprocess.check_output(["git", "show", f"{head}:{path}"], cwd=ROOT)

    for path in args.sdk_evidence:
        lane = json.loads(path.read_text())
        validate_sdk_lane(lane, head, source_tree, tracked_file)
        lane_evidence.append(
            {key: value for key, value in lane.items() if key != "results"}
        )
        for result in lane["results"]:
            node, outcome = result["test"], sdk_result_outcome(result)
            if (
                node.startswith("sdk-tests/test_large_upload_qualification.py::")
                and result["outcome"] == "passed"
            ):
                campaign = result.get("properties", {}).get("large_upload_campaign", {})
                reason = campaign_rejection_reason(node, campaign)
                if outcome == "passed":
                    validate_campaign_processes(campaign, lane["process_events"])
                else:
                    sdk_rejection_reasons.setdefault(node, {})[lane["lane"]] = (
                        reason
                        or result.get("qualification_rejection_reason")
                        or "result is not explicitly qualification eligible"
                    )
            sdk_lane_results.setdefault(node, {})[lane["lane"]] = outcome
            prior = verified_sdk_results.get(node)
            verified_sdk_results[node] = (
                "failed"
                if "failed" in (prior, outcome)
                else "passed" if "passed" in (prior, outcome) else outcome
            )
            old = results.get(node)
            results[node] = (
                "failed"
                if "failed" in (old, outcome)
                else "passed" if "passed" in (old, outcome) else outcome
            )
    for path in args.sdk_results:
        for node, outcome in sdk_records(path).items():
            outcome = sdk_junit_outcome(node, outcome, verified_sdk_results)
            # Passing one explicitly scoped lane is evidence for its scope only.
            # A failed lane must never be masked by a pass or skip in another lane.
            old = results.get(node)
            results[node] = (
                "failed"
                if "failed" in (old, outcome)
                else "passed" if "passed" in (old, outcome) else outcome
            )
    manifest_path = ROOT / "sdk-tests/acceptance-manifest.json"
    manifest = json.loads(manifest_path.read_text()) if manifest_path.exists() else {}
    matrix = json.loads((ROOT / "compatibility-matrix.json").read_text())
    report = evaluate(matrix, manifest, set(rust) | sdk, results)
    for node, assertion in report["scoped_sdk_assertions"].items():
        assertion["result"] = verified_sdk_results.get(node, "no-verified-lane-result")
        assertion["lane_results"] = sdk_lane_results.get(node, {})
        assertion["lane_rejection_reasons"] = sdk_rejection_reasons.get(node, {})
    report.update(
        schema_version=1,
        source_sha=head,
        collected_at=dt.datetime.now(dt.timezone.utc).isoformat(),
        sdk_lane_evidence=lane_evidence,
        sdk_api_versions=manifest.get("api_versions", {}),
        interpretation="Passing evidence candidates do not establish operation acceptance. Skipped, ignored, absent and uncollected tests are never proof. SDK assertions retain their selected scope and auth mode. Source, build manifest, binary digest and process checks establish consistency of trusted unedited runner artifacts, not cryptographic attestation against a dishonest artifact author. Sampled measurements do not establish instantaneous OS resource bounds.",
    )
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(report, indent=2) + "\n")
    print(
        f"Validated {len(report['operations'])} operations and {report['reference_count']} exact references; {report['passed_reference_count']} references have passing results."
    )
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except (ValueError, subprocess.CalledProcessError) as exc:
        print(str(exc), file=sys.stderr)
        raise SystemExit(1)
