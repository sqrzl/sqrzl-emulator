use crate::auth::AuthConfig;
use crate::body::Body;
use crate::mail::MailStore;
use crate::server::RequestExt as MailRequest;
use http::{HeaderMap, Method, Uri};
use hyper::Response;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

mod acs;
mod sendgrid;
mod ses;

pub use acs::AcsEmailAdapter;
pub use sendgrid::SendGridAdapter;
pub use ses::SesEmailAdapter;

pub trait MailAdapter: Send + Sync {
    fn name(&self) -> &'static str;
    fn matches(&self, req: &MailRequest) -> bool;
    fn matches_request_head(&self, _method: &Method, _uri: &Uri, _headers: &HeaderMap) -> bool {
        false
    }
    fn payload_too_large(&self, max_request_bytes: usize) -> Response<Body> {
        crate::server::ResponseBuilder::new(http::StatusCode::PAYLOAD_TOO_LARGE)
            .content_type("application/json; charset=utf-8")
            .body(
                serde_json::json!({
                    "error": {
                        "code": "RequestEntityTooLarge",
                        "message": format!("Request body exceeds the {max_request_bytes}-byte emulator limit")
                    }
                })
                .to_string()
                .into_bytes(),
            )
            .build()
    }
    fn incomplete_body(&self) -> Response<Body> {
        crate::server::ResponseBuilder::new(http::StatusCode::BAD_REQUEST)
            .content_type("application/json; charset=utf-8")
            .body(
                serde_json::json!({
                    "error": {
                        "code": "InvalidRequest",
                        "message": "The request body ended before it was complete"
                    }
                })
                .to_string()
                .into_bytes(),
            )
            .build()
    }
    fn handle<'a>(
        &'a self,
        mail: Arc<dyn MailStore>,
        auth_config: Arc<AuthConfig>,
        req: MailRequest,
    ) -> Pin<Box<dyn Future<Output = Result<Response<Body>, String>> + Send + 'a>>;
}

pub struct MailAdapterRegistry {
    adapters: Vec<Arc<dyn MailAdapter>>,
}

impl Default for MailAdapterRegistry {
    fn default() -> Self {
        Self::new(vec![
            Arc::new(SendGridAdapter),
            Arc::new(SesEmailAdapter),
            Arc::new(AcsEmailAdapter),
        ])
    }
}

impl MailAdapterRegistry {
    #[must_use]
    pub fn new(adapters: Vec<Arc<dyn MailAdapter>>) -> Self {
        Self { adapters }
    }

    pub async fn route(
        &self,
        mail: Arc<dyn MailStore>,
        auth_config: Arc<AuthConfig>,
        req: MailRequest,
    ) -> Option<Result<Response<Body>, String>> {
        for adapter in &self.adapters {
            if adapter.matches(&req) {
                return Some(adapter.handle(mail.clone(), auth_config.clone(), req).await);
            }
        }

        None
    }

    pub fn render_payload_too_large(
        &self,
        method: &Method,
        uri: &Uri,
        headers: &HeaderMap,
        max_request_bytes: usize,
    ) -> Option<Response<Body>> {
        for adapter in &self.adapters {
            if adapter.matches_request_head(method, uri, headers) {
                return Some(adapter.payload_too_large(max_request_bytes));
            }
        }

        None
    }

    #[must_use]
    pub fn render_incomplete_body(
        &self,
        method: &Method,
        uri: &Uri,
        headers: &HeaderMap,
    ) -> Option<Response<Body>> {
        self.adapters.iter().find_map(|adapter| {
            adapter
                .matches_request_head(method, uri, headers)
                .then(|| adapter.incomplete_body())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use http_body_util::BodyExt;

    #[test]
    fn should_include_default_adapters_in_registration_order() {
        // Arrange
        // Act
        // Assert
        let registry = MailAdapterRegistry::default();
        assert_eq!(registry.adapters.len(), 3);
        assert_eq!(registry.adapters[0].name(), "sendgrid");
        assert_eq!(registry.adapters[1].name(), "ses");
        assert_eq!(registry.adapters[2].name(), "acs");
    }

    #[test]
    fn should_not_render_an_oversized_response_for_non_mail_routes() {
        // Arrange
        // Act
        // Assert
        let registry = MailAdapterRegistry::default();
        let uri = Uri::from_static("http://localhost/devstoreaccount1/container/blob");
        let mut headers = HeaderMap::new();
        headers.insert("x-ms-version", http::HeaderValue::from_static("2023-11-03"));

        assert!(registry
            .render_payload_too_large(&Method::PUT, &uri, &headers, 12)
            .is_none());
    }

    #[tokio::test]
    async fn should_render_provider_shaped_incomplete_body_responses() {
        let registry = MailAdapterRegistry::default();
        let cases = [
            ("http://localhost/v3/mail/send", "errors", None),
            (
                "http://localhost/v2/email/outbound-emails",
                "ended before it was complete",
                Some(("x-amzn-errortype", "BadRequestException")),
            ),
            (
                "http://localhost/emails:send?api-version=2025-09-01",
                "InvalidRequest",
                Some(("x-ms-error-code", "InvalidRequest")),
            ),
        ];

        for (uri, expected, expected_header) in cases {
            let response = registry
                .render_incomplete_body(
                    &Method::POST,
                    &uri.parse::<Uri>().unwrap(),
                    &HeaderMap::new(),
                )
                .expect("mail provider head should be recognized");
            assert_eq!(response.status(), http::StatusCode::BAD_REQUEST);
            if let Some((name, value)) = expected_header {
                assert_eq!(response.headers()[name], value);
            }
            let body = response.into_body().collect().await.unwrap().to_bytes();
            assert!(String::from_utf8_lossy(&body).contains(expected));
        }
    }
}
