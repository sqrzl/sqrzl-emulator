from __future__ import annotations

import base64
import hashlib
import json
import os

import pytest

from large_campaign import Campaign, PART_BYTES, SegmentReader
from test_gcs_sdk import _client as gcs_client
from test_oci_sdk import _client as oci_client

pytestmark = pytest.mark.skipif(
    os.getenv("SQRZL_RUN_LARGE_UPLOAD_QUALIFICATION") != "1",
    reason="opt-in measured campaign; set SQRZL_RUN_LARGE_UPLOAD_QUALIFICATION=1",
)


def test_s3_large_multipart_qualification(sqrzl_server, tmp_path, record_property):
    boto3 = pytest.importorskip("boto3")
    config = pytest.importorskip("botocore.config")
    errors = pytest.importorskip("botocore.exceptions")
    sqrzl_server.require_provider("s3")
    client = boto3.client(
        "s3",
        endpoint_url=sqrzl_server.api_url,
        aws_access_key_id=sqrzl_server.access_key_id,
        aws_secret_access_key=sqrzl_server.secret_access_key,
        region_name="us-east-1",
        config=config.Config(
            signature_version="s3v4",
            s3={"addressing_style": "path", "payload_signing_enabled": True},
            retries={"max_attempts": 0},
            request_checksum_calculation="when_required",
            response_checksum_validation="when_required",
            connect_timeout=5,
            read_timeout=5,
        ),
    )
    bucket = sqrzl_server.bucket_name("measured-s3")
    key = "qualification/dense.bin"
    with Campaign(sqrzl_server, tmp_path, record_property, "s3") as campaign:
        client.create_bucket(Bucket=bucket)
        upload_id = client.create_multipart_upload(Bucket=bucket, Key=key)["UploadId"]
        parts = []
        campaign.phase("multipart-upload")
        for index, offset in enumerate(range(0, campaign.size, PART_BYTES), 1):
            with SegmentReader(campaign.path, offset, PART_BYTES) as body:
                part = client.upload_part(
                    Bucket=bucket,
                    Key=key,
                    UploadId=upload_id,
                    PartNumber=index,
                    Body=body,
                    ContentLength=PART_BYTES,
                )
            parts.append({"PartNumber": index, "ETag": part["ETag"]})
        client.complete_multipart_upload(
            Bucket=bucket, Key=key, UploadId=upload_id, MultipartUpload={"Parts": parts}
        )
        assert (
            client.head_object(Bucket=bucket, Key=key)["ContentLength"] == campaign.size
        )
        campaign.checkpoint("multipart-completed", acknowledged_parts=len(parts))

        def ranged(offset, length):
            response = client.get_object(
                Bucket=bucket, Key=key, Range=f"bytes={offset}-{offset + length - 1}"
            )
            try:
                return response["Body"].read(length + 1)
            finally:
                response["Body"].close()

        campaign.readback(ranged)
        campaign.restart()
        assert (
            client.head_object(Bucket=bucket, Key=key)["ContentLength"] == campaign.size
        )
        campaign.readback(ranged)
        client.delete_object(Bucket=bucket, Key=key)
        interrupted_key = "qualification/interrupted.bin"
        upload_id = client.create_multipart_upload(Bucket=bucket, Key=interrupted_key)[
            "UploadId"
        ]
        with SegmentReader(campaign.path, 0, PART_BYTES) as body:
            first = client.upload_part(
                Bucket=bucket,
                Key=interrupted_key,
                UploadId=upload_id,
                PartNumber=1,
                Body=body,
                ContentLength=PART_BYTES,
            )
        with SegmentReader(campaign.path, PART_BYTES, PART_BYTES) as body:
            campaign.interrupt(
                client._endpoint.http_session,
                lambda: client.upload_part(
                    Bucket=bucket,
                    Key=interrupted_key,
                    UploadId=upload_id,
                    PartNumber=2,
                    Body=body,
                    ContentLength=PART_BYTES,
                ),
                body,
            )
        recovered = client.list_parts(
            Bucket=bucket, Key=interrupted_key, UploadId=upload_id
        )["Parts"]
        assert [(p["PartNumber"], p["Size"], p["ETag"]) for p in recovered] == [
            (1, PART_BYTES, first["ETag"])
        ]
        with pytest.raises(errors.ClientError) as missing:
            client.head_object(Bucket=bucket, Key=interrupted_key)
        assert missing.value.response["ResponseMetadata"]["HTTPStatusCode"] == 404
        campaign.checkpoint(
            "acknowledged-part-recovered",
            part_number=1,
            part_bytes=PART_BYTES,
            partial_part_absent=True,
            incomplete_object_absent=True,
        )
        client.abort_multipart_upload(
            Bucket=bucket, Key=interrupted_key, UploadId=upload_id
        )
        assert not client.list_multipart_uploads(Bucket=bucket).get("Uploads", [])
        campaign.assert_cleanup()
        client.delete_bucket(Bucket=bucket)
    client.close()


