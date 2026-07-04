use bytes::Bytes;
use cntryl_stress::prelude::*;
use http_body_util::Full;
type Body = Full<Bytes>;

use hyper::{Request, StatusCode};
use tokio::runtime::{Builder, Runtime};

#[path = "support/mod.rs"]
mod support;

use support::live_server::{auth_disabled, rebase_url, LiveServer};

const GCS_HOST: &str = "storage.googleapis.com";

fn build_runtime() -> Runtime {
    Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("runtime should build")
}

fn record_status(ctx: &mut StressContext, status: StatusCode, expected: StatusCode) {
    let completed = u64::from(status == expected);
    let failures = u64::from(status != expected);
    let _ = ctx
        .correctness()
        .attempted(1)
        .completed(completed)
        .failures(failures);
    assert_eq!(status, expected);
}

fn record_resumable_status(
    ctx: &mut StressContext,
    init_status: StatusCode,
    upload_status: StatusCode,
) {
    let completed =
        u64::from(init_status == StatusCode::OK) + u64::from(upload_status == StatusCode::OK);
    let failures = 2 - completed;
    let _ = ctx
        .correctness()
        .attempted(2)
        .completed(completed)
        .failures(failures);
    assert_eq!(init_status, StatusCode::OK);
    assert_eq!(upload_status, StatusCode::OK);
}

async fn create_xml_bucket(server: &LiveServer, bucket: &str) {
    let request = Request::builder()
        .method("PUT")
        .uri(format!("{}/{}", server.base_url, bucket))
        .header("host", GCS_HOST)
        .body(Body::default())
        .expect("xml bucket create request should build");
    let response = server.request(request).await;
    assert_eq!(response.status(), StatusCode::OK);
}

async fn create_json_bucket(server: &LiveServer, bucket: &str) {
    let request = Request::builder()
        .method("POST")
        .uri(format!(
            "{}/storage/v1/b?project=test-project",
            server.base_url
        ))
        .header("host", GCS_HOST)
        .header("content-type", "application/json")
        .body(Body::from(
            serde_json::json!({ "name": bucket }).to_string(),
        ))
        .expect("json bucket create request should build");
    let response = server.request(request).await;
    assert_eq!(response.status(), StatusCode::OK);
}

#[stress_test(
    tier = 4,
    metadata(
        component = "provider_api",
        provider = "gcs",
        operation = "xml_put_object"
    )
)]
fn xml_put_object(ctx: &mut StressContext) {
    let runtime = build_runtime();
    let server = runtime.block_on(LiveServer::start_api(auth_disabled()));
    let bucket = "tier4-gcs-put";
    runtime.block_on(create_xml_bucket(&server, bucket));
    let payload = Bytes::from_static(b"tier4 gcs payload");
    let object_url = format!("{}/{}/hello.txt", server.base_url, bucket);

    ctx.parameter("payload_size_bytes", payload.len());
    let request = Request::builder()
        .method("PUT")
        .uri(&object_url)
        .header("host", GCS_HOST)
        .header("content-type", "text/plain")
        .body(Body::from(payload))
        .expect("object put request should build");
    let response = ctx.measure(|| runtime.block_on(server.request(request)));
    record_status(ctx, response.status(), StatusCode::OK);
    black_box(response.headers().get("etag").cloned());
}

#[stress_test(
    tier = 4,
    metadata(
        component = "provider_api",
        provider = "gcs",
        operation = "xml_get_object"
    )
)]
fn xml_get_object(ctx: &mut StressContext) {
    let runtime = build_runtime();
    let server = runtime.block_on(LiveServer::start_api(auth_disabled()));
    let bucket = "tier4-gcs-get";
    runtime.block_on(create_xml_bucket(&server, bucket));
    let payload = Bytes::from(vec![b'g'; 64 * 1024]);
    let object_url = format!("{}/{}/hello.txt", server.base_url, bucket);

    runtime.block_on(async {
        let request = Request::builder()
            .method("PUT")
            .uri(&object_url)
            .header("host", GCS_HOST)
            .header("content-type", "text/plain")
            .body(Body::from(payload.clone()))
            .expect("seed put request should build");
        let response = server.request(request).await;
        assert_eq!(response.status(), StatusCode::OK);
    });

    ctx.parameter("payload_size_bytes", payload.len());
    let request = Request::builder()
        .method("GET")
        .uri(&object_url)
        .header("host", GCS_HOST)
        .body(Body::default())
        .expect("object get request should build");
    let (status, body) =
        ctx.measure(|| runtime.block_on(server.response_bytes_with_status(request)));
    record_status(ctx, status, StatusCode::OK);
    assert_eq!(body.as_slice(), payload.as_ref());
    black_box(body);
}

