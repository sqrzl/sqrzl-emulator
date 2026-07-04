use super::ProviderAdapter;
use crate::auth::{AuthConfig, HttpRequestLike};
use crate::blob::{BlobBackend, BlobRange, PutBlobRequest, UpdateBlobMetadataRequest};
use crate::body::Body;
use crate::server::{RequestExt as Request, ResponseBuilder};
use crate::storage::Storage;
use crate::utils::request_origin;
use crate::utils::xml::push_escaped_xml;
use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use hmac::{Hmac, KeyInit, Mac};
use http::{Method, StatusCode};
use hyper::Response;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use sha1::Sha1;
use std::collections::HashMap;
use std::fmt::Write as _;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

const GCS_GENERATION_KEY: &str = "__sqrzl_gcs_generation";
const GCS_METAGENERATION_KEY: &str = "__sqrzl_gcs_metageneration";
const GCS_RESUMABLE_SESSION_STATE: &str = "gcs-resumable-session";

#[derive(Clone, Serialize, Deserialize)]
struct ResumableSession {
    bucket: String,
    key: String,
    content_type: String,
    metadata: HashMap<String, String>,
}

pub struct GcsAdapter {
    resumable_sessions: Mutex<HashMap<String, ResumableSession>>,
}

impl Default for GcsAdapter {
    fn default() -> Self {
        Self::new()
    }
}

type MultipartUploadParts = (String, String, HashMap<String, String>, Vec<u8>);

impl GcsAdapter {
    pub fn new() -> Self {
        Self {
            resumable_sessions: Mutex::new(HashMap::new()),
        }
    }

    fn response(status: StatusCode) -> ResponseBuilder {
        ResponseBuilder::new(status)
            .header("x-guploader-uploadid", &uuid::Uuid::new_v4().to_string())
            .header("date", &crate::utils::headers::format_last_modified())
    }

