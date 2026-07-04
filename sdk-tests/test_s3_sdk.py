from __future__ import annotations

import io

import pytest


boto3 = pytest.importorskip("boto3")
botocore_config = pytest.importorskip("botocore.config")


def _client(sqrzl_server):
    return boto3.client(
        "s3",
        endpoint_url=sqrzl_server.api_url,
        aws_access_key_id=sqrzl_server.access_key_id,
        aws_secret_access_key=sqrzl_server.secret_access_key,
        region_name="us-east-1",
        config=botocore_config.Config(
            signature_version="s3v4",
            s3={"addressing_style": "path"},
        ),
    )


def _empty_versioned_bucket(client, bucket: str) -> None:
    versions = client.list_object_versions(Bucket=bucket)
    for version in versions.get("Versions", []):
        client.delete_object(
            Bucket=bucket,
            Key=version["Key"],
            VersionId=version["VersionId"],
        )
    for marker in versions.get("DeleteMarkers", []):
        client.delete_object(
            Bucket=bucket,
            Key=marker["Key"],
            VersionId=marker["VersionId"],
        )


def test_s3_core_bucket_object_and_metadata_workflows(sqrzl_server):
    sqrzl_server.require_provider("s3")
    client = _client(sqrzl_server)
    bucket = sqrzl_server.bucket_name("sdk-s3-core")
    key = "folder/hello.txt"

    client.create_bucket(Bucket=bucket)
    client.put_object(
        Bucket=bucket,
        Key=key,
        Body=b"hello sdk s3",
        ContentType="text/plain",
        Metadata={"owner": "support"},
    )

    head = client.head_object(Bucket=bucket, Key=key)
    assert head["Metadata"]["owner"] == "support"
    assert head["ContentLength"] == len(b"hello sdk s3")

    ranged = client.get_object(Bucket=bucket, Key=key, Range="bytes=6-8")
    assert ranged["Body"].read() == b"sdk"

    listing = client.list_objects_v2(Bucket=bucket, Prefix="folder/")
    assert [item["Key"] for item in listing.get("Contents", [])] == [key]

    client.delete_object(Bucket=bucket, Key=key)
    client.delete_bucket(Bucket=bucket)


def test_s3_multipart_and_versioning_workflows(sqrzl_server):
    sqrzl_server.require_provider("s3")
    client = _client(sqrzl_server)
    bucket = sqrzl_server.bucket_name("sdk-s3-multipart")
    key = "multi.txt"

    client.create_bucket(Bucket=bucket)
    client.put_bucket_versioning(
        Bucket=bucket,
        VersioningConfiguration={"Status": "Enabled"},
    )

    upload = client.create_multipart_upload(Bucket=bucket, Key=key)
    parts = []
    for part_number, payload in enumerate([b"part-one-", b"part-two"], start=1):
        response = client.upload_part(
            Bucket=bucket,
            Key=key,
            UploadId=upload["UploadId"],
            PartNumber=part_number,
            Body=io.BytesIO(payload),
        )
        parts.append({"PartNumber": part_number, "ETag": response["ETag"]})
    client.complete_multipart_upload(
        Bucket=bucket,
        Key=key,
        UploadId=upload["UploadId"],
        MultipartUpload={"Parts": parts},
    )
    assert client.get_object(Bucket=bucket, Key=key)["Body"].read() == b"part-one-part-two"

    client.put_object(Bucket=bucket, Key=key, Body=b"new-version")
    versions = client.list_object_versions(Bucket=bucket, Prefix=key)
    assert len(versions.get("Versions", [])) >= 2

    _empty_versioned_bucket(client, bucket)
    client.delete_bucket(Bucket=bucket)
