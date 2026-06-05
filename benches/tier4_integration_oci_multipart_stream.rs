use bytes::Bytes;
use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use http_body_util::Full;
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
const PART_COUNT: usize = 8;
const PART_SIZE: usize = 4096;

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

fn bench_multipart_stream_upload(c: &mut Criterion) {
    let runtime = build_runtime();
    let server = runtime.block_on(LiveServer::start_api(auth_disabled()));
    let bucket = "tier4-oci-multipart-stream";
    runtime.block_on(create_bucket(&server, bucket));
    let object = "multi-stream.txt";
    let init_url = format!("{}/n/{}/b/{}/u", server.base_url, TENANT, bucket);
    let multipart_url = format!("{}/n/{}/b/{}/u/{}", server.base_url, TENANT, bucket, object);
    let parts: Vec<Bytes> = (0..PART_COUNT)
        .map(|part_index| Bytes::from(vec![b'a' + (part_index as u8 % 26); PART_SIZE]))
        .collect();

    let mut group = c.benchmark_group("tier4_integration_oci_multipart_stream");
    group.sampling_mode(criterion::SamplingMode::Flat);
    group.throughput(Throughput::Bytes((PART_COUNT * PART_SIZE) as u64));
    group.bench_function(
        BenchmarkId::new("upload_stream", format!("{}x{}", PART_COUNT, PART_SIZE)),
        |b| {
            b.iter_custom(|iters| {
                let mut total = Duration::ZERO;
                for _ in 0..iters {
                    let upload_id =
                        runtime.block_on(create_upload_session(&server, &init_url, object));
                    let requests: Vec<_> = parts
                        .iter()
                        .enumerate()
                        .map(|(index, part)| {
                            multipart_part_request(
                                &multipart_url,
                                &upload_id,
                                (index + 1) as u32,
                                part,
                            )
                        })
                        .collect();

                    let start = Instant::now();
                    for request in requests {
                        let response = runtime.block_on(server.request(request));
                        assert_eq!(response.status(), StatusCode::OK);
                        let _ = response.headers().get("etag").cloned();
                    }
                    total += start.elapsed();

                    let response =
                        runtime.block_on(server.request(abort_request(&multipart_url, &upload_id)));
                    assert_eq!(response.status(), StatusCode::NO_CONTENT);
                }
                total
            })
        },
    );
    group.finish();
}

criterion_group! {
    name = benches;
    config = criterion_config::criterion_config_for_tier4();
    targets = bench_multipart_stream_upload
}
criterion_main!(benches);
