use super::{state, ProviderAdapter};
use crate::auth::{AuthConfig, HttpRequestLike};
use crate::blob::{BlobBackend, BlobRange, PutBlobRequest, UpdateBlobMetadataRequest};
use crate::body::Body;
use crate::server::{RequestExt as Request, ResponseBuilder};
use crate::storage::Storage;
use crate::utils::request_origin;
use crate::utils::xml::push_escaped_xml;
use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use hmac::{Hmac, KeyInit, Mac};
use http::{HeaderMap, Method, StatusCode, Uri};
use hyper::Response;
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
    #[must_use]
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

    fn matches_head(uri: &Uri, headers: &HeaderMap) -> bool {
        let path = uri.path();
        let host = headers
            .get("host")
            .and_then(|value| value.to_str().ok())
            .and_then(|host| host.split(':').next())
            .unwrap_or("");
        let authorization = headers
            .get("authorization")
            .and_then(|value| value.to_str().ok())
            .unwrap_or("");
        let query = uri.query().unwrap_or("");

        host.eq_ignore_ascii_case("storage.googleapis.com")
            || host.eq_ignore_ascii_case("storage.localhost")
            || authorization.starts_with("GOOG1 ")
            || query.contains("GoogleAccessId=")
            || path.starts_with("/upload/storage/v1/")
            || path.starts_with("/storage/v1/")
            || path.starts_with("/download/storage/v1/")
    }

    fn payload_too_large_response(max_request_bytes: usize) -> Response<Body> {
        let message =
            format!("Request body exceeds SQRZL_MAX_REQUEST_BYTES ({max_request_bytes} bytes)");
        let mut body =
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?><Error><Code>EntityTooLarge</Code><Message>"
                .to_string();
        push_escaped_xml(&mut body, &message);
        body.push_str("</Message></Error>");

        Self::xml_response(StatusCode::PAYLOAD_TOO_LARGE, body)
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
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?><Error><Code>{code}</Code><Message>{message}</Message></Error>"
        );
        Self::xml_response(status, body)
    }

    fn is_gcs_host(req: &Request) -> bool {
        req.host().is_some_and(|host| {
            let host = host.split(':').next().unwrap_or(host);
            host.eq_ignore_ascii_case("storage.googleapis.com")
                || host.eq_ignore_ascii_case("storage.localhost")
        })
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
            .map(std::borrow::Cow::into_owned)
            .map_err(|err| format!("Invalid encoded GCS object path: {err}"))
    }

    fn next_generation(existing: Option<&crate::models::Object>) -> String {
        let current = existing
            .map(Self::generation)
            .and_then(|value| value.parse::<u64>().ok())
            .unwrap_or(0);
        let timestamp = u64::try_from(chrono::Utc::now().timestamp_millis().max(1)).unwrap_or(1);
        std::cmp::max(current.saturating_add(1), timestamp).to_string()
    }

    fn metadata_with_gcs_state(
        mut metadata: HashMap<String, String>,
        generation: String,
        metageneration: String,
    ) -> HashMap<String, String> {
        metadata.retain(|key, _| {
            key.as_str() != GCS_GENERATION_KEY && key.as_str() != GCS_METAGENERATION_KEY
        });
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
        Some((usize::try_from(start).ok()?, usize::try_from(end).ok()?))
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
        let marker = format!("--{boundary}");
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
                    .map(std::string::ToString::to_string);
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
            builder = builder.header(&format!("x-goog-meta-{key}"), &value);
        }
        if let Some(content_range) = content_range {
            builder = builder.header("content-range", &content_range);
        }
        builder
    }

    fn response_body_len(size: u64) -> Result<usize, String> {
        usize::try_from(size).map_err(|_| "GCS object is too large for this platform".to_string())
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
            HmacSha1::new_from_slice(&key).map_err(|err| format!("Invalid GCS key: {err}"))?;
        mac.update(payload.as_bytes());
        Ok(BASE64.encode(mac.finalize().into_bytes()))
    }

    fn string_to_sign(req: &Request, bucket: &str, object: Option<&str>, expires: &str) -> String {
        let resource = if let Some(object) = object {
            format!("/{bucket}/{object}")
        } else {
            format!("/{bucket}")
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
            lifecycle_interval: std::time::Duration::from_hours(1),
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
            lifecycle_interval: std::time::Duration::from_hours(1),
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
                &storage.clone(),
                &auth_disabled(),
                &parsed_request(
                    "PUT",
                    "http://localhost/photos",
                    &[("host", "storage.googleapis.com")],
                    b"",
                )
                .await,
            )
            .expect("bucket create should succeed");

        adapter
            .handle_request(
                &storage.clone(),
                &auth_disabled(),
                &parsed_request(
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
            .expect("object put should succeed");

        let response = adapter
            .handle_request(
                &storage.clone(),
                &auth_disabled(),
                &parsed_request(
                    "GET",
                    "http://localhost/photos",
                    &[("host", "storage.googleapis.com")],
                    b"",
                )
                .await,
            )
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
                &storage,
                &auth_disabled(),
                &parsed_request(
                    "GET",
                    "http://localhost/photos/kitten.txt",
                    &[("host", "storage.googleapis.com")],
                    b"",
                )
                .await,
            )
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
                &storage.clone(),
                &auth_disabled(),
                &parsed_request(
                    "POST",
                    "http://localhost/upload/storage/v1/b/videos/o?uploadType=resumable&name=movie.txt",
                    &[
                        ("host", "storage.googleapis.com"),
                        ("x-upload-content-type", "text/plain"),
                    ],
                    b"",
                )
                .await,)
            .expect("resumable init should succeed");
        let location = response
            .headers()
            .get("location")
            .and_then(|value| value.to_str().ok())
            .expect("location should exist")
            .to_string();

        adapter
            .handle_request(
                &storage.clone(),
                &auth_disabled(),
                &parsed_request(
                    "PUT",
                    &location,
                    &[("host", "storage.googleapis.com")],
                    b"chunked",
                )
                .await,
            )
            .expect("resumable commit should succeed");

        let expires = "4102444800";
        let request = parsed_request(
            "GET",
            &format!(
                "http://localhost/videos/movie.txt?GoogleAccessId=test-access&Expires={expires}"
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
            .handle_request(&storage, &gcs_auth(), &signed_request)
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
                &storage.clone(),
                &auth_disabled(),
                &parsed_request(
                    "POST",
                    "http://localhost/upload/storage/v1/b/videos/o?uploadType=resumable&name=restart.txt",
                    &[
                        ("host", "storage.googleapis.com"),
                        ("x-upload-content-type", "text/plain"),
                    ],
                    b"",
                )
                .await,)
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
                &storage.clone(),
                &auth_disabled(),
                &parsed_request(
                    "PUT",
                    &location,
                    &[("host", "storage.googleapis.com")],
                    b"restart gcs",
                )
                .await,
            )
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
                &storage.clone(),
                &auth_disabled(),
                &parsed_request(
                    "PUT",
                    "http://localhost/docs",
                    &[("host", "storage.googleapis.com")],
                    b"",
                )
                .await,
            )
            .expect("bucket create should succeed");

        let response = adapter
            .handle_request(
                &storage.clone(),
                &auth_disabled(),
                &parsed_request(
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
            .expect("object put should succeed");
        assert!(response.headers().get("x-goog-generation").is_some());

        let response = adapter
            .handle_request(
                &storage.clone(),
                &auth_disabled(),
                &parsed_request(
                    "HEAD",
                    "http://localhost/docs/readme.txt",
                    &[("host", "storage.googleapis.com")],
                    b"",
                )
                .await,
            )
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
                &storage,
                &auth_disabled(),
                &parsed_request(
                    "GET",
                    "http://localhost/docs/readme.txt",
                    &[("host", "storage.googleapis.com"), ("range", "bytes=6-8")],
                    b"",
                )
                .await,
            )
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

        create_gcs_json_bucket(&adapter, &storage).await;
        upload_gcs_json_object_resumably(&adapter, &storage).await;
        verify_gcs_json_object_metadata(&adapter, &storage).await;
        verify_gcs_json_media_downloads(&adapter, &storage).await;
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
                &storage.clone(),
                &auth_disabled(),
                &parsed_request(
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
            .expect("multipart upload should succeed");
        assert_eq!(response.status(), StatusCode::OK);

        let response = adapter
            .handle_request(
                &storage,
                &auth_disabled(),
                &parsed_request(
                    "GET",
                    "http://localhost/storage/v1/b/multipart-bucket/o/multi.txt",
                    &[("host", "storage.googleapis.com")],
                    b"",
                )
                .await,
            )
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

        let second_generation = verify_gcs_generation_increment(&adapter, &storage).await;
        verify_gcs_json_generation_metadata(&adapter, &storage, &second_generation).await;
        verify_gcs_metageneration_patch(&adapter, &storage, &second_generation).await;
    }

    async fn create_gcs_json_bucket(adapter: &GcsAdapter, storage: &Arc<dyn Storage>) {
        let response = adapter
            .handle_request(
                &storage.clone(),
                &auth_disabled(),
                &parsed_request(
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
            .expect("json api create bucket should succeed");
        assert_eq!(response.status(), StatusCode::OK);
    }

    async fn upload_gcs_json_object_resumably(adapter: &GcsAdapter, storage: &Arc<dyn Storage>) {
        let response = adapter
            .handle_request(
                &storage.clone(),
                &auth_disabled(),
                &parsed_request(
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
            .expect("resumable init should succeed");
        let location = header_value(&response, "location")
            .expect("location should exist")
            .to_string();

        adapter
            .handle_request(
                &storage.clone(),
                &auth_disabled(),
                &parsed_request(
                    "PUT",
                    &location,
                    &[("host", "storage.googleapis.com")],
                    b"json api",
                )
                .await,
            )
            .expect("resumable upload should succeed");
    }

    async fn verify_gcs_json_object_metadata(adapter: &GcsAdapter, storage: &Arc<dyn Storage>) {
        let response = adapter
            .handle_request(
                &storage.clone(),
                &auth_disabled(),
                &parsed_request(
                    "GET",
                    "http://localhost/storage/v1/b/json-bucket/o/hello.txt",
                    &[("host", "storage.googleapis.com")],
                    b"",
                )
                .await,
            )
            .expect("json object metadata should succeed");
        let json = String::from_utf8(read_test_body(response).await).expect("json");
        assert!(json.contains("\"name\":\"hello.txt\""));
        assert!(json.contains("\"owner\":\"jules\""));
    }

    async fn verify_gcs_json_media_downloads(adapter: &GcsAdapter, storage: &Arc<dyn Storage>) {
        let response = adapter
            .handle_request(
                &storage.clone(),
                &auth_disabled(),
                &parsed_request(
                    "GET",
                    "http://localhost/download/storage/v1/b/json-bucket/o/hello.txt?alt=media",
                    &[("host", "storage.googleapis.com")],
                    b"",
                )
                .await,
            )
            .expect("download should succeed");
        assert_eq!(read_test_body(response).await.as_slice(), b"json api");

        let response = adapter
            .handle_request(
                &storage.clone(),
                &auth_disabled(),
                &parsed_request(
                    "GET",
                    "http://localhost/download/storage/v1/b/json-bucket/o/hello.txt?alt=media",
                    &[("host", "storage.googleapis.com"), ("range", "bytes=0-3")],
                    b"",
                )
                .await,
            )
            .expect("range download should succeed");
        assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(read_test_body(response).await.as_slice(), b"json");
    }

    async fn verify_gcs_generation_increment(
        adapter: &GcsAdapter,
        storage: &Arc<dyn Storage>,
    ) -> String {
        let first_generation = put_gcs_generation_object(adapter, storage, b"v1").await;
        let second_generation = put_gcs_generation_object(adapter, storage, b"v2").await;
        assert_ne!(first_generation, second_generation);
        second_generation
    }

    async fn put_gcs_generation_object(
        adapter: &GcsAdapter,
        storage: &Arc<dyn Storage>,
        body: &[u8],
    ) -> String {
        let response = adapter
            .handle_request(
                &storage.clone(),
                &auth_disabled(),
                &parsed_request(
                    "PUT",
                    "http://localhost/gens/item.txt",
                    &[
                        ("host", "storage.googleapis.com"),
                        ("content-type", "text/plain"),
                    ],
                    body,
                )
                .await,
            )
            .expect("put should succeed");
        header_value(&response, "x-goog-generation")
            .expect("generation should exist")
            .to_string()
    }

    async fn verify_gcs_json_generation_metadata(
        adapter: &GcsAdapter,
        storage: &Arc<dyn Storage>,
        generation: &str,
    ) {
        let json = fetch_gcs_generation_metadata(adapter, storage).await;
        assert_eq!(
            json.get("generation").and_then(|value| value.as_str()),
            Some(generation)
        );
        assert_eq!(
            json.get("metageneration").and_then(|value| value.as_str()),
            Some("1")
        );
    }

    async fn verify_gcs_metageneration_patch(
        adapter: &GcsAdapter,
        storage: &Arc<dyn Storage>,
        generation: &str,
    ) {
        let response = adapter
            .handle_request(
                &storage.clone(),
                &auth_disabled(),
                &parsed_request(
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
            .expect("patch should succeed");
        let json = parse_json_body(response).await;
        assert_eq!(
            json.get("generation").and_then(|value| value.as_str()),
            Some(generation)
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

    async fn fetch_gcs_generation_metadata(
        adapter: &GcsAdapter,
        storage: &Arc<dyn Storage>,
    ) -> serde_json::Value {
        let response = adapter
            .handle_request(
                &storage.clone(),
                &auth_disabled(),
                &parsed_request(
                    "GET",
                    "http://localhost/storage/v1/b/gens/o/item.txt",
                    &[("host", "storage.googleapis.com")],
                    b"",
                )
                .await,
            )
            .expect("json metadata fetch should succeed");
        parse_json_body(response).await
    }

    async fn parse_json_body(response: Response<Body>) -> serde_json::Value {
        serde_json::from_slice(&read_test_body(response).await).expect("json should parse")
    }

    async fn read_test_body(response: Response<Body>) -> Vec<u8> {
        response
            .into_body()
            .collect()
            .await
            .expect("body should read")
            .to_bytes()
            .to_vec()
    }

    fn header_value<'a>(response: &'a Response<Body>, name: &str) -> Option<&'a str> {
        response
            .headers()
            .get(name)
            .and_then(|value| value.to_str().ok())
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn should_enforce_gcs_generation_and_metageneration_preconditions() {
        let adapter = GcsAdapter::new();
        let storage = temp_storage();
        storage.create_bucket("conds".to_string()).unwrap();

        adapter
            .handle_request(
                &storage.clone(),
                &auth_disabled(),
                &parsed_request(
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
            .expect("put should succeed");

        let response = adapter
            .handle_request(
                &storage.clone(),
                &auth_disabled(),
                &parsed_request(
                    "GET",
                    "http://localhost/storage/v1/b/conds/o/check.txt",
                    &[("host", "storage.googleapis.com")],
                    b"",
                )
                .await,
            )
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
                &storage.clone(),
                &auth_disabled(),
                &parsed_request(
                    "GET",
                    &format!(
                        "http://localhost/storage/v1/b/conds/o/check.txt?ifGenerationMatch={generation}"
                    ),
                    &[("host", "storage.googleapis.com")],
                    b"",
                )
                .await,)
            .expect("conditional get should complete");
        assert_eq!(response.status(), StatusCode::OK);

        let response = adapter
            .handle_request(
                &storage.clone(),
                &auth_disabled(),
                &parsed_request(
                    "GET",
                    "http://localhost/storage/v1/b/conds/o/check.txt?ifGenerationMatch=999999",
                    &[("host", "storage.googleapis.com")],
                    b"",
                )
                .await,
            )
            .expect("failed conditional get should complete");
        assert_eq!(response.status(), StatusCode::PRECONDITION_FAILED);

        let response = adapter
            .handle_request(
                &storage,
                &auth_disabled(),
                &parsed_request(
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
                .is_some_and(|value| value.starts_with("GOOG1 "))
            || req.query_param("GoogleAccessId").is_some()
            || req.path().starts_with("/upload/storage/v1/")
            || req.path().starts_with("/storage/v1/")
            || req.path().starts_with("/download/storage/v1/")
    }

    fn matches_request_head(&self, _method: &Method, uri: &Uri, headers: &HeaderMap) -> bool {
        Self::matches_head(uri, headers)
    }

    fn render_payload_too_large(
        &self,
        _method: &Method,
        _uri: &Uri,
        _headers: &HeaderMap,
        max_request_bytes: usize,
    ) -> Response<Body> {
        Self::payload_too_large_response(max_request_bytes)
    }

    fn handle<'a>(
        &'a self,
        storage: Arc<dyn Storage>,
        auth_config: Arc<AuthConfig>,
        req: Request,
    ) -> Pin<Box<dyn Future<Output = Result<Response<Body>, String>> + Send + 'a>> {
        Box::pin(std::future::ready(self.handle_request(
            &storage,
            &auth_config,
            &req,
        )))
    }
}

impl GcsAdapter {
    fn handle_request(
        &self,
        storage: &Arc<dyn Storage>,
        auth_config: &Arc<AuthConfig>,
        req: &Request,
    ) -> Result<Response<Body>, String> {
        if req.path().starts_with("/storage/v1/") || req.path().starts_with("/download/storage/v1/")
        {
            return Self::handle_json_api(storage, auth_config, req);
        }

        if req.path().starts_with("/upload/storage/v1/b/")
            || req.path().starts_with("/upload/resumable/")
        {
            return self.handle_resumable(storage, auth_config, req);
        }

        let (bucket, object) = Self::parse_path(req);
        let Some(bucket) = bucket else {
            return Self::handle_xml_root_request(storage, req);
        };

        if let Err(response) = Self::authorize(req, auth_config, &bucket, object.as_deref()) {
            return Ok(response);
        }

        if let Some(object) = object {
            Self::handle_xml_object_request(storage, req, &bucket, &object)
        } else {
            Self::handle_xml_bucket_request(storage, req, &bucket)
        }
    }

    fn handle_xml_root_request(
        storage: &Arc<dyn Storage>,
        req: &Request,
    ) -> Result<Response<Body>, String> {
        if req.method() != Method::GET {
            return Ok(Self::error_response(
                StatusCode::BAD_REQUEST,
                "InvalidURI",
                "Missing bucket",
            ));
        }

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
        Ok(Self::xml_response(StatusCode::OK, body))
    }

    fn handle_xml_bucket_request(
        storage: &Arc<dyn Storage>,
        req: &Request,
        bucket: &str,
    ) -> Result<Response<Body>, String> {
        match *req.method() {
            Method::PUT => {
                storage
                    .as_ref()
                    .create_namespace(bucket.to_string())
                    .map_err(|err| err.to_string())?;
                Ok(Self::empty_response(StatusCode::OK))
            }
            Method::DELETE => {
                storage
                    .as_ref()
                    .delete_namespace(bucket)
                    .map_err(|err| err.to_string())?;
                Ok(Self::empty_response(StatusCode::NO_CONTENT))
            }
            Method::GET => Self::list_xml_bucket(storage, req, bucket),
            _ => Ok(Self::unsupported_xml_operation()),
        }
    }

    fn list_xml_bucket(
        storage: &Arc<dyn Storage>,
        req: &Request,
        bucket: &str,
    ) -> Result<Response<Body>, String> {
        let blobs = storage
            .as_ref()
            .list_blobs(
                bucket,
                req.query_param("prefix"),
                req.query_param("delimiter"),
                None,
                None,
            )
            .map_err(|err| err.to_string())?;
        let mut body = String::with_capacity(128 + blobs.len() * 128);
        body.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?><ListBucketResult><Name>");
        push_escaped_xml(&mut body, bucket);
        body.push_str("</Name>");
        for blob in blobs {
            Self::append_xml_bucket_item(&mut body, &blob);
        }
        body.push_str("</ListBucketResult>");
        Ok(Self::xml_response(StatusCode::OK, body))
    }

    fn append_xml_bucket_item(body: &mut String, blob: &crate::blob::BlobRecord) {
        let generation = blob.version_id.as_deref().map_or_else(
            || blob.last_modified.timestamp_millis().max(1).to_string(),
            str::to_string,
        );
        body.push_str("<Contents><Key>");
        push_escaped_xml(body, &blob.key);
        body.push_str("</Key><Size>");
        write!(body, "{}", blob.size).unwrap();
        body.push_str("</Size><ETag>");
        push_escaped_xml(body, &blob.etag);
        body.push_str("</ETag><Generation>");
        push_escaped_xml(body, &generation);
        body.push_str("</Generation></Contents>");
    }

    fn handle_xml_object_request(
        storage: &Arc<dyn Storage>,
        req: &Request,
        bucket: &str,
        object: &str,
    ) -> Result<Response<Body>, String> {
        match *req.method() {
            Method::PUT => Self::put_xml_object(storage, req, bucket, object),
            Method::GET => Self::object_media_response(storage, req, bucket, object),
            Method::HEAD => Self::object_head_response(storage, bucket, object),
            Method::DELETE => {
                storage
                    .as_ref()
                    .delete_blob(bucket, object)
                    .map_err(|err| err.to_string())?;
                Ok(Self::empty_response(StatusCode::NO_CONTENT))
            }
            _ => Ok(Self::unsupported_xml_operation()),
        }
    }

    fn put_xml_object(
        storage: &Arc<dyn Storage>,
        req: &Request,
        bucket: &str,
        object: &str,
    ) -> Result<Response<Body>, String> {
        let stored = Self::put_blob_with_generation(
            storage.as_ref(),
            bucket.to_string(),
            object.to_string(),
            req.body.to_vec(),
            req.header("content-type")
                .unwrap_or("application/octet-stream")
                .to_string(),
            Self::metadata_from_headers(req),
        )?;
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

    fn object_media_response(
        storage: &Arc<dyn Storage>,
        req: &Request,
        bucket: &str,
        object: &str,
    ) -> Result<Response<Body>, String> {
        let blob = storage
            .as_ref()
            .get_blob(bucket, object)
            .map_err(|err| err.to_string())?;
        Self::object_media_response_for_blob(storage, req, bucket, object, blob)
    }

    fn object_media_response_for_blob(
        storage: &Arc<dyn Storage>,
        req: &Request,
        bucket: &str,
        object: &str,
        blob: crate::models::Object,
    ) -> Result<Response<Body>, String> {
        if let Some(range_header) = req.header("range") {
            return Self::object_range_response(storage, bucket, object, &blob, range_header);
        }
        let body_len = Self::response_body_len(blob.size)?;
        Ok(Self::object_response(StatusCode::OK, &blob, body_len, None)
            .body(blob.data)
            .build())
    }

    fn object_head_response(
        storage: &Arc<dyn Storage>,
        bucket: &str,
        object: &str,
    ) -> Result<Response<Body>, String> {
        let blob = storage
            .as_ref()
            .get_blob(bucket, object)
            .map_err(|err| err.to_string())?;
        let body_len = Self::response_body_len(blob.size)?;
        Ok(Self::object_response(StatusCode::OK, &blob, body_len, None).empty())
    }

    fn object_range_response(
        storage: &Arc<dyn Storage>,
        bucket: &str,
        object: &str,
        blob: &crate::models::Object,
        range_header: &str,
    ) -> Result<Response<Body>, String> {
        if let Some((start, end)) = Self::parse_range_header(range_header, blob.size) {
            let payload = storage
                .as_ref()
                .get_blob_range(
                    bucket,
                    object,
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
                Some(format!("bytes {start}-{end}/{}", blob.size)),
            )
            .body(payload.data)
            .build());
        }
        Ok(Self::error_response(
            StatusCode::RANGE_NOT_SATISFIABLE,
            "InvalidRange",
            "The requested range is not satisfiable",
        ))
    }

    fn unsupported_xml_operation() -> Response<Body> {
        Self::error_response(
            StatusCode::METHOD_NOT_ALLOWED,
            "UnsupportedHttpVerb",
            "Unsupported GCS operation",
        )
    }

    fn handle_resumable(
        &self,
        storage: &Arc<dyn Storage>,
        auth_config: &Arc<AuthConfig>,
        req: &Request,
    ) -> Result<Response<Body>, String> {
        let parts: Vec<&str> = req
            .path()
            .trim_start_matches('/')
            .split('/')
            .filter(|segment| !segment.is_empty())
            .collect();

        if parts.starts_with(&["upload", "storage", "v1", "b"]) && parts.len() >= 6 {
            return self.handle_resumable_start(storage, auth_config, req, parts[4]);
        }

        if parts.starts_with(&["upload", "resumable"]) && parts.len() == 3 {
            return self.complete_resumable_upload(storage, req, parts[2]);
        }

        Ok(Self::error_response(
            StatusCode::BAD_REQUEST,
            "InvalidURI",
            "Unsupported resumable upload path",
        ))
    }

    fn handle_resumable_start(
        &self,
        storage: &Arc<dyn Storage>,
        auth_config: &Arc<AuthConfig>,
        req: &Request,
        bucket: &str,
    ) -> Result<Response<Body>, String> {
        if let Err(response) = Self::authorize(req, auth_config, bucket, None) {
            return Ok(response);
        }
        if req.query_param("uploadType") == Some("multipart") {
            return Self::handle_multipart_upload(storage, req, bucket);
        }
        self.create_resumable_session(storage, req, bucket)
    }

    fn handle_multipart_upload(
        storage: &Arc<dyn Storage>,
        req: &Request,
        bucket: &str,
    ) -> Result<Response<Body>, String> {
        let content_type = req.header("content-type").unwrap_or("multipart/related");
        let (key, object_content_type, metadata, data) =
            Self::parse_multipart_upload(content_type, &req.body)?;
        let stored = Self::put_blob_with_generation(
            storage.as_ref(),
            bucket.to_string(),
            key,
            data,
            object_content_type,
            metadata,
        )?;
        Ok(Self::gcs_object_json_response(StatusCode::OK, &stored))
    }

    fn create_resumable_session(
        &self,
        storage: &Arc<dyn Storage>,
        req: &Request,
        bucket: &str,
    ) -> Result<Response<Body>, String> {
        let key = req
            .query_param("name")
            .ok_or_else(|| "Missing resumable upload object name".to_string())?
            .to_string();
        let session_id = uuid::Uuid::new_v4().to_string();
        let session = ResumableSession {
            bucket: bucket.to_string(),
            key,
            content_type: req
                .header("x-upload-content-type")
                .unwrap_or("application/octet-stream")
                .to_string(),
            metadata: Self::metadata_from_headers(req),
        };
        state::save_json(
            storage.as_ref(),
            GCS_RESUMABLE_SESSION_STATE,
            &session_id,
            &session,
        )?;
        self.resumable_sessions
            .lock()
            .map_err(|_| "Failed to lock resumable sessions".to_string())?
            .insert(session_id.clone(), session);
        let upload_location = format!("{}/upload/resumable/{}", request_origin(req), session_id);
        Ok(Self::response(StatusCode::OK)
            .header("location", &upload_location)
            .empty())
    }

    fn complete_resumable_upload(
        &self,
        storage: &Arc<dyn Storage>,
        req: &Request,
        session_id: &str,
    ) -> Result<Response<Body>, String> {
        let session = self.take_resumable_session(storage, session_id)?;
        let stored = Self::put_blob_with_generation(
            storage.as_ref(),
            session.bucket,
            session.key,
            req.body.to_vec(),
            session.content_type,
            session.metadata,
        )?;
        storage
            .delete_provider_state(GCS_RESUMABLE_SESSION_STATE, session_id)
            .map_err(|err| err.to_string())?;
        Ok(Self::gcs_object_json_response(StatusCode::OK, &stored))
    }

    fn take_resumable_session(
        &self,
        storage: &Arc<dyn Storage>,
        session_id: &str,
    ) -> Result<ResumableSession, String> {
        {
            let mut sessions = self
                .resumable_sessions
                .lock()
                .map_err(|_| "Failed to lock resumable sessions".to_string())?;
            sessions.remove(session_id)
        }
        .or(state::load_json(
            storage.as_ref(),
            GCS_RESUMABLE_SESSION_STATE,
            session_id,
        )?)
        .ok_or_else(|| "Unknown resumable upload session".to_string())
    }

    fn gcs_object_json_response(
        status: StatusCode,
        stored: &crate::blob::BlobRecord,
    ) -> Response<Body> {
        Self::json_response(
            status,
            &serde_json::json!({
                "kind": "storage#object",
                "name": stored.key,
                "etag": stored.etag,
                "generation": Self::generation_from_metadata(&stored.metadata),
                "metageneration": Self::metageneration_from_metadata(&stored.metadata),
            })
            .to_string(),
        )
    }

    fn handle_json_api(
        storage: &Arc<dyn Storage>,
        auth_config: &Arc<AuthConfig>,
        req: &Request,
    ) -> Result<Response<Body>, String> {
        let parts: Vec<&str> = req
            .path()
            .trim_start_matches('/')
            .split('/')
            .filter(|segment| !segment.is_empty())
            .collect();

        if parts.starts_with(&["storage", "v1", "b"]) {
            return Self::handle_json_bucket_api(storage, auth_config, req, &parts);
        }

        if parts.starts_with(&["download", "storage", "v1", "b"]) && parts.get(5) == Some(&"o") {
            return Self::handle_json_download(storage, auth_config, req, &parts);
        }

        Ok(Self::error_response(
            StatusCode::BAD_REQUEST,
            "InvalidURI",
            "Unsupported GCS JSON API path",
        ))
    }

    fn handle_json_bucket_api(
        storage: &Arc<dyn Storage>,
        auth_config: &Arc<AuthConfig>,
        req: &Request,
        parts: &[&str],
    ) -> Result<Response<Body>, String> {
        if parts.len() == 3 {
            return Self::handle_json_bucket_collection(storage, req);
        }

        let bucket = parts.get(3).copied().unwrap_or_default();
        if let Err(response) = Self::authorize(req, auth_config, bucket, None) {
            return Ok(response);
        }

        if parts.len() == 4 {
            return Self::handle_json_bucket_item(storage, req, bucket);
        }
        if parts.get(4) == Some(&"o") {
            return Self::handle_json_object_api(storage, auth_config, req, parts, bucket);
        }
        Ok(Self::error_response(
            StatusCode::BAD_REQUEST,
            "InvalidURI",
            "Unsupported GCS JSON API path",
        ))
    }

    fn handle_json_bucket_collection(
        storage: &Arc<dyn Storage>,
        req: &Request,
    ) -> Result<Response<Body>, String> {
        match *req.method() {
            Method::GET => Self::list_json_buckets(storage),
            Method::POST => Self::create_json_bucket(storage, req),
            _ => Ok(Self::error_response(
                StatusCode::METHOD_NOT_ALLOWED,
                "UnsupportedHttpVerb",
                "Unsupported GCS JSON API bucket collection operation",
            )),
        }
    }

    fn list_json_buckets(storage: &Arc<dyn Storage>) -> Result<Response<Body>, String> {
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

    fn create_json_bucket(
        storage: &Arc<dyn Storage>,
        req: &Request,
    ) -> Result<Response<Body>, String> {
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
        Ok(Self::json_bucket_response(StatusCode::OK, &namespace))
    }

    fn handle_json_bucket_item(
        storage: &Arc<dyn Storage>,
        req: &Request,
        bucket: &str,
    ) -> Result<Response<Body>, String> {
        match *req.method() {
            Method::GET => {
                let namespace = storage
                    .as_ref()
                    .get_namespace(bucket)
                    .map_err(|err| err.to_string())?;
                Ok(Self::json_bucket_response(StatusCode::OK, &namespace))
            }
            Method::DELETE => {
                storage
                    .as_ref()
                    .delete_namespace(bucket)
                    .map_err(|err| err.to_string())?;
                Ok(Self::empty_response(StatusCode::NO_CONTENT))
            }
            _ => Ok(Self::error_response(
                StatusCode::METHOD_NOT_ALLOWED,
                "UnsupportedHttpVerb",
                "Unsupported GCS JSON API bucket operation",
            )),
        }
    }

    fn json_bucket_response(
        status: StatusCode,
        namespace: &crate::blob::Namespace,
    ) -> Response<Body> {
        Self::json_response(
            status,
            &serde_json::json!({
                "kind": "storage#bucket",
                "name": namespace.name,
                "timeCreated": namespace.created_at.to_rfc3339(),
            })
            .to_string(),
        )
    }

    fn handle_json_object_api(
        storage: &Arc<dyn Storage>,
        auth_config: &Arc<AuthConfig>,
        req: &Request,
        parts: &[&str],
        bucket: &str,
    ) -> Result<Response<Body>, String> {
        if parts.len() == 5 {
            return Self::list_json_objects(storage, req, bucket);
        }

        let object = Self::decode_object_path(&parts[5..].join("/"))?;
        if let Err(response) = Self::authorize(req, auth_config, bucket, Some(&object)) {
            return Ok(response);
        }
        let alt_media = req.query_param("alt") == Some("media");
        Self::handle_json_object_item(storage, req, bucket, &object, alt_media)
    }

    fn list_json_objects(
        storage: &Arc<dyn Storage>,
        req: &Request,
        bucket: &str,
    ) -> Result<Response<Body>, String> {
        let blobs = storage
            .as_ref()
            .list_blobs(
                bucket,
                req.query_param("prefix"),
                req.query_param("delimiter"),
                None,
                None,
            )
            .map_err(|err| err.to_string())?;
        Ok(Self::json_response(
            StatusCode::OK,
            &serde_json::json!({
                "kind": "storage#objects",
                "items": blobs.into_iter().map(|blob| {
                    Self::json_blob_record_metadata(bucket, &blob)
                }).collect::<Vec<_>>()
            })
            .to_string(),
        ))
    }

    fn handle_json_object_item(
        storage: &Arc<dyn Storage>,
        req: &Request,
        bucket: &str,
        object: &str,
        alt_media: bool,
    ) -> Result<Response<Body>, String> {
        match *req.method() {
            Method::GET => Self::get_json_object(storage, req, bucket, object, alt_media),
            Method::PATCH => Self::patch_json_object(storage, req, bucket, object),
            Method::DELETE => Self::delete_json_object(storage, req, bucket, object),
            _ => Ok(Self::error_response(
                StatusCode::METHOD_NOT_ALLOWED,
                "UnsupportedHttpVerb",
                "Unsupported GCS JSON API object operation",
            )),
        }
    }

    fn get_json_object(
        storage: &Arc<dyn Storage>,
        req: &Request,
        bucket: &str,
        object: &str,
        alt_media: bool,
    ) -> Result<Response<Body>, String> {
        let blob = match Self::checked_json_blob(storage, req, bucket, object)? {
            Ok(blob) => blob,
            Err(response) => return Ok(*response),
        };
        if alt_media {
            return Self::object_media_response_for_blob(storage, req, bucket, object, blob);
        }
        Ok(Self::json_response(
            StatusCode::OK,
            &Self::json_object_metadata(bucket, &blob).to_string(),
        ))
    }

    fn patch_json_object(
        storage: &Arc<dyn Storage>,
        req: &Request,
        bucket: &str,
        object: &str,
    ) -> Result<Response<Body>, String> {
        let blob = match Self::checked_json_blob(storage, req, bucket, object)? {
            Ok(blob) => blob,
            Err(response) => return Ok(*response),
        };
        let payload: serde_json::Value =
            serde_json::from_slice(&req.body).map_err(|err| err.to_string())?;
        let updated = storage
            .as_ref()
            .update_blob_metadata(UpdateBlobMetadataRequest {
                namespace: bucket.to_string(),
                key: object.to_string(),
                metadata: Self::metadata_patch_with_gcs_state(&payload, &blob),
            })
            .map_err(|err| err.to_string())?;
        Ok(Self::json_response(
            StatusCode::OK,
            &Self::json_blob_record_metadata(bucket, &updated).to_string(),
        ))
    }

    fn delete_json_object(
        storage: &Arc<dyn Storage>,
        req: &Request,
        bucket: &str,
        object: &str,
    ) -> Result<Response<Body>, String> {
        if let Err(response) = Self::checked_json_blob(storage, req, bucket, object)? {
            return Ok(*response);
        }
        storage
            .as_ref()
            .delete_blob(bucket, object)
            .map_err(|err| err.to_string())?;
        Ok(Self::empty_response(StatusCode::NO_CONTENT))
    }

    fn checked_json_blob(
        storage: &Arc<dyn Storage>,
        req: &Request,
        bucket: &str,
        object: &str,
    ) -> Result<Result<crate::models::Object, Box<Response<Body>>>, String> {
        let blob = storage
            .as_ref()
            .get_blob(bucket, object)
            .map_err(|err| err.to_string())?;
        if let Err(response) = Self::check_gcs_preconditions(req, &blob) {
            return Ok(Err(Box::new(response)));
        }
        Ok(Ok(blob))
    }

    fn metadata_patch_with_gcs_state(
        payload: &serde_json::Value,
        blob: &crate::models::Object,
    ) -> HashMap<String, String> {
        let metadata = payload
            .get("metadata")
            .and_then(|value| value.as_object())
            .map_or_else(
                || Self::public_metadata(&blob.metadata),
                |map| {
                    map.iter()
                        .filter_map(|(key, value)| {
                            value.as_str().map(|value| (key.clone(), value.to_string()))
                        })
                        .collect::<HashMap<_, _>>()
                },
            );
        Self::metadata_with_gcs_state(
            metadata,
            Self::generation(blob),
            blob.metadata
                .get(GCS_METAGENERATION_KEY)
                .and_then(|value| value.parse::<u64>().ok())
                .unwrap_or(1)
                .saturating_add(1)
                .to_string(),
        )
    }

    fn json_object_metadata(bucket: &str, blob: &crate::models::Object) -> serde_json::Value {
        serde_json::json!({
            "name": blob.key,
            "bucket": bucket,
            "size": blob.size.to_string(),
            "etag": blob.etag,
            "generation": Self::generation(blob),
            "metageneration": Self::metageneration(blob),
            "metadata": Self::public_metadata(&blob.metadata),
        })
    }

    fn json_blob_record_metadata(
        bucket: &str,
        blob: &crate::blob::BlobRecord,
    ) -> serde_json::Value {
        serde_json::json!({
            "name": blob.key,
            "bucket": bucket,
            "size": blob.size.to_string(),
            "etag": blob.etag,
            "generation": Self::generation_from_metadata(&blob.metadata),
            "metageneration": Self::metageneration_from_metadata(&blob.metadata),
            "metadata": Self::public_metadata(&blob.metadata),
        })
    }

    fn handle_json_download(
        storage: &Arc<dyn Storage>,
        auth_config: &Arc<AuthConfig>,
        req: &Request,
        parts: &[&str],
    ) -> Result<Response<Body>, String> {
        let bucket = parts.get(4).copied().unwrap_or_default();
        let object = Self::decode_object_path(&parts[6..].join("/"))?;
        if let Err(response) = Self::authorize(req, auth_config, bucket, Some(&object)) {
            return Ok(response);
        }
        let blob = storage
            .as_ref()
            .get_blob(bucket, &object)
            .map_err(|err| err.to_string())?;
        if let Err(response) = Self::check_gcs_preconditions(req, &blob) {
            return Ok(response);
        }
        Self::object_media_response_for_blob(storage, req, bucket, &object, blob)
    }
}
