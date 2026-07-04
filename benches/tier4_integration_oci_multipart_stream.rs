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
const PART_COUNT: usize = 8;
const PART_SIZE: usize = 4096;

fn build_runtime() -> Runtime {
    Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("runtime should build")
}

fn count_as_u64(count: usize) -> u64 {
    u64::try_from(count).expect("count should fit in u64")
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

#[stress_test(
    tier = 4,
    metadata(
        component = "provider_api",
        provider = "oci",
        operation = "multipart_stream_upload"
    )
)]
fn multipart_stream_upload(ctx: &mut StressContext) {
    let runtime = build_runtime();
    let server = runtime.block_on(LiveServer::start_api(auth_disabled()));
    let bucket = "tier4-oci-multipart-stream";
    runtime.block_on(create_bucket(&server, bucket));
    let object = "multi-stream.txt";
    let init_url = format!("{}/n/{}/b/{}/u", server.base_url, TENANT, bucket);
    let multipart_url = format!("{}/n/{}/b/{}/u/{}", server.base_url, TENANT, bucket, object);
    let parts: Vec<Bytes> = (0..PART_COUNT)
        .map(|part_index| {
            let byte_offset =
                u8::try_from(part_index % 26).expect("part byte offset should fit in u8");
            Bytes::from(vec![b'a' + byte_offset; PART_SIZE])
        })
        .collect();

    ctx.parameter("part_count", PART_COUNT);
    ctx.parameter("part_size_bytes", PART_SIZE);
    ctx.parameter("payload_size_bytes", PART_COUNT * PART_SIZE);

    let upload_id = runtime.block_on(create_upload_session(&server, &init_url, object));
    let requests: Vec<_> = parts
        .iter()
        .enumerate()
        .map(|(index, part)| {
            multipart_part_request(
                &multipart_url,
                &upload_id,
                u32::try_from(index + 1).expect("part number should fit in u32"),
                part,
            )
        })
        .collect();

    let statuses = ctx.measure(|| {
        requests
            .into_iter()
            .map(|request| {
                let response = runtime.block_on(server.request(request));
                let etag = response.headers().get("etag").cloned();
                black_box(etag);
                response.status()
            })
            .collect::<Vec<_>>()
    });
    let completed = count_as_u64(
        statuses
            .iter()
            .filter(|status| **status == StatusCode::OK)
            .count(),
    );
    let attempted = count_as_u64(PART_COUNT);
    let _ = ctx
        .correctness()
        .attempted(attempted)
        .completed(completed)
        .failures(attempted - completed);
    assert!(statuses.iter().all(|status| *status == StatusCode::OK));

    let response = runtime.block_on(server.request(abort_request(&multipart_url, &upload_id)));
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
}

stress_main!();