    fn load_provider_state<T>(
        storage: &dyn Storage,
        provider: &str,
        key: &str,
    ) -> Result<Option<T>, String>
    where
        T: DeserializeOwned,
    {
        match storage.get_provider_state(provider, key) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map(Some)
                .map_err(|err| format!("Failed to parse provider state: {err}")),
            Err(crate::error::Error::KeyNotFound) => Ok(None),
            Err(err) => Err(err.to_string()),
        }
    }

    fn save_provider_state<T>(
        storage: &dyn Storage,
        provider: &str,
        key: &str,
        value: &T,
    ) -> Result<(), String>
    where
        T: Serialize,
    {
        let bytes = serde_json::to_vec(value)
            .map_err(|err| format!("Failed to serialize provider state: {err}"))?;
        storage
            .put_provider_state(provider, key, bytes)
            .map_err(|err| err.to_string())
    }

    fn xml_response(status: StatusCode, body: String) -> Response<Body> {
        Self::response(status)
            .content_type("application/xml")
            .body(body.into_bytes())
            .build()
    }

    fn empty_response(status: StatusCode) -> Response<Body> {
        Self::response(status).empty()
    }

    fn json_response(status: StatusCode, body: &str) -> Response<Body> {
        Self::response(status)
            .content_type("application/json")
            .body(body.as_bytes().to_vec())
            .build()
    }

    fn error_response(status: StatusCode, code: &str, message: &str) -> Response<Body> {
        let body = format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?><Error><Code>{}</Code><Message>{}</Message></Error>",
            code, message
        );
        Self::xml_response(status, body)
    }

    fn is_gcs_host(req: &Request) -> bool {
        req.host()
            .map(|host| {
                let host = host.split(':').next().unwrap_or(host);
                host.eq_ignore_ascii_case("storage.googleapis.com")
                    || host.eq_ignore_ascii_case("storage.localhost")
            })
            .unwrap_or(false)
    }

    fn parse_path(req: &Request) -> (Option<String>, Option<String>) {
        let parts: Vec<&str> = req
            .path()
            .trim_start_matches('/')
            .split('/')
            .filter(|segment| !segment.is_empty())
            .collect();
        let bucket = parts.first().map(|segment| (*segment).to_string());
        let object = if parts.len() > 1 {
            Some(parts[1..].join("/"))
        } else {
            None
        };
        (bucket, object)
    }

    fn generation(blob: &crate::models::Object) -> String {
        blob.metadata
            .get(GCS_GENERATION_KEY)
            .cloned()
            .unwrap_or_else(|| blob.last_modified.timestamp_millis().max(1).to_string())
    }

    fn metageneration(blob: &crate::models::Object) -> String {
        blob.metadata
            .get(GCS_METAGENERATION_KEY)
            .cloned()
            .unwrap_or_else(|| "1".to_string())
    }

    fn generation_from_metadata(metadata: &HashMap<String, String>) -> String {
        metadata
            .get(GCS_GENERATION_KEY)
            .cloned()
            .unwrap_or_else(|| "1".to_string())
    }

    fn metageneration_from_metadata(metadata: &HashMap<String, String>) -> String {
        metadata
            .get(GCS_METAGENERATION_KEY)
            .cloned()
            .unwrap_or_else(|| "1".to_string())
    }

    fn public_metadata(metadata: &HashMap<String, String>) -> HashMap<String, String> {
        metadata
            .iter()
            .filter(|(key, _)| {
                key.as_str() != GCS_GENERATION_KEY && key.as_str() != GCS_METAGENERATION_KEY
            })
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect()
    }

    fn decode_object_path(path: &str) -> Result<String, String> {
        urlencoding::decode(path)
            .map(|decoded| decoded.into_owned())
            .map_err(|err| format!("Invalid encoded GCS object path: {err}"))
    }

    fn next_generation(existing: Option<&crate::models::Object>) -> String {
        let current = existing
            .map(Self::generation)
            .and_then(|value| value.parse::<u64>().ok())
            .unwrap_or(0);
        let timestamp = chrono::Utc::now().timestamp_millis().max(1) as u64;
        std::cmp::max(current.saturating_add(1), timestamp).to_string()
    }

    fn metadata_with_gcs_state(
        metadata: HashMap<String, String>,
        generation: String,
        metageneration: String,
    ) -> HashMap<String, String> {
        let mut metadata = Self::public_metadata(&metadata);
        metadata.insert(GCS_GENERATION_KEY.to_string(), generation);
        metadata.insert(GCS_METAGENERATION_KEY.to_string(), metageneration);
        metadata
    }

    fn put_blob_with_generation(
        storage: &dyn Storage,
        bucket: String,
        key: String,
        data: Vec<u8>,
        content_type: String,
        metadata: HashMap<String, String>,
    ) -> Result<crate::blob::BlobRecord, String> {
        let existing = storage.get_blob(&bucket, &key).ok();
        let generation = Self::next_generation(existing.as_ref());
        let metadata = Self::metadata_with_gcs_state(metadata, generation, "1".to_string());
        storage
            .put_blob(PutBlobRequest {
                namespace: bucket,
                key,
                data,
                content_type,
                metadata,
                tags: HashMap::new(),
            })
            .map_err(|err| err.to_string())
    }

    #[allow(clippy::result_large_err)]
    fn check_gcs_preconditions(
        req: &Request,
        blob: &crate::models::Object,
    ) -> Result<(), Response<Body>> {
        let generation = Self::generation(blob);
        let metageneration = Self::metageneration(blob);

        if let Some(expected) = req.query_param("ifGenerationMatch") {
            if generation != expected {
                return Err(Self::json_response(
                    StatusCode::PRECONDITION_FAILED,
                    &serde_json::json!({
                        "error": {
                            "code": 412,
                            "message": "Generation precondition failed",
                            "status": "FAILED_PRECONDITION"
                        }
                    })
                    .to_string(),
                ));
            }
        }
        if let Some(expected) = req.query_param("ifGenerationNotMatch") {
            if generation == expected {
                return Err(Self::json_response(
                    StatusCode::NOT_MODIFIED,
                    &serde_json::json!({
                        "error": {
                            "code": 304,
                            "message": "Generation not-match precondition failed",
                            "status": "NOT_MODIFIED"
                        }
                    })
                    .to_string(),
                ));
            }
        }
        if let Some(expected) = req.query_param("ifMetagenerationMatch") {
            if metageneration != expected {
                return Err(Self::json_response(
                    StatusCode::PRECONDITION_FAILED,
                    &serde_json::json!({
                        "error": {
                            "code": 412,
                            "message": "Metageneration precondition failed",
                            "status": "FAILED_PRECONDITION"
                        }
                    })
                    .to_string(),
                ));
            }
        }
        if let Some(expected) = req.query_param("ifMetagenerationNotMatch") {
            if metageneration == expected {
                return Err(Self::json_response(
                    StatusCode::NOT_MODIFIED,
                    &serde_json::json!({
                        "error": {
                            "code": 304,
                            "message": "Metageneration not-match precondition failed",
                            "status": "NOT_MODIFIED"
                        }
                    })
                    .to_string(),
                ));
            }
        }
        Ok(())
    }

    fn parse_range_header(value: &str, size: u64) -> Option<(usize, usize)> {
        let range = value.strip_prefix("bytes=")?;
        let (start, end) = range.split_once('-')?;
        let start = start.parse::<u64>().ok()?;
        if start >= size {
            return None;
        }
        let end = if end.is_empty() {
            size.saturating_sub(1)
        } else {
            end.parse::<u64>().ok()?.min(size.saturating_sub(1))
        };
        if end < start {
            return None;
        }
        Some((start as usize, end as usize))
    }

    fn metadata_from_headers(req: &Request) -> HashMap<String, String> {
        req.headers()
            .into_iter()
            .filter_map(|(name, value)| {
                name.strip_prefix("x-goog-meta-")
                    .map(|key| (key.to_string(), value))
            })
            .collect()
    }

    fn multipart_boundary(content_type: &str) -> Option<String> {
        content_type.split(';').find_map(|part| {
            let part = part.trim();
            part.strip_prefix("boundary=")
                .map(|value| value.trim_matches('"').to_string())
        })
    }

    fn parse_multipart_upload(
        content_type: &str,
        body: &[u8],
    ) -> Result<MultipartUploadParts, String> {
        let boundary = Self::multipart_boundary(content_type)
            .ok_or_else(|| "Missing multipart boundary".to_string())?;
        let marker = format!("--{}", boundary);
        let payload = String::from_utf8_lossy(body);
        let mut object_name = None;
        let mut metadata = HashMap::new();
        let mut content_type = "application/octet-stream".to_string();
        let mut data = None;

        for part in payload.split(&marker) {
            let part = part.trim();
            if part.is_empty() || part == "--" {
                continue;
            }
            let part = part.trim_end_matches("--").trim();
            let Some((headers, raw_body)) = part.split_once("\r\n\r\n") else {
                continue;
            };
            let mut part_content_type = "application/octet-stream".to_string();
            for header in headers.lines() {
                if let Some(value) = header.split_once(':').map(|(_, value)| value.trim()) {
                    if header.to_ascii_lowercase().starts_with("content-type:") {
                        part_content_type = value.to_string();
                    }
                }
            }
            let raw_body = raw_body.trim_end_matches("\r\n");
            if part_content_type.contains("application/json") {
                let json: serde_json::Value =
                    serde_json::from_str(raw_body).map_err(|err| err.to_string())?;
                object_name = json
                    .get("name")
                    .and_then(|value| value.as_str())
                    .map(|value| value.to_string());
                metadata = json
                    .get("metadata")
                    .and_then(|value| value.as_object())
                    .map(|map| {
                        map.iter()
                            .filter_map(|(key, value)| {
                                value.as_str().map(|value| (key.clone(), value.to_string()))
                            })
                            .collect::<HashMap<_, _>>()
                    })
                    .unwrap_or_default();
            } else {
                content_type = part_content_type;
                data = Some(raw_body.as_bytes().to_vec());
            }
        }

        Ok((
            object_name.ok_or_else(|| "Missing multipart object name".to_string())?,
            content_type,
            metadata,
            data.ok_or_else(|| "Missing multipart object data".to_string())?,
        ))
    }

    fn object_response(
        status: StatusCode,
        blob: &crate::models::Object,
        body_len: usize,
        content_range: Option<String>,
    ) -> ResponseBuilder {
        let generation = Self::generation(blob);
        let metageneration = Self::metageneration(blob);
        let mut builder = Self::response(status)
            .header("accept-ranges", "bytes")
            .header("content-length", &body_len.to_string())
            .header("content-type", &blob.content_type)
            .header("etag", &format!("\"{}\"", blob.etag))
            .header("x-goog-generation", &generation)
            .header("x-goog-metageneration", &metageneration);
        for (key, value) in Self::public_metadata(&blob.metadata) {
            builder = builder.header(&format!("x-goog-meta-{}", key), &value);
        }
        if let Some(content_range) = content_range {
            builder = builder.header("content-range", &content_range);
        }
        builder
    }

    fn sign(config: &AuthConfig, payload: &str) -> Result<String, String> {
        type HmacSha1 = Hmac<Sha1>;
        let secret = config
            .secret_key()
            .ok_or_else(|| "Missing GCS secret key".to_string())?;
        let key = BASE64
            .decode(secret)
            .ok()
            .unwrap_or_else(|| secret.as_bytes().to_vec());
        let mut mac =
            HmacSha1::new_from_slice(&key).map_err(|err| format!("Invalid GCS key: {}", err))?;
        mac.update(payload.as_bytes());
        Ok(BASE64.encode(mac.finalize().into_bytes()))
    }

    fn string_to_sign(req: &Request, bucket: &str, object: Option<&str>, expires: &str) -> String {
        let resource = if let Some(object) = object {
            format!("/{}/{}", bucket, object)
        } else {
            format!("/{}", bucket)
        };

        format!(
            "{}\n{}\n{}\n{}\n{}",
            req.method(),
            req.header("content-md5").unwrap_or(""),
            req.header("content-type").unwrap_or(""),
            expires,
            resource
        )
    }

    #[allow(clippy::result_large_err)]
    fn authorize(
        req: &Request,
        config: &AuthConfig,
        bucket: &str,
        object: Option<&str>,
    ) -> Result<(), Response<Body>> {
        if !config.enforce_auth {
            return Ok(());
        }

        if let (Some(access_id), Some(expires), Some(signature)) = (
            req.query_param("GoogleAccessId"),
            req.query_param("Expires"),
            req.query_param("Signature"),
        ) {
            if config.access_key() != Some(access_id) {
                return Err(Self::error_response(
                    StatusCode::FORBIDDEN,
                    "AccessDenied",
                    "Invalid access id",
                ));
            }
            let expected = Self::sign(config, &Self::string_to_sign(req, bucket, object, expires))
                .map_err(|msg| {
                    Self::error_response(StatusCode::FORBIDDEN, "SignatureDoesNotMatch", &msg)
                })?;
            if expected == signature {
                return Ok(());
            }
            return Err(Self::error_response(
                StatusCode::FORBIDDEN,
                "SignatureDoesNotMatch",
                "GCS signed URL signature mismatch",
            ));
        }

        let Some(authorization) = req.header("authorization") else {
            return Err(Self::error_response(
                StatusCode::FORBIDDEN,
                "AccessDenied",
                "Missing authorization",
            ));
        };
        let prefix = format!("GOOG1 {}:", config.access_key().unwrap_or_default());
        let Some(signature) = authorization.strip_prefix(&prefix) else {
            return Err(Self::error_response(
                StatusCode::FORBIDDEN,
                "AccessDenied",
                "Unsupported authorization",
            ));
        };
        let date = req.header("date").unwrap_or("");
        let expected = Self::sign(config, &Self::string_to_sign(req, bucket, object, date))
            .map_err(|msg| {
                Self::error_response(StatusCode::FORBIDDEN, "SignatureDoesNotMatch", &msg)
            })?;
        if expected == signature {
            Ok(())
        } else {
            Err(Self::error_response(
                StatusCode::FORBIDDEN,
                "SignatureDoesNotMatch",
                "GCS HMAC signature mismatch",
            ))
        }
    }
}

