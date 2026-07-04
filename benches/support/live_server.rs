#![allow(dead_code)]

use bytes::Bytes;
use http_body_util::BodyExt;
use http_body_util::Full;
use hyper_util::client::legacy::connect::HttpConnector;
type Body = Full<Bytes>;
use hyper::{body::Incoming, Request, Response};
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;
use sqrzl_emulator::server::Server;
use sqrzl_emulator::storage::{FilesystemStorage, Storage};
use sqrzl_emulator::Config;
use std::fs;
use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::task::JoinHandle;
use tokio::time::{sleep, Duration, Instant};

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

pub fn auth_disabled() -> Config {
    Config {
        access_key_id: None,
        secret_access_key: None,
        enforce_auth: false,
        admin_auth_disabled: false,
        blobs_path: "./blobs".to_string(),
        lifecycle_interval: Duration::from_hours(1),
        api_port: 9000,
        ui_port: 9001,
        max_request_bytes: sqrzl_emulator::config::DEFAULT_SQRZL_MAX_REQUEST_BYTES,
    }
}

pub struct LiveServer {
    pub base_url: String,
    pub client: Client<HttpConnector, Body>,
    task: JoinHandle<sqrzl_emulator::Result<()>>,
    storage_dir: PathBuf,
    default_admin_authorization: Option<String>,
}

impl LiveServer {
    pub async fn start_api(auth_config: Config) -> Self {
        let api_port = reserve_port();
        let storage_dir = temp_storage_dir("sqrzl-bench-api");
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
            default_admin_authorization: None,
        };
        server.wait_until_ready().await;
        server
    }

    async fn wait_until_ready(&self) {
        self.wait_until_ready_path("/?list-type=2").await;
    }

    async fn wait_until_ready_path(&self, path: &str) {
        let deadline = Instant::now() + Duration::from_secs(6);
        while Instant::now() < deadline {
            assert!(
                !self.task.is_finished(),
                "server task exited before becoming ready"
            );

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

    pub async fn response_bytes(&self, request: Request<Body>) -> Vec<u8> {
        let body = self
            .request(request)
            .await
            .into_body()
            .collect()
            .await
            .expect("response body should read")
            .to_bytes();
        body.to_vec()
    }

    pub async fn response_text(&self, request: Request<Body>) -> String {
        String::from_utf8(self.response_bytes(request).await).expect("response body should be utf8")
    }

    async fn send_request(
        &self,
        mut request: Request<Body>,
        use_default_admin_auth: bool,
    ) -> Result<Response<Incoming>, hyper_util::client::legacy::Error> {
        if use_default_admin_auth
            && request.uri().path().starts_with("/admin/v1")
            && !request.headers().contains_key("authorization")
        {
            if let Some(auth_header) = &self.default_admin_authorization {
                request.headers_mut().insert(
                    "authorization",
                    auth_header
                        .parse()
                        .expect("authorization header should parse"),
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
