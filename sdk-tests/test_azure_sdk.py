from __future__ import annotations

import base64
from datetime import datetime, timedelta, timezone

import pytest


azure_blob = pytest.importorskip("azure.storage.blob")


def _service(sqrzl_server):
    return azure_blob.BlobServiceClient(
        account_url=f"{sqrzl_server.api_url}/{sqrzl_server.azure_account}",
        credential=(
            sqrzl_server.azure_account_key if sqrzl_server.enforce_auth else None
        ),
    )


def test_azure_same_byte_writes_revise_etags(sqrzl_server):
    from azure.core import MatchConditions
    from azure.core.exceptions import HttpResponseError

    sqrzl_server.require_provider("azure")
    service = _service(sqrzl_server)
    container = service.create_container(
        sqrzl_server.bucket_name("sdk-azure-revisions"),
        headers={"x-sqrzl-azure-versioning-enabled": "true"},
    )
    blob = container.get_blob_client("item")
    first = blob.upload_blob(b"same bytes", overwrite=True, validate_content=True)
    second = blob.upload_blob(
        b"same bytes", overwrite=True, validate_content=True,
        etag=first["etag"], match_condition=MatchConditions.IfNotModified,
    )
    assert first["etag"] != second["etag"]
    assert first["version_id"] != second["version_id"]
    for operation in [
        lambda: blob.download_blob(etag=first["etag"], match_condition=MatchConditions.IfNotModified),
        lambda: blob.get_blob_properties(etag=first["etag"], match_condition=MatchConditions.IfNotModified),
        lambda: blob.upload_blob(b"bad", overwrite=True, etag=first["etag"], match_condition=MatchConditions.IfNotModified),
    ]:
        with pytest.raises(HttpResponseError) as error:
            operation()
        assert error.value.status_code == 412
        assert error.value.error_code == "ConditionNotMet"
    assert blob.download_blob().readall() == b"same bytes"
    selected = container.get_blob_client("item", version_id=first["version_id"])
    assert selected.get_blob_properties().etag == first["etag"]
    assert selected.download_blob(etag=first["etag"], match_condition=MatchConditions.IfNotModified).readall() == b"same bytes"

    page = container.get_blob_client("page")
    original = page.create_page_blob(512)
    revised = page.upload_page(bytes(512), offset=0, length=512, validate_content=True)
    assert original["etag"] != revised["etag"]
    cleared = page.clear_page(offset=0, length=512)
    assert cleared["etag"] != revised["etag"]
    assert page.download_blob().readall() == bytes(512)
    service.delete_container(container.container_name)


