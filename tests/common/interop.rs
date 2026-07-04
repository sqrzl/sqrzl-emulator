#![allow(dead_code)]

use bytes::Bytes;
use http_body_util::BodyExt;
use http_body_util::Full;
type Body = Full<Bytes>;
use hyper::{Request as HyperRequest, Response};
use sqrzl_emulator::providers::AdapterRegistry;
use sqrzl_emulator::server::RequestExt;
use sqrzl_emulator::storage::{FilesystemStorage, Storage};
use sqrzl_emulator::Config;
use std::fs;
use std::sync::Arc;

pub const AZURE_VERSION: &str = "2023-11-03";

pub fn temp_storage() -> Arc<dyn Storage> {
    let dir = std::env::temp_dir().join(format!("sqrzl-interop-rust-{}", uuid::Uuid::new_v4()));
    let _ = fs::create_dir_all(&dir);
    Arc::new(FilesystemStorage::new(dir))
}

pub fn auth_disabled() -> Arc<Config> {
    Arc::new(Config {
        access_key_id: None,
        secret_access_key: None,
        enforce_auth: false,
        admin_auth_disabled: false,
        blobs_path: "./blobs".to_string(),
        lifecycle_interval: std::time::Duration::from_hours(1),
        api_port: 9000,
        ui_port: 9001,
        max_request_bytes: sqrzl_emulator::config::DEFAULT_SQRZL_MAX_REQUEST_BYTES,
    })
}

pub fn auth_enabled(key: &str, secret: &str) -> Arc<Config> {
    Arc::new(Config {
        access_key_id: Some(key.to_string()),
        secret_access_key: Some(secret.to_string()),
        enforce_auth: true,
        admin_auth_disabled: false,
        blobs_path: "./blobs".to_string(),
        lifecycle_interval: std::time::Duration::from_hours(1),
        api_port: 9000,
        ui_port: 9001,
        max_request_bytes: sqrzl_emulator::config::DEFAULT_SQRZL_MAX_REQUEST_BYTES,
    })
}

pub fn request(
    method: &str,
    uri: &str,
    headers: &[(&str, &str)],
    body: &[u8],
) -> HyperRequest<Body> {
    let mut builder = HyperRequest::builder().method(method).uri(uri);
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    builder
        .body(Body::from(body.to_vec()))
        .expect("request should build")
}

pub async fn call(
    storage: Arc<dyn Storage>,
    auth_config: Arc<Config>,
    request: HyperRequest<Body>,
) -> Response<Body> {
    call_with_registry(
        Arc::new(AdapterRegistry::default()),
        storage,
        auth_config,
        request,
    )
    .await
}

pub async fn call_with_registry(
    adapters: Arc<AdapterRegistry>,
    storage: Arc<dyn Storage>,
    auth_config: Arc<Config>,
    request: HyperRequest<Body>,
) -> Response<Body> {
    let parsed = RequestExt::from_hyper(request)
        .await
        .expect("request should parse");
    adapters
        .handle(storage, auth_config, parsed)
        .await
        .expect("request should complete")
}

pub async fn body_bytes(response: Response<Body>) -> Vec<u8> {
    response
        .into_body()
        .collect()
        .await
        .expect("response body should read")
        .to_bytes()
        .to_vec()
}

pub async fn body_text(response: Response<Body>) -> String {
    String::from_utf8(body_bytes(response).await).expect("body should be utf8")
}

pub fn extract_tag(xml: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = xml.find(&open)? + open.len();
    let end = xml[start..].find(&close)? + start;
    Some(xml[start..end].to_string())
}
