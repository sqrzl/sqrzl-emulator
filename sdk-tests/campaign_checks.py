"""Small independent checks for the resource campaign's checksum/interruption oracle."""

from __future__ import annotations

import concurrent.futures
import hashlib
import os
import tempfile
import unittest
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import patch

from large_campaign import Campaign, MIB, SegmentReader, generate_payload


class SampledProcess:
    def __init__(self, pid):
        self.pid = pid
        self.returncode = None

    def poll(self):
        return self.returncode


class SampledRuntime:
    def __init__(self, directory, process):
        self.runtime_dir = directory
        self.process = process
        self.after_pid_observation = None

    @property
    def process_pid(self):
        process = self.process
        pid = process.pid if process and process.poll() is None else None
        if self.after_pid_observation:
            callback, self.after_pid_observation = self.after_pid_observation, None
            callback()
        return pid


class CampaignChecks(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        self.path = Path(self.directory.name) / "payload"

    def sample_once(self, runtime, measure, phase="multipart-upload", intentional_stop=None):
        settings = SimpleNamespace(require_process=lambda: runtime)
        campaign = Campaign(settings, Path(self.directory.name), lambda *args: None, "s3")
        campaign.phase(phase)
        if intentional_stop:
            campaign._intentional_stops[runtime.process.pid] = intentional_stop
        with patch("large_campaign.rss_bytes", side_effect=measure), patch.object(
            campaign.stop_event, "wait", side_effect=lambda delay: campaign.stop_event.set()
        ):
            campaign._sample()
        return campaign

    def test_missing_client_rss_fails_the_campaign_budget_gate(self):
        runtime = SampledRuntime(Path(self.directory.name), SampledProcess(101))
        campaign = self.sample_once(
            runtime, lambda pid: None if pid == os.getpid() else MIB
        )
        self.assertTrue(campaign.errors, "missing client RSS must be a sampler error")
        with self.assertRaisesRegex(AssertionError, "resource sampler errors"):
            campaign.assert_budgets()

    def test_missing_live_service_rss_fails_the_campaign_budget_gate(self):
        for phase, intent in (("multipart-upload", None), ("normal-restart", "normal-restart")):
            with self.subTest(phase=phase):
                runtime = SampledRuntime(Path(self.directory.name), SampledProcess(101))
                campaign = self.sample_once(
                    runtime, lambda pid: MIB if pid == os.getpid() else None, phase, intent
                )
                self.assertTrue(campaign.errors, "missing live service RSS must be a sampler error")
                with self.assertRaisesRegex(AssertionError, "resource sampler errors"):
                    campaign.assert_budgets()

    def test_stop_during_rss_lookup_cannot_bind_missing_rss_to_a_new_process(self):
        old = SampledProcess(101)
        runtime = SampledRuntime(Path(self.directory.name), old)
        measured = []

        def measure(pid):
            measured.append(pid)
            if pid == os.getpid():
                return MIB
            old.returncode = -15
            runtime.process = SampledProcess(202)
            return None

        # The sampler began before the controller changed phase, and the next
        # child is already present by the failed lookup's return.
        campaign = self.sample_once(
            runtime, measure, "bounded-range-checksum", intentional_stop="normal-restart"
        )
        self.assertEqual(campaign.errors, [])
        self.assertEqual(measured, [os.getpid(), 101])
        self.assertIsNone(campaign.samples[0]["service_pid"])
        self.assertIsNone(campaign.samples[0]["service_rss_bytes"])
        self.assertEqual(campaign.samples[0]["phase"], "normal-restart")

    def test_unexpected_service_exit_is_a_sampling_error(self):
        process = SampledProcess(101)
        process.returncode = 1
        runtime = SampledRuntime(Path(self.directory.name), process)
        campaign = self.sample_once(runtime, lambda pid: MIB)
        self.assertTrue(campaign.errors)
        with self.assertRaisesRegex(AssertionError, "resource sampler errors"):
            campaign.assert_budgets()

    def test_service_pid_and_rss_use_one_captured_process(self):
        runtime = SampledRuntime(Path(self.directory.name), SampledProcess(101))
        runtime.after_pid_observation = lambda: setattr(runtime, "process", SampledProcess(202))
        campaign = self.sample_once(
            runtime, lambda pid: {os.getpid(): MIB, 101: 2 * MIB, 202: 3 * MIB}.get(pid)
        )
        self.assertEqual(campaign.errors, [])
        self.assertEqual(campaign.samples[0]["service_pid"], 101)
        self.assertEqual(campaign.samples[0]["service_rss_bytes"], 2 * MIB)

    def test_intentionally_stopped_process_snapshot_preserves_client_measurement(self):
        runtime = SampledRuntime(Path(self.directory.name), None)
        campaign = self.sample_once(
            runtime, lambda pid: MIB if pid == os.getpid() else None, "interrupted-transport"
        )
        self.assertEqual(campaign.errors, [])
        self.assertEqual(campaign.samples[0]["client_rss_bytes"], MIB)
        self.assertIsNone(campaign.samples[0]["service_pid"])
        self.assertIsNone(campaign.samples[0]["service_rss_bytes"])

    def test_dense_checksum_oracle_matches_all_written_bytes(self):
        expected = generate_payload(self.path, 3 * MIB + 7)
        data = self.path.read_bytes()
        self.assertEqual(len(data), 3 * MIB + 7)
        self.assertEqual(hashlib.sha256(data).hexdigest(), expected)
        self.assertNotEqual(data[:MIB], data[MIB : 2 * MIB])
        self.assertNotEqual(data[MIB : 2 * MIB], data[2 * MIB : 3 * MIB])

    def test_seekable_segment_cannot_leak_adjacent_part_bytes(self):
        generate_payload(self.path, 3 * MIB)
        expected = self.path.read_bytes()[MIB : 2 * MIB]
        with SegmentReader(self.path, MIB, MIB) as reader:
            self.assertEqual(reader.read(), expected)
            self.assertEqual(reader.read(1), b"")
            reader.seek(-5, 2)
            self.assertEqual(reader.read(20), expected[-5:])
            with self.assertRaises(ValueError):
                reader.seek(MIB + 1)

    def test_signed_stream_rewind_then_real_prefix_pause_preserves_bytes(self):
        generate_payload(self.path, 3 * MIB)
        expected = self.path.read_bytes()[MIB : 3 * MIB]
        with SegmentReader(self.path, MIB, 2 * MIB) as reader:
            # SDK signing is allowed to consume the complete segment before arm.
            self.assertEqual(
                hashlib.sha256(reader.read()).digest(),
                hashlib.sha256(expected).digest(),
            )
            reader.arm()
            prefix = reader.read(MIB)
            with concurrent.futures.ThreadPoolExecutor(max_workers=1) as executor:
                future = executor.submit(reader.read, MIB)
                self.assertTrue(reader.paused.wait(2))
                self.assertFalse(future.done())
                self.assertEqual(reader.delivered, MIB)
                reader.release.set()
                suffix = future.result(timeout=2)
            self.assertEqual(prefix + suffix, expected)
            self.assertEqual(reader.read(1), b"")


if __name__ == "__main__":
    unittest.main()
