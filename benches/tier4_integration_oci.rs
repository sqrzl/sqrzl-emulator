use bytes::Bytes;
use criterion::{
    criterion_group, criterion_main, BatchSize, BenchmarkId, Criterion, SamplingMode, Throughput,
};
use http_body_util::Full;
use std::hint::black_box;
type Body = Full<Bytes>;

use hyper::{Request, StatusCode};
use tokio::runtime::{Builder, Runtime};

#[path = "support/criterion_config.rs"]
mod criterion_config;

#[path = "support/mod.rs"]
mod support;

use support::live_server::{auth_disabled, LiveServer};

const TENANT: &str = "tenant";

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

fn bench_put_object(c: &mut Criterion) {
    let runtime = build_runtime();
    let server = runtime.block_on(LiveServer::start_api(auth_disabled()));
    let bucket = "tier4-oci-put";
    runtime.block_on(create_bucket(&server, bucket));
    let payload = Bytes::from_static(b"tier4 oci payload");
    let object_url = format!("{}/n/{}/b/{}/o/hello.txt", server.base_url, TENANT, bucket);

    let mut group = c.benchmark_group("tier4_integration_oci_put_object");
    group.sampling_mode(SamplingMode::Flat);
    group.throughput(Throughput::Bytes(payload.len() as u64));
    group.bench_function(BenchmarkId::new("put_object", payload.len()), |b| {
        b.iter(|| {
            let request = Request::builder()
                .method("PUT")
                .uri(&object_url)
                .header("content-type", "text/plain")
                .body(Body::from(payload.clone()))
                .expect("object put request should build");
            let response = runtime.block_on(server.request(request));
            assert_eq!(response.status(), StatusCode::OK);
            black_box(response.headers().get("etag").cloned());
        })
    });
    group.finish();
}

fn bench_get_object(c: &mut Criterion) {
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

    let mut group = c.benchmark_group("tier4_integration_oci_get_object");
    group.sampling_mode(SamplingMode::Flat);
    group.throughput(Throughput::Bytes(payload.len() as u64));
    group.bench_function(BenchmarkId::new("get_object", payload.len()), |b| {
        b.iter_batched(
            || {
                Request::builder()
                    .method("GET")
                    .uri(&object_url)
                    .body(Body::default())
                    .expect("object get request should build")
            },
            |request| {
                let body = runtime.block_on(server.response_bytes(request));
                assert_eq!(body.as_slice(), payload.as_ref());
                black_box(body);
            },
            BatchSize::SmallInput,
        )
    });
    group.finish();
}

criterion_group! {
    name = benches;
    config = criterion_config::criterion_config_for_tier4();
    targets = bench_put_object, bench_get_object
}

criterion_main!(benches);
