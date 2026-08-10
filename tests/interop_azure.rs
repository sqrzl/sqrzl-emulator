mod common;

use bytes::Bytes;
use common::interop::{
    auth_disabled, body_bytes, body_text, call, request as raw_request, temp_storage, AZURE_VERSION,
};
use http_body_util::Full;
use hyper::{Request as HyperRequest, StatusCode};
use sqrzl_emulator::storage::Storage;
use std::sync::Arc;

fn request(
    method: &str,
    uri: &str,
    headers: &[(&str, &str)],
    body: &[u8],
) -> HyperRequest<Full<Bytes>> {
    let content_length = body.len().to_string();
    let mut headers = headers.to_vec();
    if method == "PUT"
        && !headers
            .iter()
            .any(|(name, _)| name.eq_ignore_ascii_case("content-length"))
    {
        headers.push(("content-length", &content_length));
    }
    raw_request(method, uri, &headers, body)
}

#[tokio::test(flavor = "multi_thread")]
async fn should_round_trip_block_blob_given_container_exists_when_using_basic_blob_operations() {
    let storage = temp_storage();
    call(
        storage.clone(),
        auth_disabled(),
        request(
            "PUT",
            "http://localhost/devstoreaccount1/interop-azure?restype=container",
            &[("x-ms-version", AZURE_VERSION)],
            b"",
        ),
    )
    .await;
    let response = call(
        storage.clone(),
        auth_disabled(),
        request(
            "PUT",
            "http://localhost/devstoreaccount1/interop-azure/hello.txt",
            &[
                ("x-ms-version", AZURE_VERSION),
                ("x-ms-blob-type", "BlockBlob"),
                ("content-type", "text/plain"),
            ],
            b"azure smoke",
        ),
    )
    .await;
    assert_eq!(response.status(), StatusCode::CREATED);

    let body = body_bytes(
        call(
            storage,
            auth_disabled(),
            request(
                "GET",
                "http://localhost/devstoreaccount1/interop-azure/hello.txt",
                &[("x-ms-version", AZURE_VERSION)],
                b"",
            ),
        )
        .await,
    )
    .await;
    assert_eq!(body, b"azure smoke");
}

