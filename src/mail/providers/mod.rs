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
                return Some(crate::providers::payload_too_large_response(
                    max_request_bytes,
                ));
            }
        }

        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_include_default_adapters_in_registration_order() {
        let registry = MailAdapterRegistry::default();
        assert_eq!(registry.adapters.len(), 3);
        assert_eq!(registry.adapters[0].name(), "sendgrid");
        assert_eq!(registry.adapters[1].name(), "ses");
        assert_eq!(registry.adapters[2].name(), "acs");
    }

    #[test]
    fn should_not_render_an_oversized_response_for_non_mail_routes() {
        let registry = MailAdapterRegistry::default();
        let uri = Uri::from_static("http://localhost/devstoreaccount1/container/blob");
        let mut headers = HeaderMap::new();
        headers.insert("x-ms-version", http::HeaderValue::from_static("2023-11-03"));

        assert!(registry
            .render_payload_too_large(&Method::PUT, &uri, &headers, 12)
            .is_none());
    }
}
