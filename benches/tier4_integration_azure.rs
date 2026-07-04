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

const AZURE_VERSION: &str = "2023-11-03";

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

fn bench_put_blob(c: &mut Criterion) {
    let runtime = build_runtime();
    let server = runtime.block_on(LiveServer::start_api(auth_disabled()));
    let container = "tier4-azure-put";
    runtime.block_on(create_container(&server, container));
    let payload = Bytes::from_static(b"tier4 azure payload");
    let blob_url = format!(
        "{}/devstoreaccount1/{}/hello.txt",
        server.base_url, container
    );

    let mut group = c.benchmark_group("tier4_integration_azure_put_blob");
    group.sampling_mode(SamplingMode::Flat);
    group.throughput(Throughput::Bytes(payload.len() as u64));
    group.bench_function(BenchmarkId::new("put_blob", payload.len()), |b| {
        b.iter(|| {
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
        });
    });
    group.finish();
}

fn bench_get_blob(c: &mut Criterion) {
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

    let mut group = c.benchmark_group("tier4_integration_azure_get_blob");
    group.sampling_mode(SamplingMode::Flat);
    group.throughput(Throughput::Bytes(payload.len() as u64));
    group.bench_function(BenchmarkId::new("get_blob", payload.len()), |b| {
        b.iter_batched(
            || {
                Request::builder()
                    .method("GET")
                    .uri(&blob_url)
                    .header("x-ms-version", AZURE_VERSION)
                    .body(Body::default())
                    .expect("blob get request should build")
            },
            |request| {
                let body = runtime.block_on(server.response_bytes(request));
                assert_eq!(body.as_slice(), payload.as_ref());
                black_box(body);
            },
            BatchSize::SmallInput,
        );
    });
    group.finish();
}

fn bench_get_blob_range(c: &mut Criterion) {
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

    let mut group = c.benchmark_group("tier4_integration_azure_get_blob_range");
    group.sampling_mode(SamplingMode::Flat);
    group.throughput(Throughput::Bytes(4 * 1024));
    group.bench_function(BenchmarkId::new("get_blob_range", 4 * 1024), |b| {
        b.iter_batched(
            || {
                Request::builder()
                    .method("GET")
                    .uri(&blob_url)
                    .header("x-ms-version", AZURE_VERSION)
                    .header("x-ms-range", "bytes=0-4095")
                    .body(Body::default())
                    .expect("blob range request should build")
            },
            |request| {
                let body = runtime.block_on(server.response_bytes(request));
                assert_eq!(body.len(), 4 * 1024);
                black_box(body);
            },
            BatchSize::SmallInput,
        );
    });
    group.finish();
}

fn bench_list_blobs(c: &mut Criterion) {
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
    let mut group = c.benchmark_group("tier4_integration_azure_list_blobs");
    group.sampling_mode(SamplingMode::Flat);
    group.throughput(Throughput::Elements(128));
    group.bench_function(BenchmarkId::new("list_blobs", 128), |b| {
        b.iter_batched(
            || {
                Request::builder()
                    .method("GET")
                    .uri(&list_url)
                    .header("x-ms-version", AZURE_VERSION)
                    .body(Body::default())
                    .expect("blob list request should build")
            },
            |request| {
                let listing = runtime.block_on(server.response_text(request));
                assert!(listing.contains("item-000.txt"));
                black_box(listing);
            },
            BatchSize::SmallInput,
        );
    });
    group.finish();
}

criterion_group! {
    name = benches;
    config = criterion_config::criterion_config_for_tier4();
    targets = bench_put_blob, bench_get_blob, bench_get_blob_range, bench_list_blobs
}
criterion_main!(benches);
