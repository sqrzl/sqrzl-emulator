use crate::auth::AuthConfig;
use crate::body::Body;
use crate::server::RequestExt as Request;
use crate::storage::Storage;
use http::{HeaderMap, Method, Uri};
use hyper::Response;
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, OnceLock};

mod azure;
mod faults;
mod gcs;
mod oci;
mod s3;
mod state;

pub use azure::AzureBlobAdapter;
pub use gcs::GcsAdapter;
pub use oci::OciAdapter;
pub(crate) use s3::payload_too_large_response;
pub use s3::S3Adapter;

static DATA_PROTECTION_ACTIVATION_LOCKS: OnceLock<Mutex<HashMap<String, Arc<Mutex<()>>>>> =
    OnceLock::new();

/// Returns the process-wide activation lock for one provider-shared bucket namespace.
///
/// Provider adapters must hold this lock from their final foreign-owner check through
/// every persistent marker and metadata write that activates a data-protection mode.
/// This prevents two front doors from both successfully claiming the same namespace.
pub(crate) fn data_protection_activation_lock(bucket: &str) -> Result<Arc<Mutex<()>>, String> {
    let locks = DATA_PROTECTION_ACTIVATION_LOCKS.get_or_init(|| Mutex::new(HashMap::new()));
    let mut locks = locks
        .lock()
        .map_err(|_| "Failed to lock data-protection activation registry".to_string())?;
    Ok(locks
        .entry(bucket.to_string())
        .or_insert_with(|| Arc::new(Mutex::new(())))
        .clone())
}

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
    fn render_incomplete_body(
        &self,
        _method: &Method,
        _uri: &Uri,
        _headers: &HeaderMap,
    ) -> Response<Body> {
        s3::incomplete_body_response()
    }
    fn validate_request_framing(&self, _req: &Request) -> Option<Response<Body>> {
        None
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
                if let Some(response) = adapter.validate_request_framing(&req) {
                    return Ok(response);
                }
                if let faults::Before::Respond(response) =
                    faults::before(&req, adapter.name()).await
                {
                    return Ok(response);
                }
                let response = adapter.handle(storage, auth_config, req.clone()).await?;
                return faults::after(&req, response).await;
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

    pub fn render_incomplete_body(
        &self,
        method: &Method,
        uri: &Uri,
        headers: &HeaderMap,
    ) -> Response<Body> {
        for adapter in &self.adapters {
            if adapter.matches_request_head(method, uri, headers) {
                return adapter.render_incomplete_body(method, uri, headers);
            }
        }

        s3::incomplete_body_response()
    }
}

