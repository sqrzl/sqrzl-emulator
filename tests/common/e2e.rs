#![allow(dead_code)]

use bytes::Bytes;
use http_body_util::BodyExt;
use http_body_util::Full;
use hyper_util::client::legacy::connect::HttpConnector;
type Body = Full<Bytes>;
use hyper::{body::Incoming, Request, Response};
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;
use peas_emulator::api::server::start_ui_server;
use peas_emulator::server::Server;
use peas_emulator::storage::{FilesystemStorage, Storage};
use peas_emulator::Config;
use std::fs;
use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::task::JoinHandle;
use tokio::time::{sleep, Duration, Instant};

pub const AZURE_VERSION: &str = "2023-11-03";

fn reserve_port() -> u16 {
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("port reservation should bind");
    let port = listener
        .local_addr()
        .expect("listener should have local addr")
        .port();
    drop(listener);
    port
}

fn temp_storage_dir(prefix: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("{prefix}-{}", uuid::Uuid::new_v4()));
    let _ = fs::create_dir_all(&dir);
    dir
}

pub struct LiveServer {
    pub base_url: String,
    pub client: Client<HttpConnector, Body>,
    task: JoinHandle<peas_emulator::Result<()>>,
    storage_dir: PathBuf,
    default_admin_cookie: Option<String>,
}

impl LiveServer {
    pub async fn start_api(auth_config: Config) -> Self {
        let api_port = reserve_port();
        let storage_dir = temp_storage_dir("peas-e2e-s3");
        let storage: Arc<dyn Storage> = Arc::new(FilesystemStorage::new(&storage_dir));
        let config = Arc::new(Config {
            api_port,
            ui_port: reserve_port(),
            blobs_path: storage_dir.to_string_lossy().to_string(),
            ..auth_config
        });

        let task = tokio::spawn(Server::new(storage, config, api_port).start());
        let server = Self {
            base_url: format!("http://127.0.0.1:{api_port}"),
            client: Client::builder(TokioExecutor::new()).build_http(),
            task,
            storage_dir,
            default_admin_cookie: None,
        };
        server.wait_until_ready().await;
        server
    }

    pub async fn start_s3(auth_config: Config) -> Self {
        Self::start_api(auth_config).await
    }

    pub async fn start_admin(auth_config: Config) -> Self {
        let ui_port = reserve_port();
        let storage_dir = temp_storage_dir("peas-e2e-admin");
        let storage: Arc<dyn Storage> = Arc::new(FilesystemStorage::new(&storage_dir));
        let config = Arc::new(Config {
            ui_port,
            blobs_path: storage_dir.to_string_lossy().to_string(),
            ..auth_config
        });
        let task = tokio::spawn(start_ui_server(storage, config.clone()));
        let mut server = Self {
            base_url: format!("http://127.0.0.1:{ui_port}"),
            client: Client::builder(TokioExecutor::new()).build_http(),
            task,
            storage_dir,
            default_admin_cookie: None,
        };
        server.wait_until_ready_path("/admin/v1/auth/session").await;

        if config.admin_auth_enforced() {
            let login_request = Request::builder()
                .method("POST")
                .uri(format!("{}/admin/v1/auth/login", server.base_url))
                .header("content-type", "application/json")
                .body(Body::from(format!(
                    r#"{{"username":"{}","password":"{}"}}"#,
                    config
                        .access_key()
                        .expect("admin access key should exist when auth is enforced"),
                    config
                        .secret_key()
                        .expect("admin secret key should exist when auth is enforced")
                )))
                .expect("admin login request should build");
            let login_response = server.request_without_default_auth(login_request).await;
            assert_eq!(
                login_response.status(),
                hyper::StatusCode::OK,
                "admin session login should succeed before live tests run"
            );

            let cookie = login_response
                .headers()
                .get("set-cookie")
                .expect("login response should set a cookie")
                .to_str()
                .expect("set-cookie header should be valid utf-8")
                .split(';')
                .next()
                .expect("set-cookie header should contain a cookie value")
                .to_string();
            server.default_admin_cookie = Some(cookie);
        }

        server.wait_until_ready_path("/admin/v1/buckets").await;
        server
    }

    async fn wait_until_ready(&self) {
        self.wait_until_ready_path("/?list-type=2").await;
    }

    async fn wait_until_ready_path(&self, path: &str) {
        let deadline = Instant::now() + Duration::from_secs(6);
        while Instant::now() < deadline {
            if self.task.is_finished() {
                panic!("server task exited before becoming ready");
            }

            let request = Request::builder()
                .method("GET")
                .uri(format!("{}{}", self.base_url, path))
                .body(Body::default())
                .expect("readiness request should build");

            match self.send_request(request, true).await {
                Ok(_) => return,
                Err(_) => sleep(Duration::from_millis(25)).await,
            }
        }

        panic!("server did not become ready before timeout");
    }

    pub async fn request(&self, request: Request<Body>) -> Response<Incoming> {
        self.send_request(request, true)
            .await
            .expect("live request should complete")
    }

    pub async fn request_without_default_auth(&self, request: Request<Body>) -> Response<Incoming> {
        self.send_request(request, false)
            .await
            .expect("live request should complete")
    }

    async fn send_request(
        &self,
        mut request: Request<Body>,
        use_default_admin_auth: bool,
    ) -> Result<Response<Incoming>, hyper_util::client::legacy::Error> {
        if use_default_admin_auth
            && request.uri().path().starts_with("/admin/v1")
            && !request.headers().contains_key("cookie")
        {
            if let Some(cookie) = &self.default_admin_cookie {
                request.headers_mut().insert(
                    "cookie",
                    cookie.parse().expect("cookie header should parse"),
                );
            }
        }

        self.client.request(request).await
    }
}

impl Drop for LiveServer {
    fn drop(&mut self) {
        self.task.abort();
        let _ = fs::remove_dir_all(&self.storage_dir);
    }
}

pub fn auth_disabled() -> Config {
    Config {
        access_key_id: None,
        secret_access_key: None,
        enforce_auth: false,
        admin_auth_disabled: false,
        blobs_path: "./blobs".to_string(),
        lifecycle_interval: Duration::from_secs(3600),
        api_port: 9000,
        ui_port: 9001,
    }
}

pub fn auth_enabled(key: &str, secret: &str) -> Config {
    Config {
        access_key_id: Some(key.to_string()),
        secret_access_key: Some(secret.to_string()),
        enforce_auth: true,
        admin_auth_disabled: false,
        blobs_path: "./blobs".to_string(),
        lifecycle_interval: Duration::from_secs(3600),
        api_port: 9000,
        ui_port: 9001,
    }
}

pub fn auth_enabled_with_admin_bypass(key: &str, secret: &str) -> Config {
    Config {
        access_key_id: Some(key.to_string()),
        secret_access_key: Some(secret.to_string()),
        enforce_auth: true,
        admin_auth_disabled: true,
        blobs_path: "./blobs".to_string(),
        lifecycle_interval: Duration::from_secs(3600),
        api_port: 9000,
        ui_port: 9001,
    }
}

pub async fn text_body(response: Response<Incoming>) -> String {
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("response body should read")
        .to_bytes();
    String::from_utf8(bytes.to_vec()).expect("response body should be utf8")
}

pub fn rebase_url(base_url: &str, upstream_location: &str) -> String {
    let uri: hyper::Uri = upstream_location
        .parse()
        .expect("upstream location should parse");
    let path_and_query = uri
        .path_and_query()
        .expect("upstream location should include path and query")
        .as_str();
    format!("{base_url}{path_and_query}")
}
