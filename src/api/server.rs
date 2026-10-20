use crate::api::mcp::{required_scope_for_request, McpHandler, McpState};
use crate::auth::mcp::{McpAuth, McpAuthFailure, McpPrincipal};
use crate::auth::{AdminLoginRequest, AdminSessionManager};
use crate::body::{Body, RequestBody};
use crate::error::{Error, Result};
use crate::mail::MailStore;
use crate::metrics::UsageMetrics;
use crate::server::{serve_h1_connection, ResponseBuilder};
use crate::services::{json_error_response, json_response};
use crate::sms::{FilesystemSmsStore, SmsStore};
use crate::storage::Storage;
use http_body_util::BodyExt;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use mime_guess::from_path;
use rmcp::transport::streamable_http_server::{
    session::local::LocalSessionManager, StreamableHttpServerConfig, StreamableHttpService,
};
use serde::de::DeserializeOwned;
use std::convert::Infallible;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::fs as async_fs;
use tower::ServiceExt;

/// Launches the UI-focused server (port 9001) that exposes the JSON API and optionally serves the web UI.
///
/// # Errors
///
/// Returns an error when the underlying emulator operation fails.
pub async fn start_ui_server(
    storage: Arc<dyn Storage>,
    config: Arc<crate::Config>,
    mail: Arc<dyn MailStore>,
) -> crate::error::Result<()> {
    let sms = Arc::new(FilesystemSmsStore::open(&config.blobs_path)?);
    let metrics = Arc::new(UsageMetrics::default());
    start_ui_server_with_sms_and_metrics(storage, config, mail, sms, metrics).await
}

/// Launches the UI server with an explicitly shared SMS store.
///
/// # Errors
///
/// Returns an error when session initialization, listener binding, or accepting fails.
pub async fn start_ui_server_with_sms(
    storage: Arc<dyn Storage>,
    config: Arc<crate::Config>,
    mail: Arc<dyn MailStore>,
    sms: Arc<dyn SmsStore>,
) -> crate::error::Result<()> {
    let metrics = Arc::new(UsageMetrics::default());
    start_ui_server_with_sms_and_metrics(storage, config, mail, sms, metrics).await
}

