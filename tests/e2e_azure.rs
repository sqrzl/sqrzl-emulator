mod common;

use bytes::Bytes;
use common::e2e::{auth_disabled, auth_enabled, text_body, LiveServer, AZURE_VERSION};
use http_body_util::Full;
type Body = Full<Bytes>;
use hyper::{Request, StatusCode};

#[tokio::test(flavor = "multi_thread")]
async fn should_round_trip_block_blob_given_live_server_when_using_basic_azure_crud_and_range_reads(
) {
    let server = LiveServer::start_api(auth_disabled()).await;

    let create_container = Request::builder()
        .method("PUT")
        .uri(format!(
            "{}/devstoreaccount1/e2e-azure?restype=container",
            server.base_url
        ))
        .header("x-ms-version", AZURE_VERSION)
        .body(Body::default())
        .expect("container create request should build");
    let create_container_response = server.request(create_container).await;
    assert_eq!(create_container_response.status(), StatusCode::CREATED);

    let put_blob = Request::builder()
        .method("PUT")
        .uri(format!(
            "{}/devstoreaccount1/e2e-azure/hello.txt",
            server.base_url
        ))
        .header("x-ms-version", AZURE_VERSION)
        .header("x-ms-blob-type", "BlockBlob")
        .header("content-type", "text/plain")
        .body(Body::from("azure over tcp"))
        .expect("blob put request should build");
    let put_blob_response = server.request(put_blob).await;
    assert_eq!(put_blob_response.status(), StatusCode::CREATED);

    let get_blob = Request::builder()
        .method("GET")
        .uri(format!(
            "{}/devstoreaccount1/e2e-azure/hello.txt",
            server.base_url
        ))
        .header("x-ms-version", AZURE_VERSION)
        .body(Body::default())
        .expect("blob get request should build");
    let get_blob_response = server.request(get_blob).await;
    assert_eq!(get_blob_response.status(), StatusCode::OK);
    assert_eq!(text_body(get_blob_response).await, "azure over tcp");

    let range_blob = Request::builder()
        .method("GET")
        .uri(format!(
            "{}/devstoreaccount1/e2e-azure/hello.txt",
            server.base_url
        ))
        .header("x-ms-version", AZURE_VERSION)
        .header("x-ms-range", "bytes=0-4")
        .body(Body::default())
        .expect("blob range request should build");
    let range_blob_response = server.request(range_blob).await;
    assert_eq!(range_blob_response.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(text_body(range_blob_response).await, "azure");
}

#[tokio::test(flavor = "multi_thread")]
async fn should_reject_unauthorized_container_list_given_live_server_when_azure_auth_is_enforced() {
    let server = LiveServer::start_api(auth_enabled("azure-auth", "dG9wc2VjcmV0a2V5")).await;

    let list_containers = Request::builder()
        .method("GET")
        .uri(format!("{}/devstoreaccount1?comp=list", server.base_url))
        .header("x-ms-version", AZURE_VERSION)
        .body(Body::default())
        .expect("container list request should build");
    let response = server.request(list_containers).await;

    assert!(matches!(
        response.status(),
        StatusCode::FORBIDDEN | StatusCode::UNAUTHORIZED
    ));
}
