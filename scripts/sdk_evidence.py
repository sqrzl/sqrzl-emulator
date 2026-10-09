"""Validate source/digest/process consistency within trusted SDK lane evidence.

The caller supplies Git object reads for the checkout being evaluated. A build
manifest is an auditable record from a trusted runner, not cryptographic proof
against an artifact author who fabricates both the manifest and the results.
"""

from __future__ import annotations

import hashlib
from pathlib import Path

from campaign_evidence import digest, integer


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
