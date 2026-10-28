from __future__ import annotations

import base64
import hashlib
import zlib
import io
import urllib.error
import urllib.request

import pytest


boto3 = pytest.importorskip("boto3")
botocore_config = pytest.importorskip("botocore.config")
botocore_exceptions = pytest.importorskip("botocore.exceptions")


def _client(sqrzl_server, *, native_checksums=False):
    return boto3.client(
        "s3",
        endpoint_url=sqrzl_server.api_url,
        aws_access_key_id=sqrzl_server.access_key_id,
        aws_secret_access_key=sqrzl_server.secret_access_key,
        region_name="us-east-1",
        config=botocore_config.Config(
            signature_version="s3v4",
            request_checksum_calculation="when_supported" if native_checksums else "when_required",
            s3={"addressing_style": "path"},
        ),
    )


def test_s3_crc32c_full_object_multipart_contract(sqrzl_server):
    """AWS CompleteMultipartUpload: full CRC32C, selected parts, and atomic create."""
    import google_crc32c

    sqrzl_server.require_provider("s3")
    client = _client(sqrzl_server)
    bucket = sqrzl_server.bucket_name("sdk-s3-crc-multipart")
    client.create_bucket(Bucket=bucket)
    crc = lambda data: base64.b64encode(google_crc32c.value(data).to_bytes(4, "big")).decode()
    payloads = [b"a" * (5 * 1024 * 1024), b"last part"]
    upload = client.create_multipart_upload(
        Bucket=bucket, Key="checked", ChecksumAlgorithm="CRC32C", ChecksumType="FULL_OBJECT"
    )
    args = dict(Bucket=bucket, Key="checked", UploadId=upload["UploadId"])
    parts = []
    for number, payload in enumerate(payloads, 1):
        with pytest.raises(botocore_exceptions.ClientError) as bad:
            client.upload_part(**args, PartNumber=number, Body=payload, ChecksumCRC32C="AAAAAA==")
        assert bad.value.response["Error"]["Code"] == "BadDigest"
        uploaded = client.upload_part(**args, PartNumber=number, Body=payload, ChecksumCRC32C=crc(payload))
        assert uploaded["ChecksumCRC32C"] == crc(payload)
        parts.append(dict(PartNumber=number, ETag=uploaded["ETag"], ChecksumCRC32C=crc(payload)))
    listed = client.list_parts(**args)
    assert [part["ChecksumCRC32C"] for part in listed["Parts"]] == [crc(p) for p in payloads]
    # A rejected replacement must preserve its acknowledged checksum and bytes.
    with pytest.raises(botocore_exceptions.ClientError) as bad:
        client.upload_part(**args, PartNumber=1, Body=b"corrupt replacement", ChecksumCRC32C=crc(payloads[0]))
    assert bad.value.response["Error"]["Code"] == "BadDigest"
    if sqrzl_server.runtime is not None:
        sqrzl_server.restart(kill=True)
    assert [p["ChecksumCRC32C"] for p in client.list_parts(**args)["Parts"]] == [crc(p) for p in payloads]
    body = b"".join(payloads)
    for changes, code in [
        ({"ChecksumCRC32C": "AAAAAA=="}, "BadDigest"),
        ({"MpuObjectSize": len(body) + 1}, "InvalidRequest"),
        ({"ChecksumType": "COMPOSITE"}, "BadDigest"),
        ({"MultipartUpload": {"Parts": [{**parts[0], "ChecksumCRC32C": "AAAAAA=="}, parts[1]]}}, "InvalidPart"),
    ]:
        complete = dict(MultipartUpload={"Parts": parts}, ChecksumCRC32C=crc(body),
                        ChecksumType="FULL_OBJECT", MpuObjectSize=len(body), IfNoneMatch="*")
        complete.update(changes)
        with pytest.raises(botocore_exceptions.ClientError) as bad:
            client.complete_multipart_upload(**args, **complete)
        assert bad.value.response["Error"]["Code"] == code
        with pytest.raises(botocore_exceptions.ClientError) as missing:
            client.head_object(Bucket=bucket, Key="checked")
        assert missing.value.response["ResponseMetadata"]["HTTPStatusCode"] == 404
    result = client.complete_multipart_upload(
        **args, MultipartUpload={"Parts": parts}, ChecksumCRC32C=crc(body),
        ChecksumType="FULL_OBJECT", MpuObjectSize=len(body), IfNoneMatch="*"
    )
    assert result["ChecksumCRC32C"] == crc(body)
    assert result["ChecksumType"] == "FULL_OBJECT"
    fetched = client.get_object(Bucket=bucket, Key="checked", ChecksumMode="ENABLED")
    assert fetched["ChecksumCRC32C"] == crc(body)
    assert fetched["ChecksumType"] == "FULL_OBJECT"
    assert fetched["Body"].read() == body
    # Competing completion cannot overwrite the first object's authority.
    rival = client.create_multipart_upload(Bucket=bucket, Key="checked", ChecksumAlgorithm="CRC32C", ChecksumType="FULL_OBJECT")
    rival_args = {**args, "UploadId": rival["UploadId"]}
    rival_part = client.upload_part(**rival_args, PartNumber=1, Body=b"rival")
    assert rival_part["ChecksumCRC32C"] == crc(b"rival")
    for condition in ({"IfNoneMatch": "*"}, {"IfMatch": '"wrong"'}):
        with pytest.raises(botocore_exceptions.ClientError) as conflict:
            client.complete_multipart_upload(**rival_args, MultipartUpload={"Parts": [dict(PartNumber=1, ETag=rival_part["ETag"])]}, **condition)
        assert conflict.value.response["ResponseMetadata"]["HTTPStatusCode"] == 412
        assert conflict.value.response["Error"]["Code"] == "PreconditionFailed"
    assert client.get_object(Bucket=bucket, Key="checked")["Body"].read() == body
    client.abort_multipart_upload(**rival_args)
    # Missing caller digests are computed server-side, not fabricated or treated as corrupt.
    generated = client.create_multipart_upload(Bucket=bucket, Key="generated", ChecksumAlgorithm="CRC32C", ChecksumType="FULL_OBJECT")
    generated_args = dict(Bucket=bucket, Key="generated", UploadId=generated["UploadId"])
    generated_part = client.upload_part(**generated_args, PartNumber=1, Body=b"generated")
    result = client.complete_multipart_upload(**generated_args, MultipartUpload={"Parts": [dict(PartNumber=1, ETag=generated_part["ETag"])]})
    assert result["ChecksumCRC32C"] == crc(b"generated")
    assert client.get_object(Bucket=bucket, Key="generated")["Body"].read() == b"generated"
    client.delete_object(Bucket=bucket, Key="generated")
    client.delete_object(Bucket=bucket, Key="checked")
    client.delete_bucket(Bucket=bucket)


