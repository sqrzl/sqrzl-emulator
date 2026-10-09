"""Validate source/digest/process consistency within trusted SDK lane evidence.

The caller supplies Git object reads for the checkout being evaluated. A build
manifest is an auditable record from a trusted runner, not cryptographic proof
against an artifact author who fabricates both the manifest and the results.
"""

from __future__ import annotations

import hashlib
import json
import re
from pathlib import Path
from urllib.parse import urlsplit

from campaign_evidence import digest, integer


def _distribution_versions(value) -> dict[str, str]:
    if not isinstance(value, dict):
        raise ValueError("SDK version metadata must be a package mapping")
    normalized = {}
    for name, version in value.items():
        if not isinstance(name, str) or not name or not isinstance(version, str) or not version:
            raise ValueError("SDK version metadata has an invalid package or version")
        key = re.sub(r"[-_.]+", "-", name).lower()
        if key in normalized:
            raise ValueError("SDK version metadata has ambiguous distribution aliases")
        normalized[key] = version
    return normalized


def _validate_sdk_metadata(lane: dict, tracked_file) -> None:
    baseline = json.loads(tracked_file("sdk-tests/qualification-baseline.json"))
    manifest = json.loads(tracked_file("sdk-tests/acceptance-manifest.json"))
    lock = tracked_file("sdk-tests/requirements.lock")
    lock_digest = hashlib.sha256(lock).hexdigest()
    if (
        lane.get("baseline_lock_sha256") != lock_digest
        or baseline.get("requirements_lock_sha256") != lock_digest
        or baseline.get("lock_file") != "sdk-tests/requirements.lock"
    ):
        raise ValueError("SDK baseline lock metadata differs from tracked source")
    locked = {}
    for line in lock.decode().splitlines():
        if not line.strip() or line.lstrip().startswith("#"):
            continue
        name, separator, version = line.strip().partition("==")
        if not separator or name in locked:
            raise ValueError("SDK baseline lock must contain unique pinned distributions")
        locked[name] = version
    expected = _distribution_versions(baseline.get("dependency_versions"))
    if _distribution_versions(locked) != expected:
        raise ValueError("SDK baseline dependency metadata differs from tracked lock")
    installed = _distribution_versions(lane.get("dependency_versions"))
    if any(installed.get(name) != version for name, version in expected.items()):
        raise ValueError("SDK dependency metadata differs from the pinned baseline")
    direct = _distribution_versions(baseline.get("direct_sdk_versions"))
    if any(expected.get(name) != version for name, version in direct.items()):
        raise ValueError("SDK direct package metadata differs from tracked lock")
    # The runner records the direct SDKs plus these signing/model dependencies.
    recorded = direct | {name: expected[name] for name in ("botocore", "google-auth")}
    if _distribution_versions(lane.get("sdk_versions")) != recorded:
        raise ValueError("SDK version metadata differs from the pinned baseline")
    api_versions = baseline.get("api_versions")
    if (
        not isinstance(api_versions, dict)
        or not api_versions
        or manifest.get("api_versions") != api_versions
        or lane.get("api_versions") != api_versions
        or lane.get("baseline_api_versions") != api_versions
    ):
        raise ValueError("SDK API metadata differs from the tracked baseline and manifest")


