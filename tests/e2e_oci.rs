mod common;

use bytes::Bytes;
use common::e2e::{auth_disabled, text_body, LiveServer};
use http_body_util::Full;
type Body = Full<Bytes>;
use hyper::{Request, StatusCode};

#[tokio::test(flavor = "multi_thread")]
async fn should_round_trip_namespace_bucket_and_object_given_live_server_when_using_oci_core_flows()
{
    let server = LiveServer::start_api(auth_disabled()).await;

    let namespace_request = Request::builder()
        .method("GET")
        .uri(format!("{}/n/tenant", server.base_url))
        .body(Body::default())
        .expect("namespace request should build");
    let namespace_response = server.request(namespace_request).await;
    assert_eq!(namespace_response.status(), StatusCode::OK);
    assert_eq!(text_body(namespace_response).await, "tenant");

    let create_bucket = Request::builder()
        .method("POST")
        .uri(format!("{}/n/tenant/b", server.base_url))
        .header("content-type", "application/json")
        .body(Body::from(
            r#"{"name":"e2e-oci","compartmentId":"ignored"}"#,
        ))
        .expect("bucket create request should build");
    let create_bucket_response = server.request(create_bucket).await;
    assert_eq!(create_bucket_response.status(), StatusCode::OK);

    let put_object = Request::builder()
        .method("PUT")
        .uri(format!(
            "{}/n/tenant/b/e2e-oci/o/hello.txt",
            server.base_url
        ))
        .header("content-type", "text/plain")
        .body(Body::from("oci over tcp"))
        .expect("object put request should build");
    let put_object_response = server.request(put_object).await;
    assert_eq!(put_object_response.status(), StatusCode::OK);

    let get_object = Request::builder()
        .method("GET")
        .uri(format!(
            "{}/n/tenant/b/e2e-oci/o/hello.txt",
            server.base_url
        ))
        .body(Body::default())
        .expect("object get request should build");
    let get_object_response = server.request(get_object).await;
    assert_eq!(get_object_response.status(), StatusCode::OK);
    assert_eq!(text_body(get_object_response).await, "oci over tcp");
}

#[tokio::test(flavor = "multi_thread")]
async fn should_commit_multipart_object_given_live_server_when_finalizing_oci_upload() {
    let server = LiveServer::start_api(auth_disabled()).await;

    let create_bucket = Request::builder()
        .method("POST")
        .uri(format!("{}/n/tenant/b", server.base_url))
        .header("content-type", "application/json")
        .body(Body::from(
            r#"{"name":"multipart-bucket","compartmentId":"ignored"}"#,
        ))
        .expect("bucket create request should build");
    let create_bucket_response = server.request(create_bucket).await;
    assert_eq!(create_bucket_response.status(), StatusCode::OK);

    let init_upload = Request::builder()
        .method("POST")
        .uri(format!("{}/n/tenant/b/multipart-bucket/u", server.base_url))
        .header("content-type", "application/json")
        .body(Body::from(
            r#"{"object":"multi.txt","contentType":"text/plain","metadata":{"owner":"sdk"},"storageTier":"InfrequentAccess"}"#,
        ))
        .expect("multipart init request should build");
    let init_upload_response = server.request(init_upload).await;
    assert_eq!(init_upload_response.status(), StatusCode::OK);
    let init_upload_json: serde_json::Value =
        serde_json::from_str(&text_body(init_upload_response).await)
            .expect("multipart init body should parse");
    let upload_id = init_upload_json
        .get("uploadId")
        .and_then(|value| value.as_str())
        .expect("upload id should exist");

    let upload_part_one = Request::builder()
        .method("PUT")
        .uri(format!(
            "{}/n/tenant/b/multipart-bucket/u/multi.txt?uploadId={upload_id}&uploadPartNum=1",
            server.base_url
        ))
        .body(Body::from("multi"))
        .expect("multipart part one request should build");
    let upload_part_one_response = server.request(upload_part_one).await;
    assert_eq!(upload_part_one_response.status(), StatusCode::OK);
    let etag_one = upload_part_one_response
        .headers()
        .get("etag")
        .and_then(|value| value.to_str().ok())
        .expect("etag one should exist")
        .to_string();

    let upload_part_two = Request::builder()
        .method("PUT")
        .uri(format!(
            "{}/n/tenant/b/multipart-bucket/u/multi.txt?uploadId={upload_id}&uploadPartNum=2",
            server.base_url
        ))
        .body(Body::from("part"))
        .expect("multipart part two request should build");
    let upload_part_two_response = server.request(upload_part_two).await;
    assert_eq!(upload_part_two_response.status(), StatusCode::OK);
    let etag_two = upload_part_two_response
        .headers()
        .get("etag")
        .and_then(|value| value.to_str().ok())
        .expect("etag two should exist")
        .to_string();

    let commit_upload = Request::builder()
        .method("POST")
        .uri(format!(
            "{}/n/tenant/b/multipart-bucket/u/multi.txt?uploadId={upload_id}",
            server.base_url
        ))
        .header("content-type", "application/json")
        .body(Body::from(format!(
            "{{\"partsToCommit\":[{{\"partNum\":1,\"etag\":\"{etag_one}\"}},{{\"partNum\":2,\"etag\":\"{etag_two}\"}}]}}"
        )))
        .expect("multipart commit request should build");
    let commit_upload_response = server.request(commit_upload).await;
    assert_eq!(commit_upload_response.status(), StatusCode::OK);

    let get_object = Request::builder()
        .method("GET")
        .uri(format!(
            "{}/n/tenant/b/multipart-bucket/o/multi.txt",
            server.base_url
        ))
        .body(Body::default())
        .expect("multipart object get request should build");
    let get_object_response = server.request(get_object).await;
    assert_eq!(get_object_response.status(), StatusCode::OK);
    assert_eq!(text_body(get_object_response).await, "multipart");
}
