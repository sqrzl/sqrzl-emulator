use super::ProviderAdapter;
use crate::auth::AuthConfig;
use crate::body::Body;
use crate::server::{handle_s3_request, RequestExt as Request};
use crate::storage::Storage;
use hyper::Response;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

pub struct S3Adapter;

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

    fn handle<'a>(
        &'a self,
        storage: Arc<dyn Storage>,
        auth_config: Arc<AuthConfig>,
        req: Request,
    ) -> Pin<Box<dyn Future<Output = Result<Response<Body>, String>> + Send + 'a>> {
        Box::pin(async move { handle_s3_request(storage, auth_config, req).await })
    }
}
