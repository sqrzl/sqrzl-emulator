"""Cross-field checks for trusted, unedited measured-campaign artifacts.

These checks establish consistency with the selected scope, not independent
attestation of the artifact author's honesty or continuous OS resource limits.
"""

from __future__ import annotations

import math
import re
from pathlib import PurePosixPath

GIB = 1_073_741_824
PART = 67_108_864
RANGE = 8_388_608
PREFIX = 1_048_576
NAMESPACES = {
    ".multipart",
    ".provider-uploads",
    ".provider-state/azure-block-session-v2",
    ".provider-state/azure-committed-blocks-v2",
    ".provider-state/gcs-resumable-session-v2",
    ".spool",
}
SCOPES = {
    "test_s3_large_multipart_qualification": (
        "s3",
        "native-sigv4",
        "multipart-completed",
        "parts",
        "part",
        "object",
    ),
    "test_azure_large_block_blob_qualification": (
        "azure",
        "native-shared-key",
        "block-list-committed",
        "blocks",
        "block",
        "blob",
    ),
    "test_gcs_large_resumable_qualification": (
        "gcs",
        "local-bearer-convenience",
        "resumable-completed",
        "chunks",
        "chunk",
        "object",
    ),
    "test_oci_large_multipart_qualification": (
        "oci",
        "native-rsa",
        "multipart-completed",
        "parts",
        "part",
        "object",
    ),
}


def integer(value, minimum=0):
    return type(value) is int and value >= minimum


def number(value):
    return type(value) in (int, float) and math.isfinite(value) and value >= 0


def digest(value):
    return isinstance(value, str) and re.fullmatch(r"[0-9a-f]{64}", value) is not None


