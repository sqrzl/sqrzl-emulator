from __future__ import annotations

import base64
import os

import pytest

from test_azure_sdk import _service as azure_service
from test_gcs_sdk import _client as gcs_client
from test_oci_sdk import _client as oci_client
from test_s3_sdk import _client as s3_client


QUALIFICATION_BYTES = int(
    os.getenv("SQRZL_LARGE_UPLOAD_BYTES", str(8 * 1024 * 1024 * 1024))
)
PART_BYTES = 64 * 1024 * 1024

pytestmark = pytest.mark.skipif(
    os.getenv("SQRZL_RUN_LARGE_UPLOAD_QUALIFICATION") != "1",
    reason="set SQRZL_RUN_LARGE_UPLOAD_QUALIFICATION=1 for the disk-backed campaign",
)


@pytest.fixture
def sparse_large_file(tmp_path):
    path = tmp_path / "large-upload.bin"
    with path.open("wb") as payload:
        payload.seek(QUALIFICATION_BYTES - 1)
        payload.write(b"z")
    return path


def test_s3_large_multipart_qualification(sqrzl_server, sparse_large_file):
    boto_transfer = pytest.importorskip("boto3.s3.transfer")
    sqrzl_server.require_provider("s3")
    client = s3_client(sqrzl_server)
    bucket = sqrzl_server.bucket_name("large-s3")
    key = "qualification/large.bin"

    client.create_bucket(Bucket=bucket)
    client.upload_file(
        str(sparse_large_file),
        bucket,
        key,
        Config=boto_transfer.TransferConfig(
            multipart_threshold=PART_BYTES,
            multipart_chunksize=PART_BYTES,
            max_concurrency=1,
            use_threads=False,
        ),
    )

    assert client.head_object(Bucket=bucket, Key=key)["ContentLength"] == QUALIFICATION_BYTES
    client.delete_object(Bucket=bucket, Key=key)
    client.delete_bucket(Bucket=bucket)


def test_azure_large_block_blob_qualification(sqrzl_server, sparse_large_file):
    azure_blob = pytest.importorskip("azure.storage.blob")
    sqrzl_server.require_provider("azure")
    service = azure_service(sqrzl_server)
    container_name = sqrzl_server.bucket_name("large-azure")
    blob = service.create_container(container_name).get_blob_client(
        "qualification/large.bin"
    )
    block_ids = []

    with sparse_large_file.open("rb") as payload:
        for part_number, block in enumerate(iter(lambda: payload.read(PART_BYTES), b"")):
            block_id = base64.b64encode(f"{part_number:08d}".encode()).decode()
            blob.stage_block(block_id=block_id, data=block)
            block_ids.append(block_id)
    blob.commit_block_list(
        [azure_blob.BlobBlock(block_id=block_id) for block_id in block_ids]
    )

    assert blob.get_blob_properties().size == QUALIFICATION_BYTES
    blob.delete_blob()
    service.delete_container(container_name)


def test_gcs_large_resumable_qualification(sqrzl_server, sparse_large_file):
    sqrzl_server.require_provider("gcs")
    client = gcs_client(sqrzl_server)
    bucket = client.bucket(sqrzl_server.bucket_name("large-gcs"))
    bucket.create()
    blob = bucket.blob("qualification/large.bin")
    blob.chunk_size = PART_BYTES

    blob.upload_from_filename(str(sparse_large_file), retry=None)
    blob.reload()

    assert blob.size == QUALIFICATION_BYTES
    blob.delete()
    bucket.delete()


def test_oci_large_multipart_qualification(sqrzl_server, tmp_path, sparse_large_file):
    oci = pytest.importorskip("oci")
    sqrzl_server.require_provider("oci")
    client = oci_client(sqrzl_server, tmp_path)
    namespace = client.get_namespace().data
    bucket_name = sqrzl_server.bucket_name("large-oci")
    object_name = "qualification/large.bin"
    client.create_bucket(
        namespace,
        oci.object_storage.models.CreateBucketDetails(
            name=bucket_name,
            compartment_id="ocid1.compartment.oc1..sqrzl",
        ),
    )

    manager = oci.object_storage.UploadManager(
        client,
        allow_parallel_uploads=False,
    )
    manager.upload_file(
        namespace,
        bucket_name,
        object_name,
        str(sparse_large_file),
        part_size=PART_BYTES,
    )

    assert (
        int(client.head_object(namespace, bucket_name, object_name).headers["content-length"])
        == QUALIFICATION_BYTES
    )
    client.delete_object(namespace, bucket_name, object_name)
    client.delete_bucket(namespace, bucket_name)
