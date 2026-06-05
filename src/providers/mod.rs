use crate::auth::AuthConfig;
use crate::body::Body;
use crate::server::RequestExt as Request;
use crate::storage::Storage;
use hyper::Response;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

mod azure;
mod gcs;
mod oci;
mod s3;

pub use azure::AzureBlobAdapter;
pub use gcs::GcsAdapter;
pub use oci::OciAdapter;
pub use s3::S3Adapter;

pub trait ProviderAdapter: Send + Sync {
    fn name(&self) -> &'static str;
    fn matches(&self, req: &Request) -> bool;
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
    pub fn new(adapters: Vec<Arc<dyn ProviderAdapter>>) -> Self {
        Self { adapters }
    }

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
}
