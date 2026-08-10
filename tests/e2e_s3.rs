mod common;

use bytes::Bytes;
use common::e2e::{auth_disabled, auth_enabled, text_body, LiveServer};
use http_body_util::Full;
type Body = Full<Bytes>;
use hyper::{Request, StatusCode};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

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

#[tokio::test(flavor = "multi_thread")]
async fn should_not_commit_truncated_s3_request_body_over_tcp() {
    // Arrange
    let server = LiveServer::start_s3(auth_disabled()).await;
    let create_bucket = Request::builder()
        .method("PUT")
        .uri(format!("{}/truncated-s3", server.base_url))
        .body(Body::default())
        .unwrap();
    assert_eq!(server.request(create_bucket).await.status(), StatusCode::OK);
    let address = server.base_url.trim_start_matches("http://");
    let mut stream = tokio::net::TcpStream::connect(address).await.unwrap();

    // Act
    stream
        .write_all(
            b"PUT /truncated-s3/object HTTP/1.1\r\nHost: localhost\r\nContent-Length: 5\r\nConnection: close\r\n\r\nabc",
        )
        .await
        .unwrap();
    stream.shutdown().await.unwrap();
    let mut raw_response = Vec::new();
    stream.read_to_end(&mut raw_response).await.unwrap();
    let head = Request::builder()
        .method("HEAD")
        .uri(format!("{}/truncated-s3/object", server.base_url))
        .body(Body::default())
        .unwrap();

    // Assert
    assert!(!String::from_utf8_lossy(&raw_response).contains(" 200 "));
    assert_eq!(server.request(head).await.status(), StatusCode::NOT_FOUND);
}

#[tokio::test(flavor = "multi_thread")]
async fn should_commit_s3_mutation_before_losing_response_over_tcp() {
    // Arrange
    let server = LiveServer::start_s3(auth_disabled()).await;
    let create_bucket = Request::builder()
        .method("PUT")
        .uri(format!("{}/ambiguous-s3", server.base_url))
        .body(Body::default())
        .unwrap();
    assert_eq!(server.request(create_bucket).await.status(), StatusCode::OK);
    let address = server.base_url.trim_start_matches("http://");
    let mut stream = tokio::net::TcpStream::connect(address).await.unwrap();

    // Act
    stream
        .write_all(
            b"PUT /ambiguous-s3/object HTTP/1.1\r\nHost: localhost\r\nContent-Length: 5\r\nx-sqrzl-failpoint: response-loss-after-commit\r\nConnection: close\r\n\r\nvalue",
        )
        .await
        .unwrap();
    let mut raw_response = Vec::new();
    stream.read_to_end(&mut raw_response).await.unwrap();
    let get = Request::builder()
        .method("GET")
        .uri(format!("{}/ambiguous-s3/object", server.base_url))
        .body(Body::default())
        .unwrap();

    // Assert
    let raw_response = String::from_utf8_lossy(&raw_response);
    assert!(!raw_response.contains(" 200 "));
    let fetched = server.request(get).await;
    assert_eq!(fetched.status(), StatusCode::OK);
    assert_eq!(text_body(fetched).await, "value");
}

#[tokio::test(flavor = "multi_thread")]
async fn should_lose_no_content_delete_response_after_commit_over_tcp() {
    // Arrange
    let server = LiveServer::start_s3(auth_disabled()).await;
    let create_bucket = Request::builder()
        .method("PUT")
        .uri(format!("{}/ambiguous-delete-s3", server.base_url))
        .body(Body::default())
        .unwrap();
    assert_eq!(server.request(create_bucket).await.status(), StatusCode::OK);
    let put = Request::builder()
        .method("PUT")
        .uri(format!("{}/ambiguous-delete-s3/object", server.base_url))
        .body(Body::from("value"))
        .unwrap();
    assert_eq!(server.request(put).await.status(), StatusCode::OK);
    let address = server.base_url.trim_start_matches("http://");
    let mut stream = tokio::net::TcpStream::connect(address).await.unwrap();

    // Act
    stream
        .write_all(
            b"DELETE /ambiguous-delete-s3/object HTTP/1.1\r\nHost: localhost\r\nx-sqrzl-failpoint: response-loss-after-commit\r\nConnection: close\r\n\r\n",
        )
        .await
        .unwrap();
    let mut raw_response = Vec::new();
    stream.read_to_end(&mut raw_response).await.unwrap();
    let head = Request::builder()
        .method("HEAD")
        .uri(format!("{}/ambiguous-delete-s3/object", server.base_url))
        .body(Body::default())
        .unwrap();

    // Assert
    assert!(
        raw_response.is_empty(),
        "response-loss must close before serializing a clean 204 response"
    );
    assert_eq!(server.request(head).await.status(), StatusCode::NOT_FOUND);
}