/// Launches the UI and MCP listener with process-shared usage counters.
///
/// # Errors
///
/// Returns an error when configuration validation, session initialization, listener binding, or accepting fails.
// Listener setup and connection dispatch intentionally stay together at this boundary.
#[allow(clippy::too_many_lines)]
pub async fn start_ui_server_with_sms_and_metrics(
    storage: Arc<dyn Storage>,
    config: Arc<crate::Config>,
    mail: Arc<dyn MailStore>,
    sms: Arc<dyn SmsStore>,
    metrics: Arc<UsageMetrics>,
) -> crate::error::Result<()> {
    config
        .validate_mcp_config()
        .map_err(crate::error::Error::InvalidRequest)?;
    let ui_port = config.ui_port;
    let addr = std::net::SocketAddr::from(([0, 0, 0, 0], ui_port));
    let admin_session = Arc::new(AdminSessionManager::new()?);
    let mcp_auth = Arc::new(McpAuth::new(Arc::new(config.mcp.clone())));
    let mut allowed_hosts = vec![
        "localhost".to_string(),
        format!("localhost:{ui_port}"),
        "127.0.0.1".to_string(),
        format!("127.0.0.1:{ui_port}"),
        "[::1]".to_string(),
        format!("[::1]:{ui_port}"),
        "0.0.0.0".to_string(),
        format!("0.0.0.0:{ui_port}"),
    ];
    allowed_hosts.extend(config.mcp.allowed_hosts.iter().cloned());
    let allowed_origins = allowed_mcp_origins(&config);
    let mcp_http_config = StreamableHttpServerConfig::default()
        .with_json_response(true)
        .with_max_request_body_bytes(config.max_request_bytes)
        .with_allowed_hosts(allowed_hosts)
        .with_allowed_origins(allowed_origins)
        .enforce_origin_validation();
    let mcp_state = McpState {
        storage: storage.clone(),
        mail: mail.clone(),
        sms: sms.clone(),
        config: config.clone(),
        metrics: metrics.clone(),
    };
    let mcp_service = StreamableHttpService::new(
        move || Ok(McpHandler::new(mcp_state.clone())),
        Arc::new(LocalSessionManager::default()),
        mcp_http_config,
    );

    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .map_err(|e| crate::error::Error::InternalError(e.to_string()))?;
    tracing::info!("UI server listening on http://0.0.0.0:{}", ui_port);

    loop {
        let (stream, _) = listener
            .accept()
            .await
            .map_err(|e| crate::error::Error::InternalError(e.to_string()))?;
        let storage = storage.clone();
        let mail = mail.clone();
        let sms = sms.clone();
        let config = config.clone();
        let admin_session = admin_session.clone();
        let metrics = metrics.clone();
        let mcp_auth = mcp_auth.clone();
        let mcp_service = mcp_service.clone();

        tokio::spawn(async move {
            let service = service_fn(move |req| {
                let storage = storage.clone();
                let mail = mail.clone();
                let sms = sms.clone();
                let config = config.clone();
                let admin_session = admin_session.clone();
                let metrics = metrics.clone();
                let mcp_auth = mcp_auth.clone();
                let mcp_service = mcp_service.clone();
                async move {
                    let observed_request_bytes = Arc::new(AtomicU64::new(request_size(&req)));
                    let method = req.method().clone();
                    let uri = req.uri().clone();
                    let headers = req.headers().clone();
                    if req.uri().path() == "/mcp" {
                        let response = handle_mcp_request(
                            req,
                            config,
                            mcp_auth,
                            mcp_service,
                            observed_request_bytes.clone(),
                        )
                        .await;
                        metrics.record_http_request(
                            &method,
                            &uri,
                            &headers,
                            observed_request_bytes.load(Ordering::Relaxed),
                            &response,
                            None,
                        );
                        Ok::<Response<Body>, Infallible>(response)
                    } else {
                        let response = handle_ui_request(
                            storage,
                            mail,
                            sms,
                            config,
                            admin_session,
                            metrics.clone(),
                            observed_request_bytes.clone(),
                            req,
                        )
                        .await?;
                        metrics.record_http_request(
                            &method,
                            &uri,
                            &headers,
                            observed_request_bytes.load(Ordering::Relaxed),
                            &response,
                            None,
                        );
                        Ok(response)
                    }
                }
            });

            if let Err(e) = serve_h1_connection(stream, service).await {
                tracing::error!("UI connection error: {}", e);
            }
        });
    }
}

