from __future__ import annotations

import io

import pytest


oci = pytest.importorskip("oci")


def _client(sqrzl_server, tmp_path):
    serialization = pytest.importorskip("cryptography.hazmat.primitives.serialization")
    rsa = pytest.importorskip("cryptography.hazmat.primitives.asymmetric.rsa")

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
        "user": "ocid1.user.oc1..sqrzl",
        "tenancy": "ocid1.tenancy.oc1..sqrzl",
        "fingerprint": "00:11:22:33:44:55:66:77:88:99:aa:bb:cc:dd:ee:ff",
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

    parts_to_commit = []
    for part_num, payload in enumerate([b"oci-", b"multipart"], start=1):
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

    assert client.get_object(namespace, bucket_name, object_name).data.content == b"oci-multipart"
    client.delete_object(namespace, bucket_name, object_name)
    client.delete_bucket(namespace, bucket_name)
