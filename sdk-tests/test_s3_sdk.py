from __future__ import annotations

import io
import urllib.error
import urllib.request

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

    alternate_key = "folder-archive.txt"
    client.put_object(Bucket=bucket, Key=alternate_key, Body=b"archive")
    partial_prefix_listing = client.list_objects_v2(
        Bucket=bucket,
        Prefix="fol",
        Delimiter="/",
    )
    assert [entry["Prefix"] for entry in partial_prefix_listing["CommonPrefixes"]] == [
        "folder/"
    ]
    generic_delimiter_listing = client.list_objects_v2(
        Bucket=bucket,
        Prefix="folder",
        Delimiter="-",
    )
    assert [entry["Prefix"] for entry in generic_delimiter_listing["CommonPrefixes"]] == [
        "folder-"
    ]

    client.delete_object(Bucket=bucket, Key=key)
    client.delete_object(Bucket=bucket, Key=alternate_key)
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

    upload = client.create_multipart_upload(
        Bucket=bucket,
        Key=key,
        ContentType="application/x-sqrzl-multipart",
        Metadata={"owner": "multipart-sdk"},
        Tagging="project=large-documents",
        StorageClass="STANDARD_IA",
    )
    spare_upload = client.create_multipart_upload(Bucket=bucket, Key="spare.txt")
    parts = []
    first_part = b"a" * (5 * 1024 * 1024)
    final_part = b"part-two"
    for part_number, payload in enumerate([first_part, final_part], start=1):
        response = client.upload_part(
            Bucket=bucket,
            Key=key,
            UploadId=upload["UploadId"],
            PartNumber=part_number,
            Body=io.BytesIO(payload),
        )
        parts.append({"PartNumber": part_number, "ETag": response["ETag"]})

    first_parts_page = client.list_parts(
        Bucket=bucket,
        Key=key,
        UploadId=upload["UploadId"],
        MaxParts=1,
    )
    assert first_parts_page["StorageClass"] == "STANDARD_IA"
    assert first_parts_page["IsTruncated"] is True
    assert [part["PartNumber"] for part in first_parts_page["Parts"]] == [1]
    second_parts_page = client.list_parts(
        Bucket=bucket,
        Key=key,
        UploadId=upload["UploadId"],
        PartNumberMarker=first_parts_page["NextPartNumberMarker"],
        MaxParts=1,
    )
    assert second_parts_page["IsTruncated"] is False
    assert [part["PartNumber"] for part in second_parts_page["Parts"]] == [2]

    uploads = [
        listed
        for page in client.get_paginator("list_multipart_uploads").paginate(
            Bucket=bucket,
            PaginationConfig={"PageSize": 1},
        )
        for listed in page.get("Uploads", [])
    ]
    assert {(listed["Key"], listed["UploadId"]) for listed in uploads} == {
        (key, upload["UploadId"]),
        ("spare.txt", spare_upload["UploadId"]),
    }
    assert next(
        listed["StorageClass"]
        for listed in uploads
        if listed["UploadId"] == upload["UploadId"]
    ) == "STANDARD_IA"

    client.complete_multipart_upload(
        Bucket=bucket,
        Key=key,
        UploadId=upload["UploadId"],
        MultipartUpload={"Parts": parts},
    )
    assert client.get_object(Bucket=bucket, Key=key)["Body"].read() == first_part + final_part
    completed_head = client.head_object(Bucket=bucket, Key=key)
    assert completed_head["ContentType"] == "application/x-sqrzl-multipart"
    assert completed_head["Metadata"] == {"owner": "multipart-sdk"}
    assert completed_head["StorageClass"] == "STANDARD_IA"
    assert client.get_object_tagging(Bucket=bucket, Key=key)["TagSet"] == [
        {"Key": "project", "Value": "large-documents"}
    ]
    client.abort_multipart_upload(
        Bucket=bucket,
        Key="spare.txt",
        UploadId=spare_upload["UploadId"],
    )

    client.put_object(Bucket=bucket, Key=key, Body=b"new-version")
    versions = client.list_object_versions(Bucket=bucket, Prefix=key)
    assert len(versions.get("Versions", [])) >= 2

    _empty_versioned_bucket(client, bucket)
    client.delete_bucket(Bucket=bucket)


def test_s3_presigned_multipart_parts_with_authentication(sqrzl_server):
    sqrzl_server.require_provider("s3")
    if not sqrzl_server.enforce_auth:
        pytest.skip("presigned verification requires the authenticated SDK lane")

    client = _client(sqrzl_server)
    bucket = sqrzl_server.bucket_name("sdk-s3-presigned-multipart")
    key = "large document.bin"
    first_part = b"a" * (5 * 1024 * 1024)
    final_part = b"presigned-final-part"

    client.create_bucket(Bucket=bucket)
    upload = client.create_multipart_upload(Bucket=bucket, Key=key)
    completed_parts = []
    for part_number, payload in enumerate([first_part, final_part], start=1):
        url = client.generate_presigned_url(
            "upload_part",
            Params={
                "Bucket": bucket,
                "Key": key,
                "UploadId": upload["UploadId"],
                "PartNumber": part_number,
            },
            ExpiresIn=300,
            HttpMethod="PUT",
        )
        if part_number == 1:
            tampered_url = url.replace("partNumber=1", "partNumber=2")
            with pytest.raises(urllib.error.HTTPError) as rejected:
                urllib.request.urlopen(
                    urllib.request.Request(tampered_url, data=payload, method="PUT"),
                    timeout=30,
                )
            assert rejected.value.code == 403
        request = urllib.request.Request(url, data=payload, method="PUT")
        with urllib.request.urlopen(request, timeout=30) as response:
            completed_parts.append(
                {"PartNumber": part_number, "ETag": response.headers["ETag"]}
            )

    client.complete_multipart_upload(
        Bucket=bucket,
        Key=key,
        UploadId=upload["UploadId"],
        MultipartUpload={"Parts": completed_parts},
    )

    stored = client.get_object(Bucket=bucket, Key=key)["Body"].read()
    assert stored == first_part + final_part

    client.delete_object(Bucket=bucket, Key=key)
    client.delete_bucket(Bucket=bucket)


def test_s3_upload_part_copy_workflow(sqrzl_server):
    sqrzl_server.require_provider("s3")
    client = _client(sqrzl_server)
    bucket = sqrzl_server.bucket_name("sdk-s3-upload-part-copy")
    source_key = "copy source.bin"
    destination_key = "copied-part.bin"

    client.create_bucket(Bucket=bucket)
    client.put_object(Bucket=bucket, Key=source_key, Body=b"copy-source-payload")
    upload = client.create_multipart_upload(Bucket=bucket, Key=destination_key)
    copied = client.upload_part_copy(
        Bucket=bucket,
        Key=destination_key,
        UploadId=upload["UploadId"],
        PartNumber=1,
        CopySource={"Bucket": bucket, "Key": source_key},
        CopySourceRange="bytes=0-10",
    )
    client.complete_multipart_upload(
        Bucket=bucket,
        Key=destination_key,
        UploadId=upload["UploadId"],
        MultipartUpload={
            "Parts": [{"PartNumber": 1, "ETag": copied["CopyPartResult"]["ETag"]}]
        },
    )

    assert client.get_object(Bucket=bucket, Key=destination_key)["Body"].read() == b"copy-source"

    client.delete_object(Bucket=bucket, Key=source_key)
    client.delete_object(Bucket=bucket, Key=destination_key)
    client.delete_bucket(Bucket=bucket)