def test_azure_large_block_blob_qualification(sqrzl_server, tmp_path, record_property):
    azure_blob = pytest.importorskip("azure.storage.blob")
    transport_module = pytest.importorskip("azure.core.pipeline.transport")
    exceptions = pytest.importorskip("azure.core.exceptions")
    sqrzl_server.require_provider("azure")
    transport = transport_module.RequestsTransport()
    transport.open()
    service = azure_blob.BlobServiceClient(
        account_url=f"{sqrzl_server.api_url}/{sqrzl_server.azure_account}",
        credential=sqrzl_server.azure_account_key,
        transport=transport,
        retry_total=0,
        connection_timeout=5,
        read_timeout=5,
    )
    container_name = sqrzl_server.bucket_name("measured-azure")
    with Campaign(sqrzl_server, tmp_path, record_property, "azure") as campaign:
        container = service.create_container(container_name)
        blob = container.get_blob_client("qualification/dense.bin")
        block_ids = []
        campaign.phase("block-upload")
        for index, offset in enumerate(range(0, campaign.size, PART_BYTES)):
            block_id = base64.b64encode(f"{index:08d}".encode()).decode()
            with SegmentReader(campaign.path, offset, PART_BYTES) as body:
                blob.stage_block(block_id=block_id, data=body, length=PART_BYTES)
            block_ids.append(block_id)
        blob.commit_block_list(
            [azure_blob.BlobBlock(block_id=block_id) for block_id in block_ids]
        )
        assert blob.get_blob_properties().size == campaign.size
        campaign.checkpoint("block-list-committed", acknowledged_blocks=len(block_ids))
        campaign.readback(
            lambda offset, length: blob.download_blob(
                offset=offset, length=length, max_concurrency=1
            ).readall()
        )
        campaign.restart()
        assert blob.get_blob_properties().size == campaign.size
        campaign.readback(
            lambda offset, length: blob.download_blob(
                offset=offset, length=length, max_concurrency=1
            ).readall()
        )
        blob.delete_blob()
        interrupted = container.get_blob_client("qualification/interrupted.bin")
        first_id = base64.b64encode(b"00000000").decode()
        second_id = base64.b64encode(b"00000001").decode()
        with SegmentReader(campaign.path, 0, PART_BYTES) as body:
            interrupted.stage_block(block_id=first_id, data=body, length=PART_BYTES)
        with SegmentReader(campaign.path, PART_BYTES, PART_BYTES) as body:
            campaign.interrupt(
                transport.session,
                lambda: interrupted.stage_block(
                    block_id=second_id, data=body, length=PART_BYTES
                ),
                body,
            )
        blocks = interrupted.get_block_list(block_list_type="all")
        committed, uncommitted = (
            blocks
            if isinstance(blocks, tuple)
            else (blocks.committed_blocks, blocks.uncommitted_blocks)
        )
        assert not committed
        assert [(b.id, b.size) for b in uncommitted] == [(first_id, PART_BYTES)]
        with pytest.raises(exceptions.ResourceNotFoundError) as missing:
            interrupted.get_blob_properties()
        assert missing.value.status_code == 404
        campaign.checkpoint(
            "acknowledged-block-recovered",
            block_bytes=PART_BYTES,
            partial_block_absent=True,
            incomplete_blob_absent=True,
        )
        # Native Put Blob garbage-collects the uncommitted blocks.
        interrupted.upload_blob(b"", overwrite=True)
        assert interrupted.get_blob_properties().size == 0
        assert interrupted.get_block_list(block_list_type="all") == ([], [])
        interrupted.delete_blob()
        assert list(container.list_blobs()) == []
        campaign.assert_cleanup()
        service.delete_container(container_name)
    service.close()


