use bytes::Bytes;
use cntryl_stress::prelude::*;
use http_body_util::Full;
type Body = Full<Bytes>;

use hyper::{Request, StatusCode};
use tokio::runtime::{Builder, Runtime};

#[path = "support/mod.rs"]
mod support;

use support::live_server::{auth_disabled, LiveServer};

const VERSIONING_XML: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<VersioningConfiguration xmlns="http://s3.amazonaws.com/doc/2006-03-01/">
  <Status>Enabled</Status>
</VersioningConfiguration>"#;

fn build_runtime() -> Runtime {
    Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("runtime should build")
}

async fn create_bucket(server: &LiveServer, bucket: &str) {
    let request = Request::builder()
        .method("PUT")
        .uri(format!("{}/{}", server.base_url, bucket))
        .body(Body::default())
        .expect("bucket create request should build");
    let response = server.request(request).await;
    assert_eq!(response.status(), StatusCode::OK);
}

async fn enable_versioning(server: &LiveServer, bucket: &str) {
    let request = Request::builder()
        .method("PUT")
        .uri(format!("{}/{}?versioning", server.base_url, bucket))
        .header("content-type", "application/xml")
        .body(Body::from(VERSIONING_XML))
        .expect("versioning request should build");
    let response = server.request(request).await;
    assert_eq!(response.status(), StatusCode::OK);
}

#[stress_test(
    tier = 4,
    mode = "fixed_duration",
    metadata(component = "provider_api", provider = "s3", operation = "put_object")
)]
fn put_object(ctx: &mut StressContext) {
    let runtime = build_runtime();
    let server = runtime.block_on(LiveServer::start_api(auth_disabled()));
    let bucket = "tier4-s3-put";
    runtime.block_on(create_bucket(&server, bucket));
    let payload = Bytes::from_static(b"tier4 s3 payload");
    let mut sequence = 0usize;

    ctx.parameter("payload_size_bytes", payload.len());
    let operations = ctx.measure_workload(|| {
        let object_url = format!("{}/{}/object-{sequence}.txt", server.base_url, bucket);
        sequence += 1;
        let request = Request::builder()
            .method("PUT")
            .uri(&object_url)
            .header("content-type", "text/plain")
            .body(Body::from(payload.clone()))
            .expect("object put request should build");
        let response = runtime.block_on(server.request(request));
        assert_eq!(response.status(), StatusCode::OK);
        black_box(response.headers().get("etag").cloned());
    });
    let _ = ctx
        .correctness()
        .attempted(operations)
        .completed(operations);
}

#[stress_test(
    tier = 4,
    mode = "fixed_duration",
    metadata(component = "provider_api", provider = "s3", operation = "get_object")
)]
fn get_object(ctx: &mut StressContext) {
    let runtime = build_runtime();
    let server = runtime.block_on(LiveServer::start_api(auth_disabled()));
    let bucket = "tier4-s3-get";
    runtime.block_on(create_bucket(&server, bucket));
    let payload = Bytes::from(vec![b'a'; 64 * 1024]);
    let object_url = format!("{}/{}/object.txt", server.base_url, bucket);

    runtime.block_on(async {
        let request = Request::builder()
            .method("PUT")
            .uri(&object_url)
            .header("content-type", "text/plain")
            .body(Body::from(payload.clone()))
            .expect("seed put request should build");
        let response = server.request(request).await;
        assert_eq!(response.status(), StatusCode::OK);
    });

    ctx.parameter("payload_size_bytes", payload.len());
    let operations = ctx.measure_workload(|| {
        let request = Request::builder()
            .method("GET")
            .uri(&object_url)
            .body(Body::default())
            .expect("object get request should build");
        let (status, body) = runtime.block_on(server.response_bytes_with_status(request));
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body.as_slice(), payload.as_ref());
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
        provider = "s3",
        operation = "list_objects"
    )
)]
fn list_objects(ctx: &mut StressContext) {
    let runtime = build_runtime();
    let server = runtime.block_on(LiveServer::start_api(auth_disabled()));
    let bucket = "tier4-s3-list";
    runtime.block_on(create_bucket(&server, bucket));
    let payload = Bytes::from_static(b"tier4 s3 list payload");

    runtime.block_on(async {
        for index in 0..128usize {
            let request = Request::builder()
                .method("PUT")
                .uri(format!(
                    "{}/{}/item-{index:03}.txt",
                    server.base_url, bucket
                ))
                .header("content-type", "text/plain")
                .body(Body::from(payload.clone()))
                .expect("seed put request should build");
            let response = server.request(request).await;
            assert_eq!(response.status(), StatusCode::OK);
        }
    });

    let list_url = format!("{}/{}?list-type=2", server.base_url, bucket);
    ctx.parameter("object_count", 128);
    let operations = ctx.measure_workload(|| {
        let request = Request::builder()
            .method("GET")
            .uri(&list_url)
            .body(Body::default())
            .expect("object list request should build");
        let (status, listing) = runtime.block_on(server.response_text_with_status(request));
        assert_eq!(status, StatusCode::OK);
        assert!(listing.contains("item-000.txt"));
        black_box(listing);
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
        provider = "s3",
        operation = "list_versions"
    )
)]
fn list_versions(ctx: &mut StressContext) {
    let runtime = build_runtime();
    let server = runtime.block_on(LiveServer::start_api(auth_disabled()));
    let bucket = "tier4-s3-versions";
    runtime.block_on(create_bucket(&server, bucket));
    runtime.block_on(enable_versioning(&server, bucket));

    let object_url = format!("{}/{}/versioned.txt", server.base_url, bucket);
    runtime.block_on(async {
        for payload in (0..32usize).map(|index| Bytes::from(format!("version-{index:02}"))) {
            let request = Request::builder()
                .method("PUT")
                .uri(&object_url)
                .header("content-type", "text/plain")
                .body(Body::from(payload))
                .expect("versioned put request should build");
            let response = server.request(request).await;
            assert_eq!(response.status(), StatusCode::OK);
        }
    });

    let versions_url = format!(
        "{}/{}?versions&prefix=versioned.txt",
        server.base_url, bucket
    );
    let warmup_request = Request::builder()
        .method("GET")
        .uri(&versions_url)
        .body(Body::default())
        .expect("versions warmup request should build");
    let warmup_listing = runtime.block_on(server.response_text(warmup_request));
    assert!(warmup_listing.matches("<Version>").count() >= 32);

    ctx.parameter("version_count", 32);
    let operations = ctx.measure_workload(|| {
        let request = Request::builder()
            .method("GET")
            .uri(&versions_url)
            .body(Body::default())
            .expect("versions list request should build");
        let (status, listing) = runtime.block_on(server.response_text_with_status(request));
        assert_eq!(status, StatusCode::OK);
        assert!(listing.matches("<Version>").count() >= 32);
        black_box(listing);
    });
    let _ = ctx
        .correctness()
        .attempted(operations)
        .completed(operations);
}

stress_main!();
