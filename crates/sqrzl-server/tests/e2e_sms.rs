mod common;

use bytes::Bytes;
use common::e2e::{auth_disabled, text_body, LiveServer};
use http_body_util::Full;
use hyper::{Request, StatusCode};
use serde_json::Value;

type Body = Full<Bytes>;

#[tokio::test(flavor = "multi_thread")]
async fn should_simulate_list_download_and_delete_texts_through_admin_api() {
    let server = LiveServer::start_admin(auth_disabled()).await;
    let inbound = Request::builder()
        .method("POST")
        .uri(format!(
            "{}/admin/v1/text-simulations/inbound",
            server.base_url
        ))
        .header("content-type", "application/json")
        .body(Body::from(
            serde_json::json!({
                "provider": "twilio",
                "from": "+15550000002",
                "to": "+15550000001",
                "body": "admin e2e",
                "media": [{
                    "filename": "hello.txt",
                    "content_type": "text/plain",
                    "content_base64": "aGVsbG8="
                }]
            })
            .to_string(),
        ))
        .unwrap();
    let response = server.request(inbound).await;
    assert_eq!(response.status(), StatusCode::CREATED);
    let detail: Value = serde_json::from_str(&text_body(response).await).unwrap();
    assert_eq!(detail["peer"], "+15550000002");
    assert_eq!(detail["callback_attempts"], serde_json::json!([]));
    let message_id = detail["message_id"].as_str().unwrap();
    let media_id = detail["media"][0]["media_id"].as_str().unwrap();

    let list = Request::builder()
        .uri(format!(
            "{}/admin/v1/text-conversations?search=admin&limit=1",
            server.base_url
        ))
        .body(Body::default())
        .unwrap();
    let response = server.request(list).await;
    assert_eq!(response.status(), StatusCode::OK);
    let conversations: Value = serde_json::from_str(&text_body(response).await).unwrap();
    assert_eq!(conversations["items"][0]["peer"], "+15550000002");

    let encoded_peer = "%2B15550000002";
    let media = Request::builder()
        .uri(format!(
            "{}/admin/v1/text-conversations/{encoded_peer}/messages/{message_id}/media/{media_id}",
            server.base_url
        ))
        .body(Body::default())
        .unwrap();
    let response = server.request(media).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(text_body(response).await, "hello");

    let delete = Request::builder()
        .method("DELETE")
        .uri(format!(
            "{}/admin/v1/text-conversations/{encoded_peer}/messages/{message_id}",
            server.base_url
        ))
        .body(Body::default())
        .unwrap();
    assert_eq!(
        server.request(delete).await.status(),
        StatusCode::NO_CONTENT
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn should_reject_provider_inbound_media_and_disallowed_callback_hosts() {
    let server = LiveServer::start_admin(auth_disabled()).await;
    let inbound = Request::builder()
        .method("POST")
        .uri(format!(
            "{}/admin/v1/text-simulations/inbound",
            server.base_url
        ))
        .header("content-type", "application/json")
        .body(Body::from(
            r#"{"provider":"acs","from":"+15550000002","to":"+15550000001","body":"no mms","media":[{"filename":"x","content_type":"text/plain","content_base64":"eA=="}]}"#,
        ))
        .unwrap();
    assert_eq!(
        server.request(inbound).await.status(),
        StatusCode::BAD_REQUEST
    );

    let destination = Request::builder()
        .method("PUT")
        .uri(format!(
            "{}/admin/v1/text-destinations/twilio/%2B15550000001",
            server.base_url
        ))
        .header("content-type", "application/json")
        .body(Body::from(
            r#"{"callback_url":"http://example.com/callback"}"#,
        ))
        .unwrap();
    assert_eq!(
        server.request(destination).await.status(),
        StatusCode::BAD_REQUEST
    );
}
