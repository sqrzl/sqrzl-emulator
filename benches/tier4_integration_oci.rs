use bytes::Bytes;
use cntryl_stress::prelude::*;
use http_body_util::Full;
type Body = Full<Bytes>;

use hyper::{Request, StatusCode};
use tokio::runtime::{Builder, Runtime};

#[path = "support/mod.rs"]
mod support;

use support::live_server::{auth_disabled, LiveServer};

const TENANT: &str = "tenant";
const WRITE_BATCH_OPS: u64 = 8;

fn build_runtime() -> Runtime {
    Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("runtime should build")
}

async fn create_bucket(server: &LiveServer, bucket: &str) {
    let request = Request::builder()
        .method("POST")
        .uri(format!("{}/n/{}/b", server.base_url, TENANT))
        .header("content-type", "application/json")
        .body(Body::from(
            serde_json::json!({
                "name": bucket,
                "compartmentId": "ignored"
            })
            .to_string(),
        ))
        .expect("bucket create request should build");
    let response = server.request(request).await;
    assert_eq!(response.status(), StatusCode::OK);
}

#[stress_test(
    tier = 4,
    metadata(component = "provider_api", provider = "oci", operation = "put_object")
)]
fn put_object(ctx: &mut StressContext) {
    let runtime = build_runtime();
    let server = runtime.block_on(LiveServer::start_api(auth_disabled()));
    let bucket = "tier4-oci-put";
    runtime.block_on(create_bucket(&server, bucket));
    let payload = Bytes::from_static(b"tier4 oci payload");
    let object_url_prefix = format!("{}/n/{}/b/{}/o", server.base_url, TENANT, bucket);
    let mut sequence = 0usize;

    ctx.parameter("payload_size_bytes", payload.len());
    ctx.parameter("operations_per_batch", WRITE_BATCH_OPS);
    let operations = ctx.measure_batch(WRITE_BATCH_OPS, || {
        for _ in 0..WRITE_BATCH_OPS {
            let object_url = format!("{object_url_prefix}/hello-{sequence}.txt");
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
        }
    });
    let _ = ctx
        .correctness()
        .attempted(operations)
        .completed(operations);
}

#[stress_test(
    tier = 4,
    metadata(component = "provider_api", provider = "oci", operation = "get_object")
)]
fn get_object(ctx: &mut StressContext) {
    let runtime = build_runtime();
    let server = runtime.block_on(LiveServer::start_api(auth_disabled()));
    let bucket = "tier4-oci-get";
    runtime.block_on(create_bucket(&server, bucket));
    let payload = Bytes::from(vec![b'o'; 64 * 1024]);
    let object_url = format!("{}/n/{}/b/{}/o/hello.txt", server.base_url, TENANT, bucket);

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

stress_main!();
