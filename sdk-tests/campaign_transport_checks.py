"""Pinned SDK transport failures must stay finite in the measured campaign."""

from __future__ import annotations

import tempfile
import threading
import time
import unittest
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import Mock, patch

import oci
import requests

import test_large_upload_qualification as subject
from large_campaign import PART_BYTES


class InterruptedProbeReached(Exception):
    pass


class CompletionHandler(BaseHTTPRequestHandler):
    def do_POST(self):
        self.rfile.read(int(self.headers["Content-Length"]))
        self.server.calls.append(self.path)
        time.sleep(self.server.delay)
        body = (
            b'<CompleteMultipartUploadResult><ETag>"controlled"</ETag></CompleteMultipartUploadResult>'
            if self.server.response_status == 200
            else b'<Error><Code>ServiceUnavailable</Code><Message>controlled failure</Message></Error>'
        )
        self.send_response(self.server.response_status)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        try:
            self.wfile.write(body)
        except BrokenPipeError:
            pass  # The RED control closes after its old five-second deadline.

    def log_message(self, *args):
        pass


class DirectGcsTransport:
    def __init__(self, fail_method=None):
        self.fail_method = fail_method
        self.calls = []

    def request(self, method, url, **kwargs):
        timeout = kwargs["timeout"]
        self.calls.append((method, timeout))
        if method == self.fail_method:
            raise requests.exceptions.ConnectionError("controlled socket.sendall timeout")
        # Model a bulk send requiring twelve seconds without actually sleeping.
        # Requests uses the connect timeout while sending the contiguous body.
        connect_timeout = timeout[0] if isinstance(timeout, tuple) else timeout
        if method == "PUT" and connect_timeout < 12:
            raise requests.exceptions.ConnectionError("bulk send deadline too short")
        if method == "POST":
            return SimpleNamespace(status_code=200, headers=requests.structures.CaseInsensitiveDict(
                {"location": "http://localhost/session"}))
        if method == "PUT":
            if len(kwargs["data"]) != PART_BYTES:
                raise AssertionError("the real SDK must prepare the complete 64 MiB first chunk")
            return SimpleNamespace(status_code=308, headers=requests.structures.CaseInsensitiveDict(
                {"range": f"bytes=0-{PART_BYTES - 1}"}))
        raise AssertionError(f"unexpected transport method: {method}")


