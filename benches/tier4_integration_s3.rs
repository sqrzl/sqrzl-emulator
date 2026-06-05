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

fn bench_put_object(c: &mut Criterion) {
    let runtime = build_runtime();
    let server = runtime.block_on(LiveServer::start_api(auth_disabled()));
    let bucket = "tier4-s3-put";
    runtime.block_on(create_bucket(&server, bucket));
    let payload = Bytes::from_static(b"tier4 s3 payload");
    let object_url = format!("{}/{}/object.txt", server.base_url, bucket);

    let mut group = c.benchmark_group("tier4_integration_s3_put_object");
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

    let mut group = c.benchmark_group("tier4_integration_s3_get_object");
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

fn bench_list_objects(c: &mut Criterion) {
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
    let mut group = c.benchmark_group("tier4_integration_s3_list_objects");
    group.sampling_mode(SamplingMode::Flat);
    group.throughput(Throughput::Elements(128));
    group.bench_function(BenchmarkId::new("list_objects", 128), |b| {
        b.iter(|| {
            let request = Request::builder()
                .method("GET")
                .uri(&list_url)
                .body(Body::default())
                .expect("object list request should build");
            let listing = runtime.block_on(server.response_text(request));
            assert!(listing.contains("item-000.txt"));
            black_box(listing);
        })
    });
    group.finish();
}

fn bench_list_versions(c: &mut Criterion) {
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
    let mut group = c.benchmark_group("tier4_integration_s3_list_versions");
    group.sampling_mode(SamplingMode::Flat);
    group.throughput(Throughput::Elements(32));
    group.bench_function(BenchmarkId::new("list_versions", 32), |b| {
        b.iter_batched(
            || {
                Request::builder()
                    .method("GET")
                    .uri(&versions_url)
                    .body(Body::default())
                    .expect("versions list request should build")
            },
            |request| {
                let listing = runtime.block_on(server.response_text(request));
                assert!(listing.matches("<Version>").count() >= 32);
                black_box(listing);
            },
            BatchSize::SmallInput,
        )
    });
    group.finish();
}

criterion_group! {
    name = benches;
    config = criterion_config::criterion_config_for_tier4();
    targets = bench_put_object, bench_get_object, bench_list_objects, bench_list_versions
}
criterion_main!(benches);