def test_azure_conditional_current_snapshot_and_version_reads(sqrzl_server):
    from azure.core import MatchConditions
    from azure.core.exceptions import HttpResponseError

    sqrzl_server.require_provider("azure")
    service = _service(sqrzl_server)
    container = service.create_container(
        sqrzl_server.bucket_name("sdk-azure-conditions"),
        headers={"x-sqrzl-azure-versioning-enabled": "true"},
    )
    blob = container.get_blob_client("item")
    original = blob.upload_blob(b"old", overwrite=True)
    snapshot = blob.create_snapshot()["snapshot"]
    blob.upload_blob(b"new", overwrite=True)
    selected_clients = [
        (blob, b"new"),
        (container.get_blob_client("item", snapshot=snapshot), b"old"),
        (container.get_blob_client("item", version_id=original["version_id"]), b"old"),
    ]
    for selected, expected in selected_clients:
        properties = selected.get_blob_properties()
        for operation in [selected.get_blob_properties, selected.download_blob]:
            with pytest.raises(HttpResponseError) as error:
                operation(etag='"wrong"', match_condition=MatchConditions.IfNotModified)
            assert error.value.status_code == 412
            assert error.value.error_code == "ConditionNotMet"
            with pytest.raises(HttpResponseError) as error:
                operation(etag=properties.etag, match_condition=MatchConditions.IfModified)
            assert error.value.status_code == 304
            with pytest.raises(HttpResponseError) as error:
                operation(if_modified_since=properties.last_modified)
            assert error.value.status_code == 304
            with pytest.raises(HttpResponseError) as error:
                operation(if_unmodified_since=properties.last_modified - timedelta(seconds=1))
            assert error.value.status_code == 412
        assert selected.download_blob(
            etag=properties.etag,
            match_condition=MatchConditions.IfNotModified,
            if_unmodified_since=properties.last_modified,
        ).readall() == expected
        # Modern Azure combines these two cache conditions with OR.
        assert selected.download_blob(
            etag=properties.etag,
            match_condition=MatchConditions.IfModified,
            if_modified_since=properties.last_modified - timedelta(seconds=1),
        ).readall() == expected

    with pytest.raises(HttpResponseError) as error:
        blob.get_blob_properties(lease="00000000-0000-0000-0000-000000000001")
    assert error.value.status_code == 412
    lease = blob.acquire_lease(lease_duration=-1)
    assert blob.download_blob(lease=lease).readall() == b"new"
    assert blob.download_blob().readall() == b"new"
    assert blob.get_blob_properties(lease=lease).size == 3
    lease.release()
    for operation in [blob.download_blob, blob.get_block_list]:
        with pytest.raises(HttpResponseError) as error:
            operation(if_tags_match_condition='"tag" = \'value\'')
        assert error.value.status_code == 501
        assert error.value.error_code == "FeatureNotSupported"
    for operation in [
        lambda: blob.upload_blob(b"bad", overwrite=True, if_unmodified_since=datetime.now(timezone.utc)),
        lambda: blob.set_blob_metadata({"changed": "bad"}, if_modified_since=datetime.now(timezone.utc)),
        lambda: blob.delete_blob(if_unmodified_since=datetime.now(timezone.utc)),
    ]:
        with pytest.raises(HttpResponseError) as error:
            operation()
        assert error.value.status_code == 501
        assert error.value.error_code == "FeatureNotSupported"
        assert blob.download_blob().readall() == b"new"
        assert blob.get_blob_properties().metadata == {}
    service.delete_container(container.container_name)


def test_azure_core_blob_workflows(sqrzl_server):
    sqrzl_server.require_provider("azure")
    service = _service(sqrzl_server)
    container_name = sqrzl_server.bucket_name("sdk-azure-core")
    blob_name = "folder/hello.txt"

    container = service.create_container(container_name)
    blob = container.get_blob_client(blob_name)
    blob.upload_blob(
        b"hello azure sdk",
        overwrite=True,
        content_settings=azure_blob.ContentSettings(content_type="text/plain"),
        metadata={"owner": "support"},
    )

    properties = blob.get_blob_properties()
    assert properties.metadata["owner"] == "support"
    assert properties.size == len(b"hello azure sdk")

    assert blob.download_blob(offset=6, length=5).readall() == b"azure"
    assert [item.name for item in container.list_blobs(name_starts_with="folder/")] == [blob_name]
    assert container_name in [item.name for item in service.list_containers()]

    blob.delete_blob()
    service.delete_container(container_name)


def test_azure_block_blob_workflow(sqrzl_server):
    sqrzl_server.require_provider("azure")
    service = _service(sqrzl_server)
    container_name = sqrzl_server.bucket_name("sdk-azure-block")
    blob_name = "blocks/report.txt"

    container = service.create_container(container_name)
    blob = container.get_blob_client(blob_name)
    block_ids = [
        base64.b64encode(b"block-1").decode("ascii"),
        base64.b64encode(b"block-2").decode("ascii"),
    ]

    first_block = b"a" * (4 * 1024 * 1024)
    second_block = b"b" * (4 * 1024 * 1024)
    blob.stage_block(block_id=block_ids[0], data=first_block)
    blob.stage_block(block_id=block_ids[1], data=second_block)
    blob.commit_block_list(
        [azure_blob.BlobBlock(block_id=block_id) for block_id in block_ids],
        content_settings=azure_blob.ContentSettings(content_type="text/plain"),
    )

    assert blob.download_blob().readall() == first_block + second_block
    block_list = blob.get_block_list(block_list_type="committed")
    committed_blocks = (
        block_list[0]
        if isinstance(block_list, tuple)
        else block_list.committed_blocks
    )
    parsed_ids = [
        getattr(block, "id", None) or getattr(block, "name", None)
        for block in committed_blocks
    ]
    assert parsed_ids == block_ids

    blob.delete_blob()
    service.delete_container(container_name)


