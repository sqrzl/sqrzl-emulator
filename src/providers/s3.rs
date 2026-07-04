use super::ProviderAdapter;
use crate::auth::AuthConfig;
use crate::body::Body;
use crate::server::{handle_s3_request, RequestExt as Request, ResponseBuilder};
use crate::storage::Storage;
use http::{HeaderMap, Method, StatusCode, Uri};
use hyper::Response;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

pub struct S3Adapter;

pub(super) fn payload_too_large_response(max_request_bytes: usize) -> Response<Body> {
    let message =
        format!("Request body exceeds SQRZL_MAX_REQUEST_BYTES ({max_request_bytes} bytes)");
    let req_id = crate::utils::headers::generate_request_id();
    let body = crate::utils::xml::error_xml("EntityTooLarge", &message, &req_id);
    ResponseBuilder::new(StatusCode::PAYLOAD_TOO_LARGE)
        .content_type("application/xml; charset=utf-8")
        .header("x-amz-request-id", &req_id)
        .body(body.into_bytes())
        .build()
}

impl Default for S3Adapter {
    fn default() -> Self {
        Self::new()
    }
}

impl S3Adapter {
    pub fn new() -> Self {
        Self
    }
}

impl ProviderAdapter for S3Adapter {
    fn name(&self) -> &'static str {
        "s3"
    }

    fn matches(&self, _req: &Request) -> bool {
        true
    }

    fn matches_request_head(&self, _method: &Method, _uri: &Uri, _headers: &HeaderMap) -> bool {
        true
    }

    fn render_payload_too_large(
        &self,
        _method: &Method,
        _uri: &Uri,
        _headers: &HeaderMap,
        max_request_bytes: usize,
    ) -> Response<Body> {
        payload_too_large_response(max_request_bytes)
    }

    fn handle<'a>(
        &'a self,
        storage: Arc<dyn Storage>,
        auth_config: Arc<AuthConfig>,
        req: Request,
    ) -> Pin<Box<dyn Future<Output = Result<Response<Body>, String>> + Send + 'a>> {
        Box::pin(async move { handle_s3_request(storage, auth_config, req).await })
    }
}
