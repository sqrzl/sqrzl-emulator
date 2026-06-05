mod common;

use bytes::Bytes;
use common::e2e::{auth_disabled, rebase_url, text_body, LiveServer};
use http_body_util::Full;
type Body = Full<Bytes>;
use hyper::{Request, StatusCode};

#[tokio::test(flavor = "multi_thread")]
async fn should_round_trip_bucket_and_object_given_live_server_when_using_gcs_xml_api() {
    let server = LiveServer::start_api(auth_disabled()).await;

    let create_bucket = Request::builder()
        .method("PUT")
        .uri(format!("{}/e2e-gcs", server.base_url))
        .header("host", "storage.googleapis.com")
        .body(Body::default())
        .expect("bucket create request should build");
    let create_bucket_response = server.request(create_bucket).await;
    assert_eq!(create_bucket_response.status(), StatusCode::OK);

    let put_object = Request::builder()
        .method("PUT")
        .uri(format!("{}/e2e-gcs/hello.txt", server.base_url))
        .header("host", "storage.googleapis.com")
        .header("content-type", "text/plain")
        .body(Body::from("gcs over tcp"))
        .expect("object put request should build");
    let put_object_response = server.request(put_object).await;
    assert_eq!(put_object_response.status(), StatusCode::OK);

    let get_object = Request::builder()
        .method("GET")
        .uri(format!("{}/e2e-gcs/hello.txt", server.base_url))
        .header("host", "storage.googleapis.com")
        .body(Body::default())
        .expect("object get request should build");
    let get_object_response = server.request(get_object).await;
    assert_eq!(get_object_response.status(), StatusCode::OK);
    assert_eq!(text_body(get_object_response).await, "gcs over tcp");

    let list_objects = Request::builder()
        .method("GET")
        .uri(format!("{}/e2e-gcs", server.base_url))
        .header("host", "storage.googleapis.com")
        .body(Body::default())
        .expect("object list request should build");
    let list_objects_response = server.request(list_objects).await;
    assert_eq!(list_objects_response.status(), StatusCode::OK);
    let listing = text_body(list_objects_response).await;
    assert!(listing.contains("hello.txt"));
}

#[tokio::test(flavor = "multi_thread")]
async fn should_complete_resumable_upload_given_live_server_when_using_gcs_json_api() {
    let server = LiveServer::start_api(auth_disabled()).await;

    let create_bucket = Request::builder()
        .method("POST")
        .uri(format!(
            "{}/storage/v1/b?project=test-project",
            server.base_url
        ))
        .header("host", "storage.googleapis.com")
        .header("content-type", "application/json")
        .body(Body::from(r#"{"name":"json-bucket"}"#))
        .expect("json api bucket create request should build");
    let create_bucket_response = server.request(create_bucket).await;
    assert_eq!(create_bucket_response.status(), StatusCode::OK);

    let start_upload = Request::builder()
        .method("POST")
        .uri(format!(
            "{}/upload/storage/v1/b/json-bucket/o?uploadType=resumable&name=hello.txt",
            server.base_url
        ))
        .header("host", "storage.googleapis.com")
        .header("x-upload-content-type", "text/plain")
        .header("x-goog-meta-owner", "jules")
        .body(Body::default())
        .expect("resumable init request should build");
    let start_upload_response = server.request(start_upload).await;
    assert_eq!(start_upload_response.status(), StatusCode::OK);
    let upstream_location = start_upload_response
        .headers()
        .get("location")
        .and_then(|value| value.to_str().ok())
        .expect("resumable location should exist")
        .to_string();

    let upload_location = rebase_url(&server.base_url, &upstream_location);
    let upload_object = Request::builder()
        .method("PUT")
        .uri(upload_location)
        .header("host", "storage.googleapis.com")
        .body(Body::from("json api over tcp"))
        .expect("resumable upload request should build");
    let upload_object_response = server.request(upload_object).await;
    assert_eq!(upload_object_response.status(), StatusCode::OK);

    let get_metadata = Request::builder()
        .method("GET")
        .uri(format!(
            "{}/storage/v1/b/json-bucket/o/hello.txt",
            server.base_url
        ))
        .header("host", "storage.googleapis.com")
        .body(Body::default())
        .expect("json api metadata request should build");
    let get_metadata_response = server.request(get_metadata).await;
    assert_eq!(get_metadata_response.status(), StatusCode::OK);
    let metadata = text_body(get_metadata_response).await;
    assert!(metadata.contains("\"hello.txt\""));
    assert!(metadata.contains("\"owner\":\"jules\""));
}