class CampaignTransportChecks(unittest.TestCase):
    def setUp(self):
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        self.path = Path(directory.name)
        self.payload = self.path / "sparse-probe.bin"
        with self.payload.open("wb") as stream:
            stream.truncate(2 * PART_BYTES)
        self.settings = SimpleNamespace(
            api_url="http://localhost", require_provider=lambda _: None,
            require_process=lambda: None, bucket_name=lambda _: "transport-probe",
        )

    def test_s3_completion_can_outlast_five_seconds_without_retrying_failures(self):
        import botocore.exceptions

        server = ThreadingHTTPServer(("127.0.0.1", 0), CompletionHandler)
        server.calls, server.delay, server.response_status = [], 6, 200
        worker = threading.Thread(target=server.serve_forever, daemon=True)
        worker.start()
        self.addCleanup(worker.join, 2)
        self.addCleanup(server.server_close)
        self.addCleanup(server.shutdown)
        client = subject._s3_campaign_client(SimpleNamespace(
            api_url=f"http://127.0.0.1:{server.server_port}",
            access_key_id="test-access", secret_access_key="test-secret",
        ))
        self.addCleanup(client.close)
        args = dict(Bucket="bucket", Key="object", UploadId="upload", MultipartUpload={"Parts": [{"PartNumber": 1, "ETag": '"part"'}]})

        response = client.complete_multipart_upload(**args)
        self.assertEqual(response["ResponseMetadata"]["HTTPStatusCode"], 200)
        self.assertEqual(len(server.calls), 1)
        self.assertEqual(client.meta.config.connect_timeout, 5)
        self.assertLessEqual(client.meta.config.read_timeout, 60)
        server.delay, server.response_status = 0, 503
        with self.assertRaises(botocore.exceptions.ClientError):
            client.complete_multipart_upload(**args)
        self.assertEqual(len(server.calls), 2)

    def run_gcs_probe(self, transport):
        # Bypass generation and the already-qualified main upload/readbacks.
        # Keep the campaign's actual ResumableUpload initiation and first chunk.
        campaign = Mock(path=self.payload, size=2 * PART_BYTES)
        campaign.__enter__ = Mock(return_value=campaign)
        campaign.__exit__ = Mock(return_value=False)
        campaign.interrupt.side_effect = InterruptedProbeReached
        blob = Mock(size=2 * PART_BYTES)
        blob.name = "qualification/interrupted.bin"
        bucket = Mock()
        bucket.name = "transport-probe"
        bucket.blob.return_value = blob
        client = Mock()
        client.bucket.return_value = bucket
        client._http = transport
        with patch.object(subject, "Campaign", return_value=campaign), patch.object(
            subject, "gcs_client", return_value=client
        ):
            subject.test_gcs_large_resumable_qualification(
                self.settings, self.path, lambda *args: None
            )

    def test_direct_gcs_first_chunk_propagates_the_bulk_send_timeout(self):
        transport = DirectGcsTransport()
        with patch("google.resumable_media.requests._request_helpers.time.sleep") as sleep:
            sleep.side_effect = AssertionError("a slow bulk send must not be retried")
            with self.assertRaises(InterruptedProbeReached):
                self.run_gcs_probe(transport)
        self.assertEqual(transport.calls, [("POST", 60), ("PUT", 60)])
        sleep.assert_not_called()

    def test_direct_gcs_failure_raises_after_one_attempt_without_retry_sleep(self):
        for method in ("POST", "PUT"):
            with self.subTest(method=method):
                transport = DirectGcsTransport(fail_method=method)
                with patch("google.resumable_media.requests._request_helpers.time.sleep") as sleep:
                    sleep.side_effect = AssertionError("the controlled failure must not retry")
                    with self.assertRaisesRegex(requests.exceptions.ConnectionError, "controlled socket"):
                        self.run_gcs_probe(transport)
                self.assertEqual(sum(m == method for m, _ in transport.calls), 1)
                self.assertEqual(transport.calls[-1][1], 60)
                sleep.assert_not_called()

    def test_oci_control_failures_do_not_retry_including_initial_namespace_lookup(self):
        client = oci.object_storage.ObjectStorageClient(
            {"region": "us-ashburn-1", "user": "ocid1.user.oc1..probe",
             "tenancy": "ocid1.tenancy.oc1..probe",
             "fingerprint": "00:11:22:33:44:55:66:77:88:99:aa:bb:cc:dd:ee:ff",
             "key_file": "unused-by-mocked-signer"},
            signer=Mock(),
        )
        self.addCleanup(client.base_client.session.close)
        failure = requests.exceptions.ConnectionError("controlled OCI transport failure")
        with patch.object(subject, "oci_client", return_value=client), patch.object(
            client.base_client, "call_api", side_effect=failure
        ) as send, patch.object(
            oci.retry.DEFAULT_RETRY_STRATEGY, "make_retrying_call",
            side_effect=AssertionError("the campaign must disable OCI default retries"),
        ):
            with self.assertRaisesRegex(requests.exceptions.ConnectionError, "controlled OCI"):
                subject.test_oci_large_multipart_qualification(
                    self.settings, self.path, lambda *args: None
                )
            self.assertEqual(send.call_count, 1)

            # Exercise the real pinned control methods using the same configured
            # client, with no per-call override to conceal a missing default.
            controls = [
                ("get_namespace", ()),
                ("create_bucket", ("namespace", oci.object_storage.models.CreateBucketDetails(
                    name="bucket", compartment_id="compartment"))),
                ("create_multipart_upload", ("namespace", "bucket", oci.object_storage.models.CreateMultipartUploadDetails(object="object"))),
                ("head_object", ("namespace", "bucket", "object")),
                ("delete_object", ("namespace", "bucket", "object")),
                ("abort_multipart_upload", ("namespace", "bucket", "object", "upload")),
                ("delete_bucket", ("namespace", "bucket")),
            ]
            for name, args in controls:
                with self.subTest(control=name):
                    send.reset_mock()
                    with self.assertRaisesRegex(requests.exceptions.ConnectionError, "controlled OCI"):
                        getattr(client, name)(*args)
                    self.assertEqual(send.call_count, 1)


if __name__ == "__main__":
    unittest.main()
