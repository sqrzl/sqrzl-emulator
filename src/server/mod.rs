use crate::auth::AuthConfig;
use crate::body::Body;
use crate::hyper_compat::Compat;
use crate::providers::AdapterRegistry;
use crate::storage::Storage;
use hyper::service::{service_fn, Service};
use hyper::{Request, Response, StatusCode};
use std::convert::Infallible;
use std::sync::Arc;
use tracing::error;

mod handlers;
mod http;

pub(crate) use handlers::handle_request as handle_s3_request;
pub use http::{Request as RequestExt, ResponseBuilder, RouteMatch, Router};

pub async fn serve_h1_connection<S>(
    stream: tokio::net::TcpStream,
    service: S,
) -> Result<(), hyper::Error>
where
    S: Service<
            hyper::Request<hyper::body::Incoming>,
            Response = Response<Body>,
            Error = Infallible,
        > + Send
        + 'static,
    S::Future: Send + 'static,
{
    hyper::server::conn::http1::Builder::new()
        .serve_connection(Compat::new(stream), service)
        .await
}

fn simple_text_response(status: StatusCode, body: &str) -> Response<Body> {
    ResponseBuilder::new(status)
        .content_type("text/plain; charset=utf-8")
        .body_str(body)
        .build()
}

pub struct Server {
    storage: Arc<dyn Storage>,
    auth_config: Arc<AuthConfig>,
    adapters: Arc<AdapterRegistry>,
    api_port: u16,
}

impl Server {
    pub fn new(storage: Arc<dyn Storage>, auth_config: Arc<AuthConfig>, api_port: u16) -> Self {
        Self {
            storage,
            auth_config,
            adapters: Arc::new(AdapterRegistry::default()),
            api_port,
        }
    }

    pub async fn start(self) -> crate::error::Result<()> {
        let storage = self.storage.clone();
        let auth_config = self.auth_config.clone();
        let adapters = self.adapters.clone();
        let api_port = self.api_port;

        let addr = std::net::SocketAddr::from(([0, 0, 0, 0], api_port));

        let listener = tokio::net::TcpListener::bind(addr)
            .await
            .map_err(|e| crate::error::Error::InternalError(e.to_string()))?;
        tracing::info!("S3 API listening on http://0.0.0.0:{}", api_port);

        loop {
            let (stream, _) = listener
                .accept()
                .await
                .map_err(|e| crate::error::Error::InternalError(e.to_string()))?;
            let storage = storage.clone();
            let auth_config = auth_config.clone();
            let adapters = adapters.clone();

            tokio::spawn(async move {
                let service = service_fn(move |req| {
                    let storage = storage.clone();
                    let auth_config = auth_config.clone();
                    let adapters = adapters.clone();
                    handle_request(storage, auth_config, adapters, req)
                });

                if let Err(e) = serve_h1_connection(stream, service).await {
                    error!("HTTP connection error: {}", e);
                }
            });
        }
    }
}

async fn handle_request<B>(
    storage: Arc<dyn Storage>,
    auth_config: Arc<AuthConfig>,
    adapters: Arc<AdapterRegistry>,
    req: Request<B>,
) -> Result<Response<Body>, Infallible>
where
    B: hyper::body::Body<Data = bytes::Bytes> + Send + 'static,
    B::Error: std::fmt::Display,
{
    match http::Request::from_hyper(req).await {
        Ok(parsed_req) => match adapters.handle(storage, auth_config, parsed_req).await {
            Ok(response) => Ok(response),
            Err(e) => {
                error!("Handler error: {}", e);
                Ok(simple_text_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Internal Server Error",
                ))
            }
        },
        Err(e) => {
            error!("Failed to parse request: {}", e);
            Ok(simple_text_response(StatusCode::BAD_REQUEST, "Bad Request"))
        }
    }
}

#[cfg(test)]
mod adapter_routing_tests {
    use super::*;
    use crate::config::Config;
    use crate::storage::FilesystemStorage;
    use http_body_util::BodyExt;
    use hyper::Request as HyperRequest;
    use std::fs;

