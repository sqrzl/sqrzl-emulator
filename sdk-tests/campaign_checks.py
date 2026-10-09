"""Small independent checks for the resource campaign's checksum/interruption oracle."""

from __future__ import annotations

import concurrent.futures
import hashlib
import io
import json
import os
import subprocess
import sys
import tempfile
import threading
import time
import unittest
from contextlib import redirect_stdout
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


class OwnedTestRuntime:
    """A reaped child plus independent sentinel exercises harness ownership."""

    def __init__(self, directory, storage):
        self.runtime_dir = directory
        self.storage = storage
        self.process = self.spawn()
        self.stop_calls = []
        self.start_calls = 0
        self.env = {}

    @staticmethod
    def spawn():
        return subprocess.Popen([sys.executable, "-c", "import time; time.sleep(60)"])

    @property
    def process_pid(self):
        return (
            self.process.pid if self.process and self.process.poll() is None else None
        )

    def stop(self, kill=False):
        process = self.process
        if process is not None:
            self.stop_calls.append((process.pid, kill))
            process.kill() if kill else process.terminate()
            process.wait(timeout=2)
            self.process = None

    def start(self):
        self.start_calls += 1
        for spool in (self.storage / ".spool").glob(".spool-*.tmp"):
            spool.unlink()
        self.process = self.spawn()
        return self.process.pid