def campaign_rejection_reason(node: str, c: dict) -> str | None:
    """Validate the v1 property shape and the dependencies omitted by JSON Schema."""
    scope = SCOPES.get(node.partition("::")[2])
    if scope is None or not isinstance(c, dict):
        return "unknown measured campaign scope"
    provider, auth, complete, plural, singular, object_kind = scope
    if (
        type(c.get("schema_version")) is not int
        or c["schema_version"] != 1
        or c.get("provider") != provider
        or c.get("auth_mode") != auth
        or c.get("recovery_inspection_mode")
        != (
            "owned-filesystem-records"
            if provider == "oci"
            else "native-session-or-parts-query"
        )
        or c.get("campaign_kind") != "selected-1GiB-resource-qualification"
        or type(c.get("payload_bytes")) is not int
        or c["payload_bytes"] != GIB
        or c.get("completed") is not True
        or c.get("sampler_errors") != []
        or c.get("payload_generator")
        != "dense SHA256(index) repeated in unique 1MiB blocks v1"
        or c.get("measurement_kind")
        != "sampled RSS and owned logical file sizes; not OS hard limits"
        or not digest(c.get("payload_sha256"))
        or type(c.get("part_bytes")) is not int
        or c["part_bytes"] != PART
        or type(c.get("range_bytes")) is not int
        or c["range_bytes"] != RANGE
        or not integer(c.get("configured_request_cap_bytes"), PART)
        or c["configured_request_cap_bytes"] > 134_217_728
        or not number(c.get("rss_sample_interval_seconds"))
        or not 0.01 <= c["rss_sample_interval_seconds"] <= 0.1
        or c.get("disk_sample_interval_seconds") != 0.25
        or not number(c.get("elapsed_seconds"))
        or c["elapsed_seconds"] <= 0
    ):
        return "campaign does not declare the selected measured scope"

    phases = c.get("phases")
    expected = [
        "payload-generated",
        complete,
        "full-range-readback",
        "normal-process-restart",
        "full-range-readback",
        "abrupt-process-stop-during-transfer",
        "interrupted-client-error",
        "abrupt-process-restart",
        "acknowledged-" + singular + "-recovered",
        "abort-and-staging-cleanup",
    ]
    if (
        not isinstance(phases, list)
        or len(phases) != len(expected)
        or any(not isinstance(p, dict) for p in phases)
        or [p.get("name") for p in phases] != expected
        or any(
            not number(p.get("elapsed_seconds"))
            or p["elapsed_seconds"] > c["elapsed_seconds"]
            for p in phases
        )
        or any(
            a["elapsed_seconds"] > b["elapsed_seconds"]
            for a, b in zip(phases, phases[1:])
        )
    ):
        return "missing or inconsistent ordered campaign phases"
    (
        generated,
        committed,
        first,
        normal,
        second,
        stopped,
        error,
        restarted,
        recovered,
        cleanup,
    ) = phases
    old, middle, new = (
        generated.get("pid"),
        normal.get("new_pid"),
        restarted.get("new_pid"),
    )
    if (
        not all(integer(pid, 1) for pid in (old, middle, new))
        or len({old, middle, new}) != 3
        or [p.get("pid") for p in phases]
        != [old, old, old, middle, middle, None, None, new, new, new]
        or normal.get("previous_pid") != old
        or stopped.get("previous_pid") != middle
        or restarted.get("previous_pid") != middle
    ):
        return "restart PID chain does not demonstrate distinct managed processes"
    if (
        generated.get("payload_sha256") != c["payload_sha256"]
        or not integer(committed.get("acknowledged_" + plural), 1)
        or committed["acknowledged_" + plural] != GIB // PART
        or any(
            p.get("sha256") != c["payload_sha256"]
            or type(p.get("readback_bytes")) is not int
            or p["readback_bytes"] != GIB
            or type(p.get("range_bytes")) is not int
            or p["range_bytes"] != RANGE
            for p in (first, second)
        )
    ):
        return "full payload checksum or bounded readback evidence is inconsistent"
    partial = stopped.get("partial_spool_files")
    if (
        type(stopped.get("transport_prefix_bytes")) is not int
        or stopped["transport_prefix_bytes"] != PREFIX
        or not isinstance(partial, list)
        or not partial
        or any(
            not isinstance(item, (list, tuple))
            or len(item) != 2
            or not isinstance(item[0], str)
            or re.fullmatch(r"\.spool-[0-9a-f-]{36}\.tmp", item[0]) is None
            or not integer(item[1], PREFIX // 2)
            or item[1] > PREFIX
            for item in partial
        )
        or restarted.get("request_spool_cleanup") is not True
        or not isinstance(error.get("exception_type"), str)
        or re.fullmatch(r"[A-Za-z_]\w*(?:\.[A-Za-z_]\w*)+", error["exception_type"])
        is None
        or not isinstance(error.get("detail"), str)
        or not error["detail"]
    ):
        return "interrupted transport or startup spool cleanup evidence is missing"
    if (
        type(recovered.get(singular + "_bytes")) is not int
        or recovered[singular + "_bytes"] != PART
        or recovered.get("partial_" + singular + "_absent") is not True
        or recovered.get("incomplete_" + object_kind + "_absent") is not True
        or (
            provider == "s3"
            and (
                type(recovered.get("part_number")) is not int
                or recovered["part_number"] != 1
            )
        )
    ):
        return "acknowledged bounded part or absent partial publication is unproven"
    if provider == "oci":
        path = recovered.get("record_path")
        if (
            recovered.get("inspection_mode") != "owned-filesystem-records"
            or not digest(recovered.get("source_part_sha256"))
            or recovered.get("source_part_sha256")
            != recovered.get("recovered_part_sha256")
            or not isinstance(path, str)
            or PurePosixPath(path).is_absolute()
            or ".." in PurePosixPath(path).parts
            or len(PurePosixPath(path).parts) != 4
            or PurePosixPath(path).parts[1] != ".multipart"
            or PurePosixPath(path).name != "upload.json"
        ):
            return "OCI owned filesystem part recovery does not match its oracle"
    inspected = cleanup.get("inspected_namespaces")
    if (
        cleanup.get("leftover_files") != []
        or not isinstance(inspected, list)
        or len(inspected) != len(NAMESPACES)
        or any(not isinstance(p, str) for p in inspected)
        or set(inspected) != NAMESPACES
    ):
        return "abort cleanup is incomplete or its owned upload scope was not inspected"

    samples = c.get("samples")
    if (
        not isinstance(samples, list)
        or len(samples) < 2
        or not integer(c.get("sample_count"), 2)
        or c["sample_count"] != len(samples)
    ):
        return "sample count does not match the measured series"
    elapsed = []
    sampled_pids = set()
    for sample in samples:
        if (
            not isinstance(sample, dict)
            or not number(sample.get("elapsed_seconds"))
            or sample["elapsed_seconds"] > c["elapsed_seconds"]
            or not isinstance(sample.get("phase"), str)
            or not sample["phase"]
            or not integer(sample.get("client_rss_bytes"), 1)
            or not integer(sample.get("owned_disk_bytes"))
        ):
            return "invalid resource measurement sample"
        pid, rss = sample.get("service_pid"), sample.get("service_rss_bytes")
        if (
            pid is not None and (not integer(pid, 1) or pid not in (old, middle, new))
        ) or (rss is not None and (not integer(rss, 1) or pid is None)):
            return "service measurements do not refer to the campaign processes"
        if rss is not None:
            sampled_pids.add(pid)
        elapsed.append(sample["elapsed_seconds"])
    # Allow sampler/syscall overhead and health-check transitions. This is sampled
    # evidence; it cannot prove an instantaneous bound between observations.
    if (
        elapsed[0] > 0.25
        or c["elapsed_seconds"] - elapsed[-1] > 1.0
        or any(not 0 <= b - a <= 1.0 for a, b in zip(elapsed, elapsed[1:]))
        or not {old, middle} <= sampled_pids
    ):
        return "sample series does not cover upload and normal restart readback"
    for resource in ("client_rss", "service_rss", "owned_disk"):
        peak = max(sample.get(resource + "_bytes") or 0 for sample in samples)
        declared, budget = c.get(resource + "_peak_bytes"), c.get(
            resource + "_budget_bytes"
        )
        maximum = 5_368_709_120 if resource == "owned_disk" else 536_870_912
        if (
            not integer(declared, 1)
            or declared != peak
            or not integer(budget, 1)
            or not peak <= budget <= maximum
        ):
            return "declared resource peak/budget disagrees with measured samples"
    return None
