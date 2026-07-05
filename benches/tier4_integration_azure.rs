use bytes::Bytes;
use cntryl_stress::prelude::*;
use http_body_util::Full;
type Body = Full<Bytes>;

use hyper::{Request, StatusCode};
use tokio::runtime::{Builder, Runtime};

#[path = "support/mod.rs"]
mod support;

use support::live_server::{auth_disabled, LiveServer};

const AZURE_VERSION: &str = "2023-11-03";
const GET_BATCH_OPS: u64 = 8;
const LIST_BATCH_OPS: u64 = 4;
const WRITE_BATCH_OPS: u64 = 8;

fn build_runtime() -> Runtime {
    Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("runtime should build")
}

async fn create_container(server: &LiveServer, container: &str) {
    let request = Request::builder()
        .method("PUT")
        .uri(format!(
            "{}/devstoreaccount1/{}?restype=container",
            server.base_url, container
        ))
        .header("x-ms-version", AZURE_VERSION)
        .body(Body::default())
        .expect("container create request should build");
    let response = server.request(request).await;
    assert_eq!(response.status(), StatusCode::CREATED);
}

#[stress_test(
    tier = 4,
    mode = "fixed_duration",
    metadata(component = "provider_api", provider = "azure", operation = "put_blob")
)]
fn put_blob(ctx: &mut StressContext) {
    let runtime = build_runtime();
    let server = runtime.block_on(LiveServer::start_api(auth_disabled()));
    let container = "tier4-azure-put";
    runtime.block_on(create_container(&server, container));
    let payload = Bytes::from_static(b"tier4 azure payload");
    let container_url = format!("{}/devstoreaccount1/{}", server.base_url, container);
    let mut sequence = 0usize;

    ctx.parameter("payload_size_bytes", payload.len());
    ctx.parameter("operations_per_batch", WRITE_BATCH_OPS);
    let operations = ctx.measure_batch(WRITE_BATCH_OPS, || {
        for _ in 0..WRITE_BATCH_OPS {
            let blob_url = format!("{container_url}/hello-{sequence}.txt");
            sequence += 1;
            let request = Request::builder()
                .method("PUT")
                .uri(&blob_url)
                .header("x-ms-version", AZURE_VERSION)
                .header("x-ms-blob-type", "BlockBlob")
                .header("content-type", "text/plain")
                .body(Body::from(payload.clone()))
                .expect("blob put request should build");
            let response = runtime.block_on(server.request(request));
            assert_eq!(response.status(), StatusCode::CREATED);
            black_box(response.headers().get("etag").cloned());
        }
    });
    let _ = ctx
        .correctness()
        .attempted(operations)
        .completed(operations);
}

#[stress_test(
    tier = 4,
    mode = "fixed_duration",
    metadata(component = "provider_api", provider = "azure", operation = "get_blob")
)]
fn get_blob(ctx: &mut StressContext) {
    let runtime = build_runtime();
    let server = runtime.block_on(LiveServer::start_api(auth_disabled()));
    let container = "tier4-azure-get";
    runtime.block_on(create_container(&server, container));
    let payload = Bytes::from(vec![b'a'; 64 * 1024]);
    let blob_url = format!(
        "{}/devstoreaccount1/{}/hello.txt",
        server.base_url, container
    );

    runtime.block_on(async {
        let request = Request::builder()
            .method("PUT")
            .uri(&blob_url)
            .header("x-ms-version", AZURE_VERSION)
            .header("x-ms-blob-type", "BlockBlob")
            .header("content-type", "text/plain")
            .body(Body::from(payload.clone()))
            .expect("seed put request should build");
        let response = server.request(request).await;
        assert_eq!(response.status(), StatusCode::CREATED);
    });

    ctx.parameter("payload_size_bytes", payload.len());
    ctx.parameter("operations_per_batch", GET_BATCH_OPS);
    let operations = ctx.measure_batch(GET_BATCH_OPS, || {
        for _ in 0..GET_BATCH_OPS {
            let request = Request::builder()
                .method("GET")
                .uri(&blob_url)
                .header("x-ms-version", AZURE_VERSION)
                .body(Body::default())
                .expect("blob get request should build");
            let (status, body) = runtime.block_on(server.response_bytes_with_status(request));
            assert_eq!(status, StatusCode::OK);
            assert_eq!(body.as_slice(), payload.as_ref());
            black_box(body);
        }
    });
    let _ = ctx
        .correctness()
        .attempted(operations)
        .completed(operations);
}

