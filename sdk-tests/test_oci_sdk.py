from __future__ import annotations

import io
import urllib.request
from datetime import datetime, timedelta, timezone

import pytest


oci = pytest.importorskip("oci")


def _client(sqrzl_server, tmp_path):
    serialization = pytest.importorskip("cryptography.hazmat.primitives.serialization")
    rsa = pytest.importorskip("cryptography.hazmat.primitives.asymmetric.rsa")

    key_file = sqrzl_server.oci_private_key_path
    if key_file is None:
        key_file = tmp_path / "oci_api_key.pem"
        key = rsa.generate_private_key(public_exponent=65537, key_size=2048)
        key_file.write_bytes(
            key.private_bytes(
                encoding=serialization.Encoding.PEM,
                format=serialization.PrivateFormat.TraditionalOpenSSL,
                encryption_algorithm=serialization.NoEncryption(),
            )
        )
    config = {
        "user": sqrzl_server.oci_user_ocid,
        "tenancy": sqrzl_server.oci_tenancy_ocid,
        "fingerprint": sqrzl_server.oci_key_fingerprint,
        "key_file": str(key_file),
        "region": "us-ashburn-1",
    }
    client = oci.object_storage.ObjectStorageClient(config)
    client.base_client.endpoint = sqrzl_server.api_url
    return client


def test_oci_core_object_workflows(sqrzl_server, tmp_path):
    sqrzl_server.require_provider("oci")
    client = _client(sqrzl_server, tmp_path)
    namespace = client.get_namespace().data
    bucket_name = sqrzl_server.bucket_name("sdk-oci-core")
    object_name = "folder/hello.txt"

    client.create_bucket(
        namespace,
        oci.object_storage.models.CreateBucketDetails(
            name=bucket_name,
            compartment_id="ocid1.compartment.oc1..sqrzl",
        ),
    )
    client.put_object(
        namespace,
        bucket_name,
        object_name,
        io.BytesIO(b"hello oci sdk"),
        content_type="text/plain",
        opc_meta={"owner": "support"},
    )

    head = client.head_object(namespace, bucket_name, object_name)
    assert head.headers["opc-meta-owner"] == "support"

    ranged = client.get_object(
        namespace,
        bucket_name,
        object_name,
        range="bytes=6-8",
    )
    assert ranged.data.content == b"oci"

    listing = client.list_objects(namespace, bucket_name, prefix="folder/")
    assert [item.name for item in listing.data.objects] == [object_name]

    client.delete_object(namespace, bucket_name, object_name)
    client.delete_bucket(namespace, bucket_name)


def test_oci_multipart_workflow(sqrzl_server, tmp_path):
    sqrzl_server.require_provider("oci")
    client = _client(sqrzl_server, tmp_path)
    namespace = client.get_namespace().data
    bucket_name = sqrzl_server.bucket_name("sdk-oci-multipart")
    object_name = "multi.txt"

    client.create_bucket(
        namespace,
        oci.object_storage.models.CreateBucketDetails(
            name=bucket_name,
            compartment_id="ocid1.compartment.oc1..sqrzl",
        ),
    )
    upload = client.create_multipart_upload(
        namespace,
        bucket_name,
        oci.object_storage.models.CreateMultipartUploadDetails(
            object=object_name,
            content_type="text/plain",
        ),
    ).data

    first_part = b"o" * (10 * 1024 * 1024)
    final_part = b"multipart"
    parts_to_commit = []
    for part_num, payload in enumerate([first_part, final_part], start=1):
        response = client.upload_part(
            namespace,
            bucket_name,
            object_name,
            upload.upload_id,
            part_num,
            io.BytesIO(payload),
        )
        parts_to_commit.append(
            oci.object_storage.models.CommitMultipartUploadPartDetails(
                part_num=part_num,
                etag=response.headers["etag"],
            )
        )

    client.commit_multipart_upload(
        namespace,
        bucket_name,
        object_name,
        upload.upload_id,
        oci.object_storage.models.CommitMultipartUploadDetails(
            parts_to_commit=parts_to_commit,
        ),
    )

    assert (
        client.get_object(namespace, bucket_name, object_name).data.content
        == first_part + final_part
    )
    client.delete_object(namespace, bucket_name, object_name)
    client.delete_bucket(namespace, bucket_name)


def test_oci_preauthenticated_large_upload_workflow(sqrzl_server, tmp_path):
    sqrzl_server.require_provider("oci")
    client = _client(sqrzl_server, tmp_path)
    namespace = client.get_namespace().data
    bucket_name = sqrzl_server.bucket_name("sdk-oci-par")
    object_name = "large/signed.bin"
    client.create_bucket(
        namespace,
        oci.object_storage.models.CreateBucketDetails(
            name=bucket_name,
            compartment_id="ocid1.compartment.oc1..sqrzl",
        ),
    )
    par = client.create_preauthenticated_request(
        namespace,
        bucket_name,
        oci.object_storage.models.CreatePreauthenticatedRequestDetails(
            name="large-upload",
            object_name=object_name,
            access_type="ObjectReadWrite",
            time_expires=datetime.now(timezone.utc) + timedelta(hours=1),
        ),
    ).data
    access_url = f"{sqrzl_server.api_url}{par.access_uri}"

    with urllib.request.urlopen(
        urllib.request.Request(access_url, data=b"signed OCI upload", method="PUT"),
        timeout=30,
    ) as uploaded:
        assert uploaded.status == 200

    with urllib.request.urlopen(access_url, timeout=30) as downloaded:
        assert downloaded.read() == b"signed OCI upload"

    client.delete_preauthenticated_request(namespace, bucket_name, par.id)
    client.delete_object(namespace, bucket_name, object_name)
    client.delete_bucket(namespace, bucket_name)