// This is the single routing boundary for the UI and admin API.
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
async fn handle_ui_request(
    storage: Arc<dyn Storage>,
    mail: Arc<dyn MailStore>,
    sms: Arc<dyn SmsStore>,
    config: Arc<crate::Config>,
    admin_session: Arc<AdminSessionManager>,
    metrics: Arc<UsageMetrics>,
    observed_request_bytes: Arc<AtomicU64>,
    req: Request<RequestBody>,
) -> std::result::Result<Response<Body>, Infallible> {
    let path = req.uri().path().to_string();

    if path == "/healthz" {
        if req.method() == Method::GET {
            return Ok(crate::health::response(storage.as_ref(), config.as_ref()));
        }
        return Ok(json_error_response(&Error::MethodNotAllowed(format!(
            "{} /healthz",
            req.method()
        ))));
    }

    if is_protected_resource_metadata_path(&path) {
        if req.method() == Method::GET
            && config.mcp_auth_enforced()
            && config.mcp_oauth_settings_present()
        {
            return Ok(protected_resource_metadata_response(&config));
        }
        return Ok(ResponseBuilder::new(StatusCode::NOT_FOUND)
            .body_str("Not Found")
            .build());
    }

    if path == "/metrics" {
        if req.method() != Method::GET {
            return Ok(ResponseBuilder::new(StatusCode::METHOD_NOT_ALLOWED)
                .body_str("Method Not Allowed")
                .build());
        }
        if config.admin_auth_enforced()
            && !admin_request_is_authorized(&req, &config, &admin_session)
        {
            return Ok(admin_unauthorized_response());
        }
        let threshold =
            query_u64(req.uri().query().unwrap_or_default(), "min_requests").unwrap_or(0);
        return Ok(ResponseBuilder::new(StatusCode::OK)
            .content_type("text/plain; version=0.0.4; charset=utf-8")
            .body_str(&metrics.render_prometheus(threshold))
            .build());
    }

    if request_content_length_exceeds(&req, config.max_request_bytes) {
        return Ok(admin_payload_too_large_response(config.max_request_bytes));
    }

    let req = match buffer_request(req, config.max_request_bytes).await {
        Ok(req) => req,
        Err(BufferRequestError::TooLarge) => {
            return Ok(admin_payload_too_large_response(config.max_request_bytes));
        }
        Err(BufferRequestError::Invalid(message)) => {
            return Ok(json_error_response(&Error::InvalidRequest(message)));
        }
    };
    observed_request_bytes.store(
        req.body()
            .full_bytes()
            .map_or(0, |bytes| bytes.len() as u64),
        Ordering::Relaxed,
    );

    if path == crate::auth::admin_session::ADMIN_LOGIN_PATH {
        let resp = handle_admin_login(config, admin_session, req).await;
        return Ok(resp);
    }

    if path == crate::auth::admin_session::ADMIN_LOGOUT_PATH {
        let resp = handle_admin_logout(&req);
        return Ok(resp);
    }

    if path == crate::auth::admin_session::ADMIN_SESSION_PATH {
        let resp = handle_admin_session(config.as_ref(), admin_session.as_ref(), &req);
        return Ok(resp);
    }

    if path == crate::api::admin_mail::MAILBOXES_PATH_PREFIX
        || path.starts_with(&format!(
            "{}/",
            crate::api::admin_mail::MAILBOXES_PATH_PREFIX
        ))
    {
        if !admin_request_is_authorized(&req, &config, &admin_session) {
            return Ok(admin_unauthorized_response());
        }

        let resp = match crate::api::admin_mail::handle_request(&mail, &req) {
            Ok(resp) => resp,
            Err(err) => crate::api::admin::error_response(&err),
        };
        return Ok(resp);
    }

    if crate::api::admin_text::matches_path(&path) {
        if !admin_request_is_authorized(&req, &config, &admin_session) {
            return Ok(admin_unauthorized_response());
        }
        let resp = match crate::api::admin_text::handle_request(&sms, req).await {
            Ok(resp) => resp,
            Err(err) => crate::api::admin::error_response(&err),
        };
        return Ok(resp);
    }

    if path == "/admin/v1" || path.starts_with("/admin/v1/") {
        if !admin_request_is_authorized(&req, &config, &admin_session) {
            return Ok(admin_unauthorized_response());
        }

        let resp = match crate::api::admin::handle_request(storage, req).await {
            Ok(resp) => resp,
            Err(err) => crate::api::admin::error_response(&err),
        };
        return Ok(resp);
    }

    if path.starts_with("/api/") {
        return Ok(json_error_response(&Error::RouteNotFound(path)));
    }

    let has_static = Path::new("./static").exists() || Path::new("/app/ui/dist").exists();

    if has_static {
        let static_dir = if Path::new("./static").exists() {
            "./static"
        } else {
            "/app/ui/dist"
        };

        return Ok(serve_static_content(Path::new(static_dir), &path).await);
    }

    let default_content =
        "<html><body><h1>Sqrzl Emulator</h1><p>Running in headless mode</p></body></html>";
    Ok(ResponseBuilder::new(StatusCode::OK)
        .content_type("text/html; charset=utf-8")
        .body_str(default_content)
        .build())
}

fn request_size<B>(req: &Request<B>) -> u64
where
    B: hyper::body::Body,
{
    req.headers()
        .get("content-length")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
        .or_else(|| req.body().size_hint().exact())
        .unwrap_or(0)
}

