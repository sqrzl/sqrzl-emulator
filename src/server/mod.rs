use crate::auth::AuthConfig;
use crate::body::Body;
use crate::hyper_compat::Compat;
use crate::mail::providers::MailAdapterRegistry;
use crate::mail::MailStore;
use crate::providers::AdapterRegistry;
use crate::sms::providers::SmsAdapterRegistry;
use crate::sms::{FilesystemSmsStore, SmsStore};
use crate::storage::Storage;
use ::http::Method;
use hyper::service::{service_fn, Service};
use hyper::{Request, Response, StatusCode};
use std::sync::Arc;
use tracing::error;

mod handlers;
mod http;

pub(crate) use handlers::handle_request as handle_s3_request;
pub use http::{Request as RequestExt, RequestParseError, ResponseBuilder, RouteMatch, Router};

///
/// # Errors
///
/// Returns an error when the underlying emulator operation fails.
pub async fn serve_h1_connection<S>(
    stream: tokio::net::TcpStream,
    service: S,
) -> Result<(), hyper::Error>
where
    S: Service<hyper::Request<hyper::body::Incoming>, Response = Response<Body>> + Send + 'static,
    S::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
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
    mail: Arc<dyn MailStore>,
    mail_adapters: Arc<MailAdapterRegistry>,
    sms: Arc<dyn SmsStore>,
    sms_adapters: Arc<SmsAdapterRegistry>,
    adapters: Arc<AdapterRegistry>,
    api_port: u16,
}

#[derive(Clone)]
struct RequestState {
    storage: Arc<dyn Storage>,
    auth_config: Arc<AuthConfig>,
    mail: Arc<dyn MailStore>,
    mail_adapters: Arc<MailAdapterRegistry>,
    sms: Arc<dyn SmsStore>,
    sms_adapters: Arc<SmsAdapterRegistry>,
    adapters: Arc<AdapterRegistry>,
}

impl Server {
    /// Constructs an API server and opens its SMS store beneath the configured blob path.
    ///
    /// # Panics
    ///
    /// Panics when the SMS persistence tree cannot be opened. Runtime startup uses
    /// [`Self::new_with_sms`] after opening the store fallibly.
    pub fn new(
        storage: Arc<dyn Storage>,
        mail: Arc<dyn MailStore>,
        auth_config: Arc<AuthConfig>,
        api_port: u16,
    ) -> Self {
        let sms = Arc::new(
            FilesystemSmsStore::open(&auth_config.blobs_path)
                .expect("SMS filesystem store should open"),
        );
        Self::new_with_sms(storage, mail, sms, auth_config, api_port)
    }

    #[must_use]
    pub fn new_with_sms(
        storage: Arc<dyn Storage>,
        mail: Arc<dyn MailStore>,
        sms: Arc<dyn SmsStore>,
        auth_config: Arc<AuthConfig>,
        api_port: u16,
    ) -> Self {
        Self {
            storage,
            mail,
            mail_adapters: Arc::new(MailAdapterRegistry::default()),
            sms,
            sms_adapters: Arc::new(SmsAdapterRegistry::default()),
            auth_config,
            adapters: Arc::new(AdapterRegistry::default()),
            api_port,
        }
    }

    ///
    /// # Errors
    ///
    /// Returns an error when the underlying emulator operation fails.
    pub async fn start(self) -> crate::error::Result<()> {
        let state = RequestState {
            storage: self.storage,
            auth_config: self.auth_config,
            mail: self.mail,
            mail_adapters: self.mail_adapters,
            sms: self.sms,
            sms_adapters: self.sms_adapters,
            adapters: self.adapters,
        };
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
            let state = state.clone();

            tokio::spawn(async move {
                let service = service_fn(move |req| handle_request(state.clone(), req));

                if let Err(e) = serve_h1_connection(stream, service).await {
                    error!("HTTP connection error: {}", e);
                }
            });
        }
    }
}

fn handler_error(kind: &str, error: &str) -> Response<Body> {
    error!("{kind} handler error: {error}");
    simple_text_response(StatusCode::INTERNAL_SERVER_ERROR, "Internal Server Error")
}

async fn handle_request<B>(
    state: RequestState,
    req: Request<B>,
) -> Result<Response<Body>, std::io::Error>
where
    B: hyper::body::Body<Data = bytes::Bytes> + Send + Unpin + 'static,
    B::Error: std::fmt::Display,
{
    let response = match http::Request::from_hyper_with_max_body(
        req,
        Some(state.auth_config.max_request_bytes),
    )
    .await
    {
        Ok(parsed_req) => route_parsed_request(state, parsed_req).await,
        Err(error) => request_parse_error_response(&state, error),
    };
    if response.body().aborts_connection() {
        Err(std::io::Error::new(
            std::io::ErrorKind::ConnectionAborted,
            "Sqrzl response-loss failpoint",
        ))
    } else {
        Ok(response)
    }
}