def test_gcs_large_resumable_qualification(sqrzl_server, tmp_path, record_property):
    media = pytest.importorskip("google.resumable_media.requests")
    exceptions = pytest.importorskip("google.api_core.exceptions")
    sqrzl_server.require_provider("gcs")
    client = gcs_client(sqrzl_server)
    with Campaign(sqrzl_server, tmp_path, record_property, "gcs") as campaign:
        record_property(
            "campaign_auth_mode",
            "local-bearer-convenience; native V2 HMAC is a separate SDK gate",
        )
        bucket = client.bucket(sqrzl_server.bucket_name("measured-gcs"))
        bucket.create()
        blob = bucket.blob("qualification/dense.bin")
        blob.chunk_size = PART_BYTES
        campaign.phase("resumable-upload")
        blob.upload_from_filename(str(campaign.path), retry=None, timeout=60)
        blob.reload(retry=None)
        assert blob.size == campaign.size
        campaign.checkpoint(
            "resumable-completed", acknowledged_chunks=campaign.size // PART_BYTES
        )
        campaign.readback(
            lambda offset, length: blob.download_as_bytes(
                start=offset, end=offset + length - 1, retry=None, timeout=60
            )
        )
        campaign.restart()
        blob.reload(retry=None)
        assert blob.size == campaign.size
        campaign.readback(
            lambda offset, length: blob.download_as_bytes(
                start=offset, end=offset + length - 1, retry=None, timeout=60
            )
        )
        blob.delete(retry=None)
        interrupted = bucket.blob("qualification/interrupted.bin")
        upload = media.ResumableUpload(
            f"{sqrzl_server.api_url}/upload/storage/v1/b/{bucket.name}/o?uploadType=resumable",
            PART_BYTES,
        )
        with campaign.path.open("rb") as stream:
            upload.initiate(
                client._http,
                stream,
                {"name": interrupted.name},
                "application/octet-stream",
                total_bytes=campaign.size,
                timeout=(5, 5),
            )
            first = upload.transmit_next_chunk(client._http, timeout=(5, 5))
            assert (
                first.status_code == 308
                and first.headers["Range"] == f"bytes=0-{PART_BYTES - 1}"
            )
        session_uri = upload.resumable_url
        with SegmentReader(campaign.path, PART_BYTES, PART_BYTES) as body:
            campaign.interrupt(
                client._http,
                lambda: client._http.put(
                    session_uri,
                    data=body,
                    headers={
                        "Content-Length": str(PART_BYTES),
                        "Content-Type": "application/octet-stream",
                        "Content-Range": f"bytes {PART_BYTES}-{2 * PART_BYTES - 1}/{campaign.size}",
                    },
                    timeout=(5, 5),
                ),
                body,
            )
        status = client._http.put(
            session_uri,
            data=b"",
            headers={
                "Content-Length": "0",
                "Content-Range": f"bytes */{campaign.size}",
            },
            timeout=(5, 5),
        )
        assert (
            status.status_code == 308
            and status.headers["Range"] == f"bytes=0-{PART_BYTES - 1}"
        )
        with pytest.raises(exceptions.NotFound):
            interrupted.reload(retry=None)
        campaign.checkpoint(
            "acknowledged-chunk-recovered",
            chunk_bytes=PART_BYTES,
            partial_chunk_absent=True,
            incomplete_object_absent=True,
        )
        cancelled = client._http.delete(
            session_uri, headers={"Content-Length": "0"}, timeout=(5, 5)
        )
        assert cancelled.status_code == 499
        assert (
            client._http.put(
                session_uri,
                data=b"",
                headers={
                    "Content-Length": "0",
                    "Content-Range": f"bytes */{campaign.size}",
                },
                timeout=(5, 5),
            ).status_code
            == 404
        )
        campaign.assert_cleanup()
        bucket.delete(retry=None)
    client.close()