async fn handle_mcp_request(
    req: Request<RequestBody>,
    config: Arc<crate::Config>,
    auth: Arc<McpAuth>,
    service: StreamableHttpService<McpHandler, LocalSessionManager>,
    observed_request_bytes: Arc<AtomicU64>,
) -> Response<Body> {
    let origin = req
        .headers()
        .get(hyper::header::ORIGIN)
        .and_then(|value| value.to_str().ok())
        .map(ToOwned::to_owned);
    if origin.as_deref().is_some_and(|origin| {
        !allowed_mcp_origins(&config)
            .iter()
            .any(|allowed| allowed == origin)
    }) {
        return ResponseBuilder::new(StatusCode::FORBIDDEN)
            .body_str("Origin not allowed")
            .build();
    }
    if req.method() == Method::OPTIONS {
        return mcp_preflight_response(&req, origin.as_deref());
    }
    let mut response =
        handle_mcp_request_inner(req, config, auth, service, observed_request_bytes).await;
    if let Some(origin) = origin.as_deref() {
        add_mcp_cors_headers(&mut response, origin);
    }
    response
}

async fn handle_mcp_request_inner(
    req: Request<RequestBody>,
    config: Arc<crate::Config>,
    auth: Arc<McpAuth>,
    service: StreamableHttpService<McpHandler, LocalSessionManager>,
    observed_request_bytes: Arc<AtomicU64>,
) -> Response<Body> {
    let mut principal = if config.mcp_auth_enforced() {
        let authorization = req
            .headers()
            .get("authorization")
            .and_then(|value| value.to_str().ok());
        match auth.authenticate(authorization).await {
            Ok(principal) => principal,
            Err(McpAuthFailure::InvalidToken) => {
                return mcp_auth_error_response(&config, StatusCode::UNAUTHORIZED, None)
            }
            Err(McpAuthFailure::Unavailable) => {
                return mcp_auth_error_response(&config, StatusCode::SERVICE_UNAVAILABLE, None)
            }
        }
    } else {
        McpPrincipal {
            scopes: ["sqrzl:read", "sqrzl:write"]
                .into_iter()
                .map(ToString::to_string)
                .collect(),
            client: req
                .headers()
                .get("mcp-client-info")
                .or_else(|| req.headers().get("user-agent"))
                .and_then(|value| value.to_str().ok())
                .unwrap_or("local-client")
                .to_string(),
            request_bytes: 0,
        }
    };
    let buffered = match buffer_request(req, config.max_request_bytes).await {
        Ok(request) => request,
        Err(BufferRequestError::TooLarge) => {
            return admin_payload_too_large_response(config.max_request_bytes)
        }
        Err(BufferRequestError::Invalid(message)) => {
            return json_error_response(&Error::InvalidRequest(message))
        }
    };
    let (mut parts, body) = buffered.into_parts();
    let body_bytes = body.full_bytes().cloned().unwrap_or_default();
    observed_request_bytes.store(body_bytes.len() as u64, Ordering::Relaxed);
    principal.request_bytes = body_bytes.len() as u64;
    if let Ok(request_json) = serde_json::from_slice::<serde_json::Value>(&body_bytes) {
        if let Some(name) = request_json
            .pointer("/params/_meta/io.modelcontextprotocol~1clientInfo/name")
            .and_then(serde_json::Value::as_str)
        {
            principal.client = name.to_string();
        }
    }
    if let Some(scope) = required_scope_for_request(&body_bytes) {
        if !principal.scopes.contains(scope) {
            return mcp_auth_error_response(&config, StatusCode::FORBIDDEN, Some(scope));
        }
    }
    parts.headers.remove("authorization");
    parts.extensions.insert(principal);
    let request = Request::from_parts(parts, Body::from(body_bytes));
    match service.oneshot(request).await {
        Ok(response) => response.map(Body::from_stream),
        Err(never) => match never {},
    }
}

fn allowed_mcp_origins(config: &crate::Config) -> Vec<String> {
    if config.mcp.allowed_origins.is_empty() {
        vec![
            format!("http://localhost:{}", config.ui_port),
            format!("http://127.0.0.1:{}", config.ui_port),
        ]
    } else {
        config.mcp.allowed_origins.clone()
    }
}