@pytest.mark.parametrize("mode", ["missing", "corrupt"])
def test_s3_multipart_checksum_receipt_faults(sqrzl_server, mode):
    import google_crc32c

    sqrzl_server.require_provider("s3")
    client = _client(sqrzl_server)
    bucket = sqrzl_server.bucket_name("sdk-s3-receipt-fault")
    client.create_bucket(Bucket=bucket)
    payload = b"receipt proof must describe stored bytes"
    crc = base64.b64encode(google_crc32c.value(payload).to_bytes(4, "big")).decode()
    upload = client.create_multipart_upload(Bucket=bucket, Key="key", ChecksumAlgorithm="CRC32C", ChecksumType="FULL_OBJECT")
    args = dict(Bucket=bucket, Key="key", UploadId=upload["UploadId"])
    part = client.upload_part(**args, PartNumber=1, Body=payload, ChecksumCRC32C=crc)

    def inject(request, **_):
        request.headers["x-sqrzl-failpoint"] = f"checksum-receipt-{mode}"

    client.meta.events.register("before-sign.s3.CompleteMultipartUpload", inject)
    completed = client.complete_multipart_upload(**args, MultipartUpload={"Parts": [dict(PartNumber=1, ETag=part["ETag"], ChecksumCRC32C=crc)]}, ChecksumCRC32C=crc, ChecksumType="FULL_OBJECT")
    assert completed["ResponseMetadata"]["HTTPHeaders"]["x-sqrzl-failpoint-applied"] == f"checksum-receipt-{mode}"
    receipt = completed.get("ChecksumCRC32C")
    assert receipt is None if mode == "missing" else receipt != crc
    # An untrusted receipt is not proof. Independent readback still sees the real object.
    fetched = client.get_object(Bucket=bucket, Key="key", IfMatch=completed["ETag"], ChecksumMode="ENABLED")
    assert fetched["Body"].read() == payload
    assert fetched["ChecksumCRC32C"] == crc
    client.delete_object(Bucket=bucket, Key="key")
    client.delete_bucket(Bucket=bucket)


