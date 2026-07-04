from __future__ import annotations

import base64

import pytest


azure_blob = pytest.importorskip("azure.storage.blob")


def _service(sqrzl_server):
    return azure_blob.BlobServiceClient(
        account_url=f"{sqrzl_server.api_url}/{sqrzl_server.azure_account}",
        credential=None,
    )


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

    blob.stage_block(block_id=block_ids[0], data=b"first-")
    blob.stage_block(block_id=block_ids[1], data=b"second")
    blob.commit_block_list(
        [azure_blob.BlobBlock(block_id=block_id) for block_id in block_ids],
        content_settings=azure_blob.ContentSettings(content_type="text/plain"),
    )

    assert blob.download_blob().readall() == b"first-second"
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
