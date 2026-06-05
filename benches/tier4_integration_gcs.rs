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

use support::live_server::{auth_disabled, rebase_url, LiveServer};

const GCS_HOST: &str = "storage.googleapis.com";

fn build_runtime() -> Runtime {
    Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("runtime should build")
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

fn bench_xml_put_object(c: &mut Criterion) {
    let runtime = build_runtime();
    let server = runtime.block_on(LiveServer::start_api(auth_disabled()));
    let bucket = "tier4-gcs-put";
    runtime.block_on(create_xml_bucket(&server, bucket));
    let payload = Bytes::from_static(b"tier4 gcs payload");
    let object_url = format!("{}/{}/hello.txt", server.base_url, bucket);

    let mut group = c.benchmark_group("tier4_integration_gcs_put_object");
    group.sampling_mode(SamplingMode::Flat);
    group.throughput(Throughput::Bytes(payload.len() as u64));
    group.bench_function(BenchmarkId::new("put_object", payload.len()), |b| {
        b.iter(|| {
            let request = Request::builder()
                .method("PUT")
                .uri(&object_url)
                .header("host", GCS_HOST)
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

fn bench_xml_get_object(c: &mut Criterion) {
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

    let mut group = c.benchmark_group("tier4_integration_gcs_get_object");
    group.sampling_mode(SamplingMode::Flat);
    group.throughput(Throughput::Bytes(payload.len() as u64));
    group.bench_function(BenchmarkId::new("get_object", payload.len()), |b| {
        b.iter_batched(
            || {
                Request::builder()
                    .method("GET")
                    .uri(&object_url)
                    .header("host", GCS_HOST)
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

fn bench_xml_list_objects(c: &mut Criterion) {
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
    let mut group = c.benchmark_group("tier4_integration_gcs_list_objects");
    group.sampling_mode(SamplingMode::Flat);
    group.throughput(Throughput::Elements(128));
    group.bench_function(BenchmarkId::new("list_objects", 128), |b| {
        b.iter_batched(
            || {
                Request::builder()
                    .method("GET")
                    .uri(&list_url)
                    .header("host", GCS_HOST)
                    .body(Body::default())
                    .expect("object list request should build")
            },
            |request| {
                let listing = runtime.block_on(server.response_text(request));
                assert!(listing.contains("item-000.txt"));
                black_box(listing);
            },
            BatchSize::SmallInput,
        )
    });
    group.finish();
}

fn bench_json_resumable_upload(c: &mut Criterion) {
    let runtime = build_runtime();
    let server = runtime.block_on(LiveServer::start_api(auth_disabled()));
    let bucket = "tier4-gcs-json";
    runtime.block_on(create_json_bucket(&server, bucket));
    let payload = Bytes::from(vec![b'g'; 4096]);
    let init_url = format!(
        "{}/upload/storage/v1/b/{}/o?uploadType=resumable&name=hello.txt",
        server.base_url, bucket
    );

    let mut group = c.benchmark_group("tier4_integration_gcs_resumable_upload");
    group.sampling_mode(SamplingMode::Flat);
    group.throughput(Throughput::Bytes(payload.len() as u64));
    group.bench_function(BenchmarkId::new("resumable_upload", payload.len()), |b| {
        b.iter(|| {
            let init_request = Request::builder()
                .method("POST")
                .uri(&init_url)
                .header("host", GCS_HOST)
                .header("x-upload-content-type", "text/plain")
                .header("x-goog-meta-owner", "bench")
                .body(Body::default())
                .expect("resumable init request should build");
            let init_response = runtime.block_on(server.request(init_request));
            assert_eq!(init_response.status(), StatusCode::OK);
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
            assert_eq!(upload_response.status(), StatusCode::OK);
            black_box(upload_response.headers().get("etag").cloned());
        })
    });
    group.finish();
}

criterion_group! {
    name = benches;
    config = criterion_config::criterion_config_for_tier4();
    targets = bench_xml_put_object, bench_xml_get_object, bench_xml_list_objects, bench_json_resumable_upload
}
criterion_main!(benches);
