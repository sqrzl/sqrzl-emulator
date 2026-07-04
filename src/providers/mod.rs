use crate::auth::AuthConfig;
use crate::body::Body;
use crate::server::RequestExt as Request;
use crate::storage::Storage;
use http::{HeaderMap, Method, Uri};
use hyper::Response;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

mod azure;
mod gcs;
mod oci;
mod s3;
mod state;

pub use azure::AzureBlobAdapter;
pub use gcs::GcsAdapter;
pub use oci::OciAdapter;
pub use s3::S3Adapter;

pub trait ProviderAdapter: Send + Sync {
    fn name(&self) -> &'static str;
    fn matches(&self, req: &Request) -> bool;
    fn matches_request_head(&self, _method: &Method, _uri: &Uri, _headers: &HeaderMap) -> bool {
        false
    }
    fn render_payload_too_large(
        &self,
        _method: &Method,
        _uri: &Uri,
        _headers: &HeaderMap,
        max_request_bytes: usize,
    ) -> Response<Body> {
        s3::payload_too_large_response(max_request_bytes)
    }
    fn handle<'a>(
        &'a self,
        storage: Arc<dyn Storage>,
        auth_config: Arc<AuthConfig>,
        req: Request,
    ) -> Pin<Box<dyn Future<Output = Result<Response<Body>, String>> + Send + 'a>>;
}

pub struct AdapterRegistry {
    adapters: Vec<Arc<dyn ProviderAdapter>>,
}

impl Default for AdapterRegistry {
    fn default() -> Self {
        Self::new(vec![
            Arc::new(AzureBlobAdapter::default()),
            Arc::new(GcsAdapter::default()),
            Arc::new(OciAdapter),
            Arc::new(S3Adapter),
        ])
    }
}

impl AdapterRegistry {
    #[must_use]
    pub fn new(adapters: Vec<Arc<dyn ProviderAdapter>>) -> Self {
        Self { adapters }
    }

    ///
    /// # Errors
    ///
    /// Returns an error when the underlying emulator operation fails.
    pub async fn handle(
        &self,
        storage: Arc<dyn Storage>,
        auth_config: Arc<AuthConfig>,
        req: Request,
    ) -> Result<Response<Body>, String> {
        for adapter in &self.adapters {
            if adapter.matches(&req) {
                return adapter.handle(storage, auth_config, req).await;
            }
        }

        Err("No provider adapter matched the request".to_string())
    }

    pub fn render_payload_too_large(
        &self,
        method: &Method,
        uri: &Uri,
        headers: &HeaderMap,
        max_request_bytes: usize,
    ) -> Response<Body> {
        for adapter in &self.adapters {
            if adapter.matches_request_head(method, uri, headers) {
                return adapter.render_payload_too_large(method, uri, headers, max_request_bytes);
            }
        }

        s3::payload_too_large_response(max_request_bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::HeaderValue;
    use http_body_util::BodyExt;

    async fn response_body(response: Response<Body>) -> String {
        let bytes = response
            .into_body()
            .collect()
            .await
            .expect("body should read")
            .to_bytes();
        String::from_utf8(bytes.to_vec()).expect("body should be utf8")
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn should_render_azure_payload_too_large_from_request_head() {
        let registry = AdapterRegistry::default();
        let mut headers = HeaderMap::new();
        headers.insert("x-ms-version", HeaderValue::from_static("2023-11-03"));
        let uri = Uri::from_static("http://localhost/devstoreaccount1/container/blob");

        let response = registry.render_payload_too_large(&Method::PUT, &uri, &headers, 12);

        assert_eq!(response.status(), http::StatusCode::PAYLOAD_TOO_LARGE);
        assert_eq!(
            response
                .headers()
                .get("x-ms-error-code")
                .and_then(|value| value.to_str().ok()),
            Some("RequestBodyTooLarge")
        );
        assert!(response_body(response)
            .await
            .contains("RequestBodyTooLarge"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn should_render_gcs_payload_too_large_from_request_head() {
        let registry = AdapterRegistry::default();
        let mut headers = HeaderMap::new();
        headers.insert("host", HeaderValue::from_static("storage.googleapis.com"));
        let uri = Uri::from_static("http://storage.googleapis.com/bucket/object");

        let response = registry.render_payload_too_large(&Method::PUT, &uri, &headers, 12);

        assert_eq!(response.status(), http::StatusCode::PAYLOAD_TOO_LARGE);
        assert_eq!(
            response
                .headers()
                .get("content-type")
                .and_then(|value| value.to_str().ok()),
            Some("application/xml")
        );
        assert!(response_body(response).await.contains("EntityTooLarge"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn should_render_oci_payload_too_large_from_request_head() {
        let registry = AdapterRegistry::default();
        let headers = HeaderMap::new();
        let uri = Uri::from_static("http://localhost/n/namespace/b/bucket/o/object");

        let response = registry.render_payload_too_large(&Method::PUT, &uri, &headers, 12);

        assert_eq!(response.status(), http::StatusCode::PAYLOAD_TOO_LARGE);
        assert_eq!(
            response
                .headers()
                .get("content-type")
                .and_then(|value| value.to_str().ok()),
            Some("application/json")
        );
        assert!(response_body(response).await.contains("PayloadTooLarge"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn should_render_s3_payload_too_large_as_fallback() {
        let registry = AdapterRegistry::default();
        let headers = HeaderMap::new();
        let uri = Uri::from_static("http://localhost/bucket/key");

        let response = registry.render_payload_too_large(&Method::PUT, &uri, &headers, 12);

        assert_eq!(response.status(), http::StatusCode::PAYLOAD_TOO_LARGE);
        assert!(response.headers().contains_key("x-amz-request-id"));
        assert!(response_body(response).await.contains("EntityTooLarge"));
    }
}