@pytest.mark.parametrize("shape", ["wrong-root", "duplicate-etag", "nested-checksum", "nonconsecutive", "escaped-etag"])
def test_s3_checksum_manifest_structure(sqrzl_server, shape):
    sqrzl_server.require_provider("s3")
    client = _client(sqrzl_server)
    bucket = sqrzl_server.bucket_name("sdk-s3-manifest")
    client.create_bucket(Bucket=bucket)
    upload = client.create_multipart_upload(Bucket=bucket, Key="key", ChecksumAlgorithm="CRC32C", ChecksumType="FULL_OBJECT")
    args = dict(Bucket=bucket, Key="key", UploadId=upload["UploadId"])
    number = 2 if shape == "nonconsecutive" else 1
    part = client.upload_part(**args, PartNumber=number, Body=b"part")
    fields = f'<PartNumber>{number}</PartNumber><ETag>{part["ETag"]}</ETag>'
    if shape == "escaped-etag":
        fields = fields.replace('"', '&quot;')
        escaped_crc = part["ChecksumCRC32C"].replace('=', '&#61;')
        fields += f"<ChecksumCRC32C>{escaped_crc}</ChecksumCRC32C>"
    root = "WrongRoot" if shape == "wrong-root" else "CompleteMultipartUpload"
    if shape == "duplicate-etag":
        fields += f'<ETag>{part["ETag"]}</ETag>'
    if shape == "nested-checksum":
        fields += f'<Unknown><ChecksumCRC32C>{part["ChecksumCRC32C"]}</ChecksumCRC32C></Unknown>'
    xml = f"<{root}><Part>{fields}</Part></{root}>".encode()

    def replace_body(params, **_):
        params["body"] = xml

    client.meta.events.register("before-call.s3.CompleteMultipartUpload", replace_body)
    if shape == "escaped-etag":
        result = client.complete_multipart_upload(**args, MultipartUpload={"Parts": [dict(PartNumber=number, ETag=part["ETag"])]})
        assert result["ChecksumCRC32C"] == part["ChecksumCRC32C"]
        assert client.get_object(Bucket=bucket, Key="key")["Body"].read() == b"part"
        client.delete_object(Bucket=bucket, Key="key")
        client.delete_bucket(Bucket=bucket)
        return
    with pytest.raises(botocore_exceptions.ClientError) as bad:
        client.complete_multipart_upload(**args, MultipartUpload={"Parts": [dict(PartNumber=number, ETag=part["ETag"])]})
    assert bad.value.response["Error"]["Code"] == ("InvalidPartOrder" if shape == "nonconsecutive" else "MalformedXML")
    assert client.list_parts(**args)["Parts"][0]["ETag"] == part["ETag"]
    with pytest.raises(botocore_exceptions.ClientError) as missing:
        client.head_object(Bucket=bucket, Key="key")
    assert missing.value.response["ResponseMetadata"]["HTTPStatusCode"] == 404
    client.abort_multipart_upload(**args)
    client.delete_bucket(Bucket=bucket)