    fn temp_storage() -> Arc<dyn Storage> {
        let dir = std::env::temp_dir().join(format!("peas-routing-test-{}", uuid::Uuid::new_v4()));
        let _ = fs::create_dir_all(&dir);
        Arc::new(FilesystemStorage::new(dir))
    }

    fn auth_disabled() -> Arc<AuthConfig> {
        Arc::new(Config {
            access_key_id: None,
            secret_access_key: None,
            enforce_auth: false,
            admin_auth_disabled: false,
            blobs_path: "./blobs".to_string(),
            lifecycle_interval: std::time::Duration::from_secs(3600),
            api_port: 9000,
            ui_port: 9001,
        })
    }

    async fn call(storage: Arc<dyn Storage>, req: HyperRequest<Body>) -> Response<Body> {
        handle_request(
            storage,
            auth_disabled(),
            Arc::new(AdapterRegistry::default()),
            req,
        )
        .await
        .expect("request should complete")
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn should_route_azure_requests_through_azure_adapter() {
        let storage = temp_storage();

        let create = HyperRequest::builder()
            .method("PUT")
            .uri("http://localhost/devstoreaccount1/photos?restype=container")
            .header("x-ms-version", "2023-11-03")
            .body(Body::default())
            .expect("request should build");
        let resp = call(storage.clone(), create).await;
        assert_eq!(resp.status(), StatusCode::CREATED);

        let list = HyperRequest::builder()
            .method("GET")
            .uri("http://localhost/devstoreaccount1?comp=list")
            .header("x-ms-version", "2023-11-03")
            .body(Body::default())
            .expect("request should build");
        let resp = call(storage, list).await;
        let body = resp
            .into_body()
            .collect()
            .await
            .expect("body should read")
            .to_bytes();
        assert!(String::from_utf8(body.to_vec())
            .expect("utf8")
            .contains("photos"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn should_route_gcs_requests_through_gcs_adapter() {
        let storage = temp_storage();

        let create = HyperRequest::builder()
            .method("PUT")
            .uri("http://localhost/media")
            .header("host", "storage.googleapis.com")
            .body(Body::default())
            .expect("request should build");
        let resp = call(storage.clone(), create).await;
        assert_eq!(resp.status(), StatusCode::OK);

        let get = HyperRequest::builder()
            .method("GET")
            .uri("http://localhost/")
            .header("host", "storage.googleapis.com")
            .body(Body::default())
            .expect("request should build");
        let resp = call(storage, get).await;
        let body = resp
            .into_body()
            .collect()
            .await
            .expect("body should read")
            .to_bytes();
        assert!(String::from_utf8(body.to_vec())
            .expect("utf8")
            .contains("media"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn should_route_oci_requests_through_oci_adapter() {
        let storage = temp_storage();

        let req = HyperRequest::builder()
            .method("GET")
            .uri("http://localhost/n/testnamespace")
            .body(Body::default())
            .expect("request should build");
        let resp = call(storage, req).await;
        let body = resp
            .into_body()
            .collect()
            .await
            .expect("body should read")
            .to_bytes();
        assert!(String::from_utf8(body.to_vec())
            .expect("utf8")
            .contains("testnamespace"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn should_fall_back_to_s3_adapter_for_plain_requests() {
        let storage = temp_storage();

        let create = HyperRequest::builder()
            .method("PUT")
            .uri("http://localhost/plain-bucket")
            .body(Body::default())
            .expect("request should build");
        let resp = call(storage.clone(), create).await;
        assert_eq!(resp.status(), StatusCode::OK);

        let list = HyperRequest::builder()
            .method("GET")
            .uri("http://localhost/")
            .body(Body::default())
            .expect("request should build");
        let resp = call(storage, list).await;
        let body = resp
            .into_body()
            .collect()
            .await
            .expect("body should read")
            .to_bytes();
        assert!(String::from_utf8(body.to_vec())
            .expect("utf8")
            .contains("plain-bucket"));
    }
}
