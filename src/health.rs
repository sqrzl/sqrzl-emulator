use crate::body::Body;
use crate::server::ResponseBuilder;
use crate::storage::BucketStore;
use http::StatusCode;
use serde::Serialize;

#[derive(Debug, Serialize)]
struct HealthResponse {
    status: &'static str,
    version: &'static str,
    api_port: u16,
    ui_port: u16,
    auth_enforced: bool,
    admin_auth_enforced: bool,
    max_request_bytes: usize,
    storage_ready: bool,
    enabled_providers: [&'static str; 4],
}

pub fn response(
    storage: &(impl BucketStore + ?Sized),
    config: &crate::Config,
) -> hyper::Response<Body> {
    let storage_ready = storage.list_buckets().is_ok();
    let body = HealthResponse {
        status: if storage_ready { "ok" } else { "degraded" },
        version: env!("CARGO_PKG_VERSION"),
        api_port: config.api_port,
        ui_port: config.ui_port,
        auth_enforced: config.enforce_auth,
        admin_auth_enforced: config.admin_auth_enforced(),
        max_request_bytes: config.max_request_bytes,
        storage_ready,
        enabled_providers: ["s3-family", "azure-blob", "gcs", "oci-object"],
    };

    match serde_json::to_vec(&body) {
        Ok(json) => ResponseBuilder::new(if storage_ready {
            StatusCode::OK
        } else {
            StatusCode::SERVICE_UNAVAILABLE
        })
        .content_type("application/json")
        .body(json)
        .build(),
        Err(err) => ResponseBuilder::new(StatusCode::INTERNAL_SERVER_ERROR)
            .content_type("text/plain; charset=utf-8")
            .body_str(&format!("failed to render health response: {err}"))
            .build(),
    }
}