def test_oci_large_multipart_qualification(sqrzl_server, tmp_path, record_property):
    oci = pytest.importorskip("oci")
    sqrzl_server.require_provider("oci")
    client = oci_client(sqrzl_server, tmp_path)
    client.base_client.timeout = (5, 60)
    namespace = client.get_namespace().data
    bucket_name = sqrzl_server.bucket_name("measured-oci")
    object_name = "qualification/dense.bin"
    no_retry = oci.retry.NoneRetryStrategy()
    with Campaign(sqrzl_server, tmp_path, record_property, "oci") as campaign:
        client.create_bucket(
            namespace,
            oci.object_storage.models.CreateBucketDetails(
                name=bucket_name, compartment_id="ocid1.compartment.oc1..sqrzl"
            ),
        )
        upload = client.create_multipart_upload(
            namespace,
            bucket_name,
            oci.object_storage.models.CreateMultipartUploadDetails(
                object=object_name, content_type="application/octet-stream"
            ),
        ).data
        parts = []
        campaign.phase("multipart-upload")
        for index, offset in enumerate(range(0, campaign.size, PART_BYTES), 1):
            with SegmentReader(campaign.path, offset, PART_BYTES) as body:
                response = client.upload_part(
                    namespace,
                    bucket_name,
                    object_name,
                    upload.upload_id,
                    index,
                    body,
                    content_length=PART_BYTES,
                    retry_strategy=no_retry,
                )
            parts.append(
                oci.object_storage.models.CommitMultipartUploadPartDetails(
                    part_num=index, etag=response.headers["etag"]
                )
            )
        client.commit_multipart_upload(
            namespace,
            bucket_name,
            object_name,
            upload.upload_id,
            oci.object_storage.models.CommitMultipartUploadDetails(
                parts_to_commit=parts
            ),
            retry_strategy=no_retry,
        )
        assert (
            int(
                client.head_object(namespace, bucket_name, object_name).headers[
                    "content-length"
                ]
            )
            == campaign.size
        )
        campaign.checkpoint("multipart-completed", acknowledged_parts=len(parts))

        def ranged(offset, length):
            response = client.get_object(
                namespace,
                bucket_name,
                object_name,
                range=f"bytes={offset}-{offset + length - 1}",
                retry_strategy=no_retry,
            )
            try:
                return response.data.content
            finally:
                response.data.close()

        campaign.readback(ranged)
        campaign.restart()
        assert (
            int(
                client.head_object(namespace, bucket_name, object_name).headers[
                    "content-length"
                ]
            )
            == campaign.size
        )
        campaign.readback(ranged)
        client.delete_object(namespace, bucket_name, object_name)
        interrupted_name = "qualification/interrupted.bin"
        upload = client.create_multipart_upload(
            namespace,
            bucket_name,
            oci.object_storage.models.CreateMultipartUploadDetails(
                object=interrupted_name
            ),
        ).data
        with SegmentReader(campaign.path, 0, PART_BYTES) as body:
            first = client.upload_part(
                namespace,
                bucket_name,
                interrupted_name,
                upload.upload_id,
                1,
                body,
                content_length=PART_BYTES,
                retry_strategy=no_retry,
            )
        with SegmentReader(campaign.path, PART_BYTES, PART_BYTES) as body:
            campaign.interrupt(
                client.base_client.session,
                lambda: client.upload_part(
                    namespace,
                    bucket_name,
                    interrupted_name,
                    upload.upload_id,
                    2,
                    body,
                    content_length=PART_BYTES,
                    retry_strategy=no_retry,
                ),
                body,
            )
        # OCI native list-parts is outside the implemented operation scope.
        record_paths = list(
            sqrzl_server.storage_dir.glob(
                f"*/.multipart/{upload.upload_id}/upload.json"
            )
        )
        assert (
            len(record_paths) == 1
        ), "acknowledged OCI multipart record did not survive restart"
        upload_dir = record_paths[0].parent
        recovered = json.loads((upload_dir / "upload.json").read_text())
        assert (
            recovered["upload_id"] == upload.upload_id
            and recovered["key"] == interrupted_name
        )
        assert [
            (p["part_number"], p["size"], p["etag"]) for p in recovered["parts"]
        ] == [(1, PART_BYTES, first.headers["etag"])]
        assert sorted(p.name for p in upload_dir.glob("part-*")) == ["part-00001"]
        persisted_part = upload_dir / "part-00001"
        assert persisted_part.stat().st_size == PART_BYTES
        with persisted_part.open("rb") as stream:
            recovered_sha256 = hashlib.file_digest(stream, "sha256").hexdigest()
        with SegmentReader(campaign.path, 0, PART_BYTES) as stream:
            source_part_sha256 = hashlib.file_digest(stream, "sha256").hexdigest()
        assert recovered_sha256 == source_part_sha256
        with pytest.raises(oci.exceptions.ServiceError) as missing:
            client.head_object(namespace, bucket_name, interrupted_name)
        assert missing.value.status == 404  # HEAD has no JSON error body.
        campaign.checkpoint(
            "acknowledged-part-recovered",
            part_bytes=PART_BYTES,
            partial_part_absent=True,
            incomplete_object_absent=True,
            inspection_mode="owned-filesystem-records",
            source_part_sha256=source_part_sha256,
            recovered_part_sha256=recovered_sha256,
            record_path=str(
                (upload_dir / "upload.json").relative_to(sqrzl_server.storage_dir)
            ),
        )
        client.abort_multipart_upload(
            namespace, bucket_name, interrupted_name, upload.upload_id
        )
        assert not upload_dir.exists()
        campaign.assert_cleanup()
        client.delete_bucket(namespace, bucket_name)
    client.base_client.session.close()