def test_azure_sas_block_blob_workflow(sqrzl_server):
    sqrzl_server.require_provider("azure")
    if not sqrzl_server.enforce_auth:
        pytest.skip("SAS verification requires the authenticated SDK lane")

    service = _service(sqrzl_server)
    container_name = sqrzl_server.bucket_name("sdk-azure-sas-block")
    blob_name = "large/signed.bin"
    container = service.create_container(container_name)
    token = azure_blob.generate_blob_sas(
        account_name=sqrzl_server.azure_account,
        container_name=container_name,
        blob_name=blob_name,
        account_key=sqrzl_server.azure_account_key,
        permission=azure_blob.BlobSasPermissions(
            read=True, create=True, write=True, delete=True
        ),
        expiry=datetime(2035, 1, 1, tzinfo=timezone.utc),
    )
    blob = azure_blob.BlobClient.from_blob_url(
        f"{sqrzl_server.api_url}/{sqrzl_server.azure_account}/{container_name}/{blob_name}?{token}"
    )
    block_ids = [
        base64.b64encode(b"signed-block-1").decode("ascii"),
        base64.b64encode(b"signed-block-2").decode("ascii"),
    ]

    first_block = b"s" * (4 * 1024 * 1024)
    second_block = b"b" * (4 * 1024 * 1024)
    blob.stage_block(block_id=block_ids[0], data=first_block)
    blob.stage_block(block_id=block_ids[1], data=second_block)
    blob.commit_block_list(
        [azure_blob.BlobBlock(block_id=block_id) for block_id in block_ids]
    )

    assert blob.download_blob().readall() == first_block + second_block
    blob.delete_blob()
    service.delete_container(container_name)


def test_azure_content_properties_and_upload_checksum_contract(sqrzl_server):
    sqrzl_server.require_provider("azure")
    import hashlib

    service = _service(sqrzl_server)
    container = service.create_container(
        sqrzl_server.bucket_name("sdk-azure-properties"), metadata={"owner": "contracts"}
    )
    assert container.get_container_properties().metadata["owner"] == "contracts"
    blob = container.get_blob_client("properties.txt")
    observed = {}

    def capture(response):
        observed.update({key.lower(): value for key, value in response.http_response.headers.items()})

    settings = azure_blob.ContentSettings(
        content_type="text/plain",
        cache_control="max-age=60",
        content_encoding="identity",
        content_language="en-US",
        content_disposition="attachment; filename=properties.txt",
    )
    payload = b"native sdk properties"
    blob.upload_blob(payload, overwrite=True, content_settings=settings, raw_response_hook=capture)
    assert base64.b64decode(observed["content-md5"]) == hashlib.md5(payload).digest()
    assert "x-ms-content-crc64" in observed
    properties = blob.get_blob_properties().content_settings
    for key in ("content_type", "cache_control", "content_encoding", "content_language", "content_disposition"):
        assert getattr(properties, key) == getattr(settings, key)
    listed = list(container.list_blobs())[0].content_settings
    assert listed.cache_control == "max-age=60"
    assert listed.content_language == "en-US"
    assert blob.download_blob().readall() == payload

    block_id = base64.b64encode(b"property-block").decode("ascii")
    observed.clear()
    blob.stage_block(block_id=block_id, data=payload, raw_response_hook=capture)
    assert "x-ms-content-crc64" in observed
    observed.clear()
    blob.commit_block_list(
        [azure_blob.BlobBlock(block_id=block_id)],
        content_settings=azure_blob.ContentSettings(cache_control="no-cache"),
        raw_response_hook=capture,
    )
    assert "x-ms-content-crc64" in observed
    properties = blob.get_blob_properties().content_settings
    assert properties.cache_control == "no-cache"
    assert properties.content_encoding is None
    blob.delete_blob()
    service.delete_container(container.container_name)