#[stress_test(
    tier = 4,
    mode = "fixed_duration",
    metadata(
        component = "provider_api",
        provider = "azure",
        operation = "get_blob_range"
    )
)]
fn get_blob_range(ctx: &mut StressContext) {
    let runtime = build_runtime();
    let server = runtime.block_on(LiveServer::start_api(auth_disabled()));
    let container = "tier4-azure-range";
    runtime.block_on(create_container(&server, container));
    let payload = Bytes::from(vec![b'a'; 64 * 1024]);
    let blob_url = format!(
        "{}/devstoreaccount1/{}/range.txt",
        server.base_url, container
    );

    runtime.block_on(async {
        let request = Request::builder()
            .method("PUT")
            .uri(&blob_url)
            .header("x-ms-version", AZURE_VERSION)
            .header("x-ms-blob-type", "BlockBlob")
            .header("content-type", "text/plain")
            .body(Body::from(payload.clone()))
            .expect("seed put request should build");
        let response = server.request(request).await;
        assert_eq!(response.status(), StatusCode::CREATED);
    });

    ctx.parameter("payload_size_bytes", payload.len());
    ctx.parameter("range_size_bytes", 4 * 1024);
    let operations = ctx.measure_workload(|| {
        let request = Request::builder()
            .method("GET")
            .uri(&blob_url)
            .header("x-ms-version", AZURE_VERSION)
            .header("x-ms-range", "bytes=0-4095")
            .body(Body::default())
            .expect("blob range request should build");
        let (status, body) = runtime.block_on(server.response_bytes_with_status(request));
        assert_eq!(status, StatusCode::PARTIAL_CONTENT);
        assert_eq!(body.len(), 4 * 1024);
        black_box(body);
    });
    let _ = ctx
        .correctness()
        .attempted(operations)
        .completed(operations);
}

#[stress_test(
    tier = 4,
    mode = "fixed_duration",
    metadata(
        component = "provider_api",
        provider = "azure",
        operation = "list_blobs"
    )
)]
fn list_blobs(ctx: &mut StressContext) {
    let runtime = build_runtime();
    let server = runtime.block_on(LiveServer::start_api(auth_disabled()));
    let container = "tier4-azure-list";
    runtime.block_on(create_container(&server, container));
    let payload = Bytes::from_static(b"tier4 azure list payload");

    runtime.block_on(async {
        for index in 0..128usize {
            let request = Request::builder()
                .method("PUT")
                .uri(format!(
                    "{}/devstoreaccount1/{}/item-{index:03}.txt",
                    server.base_url, container
                ))
                .header("x-ms-version", AZURE_VERSION)
                .header("x-ms-blob-type", "BlockBlob")
                .header("content-type", "text/plain")
                .body(Body::from(payload.clone()))
                .expect("seed put request should build");
            let response = server.request(request).await;
            assert_eq!(response.status(), StatusCode::CREATED);
        }
    });

    let list_url = format!(
        "{}/devstoreaccount1/{}?restype=container&comp=list&prefix=item-",
        server.base_url, container
    );
    ctx.parameter("object_count", 128);
    ctx.parameter("operations_per_batch", LIST_BATCH_OPS);
    let operations = ctx.measure_batch(LIST_BATCH_OPS, || {
        for _ in 0..LIST_BATCH_OPS {
            let request = Request::builder()
                .method("GET")
                .uri(&list_url)
                .header("x-ms-version", AZURE_VERSION)
                .body(Body::default())
                .expect("blob list request should build");
            let (status, listing) = runtime.block_on(server.response_text_with_status(request));
            assert_eq!(status, StatusCode::OK);
            assert!(listing.contains("item-000.txt"));
            black_box(listing);
        }
    });
    let _ = ctx
        .correctness()
        .attempted(operations)
        .completed(operations);
}

stress_main!();