fn mcp_preflight_response<B>(req: &Request<B>, origin: Option<&str>) -> Response<Body> {
    let requested_method = req
        .headers()
        .get("access-control-request-method")
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    if !requested_method.is_empty() && !matches!(requested_method, "GET" | "POST" | "DELETE") {
        return ResponseBuilder::new(StatusCode::METHOD_NOT_ALLOWED)
            .body_str("MCP CORS method not allowed")
            .build();
    }
    let allowed_headers = [
        "authorization",
        "content-type",
        "accept",
        "mcp-protocol-version",
        "mcp-session-id",
        "last-event-id",
    ];
    if req
        .headers()
        .get("access-control-request-headers")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|requested| {
            requested
                .split(',')
                .map(str::trim)
                .any(|header| !allowed_headers.contains(&header.to_ascii_lowercase().as_str()))
        })
    {
        return ResponseBuilder::new(StatusCode::FORBIDDEN)
            .body_str("MCP CORS header not allowed")
            .build();
    }
    let mut response = ResponseBuilder::new(StatusCode::NO_CONTENT).build();
    if let Some(origin) = origin {
        add_mcp_cors_headers(&mut response, origin);
        response.headers_mut().insert(
            "access-control-allow-methods",
            hyper::header::HeaderValue::from_static("GET, POST, DELETE, OPTIONS"),
        );
        response.headers_mut().insert(
            "access-control-allow-headers",
            hyper::header::HeaderValue::from_static(
                "Authorization, Content-Type, Accept, MCP-Protocol-Version, MCP-Session-Id, Last-Event-ID",
            ),
        );
        response.headers_mut().insert(
            "access-control-max-age",
            hyper::header::HeaderValue::from_static("600"),
        );
    }
    response
}

fn add_mcp_cors_headers(response: &mut Response<Body>, origin: &str) {
    if let Ok(value) = hyper::header::HeaderValue::from_str(origin) {
        response
            .headers_mut()
            .insert("access-control-allow-origin", value);
        response.headers_mut().append(
            hyper::header::VARY,
            hyper::header::HeaderValue::from_static("Origin"),
        );
        response.headers_mut().insert(
            "access-control-expose-headers",
            hyper::header::HeaderValue::from_static(
                "MCP-Session-Id, MCP-Protocol-Version, WWW-Authenticate",
            ),
        );
    }
}

fn is_protected_resource_metadata_path(path: &str) -> bool {
    matches!(
        path,
        "/.well-known/oauth-protected-resource"
            | "/.well-known/oauth-protected-resource/mcp"
            | "/mcp/.well-known/oauth-protected-resource"
    )
}

fn protected_resource_metadata_response(config: &crate::Config) -> Response<Body> {
    let metadata = serde_json::json!({
        "resource": config.mcp.resource_url.as_deref(),
        "authorization_servers": [config.mcp.authorization_server.as_deref()],
        "scopes_supported": ["sqrzl:read", "sqrzl:write"],
        "bearer_methods_supported": ["header"]
    });
    json_response(StatusCode::OK, &metadata)
}

fn mcp_auth_error_response(
    config: &crate::Config,
    status: StatusCode,
    scope: Option<&str>,
) -> Response<Body> {
    let metadata_url = config
        .mcp
        .resource_url
        .as_deref()
        .and_then(|value| reqwest::Url::parse(value).ok())
        .map_or_else(
            || "/.well-known/oauth-protected-resource".to_string(),
            |mut url| {
                let resource_path = url.path().trim_start_matches('/');
                url.set_path(&format!(
                    "/.well-known/oauth-protected-resource/{resource_path}"
                ));
                url.set_query(None);
                url.set_fragment(None);
                url.to_string()
            },
        );
    let challenge = if let Some(scope) = scope {
        format!("Bearer error=\"insufficient_scope\", scope=\"{scope}\", resource_metadata=\"{metadata_url}\"")
    } else {
        format!("Bearer resource_metadata=\"{metadata_url}\", scope=\"sqrzl:read sqrzl:write\"")
    };
    let body = serde_json::json!({"error": if status == StatusCode::FORBIDDEN { "insufficient_scope" } else { "invalid_token" }});
    let mut response = json_response(status, &body);
    if let Ok(value) = hyper::header::HeaderValue::from_str(&challenge) {
        response.headers_mut().insert("www-authenticate", value);
    }
    response
}

