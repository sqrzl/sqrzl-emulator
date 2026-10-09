"""Adversarial checks for proof accounting, rather than source-text name matching."""

import copy
import hashlib
import json
import tempfile
import unittest
from pathlib import Path

from validate_contract_evidence import (
    evaluate,
    rust_records,
    sdk_records,
    sdk_junit_outcome,
    sdk_result_outcome,
)
from sdk_evidence import validate_campaign_processes, validate_sdk_lane
import validate_contract_evidence


def matrix():
    return {
        "providers": {
            "example": {
                "crud": {
                    "verified_by": ["interop::works"],
                    "sdk_verified_by": ["sdk-tests/test_example.py::test_works"],
                }
            }
        },
        "operation_contracts": {
            "entries": [
                {
                    "id": "example.Get.current",
                    "method": "GET",
                    "path": "/object",
                    "variant": "current",
                    "boundary": "local",
                    "support_tier": "partial",
                    "evidence_candidates": {
                        "rust": ["interop::works"],
                        "sdk": ["sdk-tests/test_example.py::test_works"],
                    },
                    "acceptance_gates": {"restart": "pending"},
                }
            ]
        },
    }


def resource_result(provider="s3"):
    """Consistent synthetic unit fixture, never a measured qualification artifact."""
    kinds = {
        "s3": ("multipart", "parts", "part", "object", "native-sigv4"),
        "azure": ("block-list", "blocks", "block", "blob", "native-shared-key"),
        "gcs": ("resumable", "chunks", "chunk", "object", "local-bearer-convenience"),
        "oci": ("multipart", "parts", "part", "object", "native-rsa"),
    }

    complete, plural, singular, object_kind, auth = kinds[provider]
    size, part, range_size = 1_073_741_824, 67_108_864, 8_388_608
    digest = "a" * 64
    phases = [
        dict(
            name="payload-generated",
            elapsed_seconds=0.3,
            pid=101,
            payload_sha256=digest,
        ),
        dict(
            name=complete + ("-committed" if provider == "azure" else "-completed"),
            elapsed_seconds=1.0,
            pid=101,
            **{"acknowledged_" + plural: 16},
        ),
        dict(
            name="full-range-readback",
            elapsed_seconds=1.4,
            pid=101,
            readback_bytes=size,
            range_bytes=range_size,
            sha256=digest,
        ),
        dict(
            name="normal-process-restart",
            elapsed_seconds=1.5,
            pid=202,
            previous_pid=101,
            new_pid=202,
        ),
        dict(
            name="full-range-readback",
            elapsed_seconds=2.5,
            pid=202,
            readback_bytes=size,
            range_bytes=range_size,
            sha256=digest,
        ),
        dict(
            name="abrupt-process-stop-during-transfer",
            elapsed_seconds=3.0,
            pid=None,
            previous_pid=202,
            transport_prefix_bytes=1_048_576,
            partial_spool_files=[
                [".spool-01234567-0123-0123-0123-0123456789ab.tmp", 1_048_576]
            ],
        ),
        dict(
            name="interrupted-client-error",
            elapsed_seconds=3.01,
            pid=None,
            exception_type="requests.exceptions.ConnectionError",
            detail="connection reset",
        ),
        dict(
            name="abrupt-process-restart",
            elapsed_seconds=3.1,
            pid=303,
            previous_pid=202,
            new_pid=303,
            request_spool_cleanup=True,
        ),
        dict(
            name="acknowledged-" + singular + "-recovered",
            elapsed_seconds=3.2,
            pid=303,
            **{
                singular + "_bytes": part,
                "partial_" + singular + "_absent": True,
                "incomplete_" + object_kind + "_absent": True,
            },
        ),
        dict(
            name="abort-and-staging-cleanup",
            elapsed_seconds=4.9,
            pid=303,
            leftover_files=[],
            inspected_namespaces=[
                ".multipart",
                ".provider-uploads",
                ".provider-state/azure-block-session-v2",
                ".provider-state/azure-committed-blocks-v2",
                ".provider-state/gcs-resumable-session-v2",
                ".spool",
            ],
        ),
    ]
    if provider == "s3":
        phases[8]["part_number"] = 1
    if provider == "oci":
        phases[8].update(
            inspection_mode="owned-filesystem-records",
            source_part_sha256="b" * 64,
            recovered_part_sha256="b" * 64,
            record_path="b-owned/.multipart/upload-id/upload.json",
        )
    samples = []
    for i in range(100):
        elapsed = round(i * 0.05, 3)
        pid = (
            101
            if elapsed < 1.5
            else 202 if elapsed < 3.0 else None if elapsed < 3.1 else 303
        )
        samples.append(
            dict(
                elapsed_seconds=elapsed,
                phase=(
                    "generation"
                    if elapsed < 0.3
                    else (
                        {
                            "s3": "multipart-upload",
                            "azure": "block-upload",
                            "gcs": "resumable-upload",
                            "oci": "multipart-upload",
                        }[provider]
                        if elapsed < 1.0
                        else (
                            "bounded-range-checksum"
                            if elapsed < 1.4
                            else (
                                "normal-restart"
                                if elapsed < 1.5
                                else (
                                    "bounded-range-checksum"
                                    if elapsed < 2.5
                                    else (
                                        "interrupted-transport"
                                        if elapsed < 3.1
                                        else "staging-recovery"
                                    )
                                )
                            )
                        )
                    )
                ),
                client_rss_bytes=67_108_864,
                service_pid=pid,
                service_rss_bytes=33_554_432 if pid else None,
                owned_disk_bytes=2_147_483_648,
            )
        )
    node = {
        "s3": "test_s3_large_multipart_qualification",
        "azure": "test_azure_large_block_blob_qualification",
        "gcs": "test_gcs_large_resumable_qualification",
        "oci": "test_oci_large_multipart_qualification",
    }[provider]
    return {
        "test": "sdk-tests/test_large_upload_qualification.py::" + node,
        "outcome": "passed",
        "qualification_eligible": True,
        "qualification_rejection_reason": None,
        "properties": {
            "large_upload_campaign": dict(
                schema_version=1,
                provider=provider,
                auth_mode=auth,
                recovery_inspection_mode=(
                    "owned-filesystem-records"
                    if provider == "oci"
                    else "native-session-or-parts-query"
                ),
                campaign_kind="selected-1GiB-resource-qualification",
                completed=True,
                payload_bytes=size,
                payload_sha256=digest,
                payload_generator="dense SHA256(index) repeated in unique 1MiB blocks v1",
                part_bytes=part,
                range_bytes=range_size,
                configured_request_cap_bytes=134_217_728,
                client_rss_budget_bytes=536_870_912,
                service_rss_budget_bytes=536_870_912,
                owned_disk_budget_bytes=5_368_709_120,
                rss_sample_interval_seconds=0.05,
                disk_sample_interval_seconds=0.25,
                measurement_kind="sampled RSS and owned logical file sizes; not OS hard limits",
                elapsed_seconds=5.0,
                sampler_errors=[],
                sample_count=len(samples),
                samples=samples,
                client_rss_peak_bytes=67_108_864,
                service_rss_peak_bytes=33_554_432,
                owned_disk_peak_bytes=2_147_483_648,
                phases=phases,
            )
        },
    }


