"""Bounded resource sampling and controlled transport interruption for opt-in gates."""

from __future__ import annotations

import concurrent.futures
import hashlib
import io
import os
import shutil
import subprocess
import threading
import time
from pathlib import Path
from unittest.mock import patch

MIB = 1024 * 1024
GIB = 1024 * MIB
PART_BYTES = 64 * MIB
RANGE_BYTES = 8 * MIB


def generate_payload(path: Path, size: int) -> str:
    """Dense, index-dependent bytes; bounded generation also calculates the oracle."""
    digest = hashlib.sha256()
    with path.open("wb") as stream:
        for index, offset in enumerate(range(0, size, MIB)):
            seed = hashlib.sha256(f"sqrzl-measured-upload-v1:{index}".encode()).digest()
            block = (seed * (MIB // len(seed)))[: min(MIB, size - offset)]
            stream.write(block)
            digest.update(block)
        stream.flush()
        os.fsync(stream.fileno())
    return digest.hexdigest()


class SegmentReader(io.RawIOBase):
    """Seekable bounded file view; SDK signers may read/rewind it before send."""

    def __init__(self, path: Path, offset: int, length: int):
        self.stream = path.open("rb")
        self.offset = offset
        self.length = length
        self.position = 0
        self.stream.seek(offset)
        self.armed = False
        self.delivered = 0
        self.paused = threading.Event()
        self.release = threading.Event()
        self.pause_at = MIB

    def __len__(self):
        return self.length

    def readable(self):
        return True

    def seekable(self):
        return True

    def tell(self):
        return self.position

    def seek(self, offset, whence=io.SEEK_SET):
        position = (
            offset
            if whence == io.SEEK_SET
            else (
                self.position + offset
                if whence == io.SEEK_CUR
                else self.length + offset
            )
        )
        if not 0 <= position <= self.length:
            raise ValueError("seek outside the bounded upload segment")
        self.stream.seek(self.offset + position)
        self.position = position
        return position

    def arm(self):
        self.seek(0)
        self.armed = True

    def read(self, size=-1):
        if self.armed and self.delivered >= self.pause_at and not self.release.is_set():
            self.paused.set()
            if not self.release.wait(timeout=30):
                raise TimeoutError("interrupted upload gate was not released")
        remaining = self.length - self.position
        count = remaining if size is None or size < 0 else min(size, remaining)
        if self.armed and not self.release.is_set():
            count = min(count, self.pause_at - self.delivered)
        data = self.stream.read(count)
        self.position += len(data)
        if self.armed:
            self.delivered += len(data)
        return data

    def readinto(self, buffer):
        data = self.read(len(buffer))
        buffer[: len(data)] = data
        return len(data)

    def close(self):
        self.release.set()
        self.stream.close()
        super().close()


def rss_bytes(pid: int | None) -> int | None:
    if pid is None:
        return None
    status = Path(f"/proc/{pid}/status")
    if status.exists():
        try:
            for line in status.read_text().splitlines():
                if line.startswith("VmRSS:"):
                    return int(line.split()[1]) * 1024
        except FileNotFoundError:
            return None
    result = subprocess.run(
        ["ps", "-o", "rss=", "-p", str(pid)], capture_output=True, text=True
    )
    return (
        int(result.stdout.strip()) * 1024
        if result.returncode == 0 and result.stdout.strip()
        else None
    )


def disk_bytes(roots: list[Path]) -> int:
    total = 0
    for root in roots:
        for parent, _, names in os.walk(root):
            for name in names:
                try:
                    total += (Path(parent) / name).stat().st_size
                except FileNotFoundError:
                    pass
    return total


class Campaign:
    def __init__(self, settings, tmp_path: Path, record_property, provider: str):
        self.settings = settings
        self.runtime = settings.require_process()
        self.tmp_path = tmp_path
        self.record_property = record_property
        self.provider = provider
        self.size = int(os.getenv("SQRZL_LARGE_UPLOAD_BYTES", str(GIB)))
        if self.size < 4 * PART_BYTES or self.size % PART_BYTES:
            raise ValueError(
                "campaign payload must be at least 256 MiB and a multiple of 64 MiB"
            )
        self.client_budget = int(
            os.getenv("SQRZL_LARGE_CLIENT_RSS_BYTES", str(512 * MIB))
        )
        self.service_budget = int(
            os.getenv("SQRZL_LARGE_SERVICE_RSS_BYTES", str(512 * MIB))
        )
        self.disk_budget = int(os.getenv("SQRZL_LARGE_DISK_BYTES", str(5 * GIB)))
        self.interval = float(os.getenv("SQRZL_LARGE_SAMPLE_SECONDS", "0.05"))
        if not 0.01 <= self.interval <= 0.1:
            raise ValueError(
                "RSS sampling interval must be between 10 and 100 milliseconds"
            )
        self.path = tmp_path / "dense-payload.bin"
        self.roots = [tmp_path, self.runtime.runtime_dir]
        self.samples = []
        self.phases = []
        self.errors = []
        self.stop_event = threading.Event()
        self.started = time.monotonic()
        self._phase = "generation"
        self.thread = threading.Thread(
            target=self._sample, name="qualification-resource-sampler", daemon=True
        )
        self.expected_sha256 = None

    def __enter__(self):
        assert (
            self.settings.enforce_auth
        ), "measured campaign requires configured storage credentials"
        assert (
            shutil.disk_usage(self.tmp_path).free >= self.disk_budget + self.size
        ), "insufficient free disk for the explicit campaign budget"
        self.thread.start()
        try:
            self.expected_sha256 = generate_payload(self.path, self.size)
            self.checkpoint("payload-generated", payload_sha256=self.expected_sha256)
        except Exception as error:
            self.__exit__(type(error), error, error.__traceback__)
            raise
        return self

    def _sample(self):
        last_disk = -1.0
        owned_disk = 0
        while not self.stop_event.is_set():
            try:
                elapsed = time.monotonic() - self.started
                if elapsed - last_disk >= 0.25:
                    owned_disk = disk_bytes(self.roots)
                    last_disk = elapsed
                self.samples.append(
                    {
                        "elapsed_seconds": round(elapsed, 3),
                        "phase": self._phase,
                        "client_rss_bytes": rss_bytes(os.getpid()),
                        "service_pid": self.runtime.process_pid,
                        "service_rss_bytes": rss_bytes(self.runtime.process_pid),
                        "owned_disk_bytes": owned_disk,
                    }
                )
            except Exception as error:
                self.errors.append(repr(error))
            self.stop_event.wait(self.interval)

    def phase(self, name: str):
        self._phase = name

    def checkpoint(self, name: str, **details):
        self.phases.append(
            {
                "name": name,
                "elapsed_seconds": round(time.monotonic() - self.started, 3),
                "pid": self.runtime.process_pid,
                **details,
            }
        )
        self.assert_budgets()

    def assert_budgets(self):
        assert not self.errors, f"resource sampler errors: {self.errors}"
        if self.samples:
            assert (
                max(s["client_rss_bytes"] or 0 for s in self.samples)
                <= self.client_budget
            ), "sampled client RSS budget exceeded"
            assert (
                max(s["service_rss_bytes"] or 0 for s in self.samples)
                <= self.service_budget
            ), "sampled service RSS budget exceeded"
            assert (
                max(s["owned_disk_bytes"] for s in self.samples) <= self.disk_budget
            ), "sampled owned logical disk budget exceeded"

    def readback(self, get_range):
        self.phase("bounded-range-checksum")
        digest = hashlib.sha256()
        for offset in range(0, self.size, RANGE_BYTES):
            length = min(RANGE_BYTES, self.size - offset)
            body = get_range(offset, length)
            assert (
                len(body) == length
            ), f"range {offset}:{length} returned {len(body)} bytes"
            digest.update(body)
        actual = digest.hexdigest()
        assert (
            actual == self.expected_sha256
        ), "full bounded-range SHA256 readback differs"
        self.checkpoint(
            "full-range-readback",
            readback_bytes=self.size,
            range_bytes=RANGE_BYTES,
            sha256=actual,
        )

    def restart(self):
        self.phase("normal-restart")
        old = self.runtime.process_pid
        new = self.settings.restart(kill=False)
        assert new != old
        self.checkpoint("normal-process-restart", previous_pid=old, new_pid=new)

    def interrupt(self, send_owner, upload, reader: SegmentReader):
        """Pause actual HTTP delivery after SDK preparation/signing, then SIGKILL."""
        self.phase("interrupted-transport")
        original_send = send_owner.send
        armed = threading.Event()

        def gated_send(request, *args, **kwargs):
            # Signing/checksum preparation already ran over identical source bytes.
            content_length = request.headers.get(
                "Content-Length", request.headers.get("content-length")
            )
            if isinstance(content_length, bytes):
                content_length = content_length.decode("ascii")
            assert str(content_length) == str(reader.length)
            encoding = request.headers.get(
                "Content-Encoding", request.headers.get("content-encoding", "")
            )
            assert "aws-chunked" not in str(
                encoding
            ), "campaign uses unencoded signed bounded parts"
            request.body = reader
            reader.arm()
            armed.set()
            return original_send(request, *args, **kwargs)

        with concurrent.futures.ThreadPoolExecutor(max_workers=1) as executor:
            with patch.object(send_owner, "send", side_effect=gated_send):
                future = executor.submit(upload)
                try:
                    if not armed.wait(15):
                        if future.done():
                            future.result()
                        raise AssertionError(
                            "SDK did not reach the instrumented native HTTP transport"
                        )
                    assert reader.paused.wait(
                        15
                    ), "HTTP stream did not pause after the selected prefix"
                    deadline = time.monotonic() + 5
                    partial = []
                    while time.monotonic() < deadline:
                        partial = [
                            (p.name, p.stat().st_size)
                            for p in self.settings.storage_dir.glob(
                                ".spool/.spool-*.tmp"
                            )
                            if p.exists() and p.stat().st_size >= reader.pause_at // 2
                        ]
                        if partial:
                            break
                        time.sleep(0.02)
                    assert partial and all(
                        0 < size < reader.length for _, size in partial
                    ), "no partial request spool observed before interruption"
                    old = self.runtime.process_pid
                    self.runtime.stop(kill=True)
                    self.checkpoint(
                        "abrupt-process-stop-during-transfer",
                        previous_pid=old,
                        transport_prefix_bytes=reader.delivered,
                        partial_spool_files=partial,
                    )
                finally:
                    reader.release.set()
                try:
                    future.result(timeout=15)
                except Exception as error:
                    from botocore.exceptions import HTTPClientError
                    from requests.exceptions import RequestException
                    from azure.core.exceptions import (
                        ServiceRequestError,
                        ServiceResponseError,
                    )
                    from oci.exceptions import (
                        RequestException as OciSdkRequestException,
                    )
                    from oci._vendor.requests.exceptions import (
                        RequestException as OciRequestException,
                    )

                    assert isinstance(
                        error,
                        (
                            HTTPClientError,
                            RequestException,
                            OciRequestException,
                            OciSdkRequestException,
                            ServiceRequestError,
                            ServiceResponseError,
                        ),
                    ), f"unexpected non-transport interruption error: {error!r}"
                    self.checkpoint(
                        "interrupted-client-error",
                        exception_type=f"{type(error).__module__}.{type(error).__name__}",
                        detail=str(error)[:1000],
                    )
                else:
                    raise AssertionError(
                        "interrupted incomplete HTTP upload unexpectedly succeeded"
                    )
        new = self.runtime.start()
        assert new != old
        assert not list(
            self.settings.storage_dir.glob(".spool/.spool-*.tmp")
        ), "orphan request spool survived process restart"
        self.checkpoint(
            "abrupt-process-restart",
            previous_pid=old,
            new_pid=new,
            request_spool_cleanup=True,
        )

    def assert_cleanup(self):
        storage = self.settings.storage_dir
        leftover = []
        for root in [
            storage / ".provider-uploads",
            storage / ".provider-state",
            storage / ".spool",
        ]:
            leftover.extend(
                str(p.relative_to(storage)) for p in root.rglob("*") if p.is_file()
            )
        for root in storage.rglob(".multipart"):
            leftover.extend(
                str(p.relative_to(storage)) for p in root.rglob("*") if p.is_file()
            )
        assert (
            not leftover
        ), f"upload/session/spool files remain after abort and cleanup: {leftover}"
        self.checkpoint("abort-and-staging-cleanup", leftover_files=leftover)

    def __exit__(self, exc_type, exc, traceback):
        self.stop_event.set()
        self.thread.join(timeout=5)
        validation_error = None
        if exc_type is None:
            try:
                assert not self.thread.is_alive(), "resource sampler did not stop"
                assert any(
                    s["service_rss_bytes"] for s in self.samples
                ), "no service RSS evidence"
                self.assert_budgets()
            except Exception as error:
                validation_error = error
        evidence = {
            "schema_version": 1,
            "provider": self.provider,
            "recovery_inspection_mode": (
                "owned-filesystem-records"
                if self.provider == "oci"
                else "native-session-or-parts-query"
            ),
            "auth_mode": {
                "s3": "native-sigv4",
                "azure": "native-shared-key",
                "gcs": "local-bearer-convenience",
                "oci": "native-rsa",
            }[self.provider],
            "configured_request_cap_bytes": int(
                self.runtime.env.get("SQRZL_MAX_REQUEST_BYTES", str(128 * MIB))
            ),
            "campaign_kind": (
                "selected-1GiB-resource-qualification"
                if self.size == GIB
                else "smaller-payload-preflight"
            ),
            "payload_bytes": self.size,
            "payload_sha256": self.expected_sha256,
            "payload_generator": "dense SHA256(index) repeated in unique 1MiB blocks v1",
            "part_bytes": PART_BYTES,
            "range_bytes": RANGE_BYTES,
            "client_rss_budget_bytes": self.client_budget,
            "service_rss_budget_bytes": self.service_budget,
            "owned_disk_budget_bytes": self.disk_budget,
            "rss_sample_interval_seconds": self.interval,
            "disk_sample_interval_seconds": 0.25,
            "measurement_kind": "sampled RSS and owned logical file sizes; not OS hard limits",
            "sample_count": len(self.samples),
            "client_rss_peak_bytes": max(
                (s["client_rss_bytes"] or 0 for s in self.samples), default=0
            ),
            "service_rss_peak_bytes": max(
                (s["service_rss_bytes"] or 0 for s in self.samples), default=0
            ),
            "owned_disk_peak_bytes": max(
                (s["owned_disk_bytes"] for s in self.samples), default=0
            ),
            "phases": self.phases,
            "samples": self.samples,
            "sampler_errors": self.errors,
            "elapsed_seconds": round(time.monotonic() - self.started, 3),
            "completed": exc_type is None and validation_error is None,
        }
        self.record_property("large_upload_campaign", evidence)
        if validation_error is not None:
            raise validation_error
        return False