fn query_u64(query: &str, key: &str) -> Option<u64> {
    query.split('&').find_map(|pair| {
        let (name, value) = pair.split_once('=')?;
        (name == key).then(|| value.parse().ok()).flatten()
    })
}

fn request_content_length_exceeds<B>(req: &Request<B>, max_request_bytes: usize) -> bool {
    req.headers()
        .get("content-length")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<usize>().ok())
        .is_some_and(|content_length| content_length > max_request_bytes)
}

enum BufferRequestError {
    TooLarge,
    Invalid(String),
}

async fn buffer_request<B>(
    req: Request<B>,
    max_request_bytes: usize,
) -> std::result::Result<Request<Body>, BufferRequestError>
where
    B: hyper::body::Body<Data = bytes::Bytes> + Unpin,
    B::Error: std::fmt::Display,
{
    let (parts, mut incoming) = req.into_parts();
    let mut body = bytes::BytesMut::new();
    while let Some(frame) = incoming.frame().await {
        let frame = frame.map_err(|error| BufferRequestError::Invalid(error.to_string()))?;
        if let Ok(data) = frame.into_data() {
            let next_len = body
                .len()
                .checked_add(data.len())
                .ok_or(BufferRequestError::TooLarge)?;
            if next_len > max_request_bytes {
                return Err(BufferRequestError::TooLarge);
            }
            body.extend_from_slice(&data);
        }
    }
    Ok(Request::from_parts(parts, Body::from(body.freeze())))
}

fn admin_payload_too_large_response(max_request_bytes: usize) -> Response<Body> {
    let body = crate::api::models::ErrorResponse {
        error: "Payload too large".to_string(),
        code: "PayloadTooLarge".to_string(),
        details: Some(format!(
            "Request body exceeds SQRZL_MAX_REQUEST_BYTES ({max_request_bytes} bytes)"
        )),
    };

    json_response(StatusCode::PAYLOAD_TOO_LARGE, &body)
}

async fn serve_static_content(static_dir: &Path, request_path: &str) -> Response<Body> {
    let normalized_path = request_path.trim_start_matches('/');
    let is_spa_route = normalized_path == "app"
        || normalized_path.starts_with("app/")
        || normalized_path == "auth"
        || normalized_path.starts_with("auth/");
    let candidates: Vec<String> = if normalized_path.is_empty() {
        vec!["index.html".to_string()]
    } else if Path::new(normalized_path).extension().is_some() && !is_spa_route {
        vec![normalized_path.to_string()]
    } else {
        vec![
            normalized_path.to_string(),
            format!("{normalized_path}/index.html"),
            "index.html".to_string(),
        ]
    };

    for relative_path in candidates {
        let file_path = static_dir.join(&relative_path);
        if let Ok(content) = async_fs::read(&file_path).await {
            let content_type = if file_path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name == "index.html")
            {
                "text/html; charset=utf-8".to_string()
            } else {
                from_path(&file_path)
                    .first_or_octet_stream()
                    .essence_str()
                    .to_string()
            };

            return ResponseBuilder::new(StatusCode::OK)
                .content_type(&content_type)
                .body(content)
                .build();
        }
    }

    ResponseBuilder::new(StatusCode::NOT_FOUND)
        .content_type("text/plain; charset=utf-8")
        .body_str("Static content not found")
        .build()
}

async fn handle_admin_login<B>(
    config: Arc<crate::Config>,
    admin_session: Arc<AdminSessionManager>,
    req: Request<B>,
) -> Response<Body>
where
    B: hyper::body::Body<Data = bytes::Bytes> + Send + 'static,
    B::Error: std::fmt::Display,
{
    if req.method() != Method::POST {
        return json_error_response(&Error::MethodNotAllowed(format!(
            "{} {}",
            req.method(),
            crate::auth::admin_session::ADMIN_LOGIN_PATH
        )));
    }

    let login_request: AdminLoginRequest = match read_json(req).await {
        Ok(request) => request,
        Err(err) => return json_error_response(&err),
    };

    if !config.validate_credentials(&login_request.username, &login_request.password) {
        return admin_login_unauthorized_response();
    }

    let cookie = match admin_session.issue_session_cookie(&login_request.username) {
        Ok(cookie) => cookie,
        Err(err) => return json_error_response(&err),
    };

    let mut response = json_response(
        StatusCode::OK,
        &crate::api::models::SuccessResponse { success: true },
    );

    match hyper::header::HeaderValue::from_str(&cookie) {
        Ok(header_value) => {
            response.headers_mut().insert("set-cookie", header_value);
            response.headers_mut().insert(
                "cache-control",
                hyper::header::HeaderValue::from_static("no-store"),
            );
            response
        }
        Err(err) => json_error_response(&Error::InternalError(format!(
            "failed to encode admin session cookie: {err}"
        ))),
    }
}

