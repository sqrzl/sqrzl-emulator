from __future__ import annotations

import io

import pytest


google_auth = pytest.importorskip("google.auth.credentials")
gcs_storage = pytest.importorskip("google.cloud.storage")


def _client(sqrzl_server):
    credentials = google_auth.AnonymousCredentials()
    if sqrzl_server.enforce_auth:
        google_oauth2 = pytest.importorskip("google.oauth2.credentials")
        credentials = google_oauth2.Credentials(token=sqrzl_server.gcs_hmac_secret)
    return gcs_storage.Client(
        project="sqrzl",
        credentials=credentials,
        client_options={"api_endpoint": sqrzl_server.api_url},
    )


def test_gcs_core_json_workflows(sqrzl_server):
    sqrzl_server.require_provider("gcs")
    client = _client(sqrzl_server)
    bucket_name = sqrzl_server.bucket_name("sdk-gcs-core")
    blob_name = "folder/hello.txt"

    bucket = client.bucket(bucket_name)
    bucket.create()
    blob = bucket.blob(blob_name)
    blob.metadata = {"owner": "support"}
    blob.upload_from_string("hello gcs sdk", content_type="text/plain")

    blob.reload()
    assert blob.metadata["owner"] == "support"
    assert blob.size == len(b"hello gcs sdk")

    assert blob.download_as_bytes(start=6, end=8) == b"gcs"
    assert [item.name for item in client.list_blobs(bucket, prefix="folder/")] == [blob_name]

    blob.delete()
    bucket.delete()


def test_gcs_resumable_upload_workflow(sqrzl_server):
    sqrzl_server.require_provider("gcs")
    client = _client(sqrzl_server)
    bucket_name = sqrzl_server.bucket_name("sdk-gcs-resumable")
    blob_name = "large/resumable.txt"

    bucket = client.bucket(bucket_name)
    bucket.create()
    blob = bucket.blob(blob_name)
    payload = b"g" * (9 * 1024 * 1024 + 17)
    blob.chunk_size = 256 * 1024
    blob.upload_from_file(
        io.BytesIO(payload),
        rewind=True,
        size=len(payload),
        content_type="text/plain",
    )

    assert blob.download_as_bytes() == payload
    blob.delete()
    bucket.delete()
