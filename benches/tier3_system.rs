use bytes::Bytes;
use cntryl_stress::prelude::*;
use http::header::CONTENT_TYPE;
use http::{HeaderMap, HeaderValue, Method, Uri};
use sqrzl_emulator::auth::AuthConfig;
use sqrzl_emulator::providers::AdapterRegistry;
use sqrzl_emulator::server::RequestExt;
use sqrzl_emulator::storage::{FilesystemStorage, Storage};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::runtime::Builder;
use uuid::Uuid;

#[path = "support/mod.rs"]
mod support;

use support::live_server::auth_disabled;

fn temp_path() -> PathBuf {
    std::env::temp_dir().join(format!("sqrzl_bench_tier3_{}", Uuid::new_v4()))
}

fn cleanup(base: &Path) {
    let _ = std::fs::remove_dir_all(base);
}

fn direct_request(method: Method, uri: &str, body: &[u8]) -> RequestExt {
    let uri: Uri = uri.parse().expect("uri should parse");
    let mut headers = HeaderMap::new();
    if !body.is_empty() {
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("text/plain"));
    }

    let mut query_params = HashMap::new();
    if let Some(query) = uri.query() {
        for param in query.split('&') {
            if param.is_empty() {
                continue;
            }

            if let Some((key, value)) = param.split_once('=') {
                let decoded_key = urlencoding::decode(key).unwrap_or_default().to_string();
                let decoded_value = urlencoding::decode(value).unwrap_or_default().to_string();
                query_params.insert(decoded_key, decoded_value);
            } else {
                let decoded_key = urlencoding::decode(param).unwrap_or_default().to_string();
                query_params.insert(decoded_key, String::new());
            }
        }
    }

    RequestExt {
        method,
        uri,
        headers,
        body: Bytes::copy_from_slice(body),
        path_params: HashMap::new(),
        query_params,
    }
}

#[stress_test(
    tier = 3,
    metadata(
        component = "adapter_registry",
        provider = "s3",
        operation = "put_object",
        scenario = "direct_request"
    )
)]
fn direct_put_object(ctx: &mut StressContext) {
    let runtime = Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("runtime should build");
    let base = temp_path();
    let storage: Arc<dyn Storage> = Arc::new(FilesystemStorage::new(&base));
    storage
        .create_bucket("bench".to_string())
        .expect("bucket create should succeed");
    let auth_config: Arc<AuthConfig> = Arc::new(auth_disabled());
    let registry = Arc::new(AdapterRegistry::default());
    let body = Bytes::from_static(b"hello from the system bench");

    ctx.parameter("payload_size_bytes", body.len());
    let response = ctx.measure(|| {
        let request = direct_request(
            Method::PUT,
            "http://localhost/bench/item.txt",
            body.as_ref(),
        );
        runtime
            .block_on(registry.handle(storage.clone(), auth_config.clone(), request))
            .expect("direct put should succeed")
    });
    black_box(response);
    cleanup(&base);
}

#[stress_test(
    tier = 3,
    metadata(
        component = "adapter_registry",
        provider = "s3",
        operation = "get_object",
        scenario = "direct_request"
    )
)]
fn direct_get_object(ctx: &mut StressContext) {
    let runtime = Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("runtime should build");
    let base = temp_path();
    let storage: Arc<dyn Storage> = Arc::new(FilesystemStorage::new(&base));
    storage
        .create_bucket("bench".to_string())
        .expect("bucket create should succeed");
    storage
        .put_object(
            "bench",
            "item.txt".to_string(),
            sqrzl_emulator::models::Object::new(
                "item.txt".to_string(),
                vec![b'a'; 1024],
                "text/plain".to_string(),
            ),
        )
        .expect("seed put should succeed");
    let auth_config: Arc<AuthConfig> = Arc::new(auth_disabled());
    let registry = Arc::new(AdapterRegistry::default());

    ctx.parameter("payload_size_bytes", 1024);
    let response = ctx.measure(|| {
        let request = direct_request(Method::GET, "http://localhost/bench/item.txt", &[]);
        runtime
            .block_on(registry.handle(storage.clone(), auth_config.clone(), request))
            .expect("direct get should succeed")
    });
    black_box(response);
    cleanup(&base);
}

#[stress_test(
    tier = 3,
    metadata(
        component = "adapter_registry",
        provider = "s3",
        operation = "list_objects",
        scenario = "direct_request"
    )
)]
fn direct_list_objects(ctx: &mut StressContext) {
    let runtime = Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("runtime should build");
    let base = temp_path();
    let storage: Arc<dyn Storage> = Arc::new(FilesystemStorage::new(&base));
    storage
        .create_bucket("bench".to_string())
        .expect("bucket create should succeed");
    let payload = vec![b'a'; 256];
    for index in 0..64usize {
        storage
            .put_object(
                "bench",
                format!("item-{index:03}.txt"),
                sqrzl_emulator::models::Object::new(
                    format!("item-{index:03}.txt"),
                    payload.clone(),
                    "text/plain".to_string(),
                ),
            )
            .expect("seed put should succeed");
    }
    let auth_config: Arc<AuthConfig> = Arc::new(auth_disabled());
    let registry = Arc::new(AdapterRegistry::default());

    ctx.parameter("object_count", 64);
    let response = ctx.measure(|| {
        let request = direct_request(Method::GET, "http://localhost/bench?list-type=2", &[]);
        runtime
            .block_on(registry.handle(storage.clone(), auth_config.clone(), request))
            .expect("direct list should succeed")
    });
    black_box(response);
    cleanup(&base);
}

stress_main!();