class CampaignChecks(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        self.path = Path(self.directory.name) / "payload"

    def interruption(self, stuck):
        import requests

        root = Path(self.directory.name)
        storage = root / "blobs"
        (storage / ".spool").mkdir(parents=True)
        runtime = OwnedTestRuntime(root, storage)
        self.addCleanup(lambda: runtime.stop(kill=True))
        settings = SimpleNamespace(require_process=lambda: runtime, storage_dir=storage)
        properties = {}
        campaign = Campaign(
            settings, root, lambda key, value: properties.update({key: value}), "s3"
        )
        campaign.thread.start()
        self.addCleanup(lambda: campaign.stop_event.set())
        unrelated = OwnedTestRuntime.spawn()
        self.addCleanup(lambda: unrelated.wait(timeout=2))
        self.addCleanup(unrelated.kill)
        generate_payload(self.path, 2 * MIB)
        release = threading.Event()
        finished = threading.Event()
        timer = threading.Timer(1.5, release.set)
        timer.daemon = True
        timer.start()
        self.addCleanup(timer.cancel)

        def send(request):
            prefix = request.body.read(MIB)
            (
                storage / ".spool/.spool-01234567-0123-0123-0123-0123456789ab.tmp"
            ).write_bytes(prefix)
            request.body.read(MIB)  # This pauses until the controller kills its child.
            if stuck:
                release.wait()
            finished.set()
            raise requests.exceptions.ConnectionError(
                "controlled owned-server interruption"
            )

        owner = SimpleNamespace(send=send)
        request = SimpleNamespace(headers={"Content-Length": str(2 * MIB)}, body=None)
        return (
            campaign,
            runtime,
            unrelated,
            properties,
            release,
            finished,
            owner,
            request,
        )

    def test_stuck_sdk_worker_cannot_turn_future_timeout_into_unbounded_join(self):
        campaign, runtime, unrelated, properties, release, finished, owner, request = (
            self.interruption(True)
        )
        original_pid = runtime.process_pid
        original_result = concurrent.futures.Future.result

        def short_result(future, timeout=None):
            return original_result(
                future, timeout=min(timeout, 0.05) if timeout is not None else None
            )

        with SegmentReader(self.path, 0, 2 * MIB) as reader:
            started = time.monotonic()
            try:
                with (
                    patch(
                        "large_campaign.INTERRUPTION_WORKER_JOIN_SECONDS",
                        0.05,
                        create=True,
                    ),
                    patch.object(concurrent.futures.Future, "result", short_result),
                ):
                    with self.assertRaises(AssertionError) as rejected:
                        campaign.interrupt(owner, lambda: owner.send(request), reader)
                elapsed = time.monotonic() - started
                self.assertLess(
                    elapsed,
                    0.5,
                    "a stuck SDK worker kept the caller waiting after its deadline",
                )
                self.assertIn("worker", str(rejected.exception))
                self.assertTrue(reader.release.is_set())
                self.assertIsNone(runtime.process)
                self.assertEqual(runtime.stop_calls, [(original_pid, True)])
                self.assertEqual(runtime.start_calls, 0)
                self.assertIsNone(
                    unrelated.poll(), "only the owned server may be reaped"
                )
                workers = [
                    thread
                    for thread in threading.enumerate()
                    if thread.name == "qualification-interrupted-s3"
                ]
                self.assertTrue(workers and all(thread.daemon for thread in workers))
                # Even a caller that catches the worker deadline cannot produce
                # completed qualification evidence at normal context teardown.
                with self.assertRaises(AssertionError):
                    campaign.__exit__(None, None, None)
                self.assertFalse(properties["large_upload_campaign"]["completed"])
                self.assertTrue(
                    any(
                        "worker" in error
                        for error in properties["large_upload_campaign"][
                            "sampler_errors"
                        ]
                    )
                )
            finally:
                release.set()
                self.assertTrue(finished.wait(2))

    def test_completed_interruption_worker_is_joined_before_transport_restoration(self):
        # Load the accepted SDK exception types before opening the scheduling
        # window: lazy imports must not hide premature transport restoration.
        for module in (
            "botocore.exceptions",
            "azure.core.exceptions",
            "oci.exceptions",
            "oci._vendor.requests.exceptions",
        ):
            __import__(module)
        campaign, runtime, unrelated, _, release, finished, owner, request = (
            self.interruption(False)
        )
        original_send = owner.send
        published = threading.Event()
        allow_worker_exit = threading.Event()
        observations = []
        original_set_exception = concurrent.futures.Future.set_exception

        def publish_before_worker_exit(future, error):
            original_set_exception(future, error)
            published.set()
            allow_worker_exit.wait(timeout=2)

        with SegmentReader(self.path, 0, 2 * MIB) as reader:

            def observe_pending_worker():
                try:
                    if published.wait(timeout=2):
                        # A result is available but its worker has not exited.
                        # Keep this real scheduling window open for the caller.
                        time.sleep(0.03)
                        observations.append(
                            (owner.send is original_send, reader.stream.closed)
                        )
                finally:
                    allow_worker_exit.set()

            observer = threading.Thread(target=observe_pending_worker, daemon=True)
            observer.start()
            with patch.object(
                concurrent.futures.Future, "set_exception", publish_before_worker_exit
            ):
                campaign.interrupt(owner, lambda: owner.send(request), reader)
            observer.join(timeout=2)
            self.assertEqual(
                observations,
                [(False, False)],
                "transport and reader must remain owned until the completed worker exits",
            )
            self.assertTrue(finished.is_set())
            self.assertFalse(
                any(
                    thread.name == "qualification-interrupted-s3"
                    for thread in threading.enumerate()
                )
            )
            self.assertIs(owner.send, original_send)
            self.assertFalse(reader.stream.closed)
            self.assertEqual(runtime.start_calls, 1)
            self.assertIsNone(unrelated.poll())
        release.set()
        campaign.stop_event.set()
        campaign.thread.join(timeout=2)

    def test_progress_is_flushed_and_retained_independently_of_session_finish(self):
        class FlushedOutput(io.StringIO):
            flushes = 0

            def flush(self):
                self.flushes += 1
                super().flush()

        root = Path(self.directory.name)
        runtime = SampledRuntime(root, SampledProcess(101))
        campaign = Campaign(
            SimpleNamespace(require_process=lambda: runtime),
            root,
            lambda *args: None,
            "gcs",
        )
        output = FlushedOutput()
        with (
            patch.dict(
                os.environ, {"SQRZL_CAMPAIGN_PROGRESS_DIR": str(root / "progress")}
            ),
            redirect_stdout(output),
        ):
            campaign.phase("resumable-upload")
            campaign.checkpoint("resumable-completed", acknowledged_chunks=16)
        self.assertGreaterEqual(output.flushes, 2)
        records = [
            json.loads(line)
            for line in (root / "progress/gcs.jsonl").read_text().splitlines()
        ]
        self.assertEqual(
            [record["name"] for record in records],
            ["resumable-upload", "resumable-completed"],
        )
        self.assertTrue(
            all(
                record["provider"] == "gcs" and record["pid"] == 101
                for record in records
            )
        )
        self.assertEqual(records[1]["acknowledged_chunks"], 16)

    def sample_once(
        self, runtime, measure, phase="multipart-upload", intentional_stop=None
    ):
        settings = SimpleNamespace(require_process=lambda: runtime)
        campaign = Campaign(
            settings, Path(self.directory.name), lambda *args: None, "s3"
        )
        campaign.phase(phase)
        if intentional_stop:
            campaign._intentional_stops[runtime.process.pid] = intentional_stop
        with (
            patch("large_campaign.rss_bytes", side_effect=measure),
            patch.object(
                campaign.stop_event,
                "wait",
                side_effect=lambda delay: campaign.stop_event.set(),
            ),
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
        for phase, intent in (
            ("multipart-upload", None),
            ("normal-restart", "normal-restart"),
        ):
            with self.subTest(phase=phase):
                runtime = SampledRuntime(Path(self.directory.name), SampledProcess(101))
                campaign = self.sample_once(
                    runtime,
                    lambda pid: MIB if pid == os.getpid() else None,
                    phase,
                    intent,
                )
                self.assertTrue(
                    campaign.errors, "missing live service RSS must be a sampler error"
                )
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
            runtime,
            measure,
            "bounded-range-checksum",
            intentional_stop="normal-restart",
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
        runtime.after_pid_observation = lambda: setattr(
            runtime, "process", SampledProcess(202)
        )
        campaign = self.sample_once(
            runtime, lambda pid: {os.getpid(): MIB, 101: 2 * MIB, 202: 3 * MIB}.get(pid)
        )
        self.assertEqual(campaign.errors, [])
        self.assertEqual(campaign.samples[0]["service_pid"], 101)
        self.assertEqual(campaign.samples[0]["service_rss_bytes"], 2 * MIB)

    def test_intentionally_stopped_process_snapshot_preserves_client_measurement(self):
        runtime = SampledRuntime(Path(self.directory.name), None)
        campaign = self.sample_once(
            runtime,
            lambda pid: MIB if pid == os.getpid() else None,
            "interrupted-transport",
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