fn handle_admin_logout<B>(req: &Request<B>) -> Response<Body> {
    if req.method() != Method::POST {
        return json_error_response(&Error::MethodNotAllowed(format!(
            "{} {}",
            req.method(),
            crate::auth::admin_session::ADMIN_LOGOUT_PATH
        )));
    }

    let mut response = json_response(
        StatusCode::OK,
        &crate::api::models::SuccessResponse { success: true },
    );
    response.headers_mut().insert(
        "set-cookie",
        hyper::header::HeaderValue::from_static(
            "sqrzl_admin_session=; Path=/; HttpOnly; SameSite=Lax; Max-Age=0",
        ),
    );
    response.headers_mut().insert(
        "cache-control",
        hyper::header::HeaderValue::from_static("no-store"),
    );
    response
}

fn handle_admin_session<B>(
    config: &crate::Config,
    admin_session: &AdminSessionManager,
    req: &Request<B>,
) -> Response<Body> {
    if req.method() != Method::GET {
        return json_error_response(&Error::MethodNotAllowed(format!(
            "{} {}",
            req.method(),
            crate::auth::admin_session::ADMIN_SESSION_PATH
        )));
    }

    let (mode, username) = if !config.admin_auth_enforced() {
        ("open", None)
    } else if let Some(username) = admin_session.subject_from_request(req) {
        ("session", Some(username))
    } else {
        return admin_unauthorized_response();
    };

    json_response(
        StatusCode::OK,
        &crate::api::models::AdminSessionResponse {
            mode: mode.to_string(),
            username,
        },
    )
}

fn admin_request_is_authorized<B>(
    req: &Request<B>,
    config: &crate::Config,
    admin_session: &AdminSessionManager,
) -> bool {
    if !config.admin_auth_enforced() {
        return true;
    }

    admin_session.has_valid_session(req)
}

fn admin_unauthorized_response() -> Response<Body> {
    let body = crate::api::models::ErrorResponse {
        error: "Unauthorized".to_string(),
        code: "Unauthorized".to_string(),
        details: Some("Provide a valid admin session cookie.".to_string()),
    };
    json_response(StatusCode::UNAUTHORIZED, &body)
}

fn admin_login_unauthorized_response() -> Response<Body> {
    let body = crate::api::models::ErrorResponse {
        error: "Unauthorized".to_string(),
        code: "Unauthorized".to_string(),
        details: Some("Invalid admin credentials".to_string()),
    };

    json_response(StatusCode::UNAUTHORIZED, &body)
}