#[cfg(test)]
#[allow(clippy::items_after_test_module)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::storage::FilesystemStorage;
    use http_body_util::BodyExt;
    use hyper::Request as HyperRequest;
    use std::fs;

    fn temp_storage() -> Arc<dyn Storage> {
        let dir = std::env::temp_dir().join(format!("sqrzl-gcs-test-{}", uuid::Uuid::new_v4()));
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
            max_request_bytes: crate::config::DEFAULT_SQRZL_MAX_REQUEST_BYTES,
        })
    }

    fn gcs_auth() -> Arc<AuthConfig> {
        Arc::new(Config {
            access_key_id: Some("test-access".to_string()),
            secret_access_key: Some(BASE64.encode("gcs-secret")),
            enforce_auth: true,
            admin_auth_disabled: false,
            blobs_path: "./blobs".to_string(),
            lifecycle_interval: std::time::Duration::from_secs(3600),
            api_port: 9000,
            ui_port: 9001,
            max_request_bytes: crate::config::DEFAULT_SQRZL_MAX_REQUEST_BYTES,
        })
    }

    async fn parsed_request(
        method: &str,
        uri: &str,
        headers: &[(&str, &str)],
        body: &[u8],
    ) -> Request {
        let mut builder = HyperRequest::builder().method(method).uri(uri);
        for (name, value) in headers {
            builder = builder.header(*name, *value);
        }
        Request::from_hyper(
            builder
                .body(Body::from(body.to_vec()))
                .expect("request should build"),
        )
        .await
        .expect("request should parse")
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn should_handle_gcs_bucket_and_object_crud() {
        let adapter = GcsAdapter::new();
        let storage = temp_storage();

        adapter
            .handle_request(
                storage.clone(),
                auth_disabled(),
                parsed_request(
                    "PUT",
                    "http://localhost/photos",
                    &[("host", "storage.googleapis.com")],
                    b"",
                )
                .await,
            )
            .await
            .expect("bucket create should succeed");

        adapter
            .handle_request(
                storage.clone(),
                auth_disabled(),
                parsed_request(
                    "PUT",
                    "http://localhost/photos/kitten.txt",
                    &[
                        ("host", "storage.googleapis.com"),
                        ("content-type", "text/plain"),
                    ],
                    b"hello gcs",
                )
                .await,
            )
            .await
            .expect("object put should succeed");

        let response = adapter
            .handle_request(
                storage.clone(),
                auth_disabled(),
                parsed_request(
                    "GET",
                    "http://localhost/photos",
                    &[("host", "storage.googleapis.com")],
                    b"",
                )
                .await,
            )
            .await
            .expect("list should succeed");
        let body = response
            .into_body()
            .collect()
            .await
            .expect("body should read")
            .to_bytes();
        assert!(String::from_utf8(body.to_vec())
            .expect("xml")
            .contains("kitten.txt"));

        let response = adapter
            .handle_request(
                storage,
                auth_disabled(),
                parsed_request(
                    "GET",
                    "http://localhost/photos/kitten.txt",
                    &[("host", "storage.googleapis.com")],
                    b"",
                )
                .await,
            )
            .await
            .expect("get should succeed");
        let body = response
            .into_body()
            .collect()
            .await
            .expect("body should read")
            .to_bytes();
        assert_eq!(body.as_ref(), b"hello gcs");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn should_support_gcs_resumable_uploads_and_signed_access() {
        let adapter = GcsAdapter::new();
        let storage = temp_storage();
        storage.create_bucket("videos".to_string()).unwrap();

        let response = adapter
            .handle_request(
                storage.clone(),
                auth_disabled(),
                parsed_request(
                    "POST",
                    "http://localhost/upload/storage/v1/b/videos/o?uploadType=resumable&name=movie.txt",
                    &[
                        ("host", "storage.googleapis.com"),
                        ("x-upload-content-type", "text/plain"),
                    ],
                    b"",
                )
                .await,
            )
            .await
            .expect("resumable init should succeed");
        let location = response
            .headers()
            .get("location")
            .and_then(|value| value.to_str().ok())
            .expect("location should exist")
            .to_string();

        adapter
            .handle_request(
                storage.clone(),
                auth_disabled(),
                parsed_request(
                    "PUT",
                    &location,
                    &[("host", "storage.googleapis.com")],
                    b"chunked",
                )
                .await,
            )
            .await
            .expect("resumable commit should succeed");

        let expires = "4102444800";
        let request = parsed_request(
            "GET",
            &format!(
                "http://localhost/videos/movie.txt?GoogleAccessId=test-access&Expires={}",
                expires
            ),
            &[("host", "storage.googleapis.com")],
            b"",
        )
        .await;
        let signature = GcsAdapter::sign(
            &gcs_auth(),
            &GcsAdapter::string_to_sign(&request, "videos", Some("movie.txt"), expires),
        )
        .expect("signature should build");
        let signed_request = parsed_request(
            "GET",
            &format!(
                "http://localhost/videos/movie.txt?GoogleAccessId=test-access&Expires={}&Signature={}",
                expires,
                urlencoding::encode(&signature)
            ),
            &[("host", "storage.googleapis.com")],
            b"",
        )
        .await;

        let response = adapter
            .handle_request(storage, gcs_auth(), signed_request)
            .await
            .expect("signed get should succeed");
        let body = response
            .into_body()
            .collect()
            .await
            .expect("body should read")
            .to_bytes();
        assert_eq!(body.as_ref(), b"chunked");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn should_complete_resumable_upload_after_adapter_restart() {
        let adapter = GcsAdapter::new();
        let storage = temp_storage();
        storage.create_bucket("videos".to_string()).unwrap();

        let response = adapter
            .handle_request(
                storage.clone(),
                auth_disabled(),
                parsed_request(
                    "POST",
                    "http://localhost/upload/storage/v1/b/videos/o?uploadType=resumable&name=restart.txt",
                    &[
                        ("host", "storage.googleapis.com"),
                        ("x-upload-content-type", "text/plain"),
                    ],
                    b"",
                )
                .await,
            )
            .await
            .expect("resumable init should succeed");
        let location = response
            .headers()
            .get("location")
            .and_then(|value| value.to_str().ok())
            .expect("location should exist")
            .to_string();

        let restarted = GcsAdapter::new();
        restarted
            .handle_request(
                storage.clone(),
                auth_disabled(),
                parsed_request(
                    "PUT",
                    &location,
                    &[("host", "storage.googleapis.com")],
                    b"restart gcs",
                )
                .await,
            )
            .await
            .expect("resumable commit after restart should succeed");

        let stored = storage
            .get_object("videos", "restart.txt")
            .expect("resumable object should persist");
        assert_eq!(stored.data.as_slice(), b"restart gcs");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn should_return_generation_headers_and_support_ranges() {
        let adapter = GcsAdapter::new();
        let storage = temp_storage();

        adapter
            .handle_request(
                storage.clone(),
                auth_disabled(),
                parsed_request(
                    "PUT",
                    "http://localhost/docs",
                    &[("host", "storage.googleapis.com")],
                    b"",
                )
                .await,
            )
            .await
            .expect("bucket create should succeed");

        let response = adapter
            .handle_request(
                storage.clone(),
                auth_disabled(),
                parsed_request(
                    "PUT",
                    "http://localhost/docs/readme.txt",
                    &[
                        ("host", "storage.googleapis.com"),
                        ("content-type", "text/plain"),
                        ("x-goog-meta-owner", "riley"),
                    ],
                    b"hello gcs range",
                )
                .await,
            )
            .await
            .expect("object put should succeed");
        assert!(response.headers().get("x-goog-generation").is_some());

        let response = adapter
            .handle_request(
                storage.clone(),
                auth_disabled(),
                parsed_request(
                    "HEAD",
                    "http://localhost/docs/readme.txt",
                    &[("host", "storage.googleapis.com")],
                    b"",
                )
                .await,
            )
            .await
            .expect("head should succeed");
        assert!(response.headers().get("x-goog-generation").is_some());
        assert_eq!(
            response
                .headers()
                .get("accept-ranges")
                .and_then(|value| value.to_str().ok()),
            Some("bytes")
        );
        assert_eq!(
            response
                .headers()
                .get("x-goog-meta-owner")
                .and_then(|value| value.to_str().ok()),
            Some("riley")
        );

        let response = adapter
            .handle_request(
                storage,
                auth_disabled(),
                parsed_request(
                    "GET",
                    "http://localhost/docs/readme.txt",
                    &[("host", "storage.googleapis.com"), ("range", "bytes=6-8")],
                    b"",
                )
                .await,
            )
            .await
            .expect("range get should succeed");
        assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(
            response
                .headers()
                .get("content-range")
                .and_then(|value| value.to_str().ok()),
            Some("bytes 6-8/15")
        );
        assert_eq!(
            response
                .headers()
                .get("x-goog-meta-owner")
                .and_then(|value| value.to_str().ok()),
            Some("riley")
        );
        let body = response
            .into_body()
            .collect()
            .await
            .expect("body should read")
            .to_bytes();
        assert_eq!(body.as_ref(), b"gcs");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn should_support_gcs_json_api_bucket_and_media_flows() {
        let adapter = GcsAdapter::new();
        let storage = temp_storage();

        let response = adapter
            .handle_request(
                storage.clone(),
                auth_disabled(),
                parsed_request(
                    "POST",
                    "http://localhost/storage/v1/b?project=test-project",
                    &[
                        ("host", "storage.googleapis.com"),
                        ("content-type", "application/json"),
                    ],
                    br#"{"name":"json-bucket"}"#,
                )
                .await,
            )
            .await
            .expect("json api create bucket should succeed");
        assert_eq!(response.status(), StatusCode::OK);

        let response = adapter
            .handle_request(
                storage.clone(),
                auth_disabled(),
                parsed_request(
                    "POST",
                    "http://localhost/upload/storage/v1/b/json-bucket/o?uploadType=resumable&name=hello.txt",
                    &[
                        ("host", "storage.googleapis.com"),
                        ("x-upload-content-type", "text/plain"),
                        ("x-goog-meta-owner", "jules"),
                    ],
                    b"",
                )
                .await,
            )
            .await
            .expect("resumable init should succeed");
        let location = response
            .headers()
            .get("location")
            .and_then(|value| value.to_str().ok())
            .expect("location should exist")
            .to_string();

        adapter
            .handle_request(
                storage.clone(),
                auth_disabled(),
                parsed_request(
                    "PUT",
                    &location,
                    &[("host", "storage.googleapis.com")],
                    b"json api",
                )
                .await,
            )
            .await
            .expect("resumable upload should succeed");

        let response = adapter
            .handle_request(
                storage.clone(),
                auth_disabled(),
                parsed_request(
                    "GET",
                    "http://localhost/storage/v1/b/json-bucket/o/hello.txt",
                    &[("host", "storage.googleapis.com")],
                    b"",
                )
                .await,
            )
            .await
            .expect("json object metadata should succeed");
        let body = response
            .into_body()
            .collect()
            .await
            .expect("body should read")
            .to_bytes();
        let json = String::from_utf8(body.to_vec()).expect("json");
        assert!(json.contains("\"name\":\"hello.txt\""));
        assert!(json.contains("\"owner\":\"jules\""));

        let response = adapter
            .handle_request(
                storage.clone(),
                auth_disabled(),
                parsed_request(
                    "GET",
                    "http://localhost/download/storage/v1/b/json-bucket/o/hello.txt?alt=media",
                    &[("host", "storage.googleapis.com")],
                    b"",
                )
                .await,
            )
            .await
            .expect("download should succeed");
        let body = response
            .into_body()
            .collect()
            .await
            .expect("body should read")
            .to_bytes();
        assert_eq!(body.as_ref(), b"json api");

        let response = adapter
            .handle_request(
                storage,
                auth_disabled(),
                parsed_request(
                    "GET",
                    "http://localhost/download/storage/v1/b/json-bucket/o/hello.txt?alt=media",
                    &[("host", "storage.googleapis.com"), ("range", "bytes=0-3")],
                    b"",
                )
                .await,
            )
            .await
            .expect("range download should succeed");
        assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
        let body = response
            .into_body()
            .collect()
            .await
            .expect("body should read")
            .to_bytes();
        assert_eq!(body.as_ref(), b"json");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn should_support_gcs_json_api_multipart_uploads() {
        let adapter = GcsAdapter::new();
        let storage = temp_storage();
        storage
            .create_bucket("multipart-bucket".to_string())
            .unwrap();

        let boundary = "sqrzl-boundary";
        let body = format!(
            "--{boundary}\r\nContent-Type: application/json; charset=UTF-8\r\n\r\n{{\"name\":\"multi.txt\",\"metadata\":{{\"owner\":\"sdk\"}}}}\r\n--{boundary}\r\nContent-Type: text/plain\r\n\r\nmultipart body\r\n--{boundary}--\r\n"
        );
        let response = adapter
            .handle_request(
                storage.clone(),
                auth_disabled(),
                parsed_request(
                    "POST",
                    "http://localhost/upload/storage/v1/b/multipart-bucket/o?uploadType=multipart",
                    &[
                        ("host", "storage.googleapis.com"),
                        ("content-type", "multipart/related; boundary=sqrzl-boundary"),
                    ],
                    body.as_bytes(),
                )
                .await,
            )
            .await
            .expect("multipart upload should succeed");
        assert_eq!(response.status(), StatusCode::OK);

        let response = adapter
            .handle_request(
                storage,
                auth_disabled(),
                parsed_request(
                    "GET",
                    "http://localhost/storage/v1/b/multipart-bucket/o/multi.txt",
                    &[("host", "storage.googleapis.com")],
                    b"",
                )
                .await,
            )
            .await
            .expect("metadata fetch should succeed");
        let body = response
            .into_body()
            .collect()
            .await
            .expect("body should read")
            .to_bytes();
        let json = String::from_utf8(body.to_vec()).expect("json");
        assert!(json.contains("\"name\":\"multi.txt\""));
        assert!(json.contains("\"owner\":\"sdk\""));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn should_increment_generation_on_overwrite_and_patch_metageneration() {
        let adapter = GcsAdapter::new();
        let storage = temp_storage();
        storage.create_bucket("gens".to_string()).unwrap();

        let response = adapter
            .handle_request(
                storage.clone(),
                auth_disabled(),
                parsed_request(
                    "PUT",
                    "http://localhost/gens/item.txt",
                    &[
                        ("host", "storage.googleapis.com"),
                        ("content-type", "text/plain"),
                    ],
                    b"v1",
                )
                .await,
            )
            .await
            .expect("first put should succeed");
        let first_generation = response
            .headers()
            .get("x-goog-generation")
            .and_then(|value| value.to_str().ok())
            .expect("generation should exist")
            .to_string();

        let response = adapter
            .handle_request(
                storage.clone(),
                auth_disabled(),
                parsed_request(
                    "PUT",
                    "http://localhost/gens/item.txt",
                    &[
                        ("host", "storage.googleapis.com"),
                        ("content-type", "text/plain"),
                    ],
                    b"v2",
                )
                .await,
            )
            .await
            .expect("second put should succeed");
        let second_generation = response
            .headers()
            .get("x-goog-generation")
            .and_then(|value| value.to_str().ok())
            .expect("generation should exist")
            .to_string();
        assert_ne!(first_generation, second_generation);

        let response = adapter
            .handle_request(
                storage.clone(),
                auth_disabled(),
                parsed_request(
                    "GET",
                    "http://localhost/storage/v1/b/gens/o/item.txt",
                    &[("host", "storage.googleapis.com")],
                    b"",
                )
                .await,
            )
            .await
            .expect("json metadata fetch should succeed");
        let body = response
            .into_body()
            .collect()
            .await
            .expect("body should read")
            .to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&body).expect("json should parse");
        assert_eq!(
            json.get("generation").and_then(|value| value.as_str()),
            Some(second_generation.as_str())
        );
        assert_eq!(
            json.get("metageneration").and_then(|value| value.as_str()),
            Some("1")
        );

        let response = adapter
            .handle_request(
                storage.clone(),
                auth_disabled(),
                parsed_request(
                    "PATCH",
                    "http://localhost/storage/v1/b/gens/o/item.txt?ifMetagenerationMatch=1",
                    &[
                        ("host", "storage.googleapis.com"),
                        ("content-type", "application/json"),
                    ],
                    br#"{"metadata":{"owner":"sdk"}}"#,
                )
                .await,
            )
            .await
            .expect("patch should succeed");
        let body = response
            .into_body()
            .collect()
            .await
            .expect("body should read")
            .to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&body).expect("json should parse");
        assert_eq!(
            json.get("generation").and_then(|value| value.as_str()),
            Some(second_generation.as_str())
        );
        assert_eq!(
            json.get("metageneration").and_then(|value| value.as_str()),
            Some("2")
        );
        assert_eq!(
            json.get("metadata")
                .and_then(|value| value.get("owner"))
                .and_then(|value| value.as_str()),
            Some("sdk")
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn should_enforce_gcs_generation_and_metageneration_preconditions() {
        let adapter = GcsAdapter::new();
        let storage = temp_storage();
        storage.create_bucket("conds".to_string()).unwrap();

        adapter
            .handle_request(
                storage.clone(),
                auth_disabled(),
                parsed_request(
                    "PUT",
                    "http://localhost/conds/check.txt",
                    &[
                        ("host", "storage.googleapis.com"),
                        ("content-type", "text/plain"),
                    ],
                    b"check",
                )
                .await,
            )
            .await
            .expect("put should succeed");

        let response = adapter
            .handle_request(
                storage.clone(),
                auth_disabled(),
                parsed_request(
                    "GET",
                    "http://localhost/storage/v1/b/conds/o/check.txt",
                    &[("host", "storage.googleapis.com")],
                    b"",
                )
                .await,
            )
            .await
            .expect("json fetch should succeed");
        let body = response
            .into_body()
            .collect()
            .await
            .expect("body should read")
            .to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&body).expect("json should parse");
        let generation = json
            .get("generation")
            .and_then(|value| value.as_str())
            .expect("generation should exist")
            .to_string();

        let response = adapter
            .handle_request(
                storage.clone(),
                auth_disabled(),
                parsed_request(
                    "GET",
                    &format!(
                        "http://localhost/storage/v1/b/conds/o/check.txt?ifGenerationMatch={}",
                        generation
                    ),
                    &[("host", "storage.googleapis.com")],
                    b"",
                )
                .await,
            )
            .await
            .expect("conditional get should complete");
        assert_eq!(response.status(), StatusCode::OK);

        let response = adapter
            .handle_request(
                storage.clone(),
                auth_disabled(),
                parsed_request(
                    "GET",
                    "http://localhost/storage/v1/b/conds/o/check.txt?ifGenerationMatch=999999",
                    &[("host", "storage.googleapis.com")],
                    b"",
                )
                .await,
            )
            .await
            .expect("failed conditional get should complete");
        assert_eq!(response.status(), StatusCode::PRECONDITION_FAILED);

        let response = adapter
            .handle_request(
                storage,
                auth_disabled(),
                parsed_request(
                    "PATCH",
                    "http://localhost/storage/v1/b/conds/o/check.txt?ifMetagenerationMatch=999",
                    &[
                        ("host", "storage.googleapis.com"),
                        ("content-type", "application/json"),
                    ],
                    br#"{"metadata":{"owner":"blocked"}}"#,
                )
                .await,
            )
            .await
            .expect("failed patch should complete");
        assert_eq!(response.status(), StatusCode::PRECONDITION_FAILED);
    }
}

impl ProviderAdapter for GcsAdapter {
    fn name(&self) -> &'static str {
        "gcs"
    }

    fn matches(&self, req: &Request) -> bool {
        Self::is_gcs_host(req)
            || req
                .header("authorization")
                .map(|value| value.starts_with("GOOG1 "))
                .unwrap_or(false)
            || req.query_param("GoogleAccessId").is_some()
            || req.path().starts_with("/upload/storage/v1/")
            || req.path().starts_with("/storage/v1/")
            || req.path().starts_with("/download/storage/v1/")
    }

    fn handle<'a>(
        &'a self,
        storage: Arc<dyn Storage>,
        auth_config: Arc<AuthConfig>,
        req: Request,
    ) -> Pin<Box<dyn Future<Output = Result<Response<Body>, String>> + Send + 'a>> {
        Box::pin(async move { self.handle_request(storage, auth_config, req).await })
    }
}

impl GcsAdapter {
    async fn handle_request(
        &self,
        storage: Arc<dyn Storage>,
        auth_config: Arc<AuthConfig>,
        req: Request,
    ) -> Result<Response<Body>, String> {
        if req.path().starts_with("/storage/v1/") || req.path().starts_with("/download/storage/v1/")
        {
            return self.handle_json_api(storage, auth_config, req).await;
        }

        if req.path().starts_with("/upload/storage/v1/b/")
            || req.path().starts_with("/upload/resumable/")
        {
            return self.handle_resumable(storage, auth_config, req).await;
        }

        let (bucket, object) = Self::parse_path(&req);
        let Some(bucket) = bucket else {
            if req.method() == Method::GET {
                let buckets = storage
                    .as_ref()
                    .list_namespaces()
                    .map_err(|err| err.to_string())?;
                let mut body = String::with_capacity(128 + buckets.len() * 64);
                body.push_str(
                    "<?xml version=\"1.0\" encoding=\"UTF-8\"?><ListAllMyBucketsResult><Buckets>",
                );
                for bucket in buckets {
                    body.push_str("<Bucket><Name>");
                    push_escaped_xml(&mut body, &bucket.name);
                    body.push_str("</Name></Bucket>");
                }
                body.push_str("</Buckets></ListAllMyBucketsResult>");
                return Ok(Self::xml_response(StatusCode::OK, body));
            }

            return Ok(Self::error_response(
                StatusCode::BAD_REQUEST,
                "InvalidURI",
                "Missing bucket",
            ));
        };

        if let Err(response) = Self::authorize(&req, &auth_config, &bucket, object.as_deref()) {
            return Ok(response);
        }

        match (req.method(), object) {
            (&Method::PUT, None) => {
                storage
                    .as_ref()
                    .create_namespace(bucket)
                    .map_err(|err| err.to_string())?;
                Ok(Self::empty_response(StatusCode::OK))
            }
            (&Method::DELETE, None) => {
                storage
                    .as_ref()
                    .delete_namespace(&bucket)
                    .map_err(|err| err.to_string())?;
                Ok(Self::empty_response(StatusCode::NO_CONTENT))
            }
            (&Method::GET, None) => {
                let blobs = storage
                    .as_ref()
                    .list_blobs(
                        &bucket,
                        req.query_param("prefix"),
                        req.query_param("delimiter"),
                        None,
                        None,
                    )
                    .map_err(|err| err.to_string())?;
                let mut body = String::with_capacity(128 + blobs.len() * 128);
                body.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?><ListBucketResult><Name>");
                push_escaped_xml(&mut body, &bucket);
                body.push_str("</Name>");
                for blob in blobs {
                    let generation = blob.version_id.as_deref().map_or_else(
                        || blob.last_modified.timestamp_millis().max(1).to_string(),
                        str::to_string,
                    );
                    body.push_str("<Contents><Key>");
                    push_escaped_xml(&mut body, &blob.key);
                    body.push_str("</Key><Size>");
                    write!(&mut body, "{}", blob.size).unwrap();
                    body.push_str("</Size><ETag>");
                    push_escaped_xml(&mut body, &blob.etag);
                    body.push_str("</ETag><Generation>");
                    push_escaped_xml(&mut body, &generation);
                    body.push_str("</Generation></Contents>");
                }
                body.push_str("</ListBucketResult>");
                Ok(Self::xml_response(StatusCode::OK, body))
            }
            (&Method::PUT, Some(object)) => {
                let stored = Self::put_blob_with_generation(
                    storage.as_ref(),
                    bucket,
                    object,
                    req.body.to_vec(),
                    req.header("content-type")
                        .unwrap_or("application/octet-stream")
                        .to_string(),
                    Self::metadata_from_headers(&req),
                )
                .map_err(|err| err.to_string())?;
                Ok(Self::response(StatusCode::OK)
                    .header("etag", &format!("\"{}\"", stored.etag))
                    .header(
                        "x-goog-generation",
                        &Self::generation_from_metadata(&stored.metadata),
                    )
                    .header(
                        "x-goog-metageneration",
                        &Self::metageneration_from_metadata(&stored.metadata),
                    )
                    .empty())
            }
            (&Method::GET, Some(object)) => {
                if let Some(range_header) = req.header("range") {
                    let blob = storage
                        .as_ref()
                        .get_blob(&bucket, &object)
                        .map_err(|err| err.to_string())?;
                    if let Some((start, end)) = Self::parse_range_header(range_header, blob.size) {
                        let payload = storage
                            .as_ref()
                            .get_blob_range(
                                &bucket,
                                &object,
                                BlobRange {
                                    start: start as u64,
                                    end: end as u64,
                                },
                            )
                            .map_err(|err| err.to_string())?;
                        return Ok(Self::object_response(
                            StatusCode::PARTIAL_CONTENT,
                            &payload.blob,
                            payload.data.len(),
                            Some(format!("bytes {}-{}/{}", start, end, blob.size)),
                        )
                        .body(payload.data)
                        .build());
                    }
                    return Ok(Self::error_response(
                        StatusCode::RANGE_NOT_SATISFIABLE,
                        "InvalidRange",
                        "The requested range is not satisfiable",
                    ));
                }
                let blob = storage
                    .as_ref()
                    .get_blob(&bucket, &object)
                    .map_err(|err| err.to_string())?;
                Ok(
                    Self::object_response(StatusCode::OK, &blob, blob.size as usize, None)
                        .body(blob.data)
                        .build(),
                )
            }
            (&Method::HEAD, Some(object)) => {
                let blob = storage
                    .as_ref()
                    .get_blob(&bucket, &object)
                    .map_err(|err| err.to_string())?;
                Ok(Self::object_response(StatusCode::OK, &blob, blob.size as usize, None).empty())
            }
            (&Method::DELETE, Some(object)) => {
                storage
                    .as_ref()
                    .delete_blob(&bucket, &object)
                    .map_err(|err| err.to_string())?;
                Ok(Self::empty_response(StatusCode::NO_CONTENT))
            }
            _ => Ok(Self::error_response(
                StatusCode::METHOD_NOT_ALLOWED,
                "UnsupportedHttpVerb",
                "Unsupported GCS operation",
            )),
        }
    }

    async fn handle_resumable(
        &self,
        storage: Arc<dyn Storage>,
        auth_config: Arc<AuthConfig>,
        req: Request,
    ) -> Result<Response<Body>, String> {
        let parts: Vec<&str> = req
            .path()
            .trim_start_matches('/')
            .split('/')
            .filter(|segment| !segment.is_empty())
            .collect();

        if parts.starts_with(&["upload", "storage", "v1", "b"]) && parts.len() >= 6 {
            let bucket = parts[4].to_string();
            if let Err(response) = Self::authorize(&req, &auth_config, &bucket, None) {
                return Ok(response);
            }
            if req.query_param("uploadType") == Some("multipart") {
                let content_type = req.header("content-type").unwrap_or("multipart/related");
                let (key, object_content_type, metadata, data) =
                    Self::parse_multipart_upload(content_type, &req.body)?;
                let stored = storage.as_ref();
                let stored = Self::put_blob_with_generation(
                    stored,
                    bucket,
                    key,
                    data,
                    object_content_type,
                    metadata,
                )?;
                return Ok(Self::json_response(
                    StatusCode::OK,
                    &serde_json::json!({
                        "kind": "storage#object",
                        "name": stored.key,
                        "etag": stored.etag,
                        "generation": stored.metadata.get(GCS_GENERATION_KEY).cloned().unwrap_or_else(|| stored.last_modified.timestamp_millis().max(1).to_string()),
                        "metageneration": stored.metadata.get(GCS_METAGENERATION_KEY).cloned().unwrap_or_else(|| "1".to_string()),
                    })
                    .to_string(),
                ));
            }
            let key = req
                .query_param("name")
                .ok_or_else(|| "Missing resumable upload object name".to_string())?
                .to_string();
            let session_id = uuid::Uuid::new_v4().to_string();
            let session = ResumableSession {
                bucket,
                key,
                content_type: req
                    .header("x-upload-content-type")
                    .unwrap_or("application/octet-stream")
                    .to_string(),
                metadata: Self::metadata_from_headers(&req),
            };
            Self::save_provider_state(
                storage.as_ref(),
                GCS_RESUMABLE_SESSION_STATE,
                &session_id,
                &session,
            )?;
            self.resumable_sessions
                .lock()
                .map_err(|_| "Failed to lock resumable sessions".to_string())?
                .insert(session_id.clone(), session);
            let upload_location =
                format!("{}/upload/resumable/{}", request_origin(&req), session_id);
            return Ok(Self::response(StatusCode::OK)
                .header("location", &upload_location)
                .empty());
        }

        let parts: Vec<&str> = req
            .path()
            .trim_start_matches('/')
            .split('/')
            .filter(|segment| !segment.is_empty())
            .collect();
        if parts.starts_with(&["upload", "resumable"]) && parts.len() == 3 {
            let session_id = parts[2];
            let session = {
                let mut sessions = self
                    .resumable_sessions
                    .lock()
                    .map_err(|_| "Failed to lock resumable sessions".to_string())?;
                sessions.remove(session_id)
            }
            .or(Self::load_provider_state(
                storage.as_ref(),
                GCS_RESUMABLE_SESSION_STATE,
                session_id,
            )?)
            .ok_or_else(|| "Unknown resumable upload session".to_string())?;
            let stored = storage.as_ref();
            let stored = Self::put_blob_with_generation(
                stored,
                session.bucket,
                session.key,
                req.body.to_vec(),
                session.content_type,
                session.metadata,
            )?;
            storage
                .delete_provider_state(GCS_RESUMABLE_SESSION_STATE, session_id)
                .map_err(|err| err.to_string())?;
            return Ok(Self::json_response(
                StatusCode::OK,
                &serde_json::json!({
                    "kind": "storage#object",
                    "name": stored.key,
                    "etag": stored.etag,
                    "generation": stored.metadata.get(GCS_GENERATION_KEY).cloned().unwrap_or_else(|| stored.last_modified.timestamp_millis().max(1).to_string()),
                    "metageneration": stored.metadata.get(GCS_METAGENERATION_KEY).cloned().unwrap_or_else(|| "1".to_string()),
                })
                .to_string(),
            ));
        }

        Ok(Self::error_response(
            StatusCode::BAD_REQUEST,
            "InvalidURI",
            "Unsupported resumable upload path",
        ))
    }

    async fn handle_json_api(
        &self,
        storage: Arc<dyn Storage>,
        auth_config: Arc<AuthConfig>,
        req: Request,
    ) -> Result<Response<Body>, String> {
        let parts: Vec<&str> = req
            .path()
            .trim_start_matches('/')
            .split('/')
            .filter(|segment| !segment.is_empty())
            .collect();

        if parts.starts_with(&["storage", "v1", "b"]) {
            if parts.len() == 3 {
                return match *req.method() {
                    Method::GET => {
                        let buckets = storage
                            .as_ref()
                            .list_namespaces()
                            .map_err(|err| err.to_string())?;
                        Ok(Self::json_response(
                            StatusCode::OK,
                            &serde_json::json!({
                                "kind": "storage#buckets",
                                "items": buckets.into_iter().map(|bucket| serde_json::json!({
                                    "name": bucket.name,
                                    "timeCreated": bucket.created_at.to_rfc3339(),
                                })).collect::<Vec<_>>()
                            })
                            .to_string(),
                        ))
                    }
                    Method::POST => {
                        let payload: serde_json::Value =
                            serde_json::from_slice(&req.body).map_err(|err| err.to_string())?;
                        let bucket = payload
                            .get("name")
                            .and_then(|value| value.as_str())
                            .ok_or_else(|| "Missing bucket name".to_string())?;
                        storage
                            .as_ref()
                            .create_namespace(bucket.to_string())
                            .map_err(|err| err.to_string())?;
                        let namespace = storage
                            .as_ref()
                            .get_namespace(bucket)
                            .map_err(|err| err.to_string())?;
                        Ok(Self::json_response(
                            StatusCode::OK,
                            &serde_json::json!({
                                "kind": "storage#bucket",
                                "name": namespace.name,
                                "timeCreated": namespace.created_at.to_rfc3339(),
                            })
                            .to_string(),
                        ))
                    }
                    _ => Ok(Self::error_response(
                        StatusCode::METHOD_NOT_ALLOWED,
                        "UnsupportedHttpVerb",
                        "Unsupported GCS JSON API bucket collection operation",
                    )),
                };
            }

            let bucket = parts.get(3).copied().unwrap_or_default().to_string();
            if let Err(response) = Self::authorize(&req, &auth_config, &bucket, None) {
                return Ok(response);
            }

            if parts.len() == 4 {
                return match *req.method() {
                    Method::GET => {
                        let namespace = storage
                            .as_ref()
                            .get_namespace(&bucket)
                            .map_err(|err| err.to_string())?;
                        Ok(Self::json_response(
                            StatusCode::OK,
                            &serde_json::json!({
                                "kind": "storage#bucket",
                                "name": namespace.name,
                                "timeCreated": namespace.created_at.to_rfc3339(),
                            })
                            .to_string(),
                        ))
                    }
                    Method::DELETE => {
                        storage
                            .as_ref()
                            .delete_namespace(&bucket)
                            .map_err(|err| err.to_string())?;
                        Ok(Self::empty_response(StatusCode::NO_CONTENT))
                    }
                    _ => Ok(Self::error_response(
                        StatusCode::METHOD_NOT_ALLOWED,
                        "UnsupportedHttpVerb",
                        "Unsupported GCS JSON API bucket operation",
                    )),
                };
            }

            if parts.get(4) == Some(&"o") {
                if parts.len() == 5 {
                    let blobs = storage
                        .as_ref()
                        .list_blobs(
                            &bucket,
                            req.query_param("prefix"),
                            req.query_param("delimiter"),
                            None,
                            None,
                        )
                        .map_err(|err| err.to_string())?;
                    return Ok(Self::json_response(
                        StatusCode::OK,
                        &serde_json::json!({
                            "kind": "storage#objects",
                            "items": blobs.into_iter().map(|blob| serde_json::json!({
                                "name": blob.key,
                                "bucket": bucket,
                                "size": blob.size.to_string(),
                                "etag": blob.etag,
                                "generation": Self::generation_from_metadata(&blob.metadata),
                                "metageneration": Self::metageneration_from_metadata(&blob.metadata),
                                "metadata": Self::public_metadata(&blob.metadata),
                            })).collect::<Vec<_>>()
                        })
                        .to_string(),
                    ));
                }

                let object = Self::decode_object_path(&parts[5..].join("/"))?;
                if let Err(response) = Self::authorize(&req, &auth_config, &bucket, Some(&object)) {
                    return Ok(response);
                }
                let alt_media = req.query_param("alt") == Some("media")
                    || req.path().starts_with("/download/storage/v1/");
                return match *req.method() {
                    Method::GET => {
                        let blob = storage
                            .as_ref()
                            .get_blob(&bucket, &object)
                            .map_err(|err| err.to_string())?;
                        if let Err(response) = Self::check_gcs_preconditions(&req, &blob) {
                            return Ok(response);
                        }
                        if alt_media {
                            if let Some(range_header) = req.header("range") {
                                if let Some((start, end)) =
                                    Self::parse_range_header(range_header, blob.size)
                                {
                                    let payload = storage
                                        .as_ref()
                                        .get_blob_range(
                                            &bucket,
                                            &object,
                                            BlobRange {
                                                start: start as u64,
                                                end: end as u64,
                                            },
                                        )
                                        .map_err(|err| err.to_string())?;
                                    return Ok(Self::object_response(
                                        StatusCode::PARTIAL_CONTENT,
                                        &payload.blob,
                                        payload.data.len(),
                                        Some(format!("bytes {}-{}/{}", start, end, blob.size)),
                                    )
                                    .body(payload.data)
                                    .build());
                                }
                                return Ok(Self::error_response(
                                    StatusCode::RANGE_NOT_SATISFIABLE,
                                    "InvalidRange",
                                    "The requested range is not satisfiable",
                                ));
                            }
                            Ok(Self::object_response(
                                StatusCode::OK,
                                &blob,
                                blob.size as usize,
                                None,
                            )
                            .body(blob.data)
                            .build())
                        } else {
                            Ok(Self::json_response(
                                StatusCode::OK,
                                &serde_json::json!({
                                    "kind": "storage#object",
                                    "name": blob.key,
                                    "bucket": bucket,
                                    "size": blob.size.to_string(),
                                    "etag": blob.etag,
                                    "generation": Self::generation(&blob),
                                    "metageneration": Self::metageneration(&blob),
                                    "metadata": Self::public_metadata(&blob.metadata),
                                })
                                .to_string(),
                            ))
                        }
                    }
                    Method::PATCH => {
                        let blob = storage
                            .as_ref()
                            .get_blob(&bucket, &object)
                            .map_err(|err| err.to_string())?;
                        if let Err(response) = Self::check_gcs_preconditions(&req, &blob) {
                            return Ok(response);
                        }
                        let payload: serde_json::Value =
                            serde_json::from_slice(&req.body).map_err(|err| err.to_string())?;
                        let metadata = payload
                            .get("metadata")
                            .and_then(|value| value.as_object())
                            .map(|map| {
                                map.iter()
                                    .filter_map(|(key, value)| {
                                        value.as_str().map(|value| (key.clone(), value.to_string()))
                                    })
                                    .collect::<HashMap<_, _>>()
                            })
                            .unwrap_or_else(|| Self::public_metadata(&blob.metadata));
                        let updated = storage
                            .as_ref()
                            .update_blob_metadata(UpdateBlobMetadataRequest {
                                namespace: bucket.clone(),
                                key: object.clone(),
                                metadata: Self::metadata_with_gcs_state(
                                    metadata,
                                    Self::generation(&blob),
                                    blob.metadata
                                        .get(GCS_METAGENERATION_KEY)
                                        .and_then(|value| value.parse::<u64>().ok())
                                        .unwrap_or(1)
                                        .saturating_add(1)
                                        .to_string(),
                                ),
                            })
                            .map_err(|err| err.to_string())?;
                        return Ok(Self::json_response(
                            StatusCode::OK,
                            &serde_json::json!({
                                "kind": "storage#object",
                                "name": updated.key,
                                "bucket": bucket,
                                "size": updated.size.to_string(),
                                "etag": updated.etag,
                                "generation": Self::generation_from_metadata(&updated.metadata),
                                "metageneration": Self::metageneration_from_metadata(&updated.metadata),
                                "metadata": Self::public_metadata(&updated.metadata),
                            })
                            .to_string(),
                        ));
                    }
                    Method::DELETE => {
                        let blob = storage
                            .as_ref()
                            .get_blob(&bucket, &object)
                            .map_err(|err| err.to_string())?;
                        if let Err(response) = Self::check_gcs_preconditions(&req, &blob) {
                            return Ok(response);
                        }
                        storage
                            .as_ref()
                            .delete_blob(&bucket, &object)
                            .map_err(|err| err.to_string())?;
                        Ok(Self::empty_response(StatusCode::NO_CONTENT))
                    }
                    _ => Ok(Self::error_response(
                        StatusCode::METHOD_NOT_ALLOWED,
                        "UnsupportedHttpVerb",
                        "Unsupported GCS JSON API object operation",
                    )),
                };
            }
        }

        if parts.starts_with(&["download", "storage", "v1", "b"]) && parts.get(5) == Some(&"o") {
            let bucket = parts.get(4).copied().unwrap_or_default().to_string();
            let object = Self::decode_object_path(&parts[6..].join("/"))?;
            if let Err(response) = Self::authorize(&req, &auth_config, &bucket, Some(&object)) {
                return Ok(response);
            }
            let blob = storage
                .as_ref()
                .get_blob(&bucket, &object)
                .map_err(|err| err.to_string())?;
            if let Err(response) = Self::check_gcs_preconditions(&req, &blob) {
                return Ok(response);
            }
            if let Some(range_header) = req.header("range") {
                if let Some((start, end)) = Self::parse_range_header(range_header, blob.size) {
                    let payload = storage
                        .as_ref()
                        .get_blob_range(
                            &bucket,
                            &object,
                            BlobRange {
                                start: start as u64,
                                end: end as u64,
                            },
                        )
                        .map_err(|err| err.to_string())?;
                    return Ok(Self::object_response(
                        StatusCode::PARTIAL_CONTENT,
                        &payload.blob,
                        payload.data.len(),
                        Some(format!("bytes {}-{}/{}", start, end, blob.size)),
                    )
                    .body(payload.data)
                    .build());
                }
                return Ok(Self::error_response(
                    StatusCode::RANGE_NOT_SATISFIABLE,
                    "InvalidRange",
                    "The requested range is not satisfiable",
                ));
            }
            return Ok(
                Self::object_response(StatusCode::OK, &blob, blob.size as usize, None)
                    .body(blob.data)
                    .build(),
            );
        }

        Ok(Self::error_response(
            StatusCode::BAD_REQUEST,
            "InvalidURI",
            "Unsupported GCS JSON API path",
        ))
    }
}