pub(crate) fn content_length_mismatch(req: &Request) -> bool {
    req.header("content-length")
        .and_then(|value| value.parse::<usize>().ok())
        .is_some_and(|declared| declared != req.body.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::FilesystemStorage;
    use bytes::Bytes;
    use http::HeaderValue;
    use http_body_util::BodyExt;
    use hyper::Request as HyperRequest;

    #[derive(Clone, Copy, Debug)]
    enum ProtectionClaim {
        S3,
        Gcs,
        Azure,
    }

    async fn response_body(response: Response<Body>) -> String {
        let bytes = response
            .into_body()
            .collect()
            .await
            .expect("body should read")
            .to_bytes();
        String::from_utf8(bytes.to_vec()).expect("body should be utf8")
    }

    fn temp_storage() -> Arc<dyn Storage> {
        let directory =
            std::env::temp_dir().join(format!("sqrzl-protection-race-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&directory).expect("temporary storage should be created");
        Arc::new(FilesystemStorage::new(directory))
    }

    fn auth_disabled() -> Arc<AuthConfig> {
        Arc::new(AuthConfig {
            access_key_id: None,
            secret_access_key: None,
            enforce_auth: false,
            admin_auth_disabled: false,
            blobs_path: "./blobs".to_string(),
            lifecycle_interval: std::time::Duration::from_hours(1),
            api_port: 9000,
            ui_port: 9001,
            max_request_bytes: crate::config::DEFAULT_SQRZL_MAX_REQUEST_BYTES,
            smtp_port: crate::config::DEFAULT_SQRZL_SMTP_PORT,
        })
    }

    async fn protection_claim_request(claim: ProtectionClaim, bucket: &str) -> Request {
        let request = match claim {
            ProtectionClaim::S3 => HyperRequest::builder()
                .method(Method::PUT)
                .uri(format!("http://localhost/{bucket}"))
                .header("content-length", "0")
                .header("x-amz-bucket-object-lock-enabled", "true")
                .body(Body::default())
                .expect("S3 request should build"),
            ProtectionClaim::Gcs => {
                let payload = serde_json::json!({
                    "name": bucket,
                    "softDeletePolicy": {"retentionDurationSeconds": "604800"}
                })
                .to_string();
                HyperRequest::builder()
                    .method(Method::POST)
                    .uri("http://localhost/storage/v1/b?project=test-project")
                    .header("host", "storage.googleapis.com")
                    .header("content-type", "application/json")
                    .header("content-length", payload.len().to_string())
                    .body(Body::from(payload))
                    .expect("GCS request should build")
            }
            ProtectionClaim::Azure => HyperRequest::builder()
                .method(Method::PUT)
                .uri(format!(
                    "http://localhost/devstoreaccount1/{bucket}?restype=container"
                ))
                .header("content-length", "0")
                .header("x-ms-version", "2023-11-03")
                .header("x-sqrzl-azure-versioning-enabled", "true")
                .body(Body::default())
                .expect("Azure request should build"),
        };
        Request::from_hyper(request)
            .await
            .expect("provider request should parse")
    }

    fn claim_succeeded(claim: ProtectionClaim, status: http::StatusCode) -> bool {
        match claim {
            ProtectionClaim::S3 | ProtectionClaim::Gcs => status == http::StatusCode::OK,
            ProtectionClaim::Azure => status == http::StatusCode::CREATED,
        }
    }

    fn claim_owns_bucket(claim: ProtectionClaim, metadata: &HashMap<String, String>) -> bool {
        let (key, expected) = match claim {
            ProtectionClaim::S3 => ("s3_object_lock_enabled", "true"),
            ProtectionClaim::Gcs => ("gcs_soft_delete_seconds", "604800"),
            ProtectionClaim::Azure => ("azure_versioning_enabled", "true"),
        };
        metadata.get(key).is_some_and(|value| value == expected)
    }

    async fn assert_native_conflict(claim: ProtectionClaim, response: Response<Body>) {
        assert_eq!(response.status(), http::StatusCode::CONFLICT, "{claim:?}");
        let body = response_body(response).await;
        let marker = match claim {
            ProtectionClaim::S3 => "BucketAlreadyOwnedByYou",
            ProtectionClaim::Gcs => "\"reason\":\"conflict\"",
            ProtectionClaim::Azure => "FeatureVersionMismatch",
        };
        assert!(
            body.contains(marker),
            "{claim:?} must return its native conflict envelope: {body}"
        );
    }

    async fn assert_serialized_protection_claims(left: ProtectionClaim, right: ProtectionClaim) {
        for iteration in 0..8 {
            let storage = temp_storage();
            let registry = Arc::new(AdapterRegistry::default());
            let bucket = format!(
                "protection-race-{iteration}-{}",
                &uuid::Uuid::new_v4().simple().to_string()[..8]
            );
            let left_request = protection_claim_request(left, &bucket).await;
            let right_request = protection_claim_request(right, &bucket).await;
            let start = Arc::new(tokio::sync::Barrier::new(3));

            let left_task = {
                let storage = storage.clone();
                let registry = registry.clone();
                let start = start.clone();
                tokio::spawn(async move {
                    start.wait().await;
                    registry
                        .handle(storage, auth_disabled(), left_request)
                        .await
                        .expect("left activation should respond")
                })
            };
            let right_task = {
                let storage = storage.clone();
                let registry = registry.clone();
                let start = start.clone();
                tokio::spawn(async move {
                    start.wait().await;
                    registry
                        .handle(storage, auth_disabled(), right_request)
                        .await
                        .expect("right activation should respond")
                })
            };

            start.wait().await;
            let left_response = left_task.await.expect("left task should complete");
            let right_response = right_task.await.expect("right task should complete");
            let left_won = claim_succeeded(left, left_response.status());
            let right_won = claim_succeeded(right, right_response.status());
            assert_ne!(left_won, right_won, "exactly one provider must activate");
            if left_won {
                assert_native_conflict(right, right_response).await;
            } else {
                assert_native_conflict(left, left_response).await;
            }

            let metadata = storage.get_bucket(&bucket).unwrap().metadata;
            let owner_count = [
                ProtectionClaim::S3,
                ProtectionClaim::Gcs,
                ProtectionClaim::Azure,
            ]
            .into_iter()
            .filter(|claim| claim_owns_bucket(*claim, &metadata))
            .count();
            assert_eq!(owner_count, 1, "bucket metadata must have one owner");
            assert_eq!(claim_owns_bucket(left, &metadata), left_won);
            assert_eq!(claim_owns_bucket(right, &metadata), right_won);
        }
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

    #[tokio::test(flavor = "multi_thread")]
    async fn should_route_local_gcs_resumable_session_uris_back_to_gcs() {
        // Arrange
        let directory = std::env::temp_dir().join(format!(
            "sqrzl-gcs-registry-resumable-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&directory).expect("temporary storage should be created");
        let storage: Arc<dyn Storage> = Arc::new(FilesystemStorage::new(directory));
        storage
            .create_bucket("resumable-local".to_string())
            .expect("test bucket should be created");
        let registry = AdapterRegistry::default();
        let auth = Arc::new(AuthConfig {
            access_key_id: None,
            secret_access_key: None,
            enforce_auth: false,
            admin_auth_disabled: false,
            blobs_path: "./blobs".to_string(),
            lifecycle_interval: std::time::Duration::from_hours(1),
            api_port: 9000,
            ui_port: 9001,
            max_request_bytes: crate::config::DEFAULT_SQRZL_MAX_REQUEST_BYTES,
            smtp_port: crate::config::DEFAULT_SQRZL_SMTP_PORT,
        });
        let initiate = Request::from_hyper(
            HyperRequest::builder()
                .method(Method::POST)
                .uri("http://localhost/upload/storage/v1/b/resumable-local/o?uploadType=resumable&name=object.txt")
                .header("content-length", "0")
                .body(Body::default())
                .expect("initiation request should build"),
        )
        .await
        .expect("initiation request should parse");
        let initiated = registry
            .handle(storage.clone(), auth.clone(), initiate)
            .await
            .expect("initiation should route to GCS");
        let location = initiated
            .headers()
            .get("location")
            .and_then(|value| value.to_str().ok())
            .expect("session location should exist")
            .to_string();
        let complete = Request::from_hyper(
            HyperRequest::builder()
                .method(Method::PUT)
                .uri(location)
                .header("content-length", "7")
                .body(Body::from(Bytes::from_static(b"payload")))
                .expect("completion request should build"),
        )
        .await
        .expect("completion request should parse");

        // Act
        let completed = registry
            .handle(storage.clone(), auth, complete)
            .await
            .expect("local session URI should route to GCS");

        // Assert
        assert_eq!(completed.status(), http::StatusCode::OK);
        assert_eq!(
            storage
                .get_object("resumable-local", "object.txt")
                .expect("completed object should exist")
                .data,
            b"payload"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn should_serialize_conflicting_azure_and_s3_protection_creation() {
        assert_serialized_protection_claims(ProtectionClaim::Azure, ProtectionClaim::S3).await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn should_serialize_conflicting_azure_and_gcs_protection_creation() {
        assert_serialized_protection_claims(ProtectionClaim::Azure, ProtectionClaim::Gcs).await;
    }
}
