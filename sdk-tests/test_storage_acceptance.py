from __future__ import annotations

import io
from concurrent.futures import ThreadPoolExecutor
from threading import Barrier

import pytest

from test_s3_sdk import _client as s3_client
from test_azure_sdk import _service as azure_service
from test_gcs_sdk import _client as gcs_client
from test_oci_sdk import _client as oci_client


def _race(create):
    barrier = Barrier(2)

    def contender(payload):
        barrier.wait(timeout=10)
        return create(payload), payload

    with ThreadPoolExecutor(max_workers=2) as pool:
        results = list(pool.map(contender, [b"first contender", b"second contender"]))
    assert sorted(result for result, _ in results) == [False, True]
    return next(payload for succeeded, payload in results if succeeded)


def _restart(server, record_property):
    old_pid = server.process_pid
    new_pid = server.restart()
    assert new_pid != old_pid
    record_property("normal_restart", {"old_pid": old_pid, "new_pid": new_pid})


def test_s3_object_pages_conditions_and_process_restart(sqrzl_server, record_property):
    sqrzl_server.require_provider("s3")
    sqrzl_server.require_process()
    from botocore.exceptions import ClientError

    client = s3_client(sqrzl_server)
    bucket = sqrzl_server.bucket_name("sdk-s3-acceptance")
    client.create_bucket(Bucket=bucket)
    keys = [f"pages/{index}.txt" for index in range(3)]
    for key in keys:
        client.put_object(Bucket=bucket, Key=key, Body=key.encode())
    pages = list(
        client.get_paginator("list_objects_v2").paginate(
            Bucket=bucket, Prefix="pages/", PaginationConfig={"PageSize": 1}
        )
    )
    assert len(pages) == 3
    assert [obj["Key"] for page in pages for obj in page["Contents"]] == keys
    tokens = [page["NextContinuationToken"] for page in pages[:-1]]
    assert len(set(tokens)) == 2
    assert client.list_objects_v2(Bucket=bucket, MaxKeys=0).get("Contents", []) == []

    def create(payload):
        try:
            client.put_object(
                Bucket=bucket, Key="race.txt", Body=payload, IfNoneMatch="*"
            )
            return True
        except ClientError as error:
            assert error.response["ResponseMetadata"]["HTTPStatusCode"] == 412
            assert error.response["Error"]["Code"] == "PreconditionFailed"
            return False

    winner = _race(create)
    with pytest.raises(ClientError) as denied:
        client.put_object(
            Bucket=bucket, Key="race.txt", Body=b"wrong etag", IfMatch='"wrong"'
        )
    assert denied.value.response["Error"]["Code"] == "PreconditionFailed"
    assert denied.value.response["ResponseMetadata"]["HTTPStatusCode"] == 412
    _restart(sqrzl_server, record_property)
    assert client.get_object(Bucket=bucket, Key="race.txt")["Body"].read() == winner
    for key in keys + ["race.txt"]:
        client.delete_object(Bucket=bucket, Key=key)
    with pytest.raises(ClientError) as missing:
        client.get_object(Bucket=bucket, Key="race.txt")
    assert missing.value.response["Error"]["Code"] == "NoSuchKey"
    assert missing.value.response["ResponseMetadata"]["HTTPStatusCode"] == 404
    client.delete_bucket(Bucket=bucket)


def test_azure_blob_pages_conditions_and_process_restart(sqrzl_server, record_property):
    sqrzl_server.require_provider("azure")
    sqrzl_server.require_process()
    from azure.core import MatchConditions
    from azure.core.exceptions import HttpResponseError

    service = azure_service(sqrzl_server)
    container = service.create_container(
        sqrzl_server.bucket_name("sdk-azure-acceptance")
    )
    keys = [f"pages/{index}.txt" for index in range(3)]
    for key in keys:
        container.get_blob_client(key).upload_blob(key.encode())
    pages = [
        list(page)
        for page in container.list_blobs(
            name_starts_with="pages/", results_per_page=1
        ).by_page()
    ]
    assert len(pages) == 3
    assert [blob.name for page in pages for blob in page] == keys
    assert (
        list(container.list_blobs(name_starts_with="missing/", results_per_page=1))
        == []
    )
    blob = container.get_blob_client("race.txt")

    def create(payload):
        try:
            blob.upload_blob(
                payload, overwrite=True, match_condition=MatchConditions.IfMissing
            )
            return True
        except HttpResponseError as error:
            assert error.status_code == 412
            assert error.error_code == "ConditionNotMet"
            return False

    winner = _race(create)
    with pytest.raises(HttpResponseError) as denied:
        blob.upload_blob(
            b"wrong etag",
            overwrite=True,
            etag='"wrong"',
            match_condition=MatchConditions.IfNotModified,
        )
    assert denied.value.status_code == 412
    assert denied.value.error_code == "ConditionNotMet"
    _restart(sqrzl_server, record_property)
    assert blob.download_blob().readall() == winner
    for key in keys + ["race.txt"]:
        container.get_blob_client(key).delete_blob()
    with pytest.raises(HttpResponseError) as missing:
        blob.download_blob()
    assert missing.value.status_code == 404
    assert missing.value.error_code == "BlobNotFound"
    container.delete_container()


