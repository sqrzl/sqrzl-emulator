mod common;

use bytes::Bytes;
use common::e2e::{auth_disabled, auth_enabled, text_body, LiveServer};
use http_body_util::Full;
type Body = Full<Bytes>;
use hyper::{Request, StatusCode};

#[tokio::test(flavor = "multi_thread")]
async fn should_report_health_given_live_server_when_using_api_port() {
    let server = LiveServer::start_s3(auth_disabled()).await;

    let request = Request::builder()
        .method("GET")
        .uri(format!("{}/healthz", server.base_url))
        .body(Body::default())
        .expect("health request should build");
    let response = server.request(request).await;

    assert_eq!(response.status(), StatusCode::OK);
    let body = text_body(response).await;
    assert!(body.contains(r#""status":"ok""#));
    assert!(body.contains(r#""storage_ready":true"#));
    assert!(body.contains("s3-family"));
}

#[tokio::test(flavor = "multi_thread")]
async fn should_round_trip_bucket_and_object_given_live_server_when_using_basic_s3_crud_flows() {
    let server = LiveServer::start_s3(auth_disabled()).await;

    let create_bucket = Request::builder()
        .method("PUT")
        .uri(format!("{}/e2e-s3", server.base_url))
        .body(Body::default())
        .expect("bucket create request should build");
    let create_bucket_response = server.request(create_bucket).await;
    assert_eq!(create_bucket_response.status(), StatusCode::OK);

    let put_object = Request::builder()
        .method("PUT")
        .uri(format!("{}/e2e-s3/hello.txt", server.base_url))
        .header("content-type", "text/plain")
        .body(Body::from("hello over tcp"))
        .expect("object put request should build");
    let put_object_response = server.request(put_object).await;
    assert_eq!(put_object_response.status(), StatusCode::OK);

    let get_object = Request::builder()
        .method("GET")
        .uri(format!("{}/e2e-s3/hello.txt", server.base_url))
        .body(Body::default())
        .expect("object get request should build");
    let get_object_response = server.request(get_object).await;
    assert_eq!(get_object_response.status(), StatusCode::OK);
    assert_eq!(text_body(get_object_response).await, "hello over tcp");

    let list_objects = Request::builder()
        .method("GET")
        .uri(format!("{}/e2e-s3?list-type=2", server.base_url))
        .body(Body::default())
        .expect("object list request should build");
    let list_objects_response = server.request(list_objects).await;
    assert_eq!(list_objects_response.status(), StatusCode::OK);
    let listing = text_body(list_objects_response).await;
    assert!(listing.contains("hello.txt"));
}

#[tokio::test(flavor = "multi_thread")]
async fn should_reject_unsigned_request_given_live_server_when_s3_auth_is_enforced() {
    let server = LiveServer::start_s3(auth_enabled("test", "test-secret")).await;

    let list_buckets = Request::builder()
        .method("GET")
        .uri(format!("{}/", server.base_url))
        .body(Body::default())
        .expect("bucket list request should build");
    let response = server.request(list_buckets).await;

    assert!(matches!(
        response.status(),
        StatusCode::FORBIDDEN | StatusCode::UNAUTHORIZED
    ));
}

#[tokio::test(flavor = "multi_thread")]
async fn should_reject_oversized_upload_given_live_server_when_max_request_bytes_is_exceeded() {
    let mut config = auth_disabled();
    config.max_request_bytes = 3;
    let server = LiveServer::start_s3(config).await;

    let request = Request::builder()
        .method("PUT")
        .uri(format!("{}/too-large-bucket/hello.txt", server.base_url))
        .header("content-type", "text/plain")
        .header("content-length", "4")
        .body(Body::from("nope"))
        .expect("oversized request should build");
    let response = server.request(request).await;

    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    let body = text_body(response).await;
    assert!(body.contains("EntityTooLarge"));
    assert!(body.contains("SQRZL_MAX_REQUEST_BYTES"));
}