def validate_sdk_lane(lane: dict, head: str, source_tree: str, tracked_file) -> None:
    if (
        lane.get("source_commit") != head
        or lane.get("binary_source_commit") != head
        or lane.get("dirty_worktree") is not False
        or lane.get("collection_only") is not False
        or type(lane.get("exit_status")) is not int
        or lane["exit_status"] != 0
        or lane.get("binary_source_verified") is not True
        or lane.get("remote_endpoint") is not False
        or lane.get("upgrade_candidate") is not False
        or not digest(lane.get("binary_sha256"))
    ):
        raise ValueError("SDK lane is not clean passing exact-head managed evidence")
    _validate_sdk_metadata(lane, tracked_file)
    provenance = lane.get("build_provenance")
    snapshot = {"commit": head, "tree": source_tree, "clean": True}
    if (
        not isinstance(provenance, dict)
        or type(provenance.get("schema_version")) is not int
        or provenance["schema_version"] != 1
        or provenance.get("kind") != "sqrzl-qualification-build"
        or provenance.get("source_verified") is not True
        or provenance.get("source_commit") != head
        or provenance.get("source_tree") != source_tree
        or provenance.get("source_before") != snapshot
        or provenance.get("source_after") != snapshot
        or provenance.get("source_before", {}).get("clean") is not True
        or provenance.get("source_after", {}).get("clean") is not True
        or provenance.get("binary_sha256") != lane["binary_sha256"]
    ):
        raise ValueError(
            "SDK build manifest does not bind the same stable source and binary digest"
        )
    for field, path in (
        ("cargo_lock_sha256", "Cargo.lock"),
        ("build_helper_sha256", "sdk-tests/build_provenance.py"),
    ):
        if provenance.get(field) != hashlib.sha256(tracked_file(path)).hexdigest():
            raise ValueError(f"SDK build manifest {field} differs from source")
    command = provenance.get("build_command")
    if (
        not isinstance(command, list)
        or len(command) != 8
        or command[:6]
        != ["cargo", "build", "--locked", "--bin", "sqrzl-emulator", "--target-dir"]
        or not isinstance(command[6], str)
        or not Path(command[6]).is_absolute()
        or Path(command[6]).name != "target"
        or command[-1] != "--message-format=json-render-diagnostics"
        or provenance.get("build_profile") != "debug"
    ):
        raise ValueError("SDK build manifest lacks the expected Cargo artifact command")
    artifact = provenance.get("compiler_artifact")
    if (
        not isinstance(artifact, dict)
        or artifact.get("reason") != "compiler-artifact"
        or artifact.get("target", {}).get("name") != "sqrzl-emulator"
        or artifact.get("target", {}).get("kind") != ["bin"]
        or artifact.get("target", {}).get("src_path")
        != str(Path(command[6]).parent / "src/main.rs")
        or not isinstance(artifact.get("executable"), str)
        or not Path(artifact["executable"]).is_absolute()
    ):
        raise ValueError(
            "SDK build manifest lacks the matching Cargo executable artifact"
        )

    events = lane.get("process_events")
    if not isinstance(events, list) or not events:
        raise ValueError("SDK lane has no managed process evidence")
    active = None
    for event in events:
        if not isinstance(event, dict) or not integer(event.get("pid"), 1):
            raise ValueError("SDK managed process event is malformed")
        if event.get("kind") == "start":
            addresses = event.get("health_addresses")
            if (
                not isinstance(addresses, dict)
                or set(addresses) != {"api", "ui"}
                or event.get("health_ownership") != "accepted-connection-child-pid"
            ):
                raise ValueError(
                    "SDK process readiness lacks owned API/UI health addresses"
                )
            parsed = [
                urlsplit(url) for url in addresses.values() if isinstance(url, str)
            ]
            if (
                len(parsed) != 2
                or any(
                    address.scheme != "http"
                    or address.hostname != "127.0.0.1"
                    or not address.port
                    or address.path
                    or address.query
                    or address.fragment
                    or address.username
                    or address.password
                    for address in parsed
                )
                or len({address.port for address in parsed}) != 2
            ):
                raise ValueError(
                    "SDK process readiness has invalid owned API/UI health addresses"
                )
            if (
                active is not None
                or event.get("binary_sha256") != lane["binary_sha256"]
            ):
                raise ValueError(
                    "SDK process start lacks the same binary digest or stop boundary"
                )
            active = event["pid"]
        elif event.get("kind") in ("normal-stop", "abrupt-stop"):
            if (
                event["pid"] != active
                or type(event.get("exit_code")) is not int
                or (event["kind"] == "abrupt-stop" and event["exit_code"] != -9)
                or (
                    event["kind"] == "normal-stop"
                    and event["exit_code"] not in (0, -15)
                )
            ):
                raise ValueError("SDK process stop lacks its matching reaped child")
            active = None
        else:
            raise ValueError("SDK managed process did not complete its stop boundary")
    if active is not None:
        raise ValueError("SDK lane leaves a managed process running")
    results = lane.get("results")
    if (
        not isinstance(results, list)
        or any(
            not isinstance(r, dict) or not isinstance(r.get("test"), str)
            for r in results
        )
        or len({r["test"] for r in results}) != len(results)
    ):
        raise ValueError("SDK lane result IDs must be exact and unique")


def validate_campaign_processes(campaign: dict, events: list[dict]) -> None:
    """Link otherwise qualified per-test restart phases to lane process events."""
    phases = campaign["phases"]
    old, middle, new = phases[0]["pid"], phases[3]["new_pid"], phases[7]["new_pid"]
    expected = [
        ("start", old),
        ("normal-stop", old),
        ("start", middle),
        ("abrupt-stop", middle),
        ("start", new),
    ]
    recorded = [(event["kind"], event["pid"]) for event in events]
    if not any(
        recorded[index : index + len(expected)] == expected
        for index in range(len(recorded))
    ):
        raise ValueError(
            "SDK campaign restart phases do not match native managed process events"
        )