def test_azure_restricted_sas_permissions_and_signed_constraints(sqrzl_server):
    sqrzl_server.require_provider("azure")
    if not sqrzl_server.enforce_auth:
        pytest.skip("Restricted SAS verification requires the authenticated SDK lane")
    from azure.core.exceptions import HttpResponseError

    service = _service(sqrzl_server)
    container = service.create_container(sqrzl_server.bucket_name("sdk-azure-restricted"))
    common = {
        "account_name": sqrzl_server.azure_account,
        "container_name": container.container_name,
        "account_key": sqrzl_server.azure_account_key,
        "expiry": datetime(2035, 1, 1, tzinfo=timezone.utc),
    }
    base_url = f"{sqrzl_server.api_url}/{sqrzl_server.azure_account}/{container.container_name}"
    token = azure_blob.generate_blob_sas(
        **common, blob_name="create.txt", permission=azure_blob.BlobSasPermissions(create=True)
    )
    create_blob = azure_blob.BlobClient.from_blob_url(f"{base_url}/create.txt?{token}")
    create_blob.upload_blob(b"original", overwrite=True)
    with pytest.raises(HttpResponseError) as denied:
        create_blob.upload_blob(b"replacement", overwrite=True)
    assert denied.value.status_code == 403
    assert container.get_blob_client("create.txt").download_blob().readall() == b"original"

    token = azure_blob.generate_container_sas(
        **common,
        permission=azure_blob.ContainerSasPermissions(read=True, list=True),
        protocol="https,http",
    )
    scoped = azure_blob.ContainerClient.from_container_url(f"{base_url}?{token}")
    assert [blob.name for blob in scoped.list_blobs()] == ["create.txt"]
    assert scoped.get_blob_client("create.txt").download_blob().readall() == b"original"
    with pytest.raises(HttpResponseError) as denied:
        scoped.get_container_properties()
    assert denied.value.status_code == 403

    for restriction in ({"ip": "192.0.2.1"}, {"policy_id": "unsupported-policy"}, {"protocol": "https"}):
        token = azure_blob.generate_blob_sas(
            **common, blob_name="create.txt", permission=azure_blob.BlobSasPermissions(read=True), **restriction
        )
        restricted = azure_blob.BlobClient.from_blob_url(f"{base_url}/create.txt?{token}")
        with pytest.raises(HttpResponseError) as denied:
            restricted.download_blob()
        assert denied.value.status_code == 403

    append = container.get_blob_client("append.txt")
    append.create_append_blob()
    token = azure_blob.generate_blob_sas(
        **common, blob_name="append.txt", permission=azure_blob.BlobSasPermissions(read=True, add=True)
    )
    restricted = azure_blob.BlobClient.from_blob_url(f"{base_url}/append.txt?{token}")
    restricted.append_block(b"added")
    assert append.download_blob().readall() == b"added"
    append.delete_blob()
    container.get_blob_client("create.txt").delete_blob()
    service.delete_container(container.container_name)


def test_azure_native_page_blob_extent_and_range_mutation(sqrzl_server):
    sqrzl_server.require_provider("azure")
    from azure.core.exceptions import HttpResponseError

    service = _service(sqrzl_server)
    container = service.create_container(sqrzl_server.bucket_name("sdk-azure-page"))
    page = container.get_blob_client("page.bin")
    page.create_page_blob(size=1024)
    assert page.download_blob().readall() == bytes(1024)
    page.upload_page(b"p" * 512, offset=0, length=512)
    assert page.download_blob().readall() == b"p" * 512 + bytes(512)
    page.clear_page(offset=0, length=512)
    assert page.download_blob().readall() == bytes(1024)
    with pytest.raises(HttpResponseError) as denied:
        page.create_page_blob(size=64 * 1024 * 1024 + 512)
    assert denied.value.status_code == 501
    assert page.download_blob().readall() == bytes(1024)
    page.delete_blob()
    service.delete_container(container.container_name)