#[stress_test(
    tier = 4,
    metadata(
        component = "provider_api",
        provider = "gcs",
        operation = "xml_list_objects"
    )
)]
fn xml_list_objects(ctx: &mut StressContext) {
    let runtime = build_runtime();
    let server = runtime.block_on(LiveServer::start_api(auth_disabled()));
    let bucket = "tier4-gcs-list";
    runtime.block_on(create_xml_bucket(&server, bucket));
    let payload = Bytes::from_static(b"tier4 gcs list payload");

    runtime.block_on(async {
        for index in 0..128usize {
            let request = Request::builder()
                .method("PUT")
                .uri(format!(
                    "{}/{}/item-{index:03}.txt",
                    server.base_url, bucket
                ))
                .header("host", GCS_HOST)
                .header("content-type", "text/plain")
                .body(Body::from(payload.clone()))
                .expect("seed put request should build");
            let response = server.request(request).await;
            assert_eq!(response.status(), StatusCode::OK);
        }
    });

    let list_url = format!("{}/{}?prefix=item-", server.base_url, bucket);
    ctx.parameter("object_count", 128);
    let request = Request::builder()
        .method("GET")
        .uri(&list_url)
        .header("host", GCS_HOST)
        .body(Body::default())
        .expect("object list request should build");
    let (status, listing) =
        ctx.measure(|| runtime.block_on(server.response_text_with_status(request)));
    record_status(ctx, status, StatusCode::OK);
    assert!(listing.contains("item-000.txt"));
    black_box(listing);
}

#[stress_test(
    tier = 4,
    metadata(
        component = "provider_api",
        provider = "gcs",
        operation = "json_resumable_upload"
    )
)]
fn json_resumable_upload(ctx: &mut StressContext) {
    let runtime = build_runtime();
    let server = runtime.block_on(LiveServer::start_api(auth_disabled()));
    let bucket = "tier4-gcs-json";
    runtime.block_on(create_json_bucket(&server, bucket));
    let payload = Bytes::from(vec![b'g'; 4096]);
    let init_url = format!(
        "{}/upload/storage/v1/b/{}/o?uploadType=resumable&name=hello.txt",
        server.base_url, bucket
    );

    ctx.parameter("payload_size_bytes", payload.len());
    let (init_status, upload_status, etag) = ctx.measure(|| {
        let init_request = Request::builder()
            .method("POST")
            .uri(&init_url)
            .header("host", GCS_HOST)
            .header("x-upload-content-type", "text/plain")
            .header("x-goog-meta-owner", "bench")
            .body(Body::default())
            .expect("resumable init request should build");
        let init_response = runtime.block_on(server.request(init_request));
        let init_status = init_response.status();
        let upstream_location = init_response
            .headers()
            .get("location")
            .and_then(|value| value.to_str().ok())
            .expect("resumable location should exist")
            .to_string();
        let upload_location = rebase_url(&server.base_url, &upstream_location);

        let upload_request = Request::builder()
            .method("PUT")
            .uri(upload_location)
            .header("host", GCS_HOST)
            .body(Body::from(payload.clone()))
            .expect("resumable upload request should build");
        let upload_response = runtime.block_on(server.request(upload_request));
        (
            init_status,
            upload_response.status(),
            upload_response.headers().get("etag").cloned(),
        )
    });
    record_resumable_status(ctx, init_status, upload_status);
    black_box(etag);
}

stress_main!();
