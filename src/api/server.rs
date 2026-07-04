use crate::auth::{AdminLoginRequest, AdminSessionManager};
use crate::body::{Body, RequestBody};
use crate::error::{Error, Result};
use crate::server::{serve_h1_connection, ResponseBuilder};
use crate::services::{json_error_response, json_response};
use crate::storage::Storage;
use http_body_util::BodyExt;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use mime_guess::from_path;
use serde::de::DeserializeOwned;
use std::convert::Infallible;
use std::path::Path;
use std::sync::Arc;
use tokio::fs as async_fs;

/// Launches the UI-focused server (port 9001) that exposes the JSON API and optionally serves the web UI.
pub async fn start_ui_server(
    storage: Arc<dyn Storage>,
    config: Arc<crate::Config>,
) -> crate::error::Result<()> {
    let ui_port = config.ui_port;
    let addr = std::net::SocketAddr::from(([0, 0, 0, 0], ui_port));
    let admin_session = Arc::new(AdminSessionManager::new()?);

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
        let config = config.clone();
        let admin_session = admin_session.clone();

        tokio::spawn(async move {
            let service = service_fn(move |req| {
                let storage = storage.clone();
                let config = config.clone();
                let admin_session = admin_session.clone();
                handle_ui_request(storage, config, admin_session, req)
            });

            if let Err(e) = serve_h1_connection(stream, service).await {
                tracing::error!("UI connection error: {}", e);
            }
        });
    }
}

async fn handle_ui_request(
    storage: Arc<dyn Storage>,
    config: Arc<crate::Config>,
    admin_session: Arc<AdminSessionManager>,
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

    if request_content_length_exceeds(&req, config.max_request_bytes) {
        return Ok(admin_payload_too_large_response(config.max_request_bytes));
    }

    if path == crate::auth::admin_session::ADMIN_LOGIN_PATH {
        let resp = handle_admin_login(config, admin_session, req).await;
        return Ok(resp);
    }

    if path == crate::auth::admin_session::ADMIN_LOGOUT_PATH {
        let resp = handle_admin_logout(req);
        return Ok(resp);
    }

    if path == crate::auth::admin_session::ADMIN_SESSION_PATH {
        let resp = handle_admin_session(config, admin_session, req);
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
    } else {
        let default_content =
            "<html><body><h1>Sqrzl Emulator</h1><p>Running in headless mode</p></body></html>";
        Ok(ResponseBuilder::new(StatusCode::OK)
            .content_type("text/html; charset=utf-8")
            .body_str(default_content)
            .build())
    }
}

fn request_content_length_exceeds(req: &Request<RequestBody>, max_request_bytes: usize) -> bool {
    req.headers()
        .get("content-length")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<usize>().ok())
        .map(|content_length| content_length > max_request_bytes)
        .unwrap_or(false)
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

async fn handle_admin_login(
    config: Arc<crate::Config>,
    admin_session: Arc<AdminSessionManager>,
    req: Request<RequestBody>,
) -> Response<Body> {
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

fn handle_admin_logout(req: Request<RequestBody>) -> Response<Body> {
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

fn handle_admin_session(
    config: Arc<crate::Config>,
    admin_session: Arc<AdminSessionManager>,
    req: Request<RequestBody>,
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
    } else if let Some(username) = admin_session.subject_from_request(&req) {
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

fn admin_request_is_authorized(
    req: &Request<RequestBody>,
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

async fn read_json<T: DeserializeOwned>(req: Request<RequestBody>) -> Result<T> {
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
    use super::serve_static_content;
    use http_body_util::BodyExt;
    use hyper::StatusCode;
    use std::fs;

    fn temp_static_dir() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("sqrzl-ui-static-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).expect("temp static dir should be created");
        dir
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