async fn route_parsed_request(state: RequestState, parsed_req: RequestExt) -> Response<Body> {
    if parsed_req.path() == "/healthz" {
        return if parsed_req.method() == Method::GET {
            crate::health::response(state.storage.as_ref(), state.auth_config.as_ref())
        } else {
            simple_text_response(StatusCode::METHOD_NOT_ALLOWED, "Method Not Allowed")
        };
    }

    if let Some(response) = state
        .sms_adapters
        .route(
            state.sms.clone(),
            state.auth_config.clone(),
            parsed_req.clone(),
        )
        .await
    {
        return response.unwrap_or_else(|error| handler_error("SMS", &error));
    }

    if let Some(response) = state
        .mail_adapters
        .route(
            state.mail.clone(),
            state.auth_config.clone(),
            parsed_req.clone(),
        )
        .await
    {
        return response.unwrap_or_else(|error| handler_error("Mail", &error));
    }

    state
        .adapters
        .handle(state.storage, state.auth_config, parsed_req)
        .await
        .unwrap_or_else(|error| handler_error("Storage", &error))
}

fn request_parse_error_response(state: &RequestState, error: RequestParseError) -> Response<Body> {
    match error {
        RequestParseError::BodyRead {
            message,
            method,
            uri,
            headers,
        } => {
            error!("Failed to read request body: {message}");
            if let Some(response) = state
                .sms_adapters
                .render_incomplete_body(&method, &uri, &headers)
            {
                return response;
            }
            if let Some(response) = state
                .mail_adapters
                .render_incomplete_body(&method, &uri, &headers)
            {
                return response;
            }
            state
                .adapters
                .render_incomplete_body(&method, &uri, &headers)
        }
        RequestParseError::BodyTooLarge {
            max_request_bytes,
            method,
            uri,
            headers,
        } => {
            if let Some(sms_response) = state.sms_adapters.render_payload_too_large(
                &method,
                &uri,
                &headers,
                max_request_bytes,
            ) {
                return sms_response;
            }
            if let Some(mail_response) = state.mail_adapters.render_payload_too_large(
                &method,
                &uri,
                &headers,
                max_request_bytes,
            ) {
                return mail_response;
            }

            state
                .adapters
                .render_payload_too_large(&method, &uri, &headers, max_request_bytes)
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
        let dir = std::env::temp_dir().join(format!("sqrzl-routing-test-{}", uuid::Uuid::new_v4()));
        let _ = fs::create_dir_all(&dir);
        Arc::new(FilesystemStorage::new(dir))
    }

    fn auth_disabled() -> Arc<AuthConfig> {
        auth_with_max(crate::config::DEFAULT_SQRZL_MAX_REQUEST_BYTES)
    }

    fn auth_with_max(max_request_bytes: usize) -> Arc<AuthConfig> {
        Arc::new(Config {
            access_key_id: None,
            secret_access_key: None,
            enforce_auth: false,
            admin_auth_disabled: false,
            blobs_path: "./blobs".to_string(),
            lifecycle_interval: std::time::Duration::from_hours(1),
            api_port: 9000,
            ui_port: 9001,
            max_request_bytes,
            smtp_port: crate::config::DEFAULT_SQRZL_SMTP_PORT,
        })
    }

    async fn call(storage: Arc<dyn Storage>, req: HyperRequest<Body>) -> Response<Body> {
        call_with_auth(storage, auth_disabled(), req).await
    }

    async fn call_with_auth(
        storage: Arc<dyn Storage>,
        auth_config: Arc<AuthConfig>,
        req: HyperRequest<Body>,
    ) -> Response<Body> {
        let mail = Arc::new(
            crate::mail::FilesystemMailStore::open(std::env::temp_dir())
                .expect("mail store should open"),
        );
        handle_request(
            RequestState {
                storage,
                auth_config,
                adapters: Arc::new(AdapterRegistry::default()),
                sms: Arc::new(
                    crate::sms::FilesystemSmsStore::open(std::env::temp_dir())
                        .expect("SMS store should open"),
                ),
                sms_adapters: Arc::new(SmsAdapterRegistry::default()),
                mail,
                mail_adapters: Arc::new(MailAdapterRegistry::default()),
            },
            req,
        )
        .await
        .expect("request should complete")
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn should_route_dotted_s3_virtual_host_to_the_complete_bucket_name() {
        let storage = temp_storage();
        storage.create_bucket("my".to_string()).unwrap();
        storage.create_bucket("my.bucket".to_string()).unwrap();
        let request = HyperRequest::builder()
            .method("PUT")
            .uri("http://localhost/object")
            .header("host", "my.bucket.s3.amazonaws.com")
            .header("content-length", "7")
            .body(Body::from("payload"))
            .expect("virtual-hosted request should build");

        let response = call(storage.clone(), request).await;

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            storage.get_object("my.bucket", "object").unwrap().data,
            b"payload"
        );
        assert!(storage.get_object("my", "object").is_err());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn should_preserve_s3_object_key_identity_through_the_http_surface() {
        let storage = temp_storage();
        storage.create_bucket("key-bucket".to_string()).unwrap();
        let cases = [
            ("space%20name.txt", "space name.txt"),
            ("percent%25name.txt", "percent%name.txt"),
            ("nested%2Fname.txt", "nested/name.txt"),
            ("snowman-%E2%98%83.txt", "snowman-☃.txt"),
            ("a//b", "a//b"),
            ("dir/", "dir/"),
            ("/leading", "/leading"),
        ];

        for (encoded, expected) in cases {
            let request = HyperRequest::builder()
                .method("PUT")
                .uri(format!("http://localhost/{encoded}"))
                .header("host", "key-bucket.s3.amazonaws.com")
                .header("content-length", "1")
                .body(Body::from("x"))
                .expect("virtual-hosted object request should build");

            let response = call(storage.clone(), request).await;

            assert_eq!(response.status(), StatusCode::OK, "{encoded}");
            assert_eq!(
                storage.get_object("key-bucket", expected).unwrap().data,
                b"x",
                "{encoded}"
            );
        }
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
            .header("content-length", "0")
            .body(Body::default())
            .expect("request should build");
        let resp = call(storage.clone(), create).await;
        assert_eq!(resp.status(), StatusCode::OK);

        let get = HyperRequest::builder()
            .method("GET")
            .uri("http://localhost/")
            .header("host", "storage.googleapis.com")
            .header("content-length", "0")
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
            .uri("http://localhost/n/")
            .body(Body::default())
            .expect("request should build");
        let resp = call(storage, req).await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(resp.headers()["content-type"]
            .to_str()
            .unwrap()
            .starts_with("text/plain"));
        let body = resp
            .into_body()
            .collect()
            .await
            .expect("body should read")
            .to_bytes();
        assert!(String::from_utf8(body.to_vec())
            .expect("utf8")
            .contains("sqrzl-emulator"));
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

    #[tokio::test(flavor = "multi_thread")]
    async fn should_preserve_provider_payload_too_large_contracts_after_mail_routing() {
        let cases = [
            (
                "azure",
                "http://localhost/devstoreaccount1/container/blob",
                Some(("x-ms-version", "2023-11-03")),
            ),
            (
                "gcs",
                "http://localhost/bucket/object",
                Some(("host", "storage.googleapis.com")),
            ),
            (
                "oci",
                "http://localhost/n/namespace/b/bucket/o/object",
                None,
            ),
        ];

        for (provider, uri, header) in cases {
            let mut builder = HyperRequest::builder().method("PUT").uri(uri);
            if let Some((name, value)) = header {
                builder = builder.header(name, value);
            }
            let request = builder
                .body(Body::from("payload exceeds limit"))
                .expect("request should build");
            let response = call_with_auth(temp_storage(), auth_with_max(4), request).await;

            assert_eq!(
                response.status(),
                StatusCode::PAYLOAD_TOO_LARGE,
                "wrong status for {provider}"
            );
            assert!(
                !response.headers().contains_key("x-amz-request-id"),
                "{provider} oversized response must not use the S3 contract"
            );
            match provider {
                "azure" => assert_eq!(
                    response
                        .headers()
                        .get("x-ms-error-code")
                        .and_then(|value| value.to_str().ok()),
                    Some("RequestBodyTooLarge")
                ),
                "gcs" => assert_eq!(
                    response
                        .headers()
                        .get("content-type")
                        .and_then(|value| value.to_str().ok()),
                    Some("application/xml")
                ),
                "oci" => assert_eq!(
                    response
                        .headers()
                        .get("content-type")
                        .and_then(|value| value.to_str().ok()),
                    Some("application/json")
                ),
                _ => unreachable!(),
            }
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn should_reject_truncated_storage_request_bodies_before_any_mutation() {
        // Arrange
        let storage = temp_storage();
        let cases = [
            (
                "truncated-s3",
                "object",
                "PUT",
                "http://localhost/truncated-s3/object",
                None,
                "application/xml",
                "IncompleteBody",
            ),
            (
                "truncated-azure",
                "object",
                "PUT",
                "http://localhost/devstoreaccount1/truncated-azure/object",
                Some(("x-ms-version", "2023-11-03")),
                "application/xml",
                "InvalidHeaderValue",
            ),
            (
                "truncated-gcs-json",
                "object",
                "POST",
                "http://localhost/upload/storage/v1/b/truncated-gcs-json/o?uploadType=media&name=object",
                None,
                "application/json",
                "invalidArgument",
            ),
            (
                "truncated-gcs-xml",
                "object",
                "PUT",
                "http://localhost/truncated-gcs-xml/object",
                Some(("host", "storage.googleapis.com")),
                "application/xml",
                "IncompleteBody",
            ),
            (
                "truncated-oci",
                "object",
                "PUT",
                "http://localhost/n/sqrzl-emulator/b/truncated-oci/o/object",
                None,
                "application/json",
                "InvalidParameter",
            ),
        ];
        for (bucket, ..) in cases {
            storage.create_bucket(bucket.to_string()).unwrap();
        }

        // Act
        // Assert
        for (bucket, key, method, uri, provider_header, content_type, expected_body) in cases {
            let mut builder = HyperRequest::builder()
                .method(method)
                .uri(uri)
                .header("content-length", "10");
            if bucket == "truncated-azure" {
                builder = builder.header("x-ms-blob-type", "BlockBlob");
            }
            if let Some((name, value)) = provider_header {
                builder = builder.header(name, value);
            }
            let request = builder
                .body(Body::truncated(bytes::Bytes::from_static(b"short"), 10))
                .expect("truncated request should build");

            let response = call(storage.clone(), request).await;

            assert_eq!(response.status(), StatusCode::BAD_REQUEST);
            assert!(response.headers()["content-type"]
                .to_str()
                .unwrap()
                .starts_with(content_type));
            let body = response
                .into_body()
                .collect()
                .await
                .expect("provider framing body should read")
                .to_bytes();
            let rendered = String::from_utf8_lossy(&body);
            assert!(
                rendered.contains(expected_body),
                "{bucket}: expected body marker {expected_body:?}, got {rendered}"
            );
            assert!(storage.get_object(bucket, key).is_err());
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn should_route_truncated_messaging_bodies_without_capturing_messages() {
        // Arrange
        let root = std::env::temp_dir().join(format!(
            "sqrzl-truncated-messaging-test-{}",
            uuid::Uuid::new_v4()
        ));
        fs::create_dir_all(&root).unwrap();
        let mail = Arc::new(
            crate::mail::FilesystemMailStore::open(&root).expect("mail store should open"),
        );
        let sms =
            Arc::new(crate::sms::FilesystemSmsStore::open(&root).expect("SMS store should open"));
        let state = RequestState {
            storage: temp_storage(),
            auth_config: auth_disabled(),
            adapters: Arc::new(AdapterRegistry::default()),
            sms: sms.clone(),
            sms_adapters: Arc::new(SmsAdapterRegistry::default()),
            mail: mail.clone(),
            mail_adapters: Arc::new(MailAdapterRegistry::default()),
        };
        let cases = [
            (
                "http://localhost/v3/mail/send",
                "errors",
                "sendgrid",
            ),
            (
                "http://localhost/2010-04-01/Accounts/AC00000000000000000000000000000001/Messages.json",
                "21606",
                "twilio",
            ),
        ];

        // Act
        // Assert
        for (uri, expected_body, provider) in cases {
            let request = HyperRequest::builder()
                .method("POST")
                .uri(uri)
                .header("content-length", "10")
                .body(Body::truncated(bytes::Bytes::from_static(b"short"), 10))
                .expect("truncated messaging request should build");
            let response = handle_request(state.clone(), request)
                .await
                .expect("truncated messaging request should respond");

            assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{provider}");
            let body = response
                .into_body()
                .collect()
                .await
                .expect("messaging framing body should read")
                .to_bytes();
            assert!(
                String::from_utf8_lossy(&body).contains(expected_body),
                "wrong {provider} error body"
            );
        }
        assert!(mail.list_mailboxes().unwrap().is_empty());
        assert!(sms.list_conversations().unwrap().is_empty());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn should_return_provider_client_errors_for_malformed_mail_payloads() {
        let cases = [
            ("sendgrid", "http://localhost/v3/mail/send", "{"),
            ("ses", "http://localhost/v2/email/outbound-emails", "{}"),
        ];

        for (provider, uri, body) in cases {
            let request = HyperRequest::builder()
                .method("POST")
                .uri(uri)
                .header("content-type", "application/json")
                .body(Body::from(body))
                .expect("request should build");
            let response = call(temp_storage(), request).await;

            assert_eq!(
                response.status(),
                StatusCode::BAD_REQUEST,
                "malformed {provider} payload should be a client error"
            );
            assert!(
                response
                    .headers()
                    .get("content-type")
                    .and_then(|value| value.to_str().ok())
                    .is_some_and(|value| value.contains("json")),
                "malformed {provider} payload should use its JSON error contract"
            );
        }
    }
}
