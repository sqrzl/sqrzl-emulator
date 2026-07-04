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
    metadata(component = "provider_api", provider = "azure", operation = "put_blob")
)]
fn put_blob(ctx: &mut StressContext) {
    let runtime = build_runtime();
    let server = runtime.block_on(LiveServer::start_api(auth_disabled()));
    let container = "tier4-azure-put";
    runtime.block_on(create_container(&server, container));
    let payload = Bytes::from_static(b"tier4 azure payload");
    let blob_url = format!(
        "{}/devstoreaccount1/{}/hello.txt",
        server.base_url, container
    );

    ctx.parameter("payload_size_bytes", payload.len());
    let request = Request::builder()
        .method("PUT")
        .uri(&blob_url)
        .header("x-ms-version", AZURE_VERSION)
        .header("x-ms-blob-type", "BlockBlob")
        .header("content-type", "text/plain")
        .body(Body::from(payload))
        .expect("blob put request should build");
    let response = ctx.measure(|| runtime.block_on(server.request(request)));
    record_status(ctx, response.status(), StatusCode::CREATED);
    black_box(response.headers().get("etag").cloned());
}

#[stress_test(
    tier = 4,
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
    let request = Request::builder()
        .method("GET")
        .uri(&blob_url)
        .header("x-ms-version", AZURE_VERSION)
        .body(Body::default())
        .expect("blob get request should build");
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
    let request = Request::builder()
        .method("GET")
        .uri(&blob_url)
        .header("x-ms-version", AZURE_VERSION)
        .header("x-ms-range", "bytes=0-4095")
        .body(Body::default())
        .expect("blob range request should build");
    let (status, body) =
        ctx.measure(|| runtime.block_on(server.response_bytes_with_status(request)));
    record_status(ctx, status, StatusCode::PARTIAL_CONTENT);
    assert_eq!(body.len(), 4 * 1024);
    black_box(body);
}

#[stress_test(
    tier = 4,
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
    let request = Request::builder()
        .method("GET")
        .uri(&list_url)
        .header("x-ms-version", AZURE_VERSION)
        .body(Body::default())
        .expect("blob list request should build");
    let (status, listing) =
        ctx.measure(|| runtime.block_on(server.response_text_with_status(request)));
    record_status(ctx, status, StatusCode::OK);
    assert!(listing.contains("item-000.txt"));
    black_box(listing);
}

stress_main!();