def test_s3_crc32c_part_sdk_algorithm_header_contract(sqrzl_server):
    sqrzl_server.require_provider("s3")
    client = _client(sqrzl_server)
    bucket = sqrzl_server.bucket_name("sdk-s3-sdk-digest")
    client.create_bucket(Bucket=bucket)
    upload = client.create_multipart_upload(Bucket=bucket, Key="key", ChecksumAlgorithm="CRC32C", ChecksumType="FULL_OBJECT")
    args = dict(Bucket=bucket, Key="key", UploadId=upload["UploadId"])

    def inject(request, **_):
        request.headers["x-amz-sdk-checksum-algorithm"] = "CRC32C"

    client.meta.events.register("before-sign.s3.UploadPart", inject)
    with pytest.raises(botocore_exceptions.ClientError) as bad:
        client.upload_part(**args, PartNumber=1, Body=b"part")
    assert bad.value.response["ResponseMetadata"]["HTTPStatusCode"] == 400
    assert client.list_parts(**args).get("Parts", []) == []
    # AWS ignores the SDK algorithm selector when the individual digest is provided.
    client.meta.events.unregister("before-sign.s3.UploadPart", inject)

    def ignored_selector(request, **_):
        request.headers["x-amz-sdk-checksum-algorithm"] = "SHA256"

    client.meta.events.register("before-sign.s3.UploadPart", ignored_selector)
    import google_crc32c
    crc = base64.b64encode(google_crc32c.value(b"part").to_bytes(4, "big")).decode()
    part = client.upload_part(**args, PartNumber=1, Body=b"part", ChecksumCRC32C=crc)
    assert part["ChecksumCRC32C"] == crc
    client.abort_multipart_upload(**args)
    client.delete_bucket(Bucket=bucket)


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
    assert bucket in {item["Name"] for item in client.list_buckets()["Buckets"]}
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


def test_s3_direct_put_native_checksums_and_rejection(sqrzl_server):
    sqrzl_server.require_provider("s3")
    client = _client(sqrzl_server, native_checksums=True)
    bucket = sqrzl_server.bucket_name("sdk-s3-checksums")
    payload = b"123456789"
    encode = lambda digest: base64.b64encode(digest).decode("ascii")
    checksums = {
        "CRC32": encode(zlib.crc32(payload).to_bytes(4, "big")),
        "CRC32C": encode(bytes.fromhex("e3069283")),
        "CRC64NVME": encode(bytes.fromhex("ae8b14860a799888")),
        "SHA1": encode(hashlib.sha1(payload).digest()),
        "SHA256": encode(hashlib.sha256(payload).digest()),
        "MD5": encode(hashlib.md5(payload).digest()),
    }
    client.create_bucket(Bucket=bucket)
    for algorithm, checksum in checksums.items():
        field = f"Checksum{algorithm}"
        put = client.put_object(Bucket=bucket, Key=algorithm, Body=payload,
                                ChecksumAlgorithm=algorithm, **{field: checksum})
        assert put[field] == checksum
        assert put["ChecksumType"] == "FULL_OBJECT"
        if algorithm == "SHA256":
            # Tagging requires a request XML checksum even under the optout profile.
            client.put_object_tagging(Bucket=bucket, Key=algorithm,
                                      Tagging={"TagSet": [{"Key": "proof", "Value": "transactional"}]})

        for method in (client.get_object, client.head_object):
            response = method(Bucket=bucket, Key=algorithm, ChecksumMode="ENABLED")
            assert response[field] == checksum
            assert response["ChecksumType"] == "FULL_OBJECT"
            if "Body" in response:
                assert response["Body"].read() == payload
        with pytest.raises(botocore_exceptions.ClientError) as failure:
            client.put_object(Bucket=bucket, Key=algorithm, Body=b"different",
                              ChecksumAlgorithm=algorithm, **{field: checksum})
        assert failure.value.response["Error"]["Code"] == "BadDigest"
        assert client.get_object(Bucket=bucket, Key=algorithm)["Body"].read() == payload
        client.delete_object(Bucket=bucket, Key=algorithm)
    # Current botocore's default direct PUT CRC32 profile must also work.
    put = client.put_object(Bucket=bucket, Key="default", Body=payload)
    assert put["ChecksumCRC32"] == checksums["CRC32"]
    client.delete_object(Bucket=bucket, Key="default")
    client.delete_bucket(Bucket=bucket)


