use bytes::Bytes;
use criterion::{
    criterion_group, criterion_main, BatchSize, BenchmarkId, Criterion, SamplingMode, Throughput,
};
use http_body_util::Full;
use std::hint::black_box;
type Body = Full<Bytes>;

use hyper::{Request, StatusCode};
use std::time::{Duration, Instant};
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

fn multipart_init_request(init_url: &str, object: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(init_url)
        .header("content-type", "application/json")
        .body(Body::from(
            serde_json::json!({
                "object": object,
                "contentType": "text/plain",
                "metadata": { "owner": "bench" },
                "storageTier": "InfrequentAccess"
            })
            .to_string(),
        ))
        .expect("multipart init request should build")
}

fn multipart_part_request(
    multipart_url: &str,
    upload_id: &str,
    part_number: u32,
    part: &Bytes,
) -> Request<Body> {
    Request::builder()
        .method("PUT")
        .uri(format!(
            "{multipart_url}?uploadId={upload_id}&uploadPartNum={part_number}"
        ))
        .body(Body::from(part.clone()))
        .expect("multipart part request should build")
}

fn multipart_commit_request(
    multipart_url: &str,
    upload_id: &str,
    etag_one: &str,
    etag_two: &str,
) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(format!("{multipart_url}?uploadId={upload_id}"))
        .header("content-type", "application/json")
        .body(Body::from(
            serde_json::json!({
                "partsToCommit": [
                    { "partNum": 1, "etag": etag_one },
                    { "partNum": 2, "etag": etag_two }
                ]
            })
            .to_string(),
        ))
        .expect("multipart commit request should build")
}

fn abort_request(multipart_url: &str, upload_id: &str) -> Request<Body> {
    Request::builder()
        .method("DELETE")
        .uri(format!("{multipart_url}?uploadId={upload_id}"))
        .body(Body::default())
        .expect("multipart abort request should build")
}

async fn create_upload_session(server: &LiveServer, init_url: &str, object: &str) -> String {
    let request = multipart_init_request(init_url, object);
    let body = server.response_bytes(request).await;
    let init_json: serde_json::Value =
        serde_json::from_slice(&body).expect("multipart init body should parse");
    init_json
        .get("uploadId")
        .and_then(|value| value.as_str())
        .expect("multipart upload id should exist")
        .to_string()
}

async fn upload_part(
    server: &LiveServer,
    multipart_url: &str,
    upload_id: &str,
    part_number: u32,
    part: &Bytes,
) -> String {
    let request = multipart_part_request(multipart_url, upload_id, part_number, part);
    let response = server.request(request).await;
    assert_eq!(response.status(), StatusCode::OK);
    response
        .headers()
        .get("etag")
        .and_then(|value| value.to_str().ok())
        .expect("multipart part etag should exist")
        .to_string()
}

struct MultipartCommitState {
    upload_id: String,
    etag_one: String,
    etag_two: String,
}

async fn prepare_commit_state(
    server: &LiveServer,
    init_url: &str,
    multipart_url: &str,
    object: &str,
    part_one: &Bytes,
    part_two: &Bytes,
) -> MultipartCommitState {
    let upload_id = create_upload_session(server, init_url, object).await;
    let etag_one = upload_part(server, multipart_url, &upload_id, 1, part_one).await;
    let etag_two = upload_part(server, multipart_url, &upload_id, 2, part_two).await;
    MultipartCommitState {
        upload_id,
        etag_one,
        etag_two,
    }
}