#[tokio::test(flavor = "multi_thread")]
async fn should_return_custom_metadata_given_blob_metadata_headers_when_requesting_blob_head() {
    let storage = temp_storage();
    call(
        storage.clone(),
        auth_disabled(),
        request(
            "PUT",
            "http://localhost/devstoreaccount1/interop-azure?restype=container",
            &[("x-ms-version", AZURE_VERSION)],
            b"",
        ),
    )
    .await;
    call(
        storage.clone(),
        auth_disabled(),
        request(
            "PUT",
            "http://localhost/devstoreaccount1/interop-azure/hello.txt",
            &[
                ("x-ms-version", AZURE_VERSION),
                ("x-ms-blob-type", "BlockBlob"),
                ("x-ms-meta-owner", "sdk"),
            ],
            b"azure smoke",
        ),
    )
    .await;
    let response = call(
        storage,
        auth_disabled(),
        request(
            "HEAD",
            "http://localhost/devstoreaccount1/interop-azure/hello.txt",
            &[("x-ms-version", AZURE_VERSION)],
            b"",
        ),
    )
    .await;
    assert_eq!(
        response
            .headers()
            .get("x-ms-meta-owner")
            .and_then(|value| value.to_str().ok()),
        Some("sdk")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn should_return_blob_not_found_given_missing_lease_blob_when_requesting_blob_head() {
    let storage = temp_storage();
    call(
        storage.clone(),
        auth_disabled(),
        request(
            "PUT",
            "http://localhost/sqrzl-access/cassie?restype=container",
            &[("x-ms-version", AZURE_VERSION)],
            b"",
        ),
    )
    .await;
    let response = call(
        storage,
        auth_disabled(),
        request(
            "HEAD",
            "http://localhost/sqrzl-access/cassie/midge_primary_lease.json",
            &[("x-ms-version", AZURE_VERSION)],
            b"",
        ),
    )
    .await;

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(
        response
            .headers()
            .get("x-ms-error-code")
            .and_then(|value| value.to_str().ok()),
        Some("BlobNotFound")
    );
    assert_eq!(
        response
            .headers()
            .get("x-ms-version")
            .and_then(|value| value.to_str().ok()),
        Some(AZURE_VERSION)
    );
    assert!(response.headers().get("x-ms-request-id").is_some());
    assert!(body_bytes(response).await.is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn should_return_blob_not_found_given_missing_lease_blob_when_requesting_blob_get() {
    let storage = temp_storage();
    call(
        storage.clone(),
        auth_disabled(),
        request(
            "PUT",
            "http://localhost/sqrzl-access/cassie?restype=container",
            &[("x-ms-version", AZURE_VERSION)],
            b"",
        ),
    )
    .await;
    let response = call(
        storage,
        auth_disabled(),
        request(
            "GET",
            "http://localhost/sqrzl-access/cassie/midge_primary_lease.json",
            &[("x-ms-version", AZURE_VERSION)],
            b"",
        ),
    )
    .await;

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(
        response
            .headers()
            .get("x-ms-error-code")
            .and_then(|value| value.to_str().ok()),
        Some("BlobNotFound")
    );
    assert_eq!(
        response
            .headers()
            .get("x-ms-version")
            .and_then(|value| value.to_str().ok()),
        Some(AZURE_VERSION)
    );
    assert!(response
        .headers()
        .get("x-ms-request-id")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|id| !id.is_empty()));
    let body = body_text(response).await;
    assert!(body.contains("<Code>BlobNotFound</Code>"));
}

#[tokio::test(flavor = "multi_thread")]
async fn should_return_requested_slice_given_range_header_when_reading_blob_content() {
    let storage = temp_storage();
    call(
        storage.clone(),
        auth_disabled(),
        request(
            "PUT",
            "http://localhost/devstoreaccount1/interop-azure?restype=container",
            &[("x-ms-version", AZURE_VERSION)],
            b"",
        ),
    )
    .await;
    call(
        storage.clone(),
        auth_disabled(),
        request(
            "PUT",
            "http://localhost/devstoreaccount1/interop-azure/hello.txt",
            &[
                ("x-ms-version", AZURE_VERSION),
                ("x-ms-blob-type", "BlockBlob"),
            ],
            b"azure smoke",
        ),
    )
    .await;
    let body = body_bytes(
        call(
            storage,
            auth_disabled(),
            request(
                "GET",
                "http://localhost/devstoreaccount1/interop-azure/hello.txt",
                &[("x-ms-version", AZURE_VERSION), ("x-ms-range", "bytes=0-4")],
                b"",
            ),
        )
        .await,
    )
    .await;
    assert_eq!(body, b"azure");
}

#[tokio::test(flavor = "multi_thread")]
async fn should_list_containers_and_blobs_given_stored_objects_when_querying_azure_lists() {
    let storage = temp_storage();
    call(
        storage.clone(),
        auth_disabled(),
        request(
            "PUT",
            "http://localhost/devstoreaccount1/interop-azure?restype=container",
            &[("x-ms-version", AZURE_VERSION)],
            b"",
        ),
    )
    .await;
    call(
        storage.clone(),
        auth_disabled(),
        request(
            "PUT",
            "http://localhost/devstoreaccount1/interop-azure/hello.txt",
            &[
                ("x-ms-version", AZURE_VERSION),
                ("x-ms-blob-type", "BlockBlob"),
            ],
            b"azure smoke",
        ),
    )
    .await;
    let containers = body_text(
        call(
            storage.clone(),
            auth_disabled(),
            request(
                "GET",
                "http://localhost/devstoreaccount1?comp=list",
                &[
                    ("host", "storage.localhost:9443"),
                    ("x-forwarded-proto", "https"),
                    ("x-ms-version", AZURE_VERSION),
                ],
                b"",
            ),
        )
        .await,
    )
    .await;
    assert!(containers.contains("interop-azure"));
    assert!(
        containers.contains("ServiceEndpoint=\"https://storage.localhost:9443/devstoreaccount1\"")
    );

    let blobs = body_text(
        call(
            storage,
            auth_disabled(),
            request(
                "GET",
                "http://localhost/devstoreaccount1/interop-azure?restype=container&comp=list&prefix=hell",
                &[("x-ms-version", AZURE_VERSION)],
                b"",
            ),
        )
        .await,
    )
    .await;
    assert!(blobs.contains("hello.txt"));
}

#[tokio::test(flavor = "multi_thread")]
#[allow(clippy::too_many_lines)] // One end-to-end assertion sequence covers both specialized blob types.
async fn should_persist_append_and_page_blob_writes_given_specialized_blob_types_when_uploading_content(
) {
    let storage = temp_storage();
    call(
        storage.clone(),
        auth_disabled(),
        request(
            "PUT",
            "http://localhost/devstoreaccount1/state?restype=container",
            &[("x-ms-version", AZURE_VERSION)],
            b"",
        ),
    )
    .await;
    call(
        storage.clone(),
        auth_disabled(),
        request(
            "PUT",
            "http://localhost/devstoreaccount1/state/events.log",
            &[
                ("x-ms-version", AZURE_VERSION),
                ("x-ms-blob-type", "AppendBlob"),
            ],
            b"",
        ),
    )
    .await;
    call(
        storage.clone(),
        auth_disabled(),
        request(
            "PUT",
            "http://localhost/devstoreaccount1/state/events.log?comp=appendblock",
            &[("x-ms-version", AZURE_VERSION)],
            b"hello",
        ),
    )
    .await;
    call(
        storage.clone(),
        auth_disabled(),
        request(
            "PUT",
            "http://localhost/devstoreaccount1/state/events.log?comp=appendblock",
            &[("x-ms-version", AZURE_VERSION)],
            b" azure",
        ),
    )
    .await;
    let append = body_bytes(
        call(
            storage.clone(),
            auth_disabled(),
            request(
                "GET",
                "http://localhost/devstoreaccount1/state/events.log",
                &[("x-ms-version", AZURE_VERSION)],
                b"",
            ),
        )
        .await,
    )
    .await;
    assert_eq!(append, b"hello azure");
    let stale_append = call(
        storage.clone(),
        auth_disabled(),
        request(
            "PUT",
            "http://localhost/devstoreaccount1/state/events.log?comp=appendblock",
            &[("x-ms-version", AZURE_VERSION), ("if-match", "\"stale\"")],
            b" should-not-appear",
        ),
    )
    .await;
    assert_eq!(stale_append.status(), StatusCode::PRECONDITION_FAILED);
    assert_eq!(
        storage
            .get_object("state", "events.log")
            .expect("append blob should remain")
            .data,
        b"hello azure"
    );

    call(
        storage.clone(),
        auth_disabled(),
        request(
            "PUT",
            "http://localhost/devstoreaccount1/state/page.bin",
            &[
                ("x-ms-version", AZURE_VERSION),
                ("x-ms-blob-type", "PageBlob"),
                ("x-ms-blob-content-length", "512"),
            ],
            b"",
        ),
    )
    .await;
    let stale_page = call(
        storage.clone(),
        auth_disabled(),
        request(
            "PUT",
            "http://localhost/devstoreaccount1/state/page.bin?comp=page",
            &[
                ("x-ms-version", AZURE_VERSION),
                ("x-ms-range", "bytes=0-511"),
                ("x-ms-page-write", "update"),
                ("if-match", "\"stale\""),
            ],
            &vec![b'c'; 512],
        ),
    )
    .await;
    assert_eq!(stale_page.status(), StatusCode::PRECONDITION_FAILED);
    assert_eq!(
        storage
            .get_object("state", "page.bin")
            .expect("page blob should remain")
            .data,
        vec![0_u8; 512]
    );
    call(
        storage.clone(),
        auth_disabled(),
        request(
            "PUT",
            "http://localhost/devstoreaccount1/state/page.bin?comp=page",
            &[
                ("x-ms-version", AZURE_VERSION),
                ("x-ms-range", "bytes=0-511"),
                ("x-ms-page-write", "update"),
            ],
            &vec![b'b'; 512],
        ),
    )
    .await;
    let page = body_bytes(
        call(
            storage,
            auth_disabled(),
            request(
                "GET",
                "http://localhost/devstoreaccount1/state/page.bin",
                &[("x-ms-version", AZURE_VERSION), ("x-ms-range", "bytes=0-7")],
                b"",
            ),
        )
        .await,
    )
    .await;
    assert_eq!(page, b"bbbbbbbb");
}

#[tokio::test(flavor = "multi_thread")]
async fn should_enforce_leases_and_retention_given_snapshot_and_immutability_operations_when_deleting_blob(
) {
    let storage = temp_storage();
    create_state_container_and_blob(storage.clone()).await;
    acquire_release_and_verify_lease(storage.clone()).await;
    create_and_verify_snapshot(storage.clone()).await;
    enable_immutability_and_legal_hold(storage.clone()).await;

    assert_eq!(
        call(
            storage,
            auth_disabled(),
            request(
                "DELETE",
                "http://localhost/devstoreaccount1/state/lease.txt",
                &[("x-ms-version", AZURE_VERSION)],
                b"",
            ),
        )
        .await
        .status(),
        StatusCode::CONFLICT
    );
}

async fn create_state_container_and_blob(storage: Arc<dyn Storage>) {
    call(
        storage.clone(),
        auth_disabled(),
        request(
            "PUT",
            "http://localhost/devstoreaccount1/state?restype=container",
            &[("x-ms-version", AZURE_VERSION)],
            b"",
        ),
    )
    .await;
    call(
        storage,
        auth_disabled(),
        request(
            "PUT",
            "http://localhost/devstoreaccount1/state/lease.txt",
            &[
                ("x-ms-version", AZURE_VERSION),
                ("x-ms-blob-type", "BlockBlob"),
            ],
            b"initial",
        ),
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn should_delete_nonempty_container_given_delete_container_request() {
    let storage = temp_storage();
    call(
        storage.clone(),
        auth_disabled(),
        request(
            "PUT",
            "http://localhost/devstoreaccount1/delete-me?restype=container",
            &[("x-ms-version", AZURE_VERSION)],
            b"",
        ),
    )
    .await;
    call(
        storage.clone(),
        auth_disabled(),
        request(
            "PUT",
            "http://localhost/devstoreaccount1/delete-me/blob",
            &[
                ("x-ms-version", AZURE_VERSION),
                ("x-ms-blob-type", "BlockBlob"),
            ],
            b"data",
        ),
    )
    .await;

    let response = call(
        storage.clone(),
        auth_disabled(),
        request(
            "DELETE",
            "http://localhost/devstoreaccount1/delete-me?restype=container",
            &[
                ("x-ms-version", AZURE_VERSION),
                ("x-sqrzl-azure-delete-delay-ms", "1000"),
            ],
            b"",
        ),
    )
    .await;

    assert_eq!(response.status(), StatusCode::ACCEPTED);
    let recreating = call(
        storage.clone(),
        auth_disabled(),
        request(
            "PUT",
            "http://localhost/devstoreaccount1/delete-me?restype=container",
            &[("x-ms-version", AZURE_VERSION)],
            b"",
        ),
    )
    .await;
    assert_eq!(recreating.status(), StatusCode::CONFLICT);
    assert_eq!(
        recreating
            .headers()
            .get("x-ms-error-code")
            .and_then(|value| value.to_str().ok()),
        Some("ContainerBeingDeleted")
    );

    tokio::time::sleep(std::time::Duration::from_millis(1050)).await;
    let recreated = call(
        storage.clone(),
        auth_disabled(),
        request(
            "PUT",
            "http://localhost/devstoreaccount1/delete-me?restype=container",
            &[("x-ms-version", AZURE_VERSION)],
            b"",
        ),
    )
    .await;
    assert_eq!(recreated.status(), StatusCode::CREATED);
    assert!(storage.get_object("delete-me", "blob").is_err());
}

#[tokio::test(flavor = "multi_thread")]
#[allow(clippy::too_many_lines)] // Keep all failed-condition mutations beside the shared no-mutation assertions.
async fn should_not_apply_azure_subresource_mutations_given_stale_or_weak_conditions() {
    // Arrange
    let storage = temp_storage();
    create_state_container_and_blob(storage.clone()).await;
    let head = call(
        storage.clone(),
        auth_disabled(),
        request(
            "HEAD",
            "http://localhost/devstoreaccount1/state/lease.txt",
            &[("x-ms-version", AZURE_VERSION)],
            b"",
        ),
    )
    .await;
    let etag = head
        .headers()
        .get("etag")
        .and_then(|value| value.to_str().ok())
        .expect("etag should exist")
        .to_string();

    // Act
    let stale_metadata = call(
        storage.clone(),
        auth_disabled(),
        request(
            "PUT",
            "http://localhost/devstoreaccount1/state/lease.txt?comp=metadata",
            &[
                ("x-ms-version", AZURE_VERSION),
                ("if-match", "\"stale\""),
                ("x-ms-meta-owner", "changed"),
            ],
            b"",
        ),
    )
    .await;
    let weak_snapshot = call(
        storage.clone(),
        auth_disabled(),
        request(
            "PUT",
            "http://localhost/devstoreaccount1/state/lease.txt?comp=snapshot",
            &[
                ("x-ms-version", AZURE_VERSION),
                ("if-match", &format!("W/{etag}")),
            ],
            b"",
        ),
    )
    .await;
    let weak_if_none_put = call(
        storage.clone(),
        auth_disabled(),
        request(
            "PUT",
            "http://localhost/devstoreaccount1/state/lease.txt",
            &[
                ("x-ms-version", AZURE_VERSION),
                ("x-ms-blob-type", "BlockBlob"),
                ("if-none-match", &format!("W/{etag}")),
            ],
            b"changed",
        ),
    )
    .await;
    call(
        storage.clone(),
        auth_disabled(),
        request(
            "PUT",
            "http://localhost/devstoreaccount1/state/lease.txt?comp=block&blockid=YmxvY2s=",
            &[("x-ms-version", AZURE_VERSION)],
            b"changed by block list",
        ),
    )
    .await;
    let stale_block_list = call(
        storage.clone(),
        auth_disabled(),
        request(
            "PUT",
            "http://localhost/devstoreaccount1/state/lease.txt?comp=blocklist",
            &[
                ("x-ms-version", AZURE_VERSION),
                ("content-type", "application/xml"),
                ("if-match", "\"stale\""),
            ],
            br"<BlockList><Latest>YmxvY2s=</Latest></BlockList>",
        ),
    )
    .await;

    // Assert
    assert_eq!(stale_metadata.status(), StatusCode::PRECONDITION_FAILED);
    assert_eq!(weak_snapshot.status(), StatusCode::PRECONDITION_FAILED);
    assert_eq!(weak_if_none_put.status(), StatusCode::PRECONDITION_FAILED);
    assert_eq!(stale_block_list.status(), StatusCode::PRECONDITION_FAILED);
    let stored = storage
        .get_object("state", "lease.txt")
        .expect("base blob should remain");
    assert!(!stored.metadata.contains_key("owner"));
    let listing = body_text(
        call(
            storage,
            auth_disabled(),
            request(
                "GET",
                "http://localhost/devstoreaccount1/state?restype=container&comp=list",
                &[("x-ms-version", AZURE_VERSION)],
                b"",
            ),
        )
        .await,
    )
    .await;
    assert!(!listing.contains("__sqrzl_azure_snapshot__"));
}

#[tokio::test(flavor = "multi_thread")]
async fn should_read_selected_azure_version_bytes_given_a_range_request() {
    // Arrange
    let storage = temp_storage();
    let container = call(
        storage.clone(),
        auth_disabled(),
        request(
            "PUT",
            "http://localhost/devstoreaccount1/versions?restype=container",
            &[
                ("x-ms-version", AZURE_VERSION),
                ("x-sqrzl-azure-versioning-enabled", "true"),
            ],
            b"",
        ),
    )
    .await;
    assert_eq!(container.status(), StatusCode::CREATED);
    let first = call(
        storage.clone(),
        auth_disabled(),
        request(
            "PUT",
            "http://localhost/devstoreaccount1/versions/value",
            &[
                ("x-ms-version", AZURE_VERSION),
                ("x-ms-blob-type", "BlockBlob"),
            ],
            b"first",
        ),
    )
    .await;
    let version_id = first
        .headers()
        .get("x-ms-version-id")
        .and_then(|value| value.to_str().ok())
        .expect("version id should exist")
        .to_string();
    call(
        storage.clone(),
        auth_disabled(),
        request(
            "PUT",
            "http://localhost/devstoreaccount1/versions/value",
            &[
                ("x-ms-version", AZURE_VERSION),
                ("x-ms-blob-type", "BlockBlob"),
            ],
            b"later",
        ),
    )
    .await;

    // Act
    let ranged = call(
        storage.clone(),
        auth_disabled(),
        request(
            "GET",
            &format!("http://localhost/devstoreaccount1/versions/value?versionid={version_id}"),
            &[("x-ms-version", AZURE_VERSION), ("range", "bytes=1-3")],
            b"",
        ),
    )
    .await;

    // Assert
    assert_eq!(ranged.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(
        ranged
            .headers()
            .get("x-ms-is-current-version")
            .and_then(|value| value.to_str().ok()),
        Some("false")
    );
    assert_eq!(body_bytes(ranged).await, b"irs");

    let stale_delete = call(
        storage.clone(),
        auth_disabled(),
        request(
            "DELETE",
            &format!("http://localhost/devstoreaccount1/versions/value?versionid={version_id}"),
            &[("x-ms-version", AZURE_VERSION), ("if-match", "\"stale\"")],
            b"",
        ),
    )
    .await;
    assert_eq!(stale_delete.status(), StatusCode::PRECONDITION_FAILED);
    assert_eq!(
        call(
            storage,
            auth_disabled(),
            request(
                "HEAD",
                &format!("http://localhost/devstoreaccount1/versions/value?versionid={version_id}"),
                &[("x-ms-version", AZURE_VERSION)],
                b"",
            ),
        )
        .await
        .status(),
        StatusCode::OK
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn should_page_azure_blob_prefixes_without_exposing_snapshot_storage_keys() {
    // Arrange
    let storage = temp_storage();
    call(
        storage.clone(),
        auth_disabled(),
        request(
            "PUT",
            "http://localhost/devstoreaccount1/listing?restype=container",
            &[("x-ms-version", AZURE_VERSION)],
            b"",
        ),
    )
    .await;
    for key in ["a/one", "b/one", "z"] {
        call(
            storage.clone(),
            auth_disabled(),
            request(
                "PUT",
                &format!("http://localhost/devstoreaccount1/listing/{key}"),
                &[
                    ("x-ms-version", AZURE_VERSION),
                    ("x-ms-blob-type", "BlockBlob"),
                ],
                b"data",
            ),
        )
        .await;
    }
    call(
        storage.clone(),
        auth_disabled(),
        request(
            "PUT",
            "http://localhost/devstoreaccount1/listing/z?comp=snapshot",
            &[("x-ms-version", AZURE_VERSION)],
            b"",
        ),
    )
    .await;

    // Act
    let first = body_text(
        call(
            storage.clone(),
            auth_disabled(),
            request(
                "GET",
                "http://localhost/devstoreaccount1/listing?restype=container&comp=list&delimiter=/&maxresults=1",
                &[("x-ms-version", AZURE_VERSION)],
                b"",
            ),
        )
        .await,
    )
    .await;
    let flat = body_text(
        call(
            storage.clone(),
            auth_disabled(),
            request(
                "GET",
                "http://localhost/devstoreaccount1/listing?restype=container&comp=list&maxresults=10",
                &[("x-ms-version", AZURE_VERSION)],
                b"",
            ),
        )
        .await,
    )
    .await;
    let second = body_text(
        call(
            storage,
            auth_disabled(),
            request(
                "GET",
                "http://localhost/devstoreaccount1/listing?restype=container&comp=list&delimiter=/&maxresults=1&marker=b/",
                &[("x-ms-version", AZURE_VERSION)],
                b"",
            ),
        )
        .await,
    )
    .await;

    // Assert
    assert!(first.contains("<BlobPrefix><Name>a/</Name></BlobPrefix>"));
    assert!(first.contains("<NextMarker>b/</NextMarker>"));
    assert!(!first.contains("<BlobPrefix><Name>b/</Name></BlobPrefix>"));
    assert!(second.contains("<BlobPrefix><Name>b/</Name></BlobPrefix>"));
    assert!(second.contains("<NextMarker>z</NextMarker>"));
    assert!(!flat.contains("__sqrzl_azure_snapshot__"));
}

#[tokio::test(flavor = "multi_thread")]
async fn should_require_real_blob_type_and_specialized_blob_creation_headers() {
    let storage = temp_storage();
    call(
        storage.clone(),
        auth_disabled(),
        request(
            "PUT",
            "http://localhost/devstoreaccount1/framing?restype=container",
            &[("x-ms-version", AZURE_VERSION)],
            b"",
        ),
    )
    .await;

    for (name, headers, body, expected_code) in [
        (
            "missing-type",
            vec![("x-ms-version", AZURE_VERSION)],
            b"data".as_slice(),
            "MissingRequiredHeader",
        ),
        (
            "append-with-body",
            vec![
                ("x-ms-version", AZURE_VERSION),
                ("x-ms-blob-type", "AppendBlob"),
            ],
            b"data".as_slice(),
            "InvalidHeaderValue",
        ),
        (
            "page-without-length",
            vec![
                ("x-ms-version", AZURE_VERSION),
                ("x-ms-blob-type", "PageBlob"),
            ],
            b"".as_slice(),
            "MissingRequiredHeader",
        ),
    ] {
        let response = call(
            storage.clone(),
            auth_disabled(),
            request(
                "PUT",
                &format!("http://localhost/devstoreaccount1/framing/{name}"),
                &headers,
                body,
            ),
        )
        .await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            response
                .headers()
                .get("x-ms-error-code")
                .and_then(|value| value.to_str().ok()),
            Some(expected_code)
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn should_require_content_length_for_azure_put_blob_without_mutating() {
    let storage = temp_storage();
    call(
        storage.clone(),
        auth_disabled(),
        request(
            "PUT",
            "http://localhost/devstoreaccount1/content-length?restype=container",
            &[("x-ms-version", AZURE_VERSION)],
            b"",
        ),
    )
    .await;
    storage
        .put_object(
            "content-length",
            "existing".to_string(),
            sqrzl_emulator::models::Object::new(
                "existing".to_string(),
                b"preserve".to_vec(),
                "application/octet-stream".to_string(),
            ),
        )
        .expect("existing blob fixture should be written");

    for (name, body, transfer_encoding) in [
        ("missing", b"".as_slice(), None),
        ("existing", b"replacement".as_slice(), None),
        ("chunked", b"streamed".as_slice(), Some("chunked")),
    ] {
        let mut headers = vec![
            ("x-ms-version", AZURE_VERSION),
            ("x-ms-blob-type", "BlockBlob"),
        ];
        if let Some(transfer_encoding) = transfer_encoding {
            headers.push(("transfer-encoding", transfer_encoding));
        }
        let response = call(
            storage.clone(),
            auth_disabled(),
            raw_request(
                "PUT",
                &format!("http://localhost/devstoreaccount1/content-length/{name}"),
                &headers,
                body,
            ),
        )
        .await;
        assert_eq!(response.status(), StatusCode::LENGTH_REQUIRED);
        assert_eq!(
            response
                .headers()
                .get("x-ms-error-code")
                .and_then(|value| value.to_str().ok()),
            Some("MissingContentLengthHeader")
        );
    }

    assert!(storage.get_object("content-length", "missing").is_err());
    assert!(storage.get_object("content-length", "chunked").is_err());
    assert_eq!(
        storage
            .get_object("content-length", "existing")
            .expect("rejected overwrite must preserve existing blob")
            .data,
        b"preserve"
    );

    let response = call(
        storage.clone(),
        auth_disabled(),
        raw_request(
            "PUT",
            "http://localhost/devstoreaccount1/content-length/empty",
            &[
                ("x-ms-version", AZURE_VERSION),
                ("x-ms-blob-type", "BlockBlob"),
                ("content-length", "0"),
            ],
            b"",
        ),
    )
    .await;
    assert_eq!(response.status(), StatusCode::CREATED);
    assert!(storage
        .get_object("content-length", "empty")
        .expect("explicit zero-length blob should be stored")
        .data
        .is_empty());
}

#[tokio::test(flavor = "multi_thread")]
#[allow(clippy::too_many_lines)]
async fn should_require_content_length_for_azure_block_append_and_page_mutations() {
    // Arrange
    let storage = temp_storage();
    call(
        storage.clone(),
        auth_disabled(),
        request(
            "PUT",
            "http://localhost/devstoreaccount1/framed-operations?restype=container",
            &[("x-ms-version", AZURE_VERSION)],
            b"",
        ),
    )
    .await;
    let block_id = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, "block-001");
    let encoded_block_id = urlencoding::encode(&block_id);
    let staged_uri = format!(
        "http://localhost/devstoreaccount1/framed-operations/staged?comp=block&blockid={encoded_block_id}"
    );
    let committed_uri =
        "http://localhost/devstoreaccount1/framed-operations/committed?comp=blocklist";
    let committed_block_uri = format!(
        "http://localhost/devstoreaccount1/framed-operations/committed?comp=block&blockid={encoded_block_id}"
    );
    call(
        storage.clone(),
        auth_disabled(),
        request(
            "PUT",
            &committed_block_uri,
            &[("x-ms-version", AZURE_VERSION)],
            b"preserved",
        ),
    )
    .await;
    call(
        storage.clone(),
        auth_disabled(),
        request(
            "PUT",
            "http://localhost/devstoreaccount1/framed-operations/append",
            &[
                ("x-ms-version", AZURE_VERSION),
                ("x-ms-blob-type", "AppendBlob"),
            ],
            b"",
        ),
    )
    .await;
    call(
        storage.clone(),
        auth_disabled(),
        request(
            "PUT",
            "http://localhost/devstoreaccount1/framed-operations/page",
            &[
                ("x-ms-version", AZURE_VERSION),
                ("x-ms-blob-type", "PageBlob"),
                ("x-ms-blob-content-length", "512"),
            ],
            b"",
        ),
    )
    .await;
    let block_list = format!("<BlockList><Latest>{block_id}</Latest></BlockList>");

    // Act
    let missing_put_block = call(
        storage.clone(),
        auth_disabled(),
        raw_request(
            "PUT",
            &staged_uri,
            &[("x-ms-version", AZURE_VERSION)],
            b"must-not-stage",
        ),
    )
    .await;
    let missing_block_list = call(
        storage.clone(),
        auth_disabled(),
        raw_request(
            "PUT",
            committed_uri,
            &[("x-ms-version", AZURE_VERSION)],
            block_list.as_bytes(),
        ),
    )
    .await;
    let missing_append = call(
        storage.clone(),
        auth_disabled(),
        raw_request(
            "PUT",
            "http://localhost/devstoreaccount1/framed-operations/append?comp=appendblock",
            &[("x-ms-version", AZURE_VERSION)],
            b"must-not-append",
        ),
    )
    .await;
    let missing_page = call(
        storage.clone(),
        auth_disabled(),
        raw_request(
            "PUT",
            "http://localhost/devstoreaccount1/framed-operations/page?comp=page",
            &[
                ("x-ms-version", AZURE_VERSION),
                ("x-ms-page-write", "update"),
                ("x-ms-range", "bytes=0-511"),
            ],
            &[1_u8; 512],
        ),
    )
    .await;
    let retry_block_list = call(
        storage.clone(),
        auth_disabled(),
        request(
            "PUT",
            committed_uri,
            &[("x-ms-version", AZURE_VERSION)],
            block_list.as_bytes(),
        ),
    )
    .await;

    // Assert
    for response in [
        missing_put_block,
        missing_block_list,
        missing_append,
        missing_page,
    ] {
        assert_eq!(response.status(), StatusCode::LENGTH_REQUIRED);
        assert_eq!(
            response
                .headers()
                .get("x-ms-error-code")
                .and_then(|value| value.to_str().ok()),
            Some("MissingContentLengthHeader")
        );
    }
    assert!(storage.get_object("framed-operations", "staged").is_err());
    assert_eq!(retry_block_list.status(), StatusCode::CREATED);
    assert_eq!(
        storage
            .get_object("framed-operations", "committed")
            .expect("failed framing must retain staged blocks for retry")
            .data,
        b"preserved"
    );
    assert!(storage
        .get_object("framed-operations", "append")
        .expect("append blob should remain")
        .data
        .is_empty());
    assert!(storage
        .get_object("framed-operations", "page")
        .expect("page blob should remain")
        .data
        .iter()
        .all(|byte| *byte == 0));
}

#[tokio::test(flavor = "multi_thread")]
async fn should_require_snapshot_delete_mode_and_preserve_base_on_only() {
    let storage = temp_storage();
    create_state_container_and_blob(storage.clone()).await;
    let snapshot = call(
        storage.clone(),
        auth_disabled(),
        request(
            "PUT",
            "http://localhost/devstoreaccount1/state/lease.txt?comp=snapshot",
            &[("x-ms-version", AZURE_VERSION)],
            b"",
        ),
    )
    .await;
    assert_eq!(snapshot.status(), StatusCode::CREATED);
    let snapshot_time = snapshot
        .headers()
        .get("x-ms-snapshot")
        .and_then(|value| value.to_str().ok())
        .expect("snapshot timestamp")
        .to_string();
    let stale_snapshot_delete = call(
        storage.clone(),
        auth_disabled(),
        request(
            "DELETE",
            &format!("http://localhost/devstoreaccount1/state/lease.txt?snapshot={snapshot_time}"),
            &[("x-ms-version", AZURE_VERSION), ("if-match", "\"stale\"")],
            b"",
        ),
    )
    .await;
    assert_eq!(
        stale_snapshot_delete.status(),
        StatusCode::PRECONDITION_FAILED
    );
    assert_eq!(
        call(
            storage.clone(),
            auth_disabled(),
            request(
                "HEAD",
                &format!(
                    "http://localhost/devstoreaccount1/state/lease.txt?snapshot={snapshot_time}"
                ),
                &[("x-ms-version", AZURE_VERSION)],
                b"",
            ),
        )
        .await
        .status(),
        StatusCode::OK
    );

    let denied = call(
        storage.clone(),
        auth_disabled(),
        request(
            "DELETE",
            "http://localhost/devstoreaccount1/state/lease.txt",
            &[("x-ms-version", AZURE_VERSION)],
            b"",
        ),
    )
    .await;
    assert_eq!(denied.status(), StatusCode::CONFLICT);
    assert_eq!(
        denied
            .headers()
            .get("x-ms-error-code")
            .and_then(|value| value.to_str().ok()),
        Some("SnapshotsPresent")
    );

    let only = call(
        storage.clone(),
        auth_disabled(),
        request(
            "DELETE",
            "http://localhost/devstoreaccount1/state/lease.txt",
            &[
                ("x-ms-version", AZURE_VERSION),
                ("x-ms-delete-snapshots", "only"),
            ],
            b"",
        ),
    )
    .await;
    assert_eq!(only.status(), StatusCode::ACCEPTED);
    assert!(storage.get_object("state", "lease.txt").is_ok());
}

#[tokio::test(flavor = "multi_thread")]
async fn should_return_azure_pagination_and_container_identity_headers() {
    let storage = temp_storage();
    for container in ["page-a", "page-b"] {
        let created = call(
            storage.clone(),
            auth_disabled(),
            request(
                "PUT",
                &format!("http://localhost/devstoreaccount1/{container}?restype=container"),
                &[("x-ms-version", AZURE_VERSION)],
                b"",
            ),
        )
        .await;
        assert_eq!(created.status(), StatusCode::CREATED);
        assert!(created.headers().contains_key("etag"));
        assert!(created.headers().contains_key("last-modified"));
    }

    let invalid = call(
        storage.clone(),
        auth_disabled(),
        request(
            "GET",
            "http://localhost/devstoreaccount1?comp=list&maxresults=0",
            &[("x-ms-version", AZURE_VERSION)],
            b"",
        ),
    )
    .await;
    assert_eq!(invalid.status(), StatusCode::BAD_REQUEST);

    let first = body_text(
        call(
            storage.clone(),
            auth_disabled(),
            request(
                "GET",
                "http://localhost/devstoreaccount1?comp=list&prefix=page-&maxresults=1",
                &[("x-ms-version", AZURE_VERSION)],
                b"",
            ),
        )
        .await,
    )
    .await;
    assert!(first.contains("<MaxResults>1</MaxResults>"));
    assert!(first.contains("<NextMarker>page-b</NextMarker>"));

    let second = body_text(
        call(
            storage.clone(),
            auth_disabled(),
            request(
                "GET",
                "http://localhost/devstoreaccount1?comp=list&prefix=page-&maxresults=1&marker=page-b",
                &[("x-ms-version", AZURE_VERSION)],
                b"",
            ),
        )
        .await,
    )
    .await;
    assert!(second.contains("<Name>page-b</Name>"));
    assert!(second.contains("<NextMarker></NextMarker>"));

    let unparameterized = body_text(
        call(
            storage.clone(),
            auth_disabled(),
            request(
                "GET",
                "http://localhost/devstoreaccount1?comp=list",
                &[("x-ms-version", AZURE_VERSION)],
                b"",
            ),
        )
        .await,
    )
    .await;
    assert!(!unparameterized.contains("<Prefix>"));
    assert!(!unparameterized.contains("<Marker>"));
    assert!(!unparameterized.contains("<MaxResults>"));

    let properties = call(
        storage,
        auth_disabled(),
        request(
            "GET",
            "http://localhost/devstoreaccount1/page-a?restype=container",
            &[("x-ms-version", AZURE_VERSION)],
            b"",
        ),
    )
    .await;
    assert_eq!(properties.status(), StatusCode::OK);
    assert!(properties.headers().contains_key("etag"));
    assert!(properties.headers().contains_key("last-modified"));
}

async fn acquire_release_and_verify_lease(storage: Arc<dyn Storage>) {
    let lease = call(
        storage.clone(),
        auth_disabled(),
        request(
            "PUT",
            "http://localhost/devstoreaccount1/state/lease.txt?comp=lease",
            &[
                ("x-ms-version", AZURE_VERSION),
                ("x-ms-lease-action", "acquire"),
                ("x-ms-lease-duration", "-1"),
            ],
            b"",
        ),
    )
    .await;
    let lease_id = lease
        .headers()
        .get("x-ms-lease-id")
        .and_then(|value| value.to_str().ok())
        .expect("lease id should exist")
        .to_string();

    assert_eq!(
        call(
            storage.clone(),
            auth_disabled(),
            request(
                "DELETE",
                "http://localhost/devstoreaccount1/state/lease.txt",
                &[("x-ms-version", AZURE_VERSION)],
                b"",
            ),
        )
        .await
        .status(),
        StatusCode::PRECONDITION_FAILED
    );

    let release = call(
        storage.clone(),
        auth_disabled(),
        request(
            "PUT",
            "http://localhost/devstoreaccount1/state/lease.txt?comp=lease",
            &[
                ("x-ms-version", AZURE_VERSION),
                ("x-ms-lease-action", "release"),
                ("x-ms-lease-id", &lease_id),
            ],
            b"",
        ),
    )
    .await;
    assert_eq!(release.status(), StatusCode::OK);
}

async fn create_and_verify_snapshot(storage: Arc<dyn Storage>) {
    let snapshot = call(
        storage.clone(),
        auth_disabled(),
        request(
            "PUT",
            "http://localhost/devstoreaccount1/state/lease.txt?comp=snapshot",
            &[("x-ms-version", AZURE_VERSION)],
            b"",
        ),
    )
    .await;
    let snapshot_time = snapshot
        .headers()
        .get("x-ms-snapshot")
        .and_then(|value| value.to_str().ok())
        .expect("snapshot should exist")
        .to_string();

    let snap_body = body_bytes(
        call(
            storage.clone(),
            auth_disabled(),
            request(
                "GET",
                &format!(
                    "http://localhost/devstoreaccount1/state/lease.txt?snapshot={snapshot_time}"
                ),
                &[("x-ms-version", AZURE_VERSION)],
                b"",
            ),
        )
        .await,
    )
    .await;
    assert_eq!(snap_body, b"initial");
}

async fn enable_immutability_and_legal_hold(storage: Arc<dyn Storage>) {
    call(
        storage.clone(),
        auth_disabled(),
        request(
            "PUT",
            "http://localhost/devstoreaccount1/state/lease.txt?comp=immutabilityPolicies",
            &[
                ("x-ms-version", AZURE_VERSION),
                (
                    "x-ms-immutability-policy-until-date",
                    "Thu, 01 Jan 2099 00:00:00 GMT",
                ),
                ("x-ms-immutability-policy-mode", "Unlocked"),
            ],
            b"",
        ),
    )
    .await;
    call(
        storage,
        auth_disabled(),
        request(
            "PUT",
            "http://localhost/devstoreaccount1/state/lease.txt?comp=legalhold",
            &[("x-ms-version", AZURE_VERSION), ("x-ms-legal-hold", "true")],
            b"",
        ),
    )
    .await;
}
