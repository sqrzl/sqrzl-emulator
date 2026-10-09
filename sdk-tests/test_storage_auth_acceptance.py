from __future__ import annotations

import base64
import io
from dataclasses import replace

import pytest

from test_s3_sdk import _client as s3_client
from test_azure_sdk import _service as azure_service
from test_oci_sdk import _client as oci_client


def _require_auth(server, provider):
    server.require_provider(provider)
    if not server.enforce_auth:
        pytest.skip(
            "native credential negatives require the authenticated storage lane"
        )


def test_s3_native_signature_error_preserves_object(sqrzl_server):
    _require_auth(sqrzl_server, "s3")
    from botocore.exceptions import ClientError

    client = s3_client(sqrzl_server)
    wrong = s3_client(replace(sqrzl_server, secret_access_key="wrong-key"))
    bucket = sqrzl_server.bucket_name("sdk-s3-auth-negative")
    client.create_bucket(Bucket=bucket)
    client.put_object(Bucket=bucket, Key="original", Body=b"original")
    with pytest.raises(ClientError) as denied:
        wrong.put_object(Bucket=bucket, Key="original", Body=b"unauthorized")
    assert denied.value.response["ResponseMetadata"]["HTTPStatusCode"] == 403
    assert denied.value.response["Error"]["Code"] == "SignatureDoesNotMatch"
    assert (
        client.get_object(Bucket=bucket, Key="original")["Body"].read() == b"original"
    )
    client.delete_object(Bucket=bucket, Key="original")
    client.delete_bucket(Bucket=bucket)


def test_azure_native_signature_error_preserves_blob(sqrzl_server):
    _require_auth(sqrzl_server, "azure")
    from azure.core.exceptions import HttpResponseError

    service = azure_service(sqrzl_server)
    wrong = azure_service(
        replace(sqrzl_server, azure_account_key=base64.b64encode(b"wrong-key").decode())
    )
    container = service.create_container(
        sqrzl_server.bucket_name("sdk-azure-auth-negative")
    )
    blob = container.get_blob_client("original")
    blob.upload_blob(b"original", overwrite=True)
    with pytest.raises(HttpResponseError) as denied:
        wrong.get_blob_client(container.container_name, "original").upload_blob(
            b"unauthorized", overwrite=True
        )
    assert denied.value.status_code == 403
    assert denied.value.error_code == "AuthenticationFailed"
    assert blob.download_blob().readall() == b"original"
    blob.delete_blob()
    container.delete_container()


def test_oci_native_signature_error_preserves_object(sqrzl_server, tmp_path):
    _require_auth(sqrzl_server, "oci")
    import oci

    client = oci_client(sqrzl_server, tmp_path)
    wrong = oci_client(replace(sqrzl_server, oci_private_key_path=None), tmp_path)
    namespace = client.get_namespace().data
    bucket = sqrzl_server.bucket_name("sdk-oci-auth-negative")
    client.create_bucket(
        namespace,
        oci.object_storage.models.CreateBucketDetails(
            name=bucket, compartment_id="ocid1.compartment.oc1..sqrzl"
        ),
    )
    client.put_object(namespace, bucket, "original", io.BytesIO(b"original"))
    with pytest.raises(oci.exceptions.ServiceError) as denied:
        wrong.put_object(namespace, bucket, "original", io.BytesIO(b"unauthorized"))
    assert denied.value.status == 401
    assert denied.value.code == "NotAuthenticated"
    assert client.get_object(namespace, bucket, "original").data.content == b"original"
    client.delete_object(namespace, bucket, "original")
    client.delete_bucket(namespace, bucket)
