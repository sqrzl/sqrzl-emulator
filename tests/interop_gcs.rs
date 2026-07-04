mod common;

use common::interop::{
    auth_disabled, body_bytes, body_text, call, call_with_registry, request, temp_storage,
};
use sqrzl_emulator::providers::AdapterRegistry;
use std::sync::Arc;

#[tokio::test(flavor = "multi_thread")]
async fn should_round_trip_bucket_and_object_operations_given_basic_gcs_requests_when_using_xml_api(
) {
    let storage = temp_storage();
    call(
        storage.clone(),
        auth_disabled(),
        request(
            "PUT",
            "http://localhost/interop-gcs",
            &[("host", "storage.googleapis.com")],
            b"",
        )
        .await,
    )
    .await;
    call(
        storage.clone(),
        auth_disabled(),
        request(
            "PUT",
            "http://localhost/interop-gcs/hello.txt",
            &[
                ("host", "storage.googleapis.com"),
                ("content-type", "text/plain"),
            ],
            b"gcs smoke",
        )
        .await,
    )
    .await;
    let body = body_bytes(
        call(
            storage,
            auth_disabled(),
            request(
                "GET",
                "http://localhost/interop-gcs/hello.txt",
                &[("host", "storage.googleapis.com")],
                b"",
            )
            .await,
        )
        .await,
    )
    .await;
    assert_eq!(body, b"gcs smoke");
}

#[tokio::test(flavor = "multi_thread")]
async fn should_return_custom_metadata_given_gcs_metadata_headers_when_requesting_object_head() {
    let storage = temp_storage();
    call(
        storage.clone(),
        auth_disabled(),
        request(
            "PUT",
            "http://localhost/interop-gcs",
            &[("host", "storage.googleapis.com")],
            b"",
        )
        .await,
    )
    .await;
    call(
        storage.clone(),
        auth_disabled(),
        request(
            "PUT",
            "http://localhost/interop-gcs/hello.txt",
            &[
                ("host", "storage.googleapis.com"),
                ("content-type", "text/plain"),
                ("x-goog-meta-owner", "sdk"),
            ],
            b"gcs smoke",
        )
        .await,
    )
    .await;
    let response = call(
        storage,
        auth_disabled(),
        request(
            "HEAD",
            "http://localhost/interop-gcs/hello.txt",
            &[("host", "storage.googleapis.com")],
            b"",
        )
        .await,
    )
    .await;
    assert_eq!(
        response
            .headers()
            .get("x-goog-meta-owner")
            .and_then(|value| value.to_str().ok()),
        Some("sdk")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn should_return_requested_slice_given_range_header_when_reading_gcs_object_content() {
    let storage = temp_storage();
    call(
        storage.clone(),
        auth_disabled(),
        request(
            "PUT",
            "http://localhost/interop-gcs",
            &[("host", "storage.googleapis.com")],
            b"",
        )
        .await,
    )
    .await;
    call(
        storage.clone(),
        auth_disabled(),
        request(
            "PUT",
            "http://localhost/interop-gcs/hello.txt",
            &[
                ("host", "storage.googleapis.com"),
                ("content-type", "text/plain"),
            ],
            b"gcs smoke",
        )
        .await,
    )
    .await;
    let body = body_bytes(
        call(
            storage,
            auth_disabled(),
            request(
                "GET",
                "http://localhost/interop-gcs/hello.txt",
                &[("host", "storage.googleapis.com"), ("range", "bytes=0-2")],
                b"",
            )
            .await,
        )
        .await,
    )
    .await;
    assert_eq!(body, b"gcs");
}

#[tokio::test(flavor = "multi_thread")]
async fn should_list_matching_objects_given_existing_keys_when_querying_gcs_bucket_contents() {
    let storage = temp_storage();
    call(
        storage.clone(),
        auth_disabled(),
        request(
            "PUT",
            "http://localhost/interop-gcs",
            &[("host", "storage.googleapis.com")],
            b"",
        )
        .await,
    )
    .await;
    call(
        storage.clone(),
        auth_disabled(),
        request(
            "PUT",
            "http://localhost/interop-gcs/hello.txt",
            &[
                ("host", "storage.googleapis.com"),
                ("content-type", "text/plain"),
            ],
            b"gcs smoke",
        )
        .await,
    )
    .await;
    let listing = body_text(
        call(
            storage,
            auth_disabled(),
            request(
                "GET",
                "http://localhost/interop-gcs",
                &[("host", "storage.googleapis.com")],
                b"",
            )
            .await,
        )
        .await,
    )
    .await;
    assert!(listing.contains("hello.txt"));
}

#[tokio::test(flavor = "multi_thread")]
async fn should_complete_resumable_upload_given_json_api_session_when_finalizing_media_object() {
    let storage = temp_storage();
    let adapters = Arc::new(AdapterRegistry::default());
    call_with_registry(
        adapters.clone(),
        storage.clone(),
        auth_disabled(),
        request(
            "POST",
            "http://localhost/storage/v1/b?project=test-project",
            &[
                ("host", "storage.googleapis.com"),
                ("content-type", "application/json"),
            ],
            br#"{"name":"json-bucket"}"#,
        )
        .await,
    )
    .await;
    let init = call_with_registry(
        adapters.clone(),
        storage.clone(),
        auth_disabled(),
        request(
            "POST",
            "http://localhost/upload/storage/v1/b/json-bucket/o?uploadType=resumable&name=hello.txt",
            &[
                ("host", "storage.googleapis.com:8443"),
                ("x-forwarded-proto", "https"),
                ("x-upload-content-type", "text/plain"),
                ("x-goog-meta-owner", "jules"),
            ],
            b"",
        )
        .await,
    )
    .await;
    let location = init
        .headers()
        .get("location")
        .and_then(|value| value.to_str().ok())
        .expect("location should exist")
        .to_string();
    assert!(location.starts_with("https://storage.googleapis.com:8443/upload/resumable/"));
    call_with_registry(
        adapters.clone(),
        storage.clone(),
        auth_disabled(),
        request(
            "PUT",
            &location,
            &[
                ("host", "storage.googleapis.com:8443"),
                ("x-forwarded-proto", "https"),
            ],
            b"json api",
        )
        .await,
    )
    .await;
    let json = body_text(
        call_with_registry(
            adapters,
            storage,
            auth_disabled(),
            request(
                "GET",
                "http://localhost/storage/v1/b/json-bucket/o/hello.txt",
                &[("host", "storage.googleapis.com")],
                b"",
            )
            .await,
        )
        .await,
    )
    .await;
    assert!(json.contains("\"hello.txt\""));
    assert!(json.contains("\"owner\":\"jules\""));
}