def test_s3_http_properties_copy_and_plain_multipart(sqrzl_server):
    sqrzl_server.require_provider("s3")
    client = _client(sqrzl_server)
    bucket = sqrzl_server.bucket_name("sdk-s3-properties")
    properties = dict(CacheControl="max-age=90", ContentEncoding="identity",
                      ContentLanguage="en-US", ContentDisposition="attachment; filename=report.txt")
    client.create_bucket(Bucket=bucket)
    client.put_object(Bucket=bucket, Key="source", Body=b"properties", **properties)
    for method in (client.get_object, client.head_object):
        response = method(Bucket=bucket, Key="source")
        assert {name: response[name] for name in properties} == properties
        if "Body" in response:
            response["Body"].close()
    client.copy_object(Bucket=bucket, Key="copied", CopySource={"Bucket": bucket, "Key": "source"})
    head = client.head_object(Bucket=bucket, Key="copied")
    assert {name: head[name] for name in properties} == properties
    client.copy_object(Bucket=bucket, Key="replaced", CopySource={"Bucket": bucket, "Key": "source"},
                       MetadataDirective="REPLACE", CacheControl="no-cache", ContentLanguage="fr")
    head = client.head_object(Bucket=bucket, Key="replaced")
    assert head["CacheControl"] == "no-cache"
    assert head["ContentLanguage"] == "fr"
    assert "ContentDisposition" not in head and "ContentEncoding" not in head
    upload = client.create_multipart_upload(Bucket=bucket, Key="multi", **properties)
    part = client.upload_part(Bucket=bucket, Key="multi", UploadId=upload["UploadId"], PartNumber=1, Body=b"multipart")
    client.complete_multipart_upload(Bucket=bucket, Key="multi", UploadId=upload["UploadId"],
                                     MultipartUpload={"Parts": [{"PartNumber": 1, "ETag": part["ETag"]}]})
    head = client.head_object(Bucket=bucket, Key="multi")
    assert {name: head[name] for name in properties} == properties
    # A fresh replacement resets omitted properties.
    client.put_object(Bucket=bucket, Key="source", Body=b"new")
    head = client.head_object(Bucket=bucket, Key="source")
    assert all(name not in head for name in properties)
    for key in ("source", "copied", "replaced", "multi"):
        client.delete_object(Bucket=bucket, Key=key)
    client.delete_bucket(Bucket=bucket)