async fn read_json<T, B>(req: Request<B>) -> Result<T>
where
    T: DeserializeOwned,
    B: hyper::body::Body<Data = bytes::Bytes> + Send + 'static,
    B::Error: std::fmt::Display,
{
    let bytes = req
        .into_body()
        .collect()
        .await
        .map_err(|e| Error::InvalidRequest(e.to_string()))?
        .to_bytes();
    serde_json::from_slice(&bytes).map_err(|e| Error::InvalidRequest(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::{buffer_request, mcp_preflight_response, serve_static_content, BufferRequestError};
    use bytes::Bytes;
    use http_body_util::BodyExt;
    use http_body_util::Full;
    use hyper::{Request, StatusCode};
    use std::fs;

    fn temp_static_dir() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("sqrzl-ui-static-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).expect("temp static dir should be created");
        dir
    }

    #[test]
    fn should_answer_allowed_mcp_cors_preflight_without_bearer_auth() {
        // Arrange
        let request = Request::builder()
            .method("OPTIONS")
            .header("access-control-request-method", "POST")
            .header(
                "access-control-request-headers",
                "authorization,content-type,mcp-protocol-version",
            )
            .body(Full::new(Bytes::new()))
            .expect("preflight request should build");

        // Act
        let response = mcp_preflight_response(&request, Some("https://client.example"));

        // Assert
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        assert_eq!(
            response
                .headers()
                .get("access-control-allow-origin")
                .unwrap(),
            "https://client.example"
        );
        assert!(response
            .headers()
            .get("access-control-allow-headers")
            .is_some());
    }

    #[test]
    fn should_reject_unknown_mcp_cors_headers() {
        // Arrange
        let request = Request::builder()
            .method("OPTIONS")
            .header("access-control-request-method", "POST")
            .header("access-control-request-headers", "x-secret-header")
            .body(Full::new(Bytes::new()))
            .expect("preflight request should build");

        // Act
        let response = mcp_preflight_response(&request, Some("https://client.example"));

        // Assert
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn should_bound_ui_request_bodies_without_relying_on_content_length() {
        let request = Request::builder()
            .uri("/admin/v1/buckets")
            .body(Full::new(Bytes::from_static(b"12345")))
            .expect("request should build");

        let result = buffer_request(request, 4).await;

        assert!(matches!(result, Err(BufferRequestError::TooLarge)));
    }

    #[tokio::test]
    async fn should_serve_static_assets_with_their_real_mime_type() {
        let static_dir = temp_static_dir();
        fs::create_dir_all(static_dir.join("assets")).expect("asset dir should be created");
        fs::write(
            static_dir.join("assets/app.js"),
            "export const app = 'sqrzl';",
        )
        .expect("asset should be written");

        let response = serve_static_content(&static_dir, "/assets/app.js").await;

        assert_eq!(response.status(), StatusCode::OK);
        let content_type = response
            .headers()
            .get("content-type")
            .expect("content-type header should exist")
            .to_str()
            .expect("content-type should be valid utf-8");
        assert!(
            content_type.contains("javascript"),
            "content-type = {content_type}"
        );

        let body = response
            .into_body()
            .collect()
            .await
            .expect("body should read")
            .to_bytes();
        assert_eq!(body.as_ref(), b"export const app = 'sqrzl';");
    }

    #[tokio::test]
    async fn should_fall_back_to_index_for_spa_routes() {
        let static_dir = temp_static_dir();
        fs::write(
            static_dir.join("index.html"),
            "<!doctype html><div id=\"app\"></div>",
        )
        .expect("index should be written");

        let response = serve_static_content(&static_dir, "/dashboard/settings").await;

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response
                .headers()
                .get("content-type")
                .expect("content-type header should exist")
                .to_str()
                .expect("content-type should be valid utf-8"),
            "text/html; charset=utf-8"
        );

        let body = response
            .into_body()
            .collect()
            .await
            .expect("body should read")
            .to_bytes();
        assert!(String::from_utf8(body.to_vec())
            .expect("body should be utf-8")
            .contains("id=\"app\""));
    }

    #[tokio::test]
    async fn should_return_not_found_for_missing_static_assets() {
        let static_dir = temp_static_dir();
        fs::write(
            static_dir.join("index.html"),
            "<!doctype html><div id=\"app\"></div>",
        )
        .expect("index should be written");

        let response = serve_static_content(&static_dir, "/assets/missing.js").await;

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        let body = response
            .into_body()
            .collect()
            .await
            .expect("body should read")
            .to_bytes();
        assert_eq!(body.as_ref(), b"Static content not found");
    }

    #[tokio::test]
    async fn should_fall_back_to_index_for_spa_routes_with_dotted_segments() {
        let static_dir = temp_static_dir();
        fs::write(
            static_dir.join("index.html"),
            "<!doctype html><div id=\"app\"></div>",
        )
        .expect("index should be written");

        let response =
            serve_static_content(&static_dir, "/app/buckets/demo/blobs/readme.txt").await;

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response
                .headers()
                .get("content-type")
                .expect("content-type header should exist")
                .to_str()
                .expect("content-type should be valid utf-8"),
            "text/html; charset=utf-8"
        );
    }
}