def test_gcs_json_object_pages_conditions_and_process_restart(
    sqrzl_server, record_property
):
    sqrzl_server.require_provider("gcs")
    sqrzl_server.require_process()
    from google.api_core.exceptions import NotFound, PreconditionFailed

    client = gcs_client(sqrzl_server)
    bucket = client.bucket(sqrzl_server.bucket_name("sdk-gcs-acceptance"))
    bucket.create()
    keys = [f"pages/{index}.txt" for index in range(3)]
    for key in keys:
        bucket.blob(key).upload_from_string(key.encode())
    pages = [
        list(page)
        for page in client.list_blobs(bucket, prefix="pages/", page_size=1).pages
    ]
    assert len(pages) == 3
    assert [blob.name for page in pages for blob in page] == keys
    assert list(client.list_blobs(bucket, prefix="missing/", page_size=1)) == []
    blob = bucket.blob("race.txt")

    def create(payload):
        try:
            bucket.blob("race.txt").upload_from_string(
                payload, if_generation_match=0, retry=None
            )
            return True
        except PreconditionFailed as error:
            assert error.code == 412
            return False

    winner = _race(create)
    blob.reload()
    with pytest.raises(PreconditionFailed) as denied:
        blob.upload_from_string(
            b"wrong generation",
            if_generation_match=int(blob.generation) + 1,
            retry=None,
        )
    assert denied.value.code == 412
    _restart(sqrzl_server, record_property)
    assert blob.download_as_bytes() == winner
    for key in keys + ["race.txt"]:
        bucket.blob(key).delete()
    with pytest.raises(NotFound) as missing:
        blob.download_as_bytes()
    assert missing.value.code == 404
    bucket.delete()


def test_oci_object_pages_conditions_and_process_restart(
    sqrzl_server, tmp_path, record_property
):
    sqrzl_server.require_provider("oci")
    sqrzl_server.require_process()
    import oci

    client = oci_client(sqrzl_server, tmp_path)
    namespace = client.get_namespace().data
    bucket = sqrzl_server.bucket_name("sdk-oci-acceptance")
    client.create_bucket(
        namespace,
        oci.object_storage.models.CreateBucketDetails(
            name=bucket, compartment_id="ocid1.compartment.oc1..sqrzl"
        ),
    )
    keys = [f"pages/{index}.txt" for index in range(3)]
    for key in keys:
        client.put_object(namespace, bucket, key, io.BytesIO(key.encode()))
    items, tokens = [], []
    start = None
    while True:
        page = client.list_objects(
            namespace,
            bucket,
            prefix="pages/",
            limit=1,
            **({"start": start} if start else {}),
        ).data
        items.extend(item.name for item in page.objects)
        if page.next_start_with is None:
            break
        start = page.next_start_with
        assert start not in tokens
        tokens.append(start)
    assert items == keys
    assert len(tokens) == 2
    assert (
        client.list_objects(namespace, bucket, prefix="missing/", limit=1).data.objects
        == []
    )

    def create(payload):
        try:
            client.put_object(
                namespace, bucket, "race.txt", io.BytesIO(payload), if_none_match="*"
            )
            return True
        except oci.exceptions.ServiceError as error:
            assert error.status == 412
            assert error.code == "NoEtagMatch"
            return False

    winner = _race(create)
    with pytest.raises(oci.exceptions.ServiceError) as denied:
        client.put_object(
            namespace, bucket, "race.txt", io.BytesIO(b"wrong etag"), if_match='"wrong"'
        )
    assert denied.value.status == 412
    assert denied.value.code == "NoEtagMatch"
    _restart(sqrzl_server, record_property)
    assert client.get_object(namespace, bucket, "race.txt").data.content == winner
    for key in keys + ["race.txt"]:
        client.delete_object(namespace, bucket, key)
    with pytest.raises(oci.exceptions.ServiceError) as missing:
        client.get_object(namespace, bucket, "race.txt")
    assert missing.value.status == 404
    assert missing.value.code == "ObjectNotFound"
    client.delete_bucket(namespace, bucket)