def source_lane():
    head, tree, binary = "a" * 40, "b" * 40, "c" * 64
    snapshot = {"commit": head, "tree": tree, "clean": True}
    source = {
        "Cargo.lock": b"tracked lock",
        "sdk-tests/build_provenance.py": b"tracked builder",
    }
    for name in ("qualification-baseline.json", "acceptance-manifest.json", "requirements.lock"):
        source[f"sdk-tests/{name}"] = (Path(__file__).resolve().parents[1] / "sdk-tests" / name).read_bytes()
    baseline = json.loads(source["sdk-tests/qualification-baseline.json"])
    lane = {
        "source_commit": head,
        "binary_source_commit": head,
        "binary_source_verified": True,
        "binary_sha256": binary,
        "dirty_worktree": False,
        "collection_only": False,
        "exit_status": 0,
        "remote_endpoint": False,
        "upgrade_candidate": False,
        "sdk_versions": {
            name: baseline["dependency_versions"][name]
            for name in (*baseline["direct_sdk_versions"], "botocore", "google-auth")
        },
        "dependency_versions": dict(baseline["dependency_versions"]),
        "api_versions": dict(baseline["api_versions"]),
        "baseline_api_versions": dict(baseline["api_versions"]),
        "baseline_lock_sha256": baseline["requirements_lock_sha256"],
        "build_provenance": {
            "schema_version": 1,
            "kind": "sqrzl-qualification-build",
            "source_verified": True,
            "source_commit": head,
            "source_tree": tree,
            "source_before": dict(snapshot),
            "source_after": dict(snapshot),
            "binary_sha256": binary,
            "cargo_lock_sha256": hashlib.sha256(source["Cargo.lock"]).hexdigest(),
            "build_helper_sha256": hashlib.sha256(
                source["sdk-tests/build_provenance.py"]
            ).hexdigest(),
            "build_command": [
                "cargo",
                "build",
                "--locked",
                "--bin",
                "sqrzl-emulator",
                "--target-dir",
                "/owned/repo/target",
                "--message-format=json-render-diagnostics",
            ],
            "build_profile": "debug",
            "compiler_artifact": {
                "reason": "compiler-artifact",
                "target": {
                    "name": "sqrzl-emulator",
                    "kind": ["bin"],
                    "src_path": "/owned/repo/src/main.rs",
                },
                "executable": "/owned/repo/target/aarch64-apple-darwin/debug/sqrzl-emulator",
            },
        },
        "results": [resource_result()],
        "process_events": [
            {"kind": "start", "pid": 101, "binary_sha256": binary},
            {"kind": "normal-stop", "pid": 101, "exit_code": -15},
            {"kind": "start", "pid": 202, "binary_sha256": binary},
            {"kind": "abrupt-stop", "pid": 202, "exit_code": -9},
            {"kind": "start", "pid": 303, "binary_sha256": binary},
            {"kind": "normal-stop", "pid": 303, "exit_code": -15},
        ],
    }
    for event in lane["process_events"]:
        if event["kind"] == "start":
            event.update(
                health_addresses={
                    "api": "http://127.0.0.1:19000",
                    "ui": "http://127.0.0.1:19001",
                },
                health_ownership="accepted-connection-child-pid",
            )
    return lane, head, tree, source