fn bench_multipart_init(c: &mut Criterion) {
    let runtime = build_runtime();
    let server = runtime.block_on(LiveServer::start_api(auth_disabled()));
    let bucket = "tier4-oci-multipart-init";
    runtime.block_on(create_bucket(&server, bucket));
    let object = "multi.txt";
    let init_url = format!("{}/n/{}/b/{}/u", server.base_url, TENANT, bucket);

    let mut group = c.benchmark_group("tier4_integration_oci_multipart_init");
    group.sampling_mode(SamplingMode::Flat);
    group.throughput(Throughput::Elements(1));
    let multipart_url = format!("{}/n/{}/b/{}/u/{}", server.base_url, TENANT, bucket, object);
    group.bench_function(BenchmarkId::new("init", 1), |b| {
        b.iter_custom(|iters| {
            let mut total = Duration::ZERO;
            for _ in 0..iters {
                let request = multipart_init_request(&init_url, object);
                let start = Instant::now();
                let body = runtime.block_on(server.response_bytes(request));
                total += start.elapsed();
                let init_json: serde_json::Value =
                    serde_json::from_slice(&body).expect("multipart init body should parse");
                let upload_id = init_json
                    .get("uploadId")
                    .and_then(|value| value.as_str())
                    .expect("multipart upload id should exist")
                    .to_string();
                let response =
                    runtime.block_on(server.request(abort_request(&multipart_url, &upload_id)));
                assert_eq!(response.status(), StatusCode::NO_CONTENT);
                black_box(body);
            }
            total
        });
    });
    group.finish();
}

fn bench_multipart_part_upload(c: &mut Criterion) {
    let runtime = build_runtime();
    let server = runtime.block_on(LiveServer::start_api(auth_disabled()));
    let bucket = "tier4-oci-multipart-part";
    runtime.block_on(create_bucket(&server, bucket));
    let object = "multi.txt";
    let init_url = format!("{}/n/{}/b/{}/u", server.base_url, TENANT, bucket);
    let multipart_url = format!("{}/n/{}/b/{}/u/{}", server.base_url, TENANT, bucket, object);
    let part = Bytes::from(vec![b'a'; 4096]);
    let upload_id = runtime.block_on(create_upload_session(&server, &init_url, object));

    let mut group = c.benchmark_group("tier4_integration_oci_multipart_part_upload");
    group.sampling_mode(SamplingMode::Flat);
    group.throughput(Throughput::Bytes(part.len() as u64));
    group.bench_function(BenchmarkId::new("upload_part", part.len()), |b| {
        b.iter_batched(
            || multipart_part_request(&multipart_url, &upload_id, 1, &part),
            |request| {
                let response = runtime.block_on(server.request(request));
                assert_eq!(response.status(), StatusCode::OK);
                black_box(response.headers().get("etag").cloned());
            },
            BatchSize::SmallInput,
        );
    });
    group.finish();
    let response = runtime.block_on(server.request(abort_request(&multipart_url, &upload_id)));
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
}

fn bench_multipart_commit(c: &mut Criterion) {
    let runtime = build_runtime();
    let server = runtime.block_on(LiveServer::start_api(auth_disabled()));
    let bucket = "tier4-oci-multipart-commit";
    runtime.block_on(create_bucket(&server, bucket));
    let object = "multi.txt";
    let init_url = format!("{}/n/{}/b/{}/u", server.base_url, TENANT, bucket);
    let multipart_url = format!("{}/n/{}/b/{}/u/{}", server.base_url, TENANT, bucket, object);
    let part_one = Bytes::from(vec![b'a'; 4096]);
    let part_two = Bytes::from(vec![b'b'; 4096]);

    let mut group = c.benchmark_group("tier4_integration_oci_multipart_commit");
    group.sampling_mode(SamplingMode::Flat);
    group.throughput(Throughput::Bytes((part_one.len() + part_two.len()) as u64));
    group.bench_function(
        BenchmarkId::new("commit", part_one.len() + part_two.len()),
        |b| {
            b.iter_batched(
                || {
                    runtime.block_on(async {
                        let state = prepare_commit_state(
                            &server,
                            &init_url,
                            &multipart_url,
                            object,
                            &part_one,
                            &part_two,
                        )
                        .await;
                        multipart_commit_request(
                            &multipart_url,
                            &state.upload_id,
                            &state.etag_one,
                            &state.etag_two,
                        )
                    })
                },
                |request| {
                    let response = runtime.block_on(server.request(request));
                    assert_eq!(response.status(), StatusCode::OK);
                    black_box(response.headers().get("etag").cloned());
                },
                BatchSize::SmallInput,
            );
        },
    );
    group.finish();
}

criterion_group! {
    name = benches;
    config = criterion_config::criterion_config_for_tier4();
    targets = bench_multipart_init, bench_multipart_part_upload, bench_multipart_commit
}
criterion_main!(benches);