def test_s3_native_default_multipart_checksum_boundary(sqrzl_server):
    sqrzl_server.require_provider("s3")
    native_client = _client(sqrzl_server, native_checksums=True)
    client = _client(sqrzl_server)
    bucket = sqrzl_server.bucket_name("sdk-s3-multipart-checksum-boundary")
    client.create_bucket(Bucket=bucket)
    with pytest.raises(botocore_exceptions.ClientError) as failure:
        native_client.create_multipart_upload(Bucket=bucket, Key="explicit", ChecksumAlgorithm="CRC32")
    assert failure.value.response["Error"]["Code"] == "NotImplemented"
    assert client.list_multipart_uploads(Bucket=bucket).get("Uploads", []) == []
    # Current botocore default initiation sends no checksum; part upload adds CRC32.
    upload = native_client.create_multipart_upload(Bucket=bucket, Key="plain")
    with pytest.raises(botocore_exceptions.ClientError) as failure:
        native_client.upload_part(Bucket=bucket, Key="plain", UploadId=upload["UploadId"], PartNumber=1, Body=b"part")
    assert failure.value.response["Error"]["Code"] == "NotImplemented"
    assert client.list_parts(Bucket=bucket, Key="plain", UploadId=upload["UploadId"]).get("Parts", []) == []
    part = client.upload_part(Bucket=bucket, Key="plain", UploadId=upload["UploadId"], PartNumber=1, Body=b"part")
    with pytest.raises(botocore_exceptions.ClientError) as failure:
        client.complete_multipart_upload(Bucket=bucket, Key="plain", UploadId=upload["UploadId"],
                                         MultipartUpload={"Parts": [{"PartNumber": 1, "ETag": part["ETag"], "ChecksumCRC32": "AAAAAA=="}]})
    assert failure.value.response["Error"]["Code"] == "NotImplemented"
    assert len(client.list_parts(Bucket=bucket, Key="plain", UploadId=upload["UploadId"])["Parts"]) == 1
    client.abort_multipart_upload(Bucket=bucket, Key="plain", UploadId=upload["UploadId"])
    checked = client.create_multipart_upload(Bucket=bucket, Key="checked", ChecksumAlgorithm="CRC32C", ChecksumType="FULL_OBJECT")
    with pytest.raises(botocore_exceptions.ClientError) as failure:
        client.upload_part_copy(Bucket=bucket, Key="checked", UploadId=checked["UploadId"], PartNumber=1, CopySource={"Bucket": bucket, "Key": "absent"})
    assert failure.value.response["Error"]["Code"] == "NotImplemented"
    assert client.list_parts(Bucket=bucket, Key="checked", UploadId=checked["UploadId"]).get("Parts", []) == []
    client.abort_multipart_upload(Bucket=bucket, Key="checked", UploadId=checked["UploadId"])
    client.delete_bucket(Bucket=bucket)


def test_s3_multipart_part_content_md5_preserves_acknowledged_bytes(sqrzl_server):
    sqrzl_server.require_provider("s3")
    client = _client(sqrzl_server)
    bucket = sqrzl_server.bucket_name("sdk-s3-part-md5")
    client.create_bucket(Bucket=bucket)
    upload = client.create_multipart_upload(Bucket=bucket, Key="checksummed")
    params = dict(Bucket=bucket, Key="checksummed", UploadId=upload["UploadId"], PartNumber=1)
    payload = b"acknowledged part"
    part = client.upload_part(
        **params, Body=io.BytesIO(payload),
        ContentMD5=base64.b64encode(hashlib.md5(payload).digest()).decode("ascii"),
    )
    for digest, code in (("not-base64", "InvalidDigest"), ("AAAAAA==", "InvalidDigest"),
                         ("AAAAAAAAAAAAAAAAAAAAAA==", "BadDigest")):
        with pytest.raises(botocore_exceptions.ClientError) as error:
            client.upload_part(**params, Body=io.BytesIO(b"changed"), ContentMD5=digest)
        assert error.value.response["ResponseMetadata"]["HTTPStatusCode"] == 400
        assert error.value.response["Error"]["Code"] == code
        stored = client.list_parts(Bucket=bucket, Key="checksummed", UploadId=upload["UploadId"])["Parts"]
        assert len(stored) == 1
        assert stored[0]["ETag"] == part["ETag"]
        assert stored[0]["Size"] == len(payload)
    client.complete_multipart_upload(
        Bucket=bucket, Key="checksummed", UploadId=upload["UploadId"],
        MultipartUpload={"Parts": [{"PartNumber": 1, "ETag": part["ETag"]}]},
    )
    assert client.get_object(Bucket=bucket, Key="checksummed")["Body"].read() == payload
    client.delete_object(Bucket=bucket, Key="checksummed")
    client.delete_bucket(Bucket=bucket)