class EvidenceTests(unittest.TestCase):
    def test_should_reject_missing_or_conflicting_sdk_metadata(self):
        for field in ("sdk_versions", "dependency_versions", "api_versions", "baseline_api_versions", "baseline_lock_sha256"):
            for change in ("missing", "stale"):
                with self.subTest(field=field, change=change):
                    lane, head, tree, source = source_lane()
                    if change == "missing":
                        lane.pop(field)
                    elif isinstance(lane[field], dict):
                        key = next(iter(lane[field]))
                        lane[field][key] = "0.0.0"
                    else:
                        lane[field] = "0" * 64
                    with self.assertRaisesRegex(ValueError, "SDK.*(metadata|baseline|lock)"):
                        validate_sdk_lane(lane, head, tree, source.__getitem__)

    def test_should_require_all_recorded_sdk_and_dependency_packages(self):
        for field, package in (("sdk_versions", "google-auth"), ("dependency_versions", "botocore")):
            with self.subTest(field=field):
                lane, head, tree, source = source_lane()
                lane[field].pop(package)
                with self.assertRaises(ValueError):
                    validate_sdk_lane(lane, head, tree, source.__getitem__)

    def test_should_normalize_distribution_names_without_allowing_ambiguous_aliases(self):
        lane, head, tree, source = source_lane()
        value = lane["dependency_versions"].pop("typing_extensions")
        lane["dependency_versions"]["Typing.Extensions"] = value
        lane["dependency_versions"]["pip"] = "26.0"
        validate_sdk_lane(lane, head, tree, source.__getitem__)
        lane["dependency_versions"]["typing-extensions"] = value
        with self.assertRaises(ValueError):
            validate_sdk_lane(lane, head, tree, source.__getitem__)

    def test_should_bind_baseline_to_tracked_lock_and_api_manifest(self):
        for name in ("requirements.lock", "acceptance-manifest.json", "qualification-baseline.json"):
            with self.subTest(name=name):
                lane, head, tree, source = source_lane()
                path = f"sdk-tests/{name}"
                if name == "requirements.lock":
                    source[path] += b"\nextra==1.0\n"
                else:
                    data = json.loads(source[path])
                    data["api_versions"]["s3"] = "2099-01-01"
                    source[path] = json.dumps(data).encode()
                with self.assertRaises(ValueError):
                    validate_sdk_lane(lane, head, tree, source.__getitem__)

    def test_should_require_distinct_owned_api_and_ui_readiness_evidence(self):
        for change in ("missing", "foreign", "duplicate-port", "remote"):
            with self.subTest(change=change):
                lane, head, tree, source = source_lane()
                event = lane["process_events"][0]
                if change == "missing":
                    del event["health_addresses"]["ui"]
                elif change == "foreign":
                    event["health_ownership"] = "HTTP200 only"
                elif change == "duplicate-port":
                    event["health_addresses"]["ui"] = event["health_addresses"]["api"]
                else:
                    event["health_addresses"]["ui"] = "http://example.invalid:19001"
                with self.assertRaisesRegex(ValueError, "readiness"):
                    validate_sdk_lane(lane, head, tree, source.__getitem__)

    def test_should_require_each_explicit_selected_measured_campaign(self):
        nodes = [resource_result(provider)["test"] for provider in ("s3", "azure")]
        report = {
            "scoped_sdk_assertions": {
                node: {
                    "result": "passed",
                    "lane_results": {"measured-upload": "passed"},
                }
                for node in nodes
            },
            "sdk_lane_evidence": [
                {
                    "lane": "measured-upload",
                    "enabled_providers": ["s3", "azure"],
                    "storage_auth_enforced": True,
                }
            ],
        }
        validate_contract_evidence.require_measured_campaigns(report, "s3,azure")
        # Unrelated provider scopes may remain explicitly unproven.
        for change in (
            "missing",
            "skipped",
            "scope-unqualified",
            "wrong-lane",
            "auth-disabled",
        ):
            with self.subTest(change=change):
                altered = copy.deepcopy(report)
                selected = altered["scoped_sdk_assertions"][nodes[1]]
                if change == "missing":
                    del altered["scoped_sdk_assertions"][nodes[1]]
                elif change == "wrong-lane":
                    selected["lane_results"] = {"functional": "passed"}
                elif change == "auth-disabled":
                    altered["sdk_lane_evidence"][0]["storage_auth_enforced"] = False
                else:
                    selected["result"] = change
                    selected["lane_results"]["measured-upload"] = change
                with self.assertRaisesRegex(ValueError, "selected measured"):
                    validate_contract_evidence.require_measured_campaigns(
                        altered, "s3,azure"
                    )

    def test_should_reject_empty_or_unknown_required_measured_scope(self):
        for providers in ("", "s3,", "s3,gmail", "s3,s3"):
            with self.subTest(providers=providers):
                with self.assertRaisesRegex(ValueError, "selected measured"):
                    validate_contract_evidence.require_measured_campaigns({}, providers)

    def test_should_not_accept_an_unexpected_exit_as_a_normal_stop(self):
        for exit_code in (1, -9):
            with self.subTest(exit_code=exit_code):
                lane, head, tree, source = source_lane()
                lane["process_events"][-1]["exit_code"] = exit_code
                with self.assertRaisesRegex(ValueError, "stop"):
                    validate_sdk_lane(lane, head, tree, source.__getitem__)

    def test_should_reject_sdk_scope_with_an_unknown_operation_id(self):
        node = "sdk-tests/test_example.py::test_works"
        manifest = {"tests": {node: {"operation_ids": ["example.Unknown.current"]}}}
        with self.assertRaisesRegex(ValueError, "operation IDs"):
            evaluate(matrix(), manifest, {"interop::works", node}, {})

    def test_should_reject_sdk_scope_without_its_operation_evidence_link(self):
        node = "sdk-tests/test_example.py::test_unlinked"
        manifest = {"tests": {node: {"operation_ids": ["example.Get.current"]}}}
        with self.assertRaisesRegex(ValueError, "evidence link"):
            evaluate(
                matrix(),
                manifest,
                {"interop::works", "sdk-tests/test_example.py::test_works", node},
                {},
            )

    def test_should_reject_missing_client_or_one_live_service_rss_measurement(self):
        for resource in ("client_rss_bytes", "service_rss_bytes"):
            with self.subTest(resource=resource):
                result = resource_result()
                result["properties"]["large_upload_campaign"]["samples"][10][
                    resource
                ] = None
                self.assertEqual(sdk_result_outcome(result), "scope-unqualified")

    def test_should_require_upload_and_both_readback_measurements(self):
        for start, end in ((0.3, 1.0), (1.0, 1.4), (1.5, 2.5)):
            with self.subTest(window=(start, end)):
                result = resource_result()
                for sample in result["properties"]["large_upload_campaign"]["samples"]:
                    if start <= sample["elapsed_seconds"] <= end:
                        sample["phase"] = "unmeasured-operation"
                self.assertEqual(sdk_result_outcome(result), "scope-unqualified")

    def test_should_require_recovery_measurements_when_recovery_outlasts_sampling_slack(
        self,
    ):
        result = resource_result()
        for sample in result["properties"]["large_upload_campaign"]["samples"]:
            if sample["elapsed_seconds"] >= 3.1:
                sample["phase"] = "interrupted-transport"
        self.assertEqual(sdk_result_outcome(result), "scope-unqualified")

    def test_should_accept_timed_short_recovery_between_resource_samples(self):
        for provider in ("s3", "azure", "gcs", "oci"):
            with self.subTest(provider=provider):
                result = resource_result(provider)
                campaign = result["properties"]["large_upload_campaign"]
                campaign["phases"][8]["elapsed_seconds"] = 3.12
                campaign["phases"][9]["elapsed_seconds"] = 3.14
                campaign["elapsed_seconds"] = 3.15
                campaign["samples"] = [
                    s for s in campaign["samples"] if s["elapsed_seconds"] <= 3.0
                ]
                campaign["sample_count"] = len(campaign["samples"])
                self.assertEqual(sdk_result_outcome(result), "passed")

    def test_should_not_restore_resource_proof_from_passing_junit(self):
        node = resource_result()["test"]
        self.assertEqual(sdk_junit_outcome(node, "passed", {}), "scope-unqualified")
        self.assertEqual(
            sdk_junit_outcome(node, "passed", {node: "scope-unqualified"}),
            "scope-unqualified",
        )
        self.assertEqual(sdk_junit_outcome(node, "passed", {node: "passed"}), "passed")
        self.assertEqual(sdk_junit_outcome(node, "failed", {node: "passed"}), "failed")

    def test_should_accept_consistent_exact_source_lane_and_process_binding(self):
        lane, head, tree, source = source_lane()
        validate_sdk_lane(lane, head, tree, source.__getitem__)
        validate_campaign_processes(
            lane["results"][0]["properties"]["large_upload_campaign"],
            lane["process_events"],
        )

    def test_should_reject_unbound_binary_source_and_process_evidence(self):
        cases = [
            (
                "different binary source",
                lambda a: a.update(binary_source_commit="d" * 40),
            ),
            ("verified string", lambda a: a.update(binary_source_verified="true")),
            ("missing clean flag", lambda a: a.pop("dirty_worktree")),
            ("upgrade candidate", lambda a: a.update(upgrade_candidate=True)),
            ("remote endpoint", lambda a: a.update(remote_endpoint=True)),
            ("no build manifest", lambda a: a.pop("build_provenance")),
            (
                "different nested digest",
                lambda a: a["build_provenance"].update(binary_sha256="d" * 64),
            ),
            (
                "different source tree",
                lambda a: a["build_provenance"].update(source_tree="d" * 40),
            ),
            (
                "boolean-like nested clean flag",
                lambda a: a["build_provenance"]["source_after"].update(clean=1),
            ),
            (
                "source changed during build",
                lambda a: a["build_provenance"]["source_after"].update(commit="d" * 40),
            ),
            (
                "different tracked lock",
                lambda a: a["build_provenance"].update(cargo_lock_sha256="d" * 64),
            ),
            (
                "different tracked helper",
                lambda a: a["build_provenance"].update(build_helper_sha256="d" * 64),
            ),
            (
                "assumed executable path",
                lambda a: a["build_provenance"].pop("compiler_artifact"),
            ),
            (
                "different main target",
                lambda a: a["build_provenance"]["compiler_artifact"]["target"].update(
                    src_path="/other/repo/src/main.rs"
                ),
            ),
            (
                "stale executable started",
                lambda a: a["process_events"][0].update(binary_sha256="d" * 64),
            ),
            (
                "no process stops",
                lambda a: a.update(process_events=a["process_events"][:1]),
            ),
            ("wrong killed child", lambda a: a["process_events"][3].update(pid=404)),
            ("kill not reaped", lambda a: a["process_events"][3].pop("exit_code")),
            (
                "stop timed out",
                lambda a: a["process_events"][1].update(kind="stop-timeout"),
            ),
            (
                "duplicate exact result",
                lambda a: a["results"].append(copy.deepcopy(a["results"][0])),
            ),
        ]
        for name, mutate in cases:
            with self.subTest(case=name):
                lane, head, tree, source = source_lane()
                mutate(lane)
                with self.assertRaises(ValueError):
                    validate_sdk_lane(lane, head, tree, source.__getitem__)

    def test_should_reject_campaign_restart_claims_without_matching_lane_events(self):
        lane, _, _, _ = source_lane()
        lane["process_events"][3]["kind"] = "normal-stop"
        with self.assertRaisesRegex(ValueError, "restart phases"):
            validate_campaign_processes(
                lane["results"][0]["properties"]["large_upload_campaign"],
                lane["process_events"],
            )

    def test_should_accept_consistent_selected_resource_fixtures(self):
        for provider in ("s3", "azure", "gcs", "oci"):
            with self.subTest(provider=provider):
                self.assertEqual(
                    sdk_result_outcome(resource_result(provider)), "passed"
                )

    def test_should_reject_inconsistent_resource_measurements_and_recovery(self):
        valid = resource_result()
        cases = [
            ("empty samples", lambda c: c.update(samples=[])),
            ("invented count", lambda c: c.update(sample_count=1)),
            ("invented peak", lambda c: c.update(service_rss_peak_bytes=1)),
            (
                "budget exceeded by series",
                lambda c: c["samples"][0].update(service_rss_bytes=536_870_913),
            ),
            (
                "missing service RSS",
                lambda c: [s.update(service_rss_bytes=None) for s in c["samples"]],
            ),
            ("negative RSS", lambda c: c["samples"][0].update(client_rss_bytes=-1)),
            (
                "NaN time",
                lambda c: c["samples"][0].update(elapsed_seconds=float("nan")),
            ),
            ("unordered samples", lambda c: c["samples"][5].update(elapsed_seconds=0)),
            ("no campaign coverage", lambda c: c.update(elapsed_seconds=50)),
            (
                "unrelated sampled PID",
                lambda c: c["samples"][0].update(service_pid=999),
            ),
            ("same normal PID", lambda c: c["phases"][3].update(new_pid=101, pid=101)),
            ("same abrupt PID", lambda c: c["phases"][7].update(new_pid=202, pid=202)),
            ("broken restart chain", lambda c: c["phases"][7].update(previous_pid=101)),
            (
                "no HTTP prefix",
                lambda c: c["phases"][5].update(transport_prefix_bytes=0),
            ),
            (
                "no observed partial spool",
                lambda c: c["phases"][5].update(partial_spool_files=[]),
            ),
            (
                "full next request",
                lambda c: c["phases"][5].update(
                    partial_spool_files=[
                        [".spool-01234567-0123-0123-0123-0123456789ab.tmp", 67_108_864]
                    ]
                ),
            ),
            (
                "unrelated spool path",
                lambda c: c["phases"][5].update(
                    partial_spool_files=[["../secret", 1_048_576]]
                ),
            ),
            (
                "spool survived restart",
                lambda c: c["phases"][7].update(request_spool_cleanup=False),
            ),
            (
                "partial part survived",
                lambda c: c["phases"][8].update(partial_part_absent=False),
            ),
            (
                "incomplete object published",
                lambda c: c["phases"][8].update(incomplete_object_absent=False),
            ),
            (
                "staging leftovers",
                lambda c: c["phases"][9].update(leftover_files=[".spool/leak"]),
            ),
            (
                "cleanup not inspected",
                lambda c: c["phases"][9].pop("inspected_namespaces"),
            ),
            (
                "unbounded full read",
                lambda c: c["phases"][2].update(range_bytes=2_147_483_648),
            ),
            (
                "partial readback",
                lambda c: c["phases"][4].update(readback_bytes=268_435_456),
            ),
            ("checksum mismatch", lambda c: c["phases"][4].update(sha256="c" * 64)),
            ("fake eligibility", lambda c: c.update(completed=1)),
            ("boolean count", lambda c: c.update(sample_count=True)),
            (
                "unsupported request cap",
                lambda c: c.update(configured_request_cap_bytes=1_073_741_824),
            ),
        ]
        for name, mutate in cases:
            with self.subTest(case=name):
                result = copy.deepcopy(valid)
                mutate(result["properties"]["large_upload_campaign"])
                self.assertEqual(sdk_result_outcome(result), "scope-unqualified")

    def test_should_preserve_oci_filesystem_recovery_boundary(self):
        result = resource_result("oci")
        result["properties"]["large_upload_campaign"]["phases"][8][
            "recovered_part_sha256"
        ] = ("c" * 64)
        self.assertEqual(sdk_result_outcome(result), "scope-unqualified")

    def test_should_not_accept_smaller_successful_resource_preflight(self):
        result = {
            "test": "sdk-tests/test_large_upload_qualification.py::test_large",
            "outcome": "passed",
            "qualification_eligible": False,
            "properties": {
                "large_upload_campaign": {
                    "payload_bytes": 268_435_456,
                    "campaign_kind": "smaller-payload-preflight",
                    "completed": True,
                }
            },
        }
        self.assertEqual(sdk_result_outcome(result), "scope-unqualified")
        result.pop("qualification_eligible")
        self.assertEqual(sdk_result_outcome(result), "scope-unqualified")

    def test_should_not_accept_resource_pass_without_measured_recovery(self):
        result = {
            "test": "sdk-tests/test_large_upload_qualification.py::test_large",
            "outcome": "passed",
            "qualification_eligible": True,
            "properties": {
                "large_upload_campaign": {
                    "payload_bytes": 1_073_741_824,
                    "campaign_kind": "selected-1GiB-resource-qualification",
                    "completed": True,
                }
            },
        }
        self.assertEqual(sdk_result_outcome(result), "scope-unqualified")

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
        self.assertEqual(
            report["operations"][0]["acceptance_gates"]["restart"], "pending"
        )

    def test_should_distinguish_duplicate_bare_names_across_rust_binaries(self):
        log = "Running unittests src/lib.rs (target/lib)\ntest works ... ok\nRunning tests/interop.rs (target/test)\ntest works ... ignored\nRunning benches/load.rs (target/bench)\ntest works ... ok\n"
        self.assertEqual(
            rust_records(log, listing=False),
            {"works": "passed", "interop::works": "skipped"},
        )

    def test_should_read_exact_parametrized_sdk_ids_and_errors(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "junit.xml"
            path.write_text(
                '<testsuites><testsuite><testcase classname="sdk-tests.test_example" name="test_works[empty]"><error/></testcase></testsuite></testsuites>'
            )
            self.assertEqual(
                sdk_records(path),
                {"sdk-tests/test_example.py::test_works[empty]": "failed"},
            )


if __name__ == "__main__":
    unittest.main()
