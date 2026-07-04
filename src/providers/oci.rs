use super::ProviderAdapter;
use crate::auth::{AuthConfig, HttpRequestLike};
use crate::blob::{BlobBackend, BlobRange, CreateUploadSessionRequest, PutBlobRequest};
use crate::body::Body;
use crate::server::{RequestExt as Request, ResponseBuilder};
use crate::storage::Storage;
use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use hmac::{Hmac, KeyInit, Mac};
use http::{HeaderMap, Method, StatusCode, Uri};
use hyper::Response;
use sha2::Sha256;
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

pub struct OciAdapter;

impl Default for OciAdapter {
    fn default() -> Self {
        Self::new()
    }
}

impl OciAdapter {
    #[must_use]
    pub fn new() -> Self {
        Self
    }

    fn request_target(req: &Request) -> String {
        if let Some(query) = req.query() {
            format!("{}?{}", req.path(), query)
        } else {
            req.path().to_string()
        }
    }

    fn response(status: StatusCode) -> ResponseBuilder {
        ResponseBuilder::new(status)
            .header("opc-request-id", &uuid::Uuid::new_v4().to_string())
            .header("date", &crate::utils::headers::format_last_modified())
    }

    fn matches_head(uri: &Uri, headers: &HeaderMap) -> bool {
        let authorization = headers
            .get("authorization")
            .and_then(|value| value.to_str().ok())
            .unwrap_or("");

        uri.path().starts_with("/n/") || authorization.starts_with("Signature ")
    }

    fn payload_too_large_response(max_request_bytes: usize) -> Response<Body> {
        let message =
            format!("Request body exceeds SQRZL_MAX_REQUEST_BYTES ({max_request_bytes} bytes)");
        let body = serde_json::json!({
            "code": "PayloadTooLarge",
            "message": message,
        });
        Self::json_response(StatusCode::PAYLOAD_TOO_LARGE, &body.to_string())
    }

    fn json_response(status: StatusCode, body: &str) -> Response<Body> {
        Self::response(status)
            .content_type("application/json")
            .body(body.as_bytes().to_vec())
            .build()
    }

    fn text_response(status: StatusCode, body: &str) -> Response<Body> {
        Self::response(status)
            .content_type("text/plain; charset=utf-8")
            .body(body.as_bytes().to_vec())
            .build()
    }

    fn error_response(status: StatusCode, code: &str, message: &str) -> Response<Body> {
        Self::json_response(
            status,
            &format!("{{\"code\":\"{code}\",\"message\":\"{message}\"}}"),
        )
    }

    fn parse_path(req: &Request) -> Result<(String, Vec<String>), String> {
        let parts: Vec<&str> = req
            .path()
            .trim_start_matches('/')
            .split('/')
            .filter(|segment| !segment.is_empty())
            .collect();
        if parts.is_empty() || parts[0] != "n" {
            return Err("OCI requests must start with /n".to_string());
        }
        Ok((
            parts
                .get(1)
                .copied()
                .unwrap_or("sqrzl-emulator")
                .to_string(),
            parts
                .iter()
                .skip(2)
                .map(|segment| (*segment).to_string())
                .collect(),
        ))
    }

    fn signing_string(req: &Request) -> String {
        format!(
            "date: {}\n(request-target): {} {}\nhost: {}",
            req.header("date").unwrap_or(""),
            req.method().as_str().to_lowercase(),
            Self::request_target(req),
            req.host().unwrap_or("localhost")
        )
    }

    fn metadata_from_headers(req: &Request) -> HashMap<String, String> {
        req.headers()
            .into_iter()
            .filter_map(|(name, value)| {
                name.strip_prefix("opc-meta-")
                    .map(|key| (key.to_string(), value))
            })
            .collect()
    }

    fn metadata_from_json(value: Option<&serde_json::Value>) -> HashMap<String, String> {
        value
            .and_then(|value| value.as_object())
            .map(|map| {
                map.iter()
                    .filter_map(|(key, value)| {
                        value.as_str().map(|value| (key.clone(), value.to_string()))
                    })
                    .collect()
            })
            .unwrap_or_default()
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

    fn decode_object_path(path: &str) -> Result<String, String> {
        urlencoding::decode(path)
            .map(std::borrow::Cow::into_owned)
            .map_err(|err| format!("Invalid encoded OCI object path: {err}"))
    }

    fn object_response(status: StatusCode, blob: &crate::models::Object) -> ResponseBuilder {
        let mut builder = Self::response(status)
            .header("accept-ranges", "bytes")
            .header("content-length", &blob.size.to_string())
            .header("content-type", &blob.content_type)
            .header("etag", &blob.etag)
            .header("last-modified", &blob.last_modified.to_rfc2822());
        for (key, value) in &blob.metadata {
            builder = builder.header(&format!("opc-meta-{key}"), value);
        }
        builder
    }

    #[allow(clippy::result_large_err)]
    fn authorize(req: &Request, config: &AuthConfig) -> Result<(), Response<Body>> {
        type HmacSha256 = Hmac<Sha256>;

        if !config.enforce_auth {
            return Ok(());
        }

        let Some(auth) = req.header("authorization") else {
            return Err(Self::error_response(
                StatusCode::UNAUTHORIZED,
                "NotAuthenticated",
                "Missing authorization",
            ));
        };
        if !auth.starts_with("Signature ") {
            return Err(Self::error_response(
                StatusCode::UNAUTHORIZED,
                "NotAuthenticated",
                "Unsupported OCI auth scheme",
            ));
        }
        let signature = auth
            .split(',')
            .find_map(|part| {
                part.trim()
                    .strip_prefix("signature=\"")
                    .map(|value| value.trim_end_matches('"').to_string())
            })
            .ok_or_else(|| {
                Self::error_response(
                    StatusCode::UNAUTHORIZED,
                    "NotAuthenticated",
                    "Missing OCI signature",
                )
            })?;
        let key_id = auth
            .split(',')
            .find_map(|part| {
                part.trim()
                    .strip_prefix("Signature keyId=\"")
                    .or_else(|| part.trim().strip_prefix("keyId=\""))
                    .map(|value| value.trim_end_matches('"').to_string())
            })
            .unwrap_or_default();

        if config.access_key() != Some(key_id.as_str()) {
            return Err(Self::error_response(
                StatusCode::UNAUTHORIZED,
                "NotAuthenticated",
                "Invalid OCI keyId",
            ));
        }

        let secret = config.secret_key().unwrap_or_default().as_bytes().to_vec();
        let mut mac = HmacSha256::new_from_slice(&secret).map_err(|_| {
            Self::error_response(
                StatusCode::UNAUTHORIZED,
                "NotAuthenticated",
                "Invalid OCI key",
            )
        })?;
        mac.update(Self::signing_string(req).as_bytes());
        let expected = BASE64.encode(mac.finalize().into_bytes());
        if expected == signature {
            Ok(())
        } else {
            Err(Self::error_response(
                StatusCode::UNAUTHORIZED,
                "NotAuthenticated",
                "OCI signature mismatch",
            ))
        }
    }

    fn handle_request(
        &self,
        storage: &Arc<dyn Storage>,
        auth_config: &Arc<AuthConfig>,
        req: &Request,
    ) -> Result<Response<Body>, String> {
        let _ = self.name();
        let (namespace, parts) = match Self::parse_path(req) {
            Ok(parsed) => parsed,
            Err(msg) => {
                return Ok(Self::error_response(
                    StatusCode::BAD_REQUEST,
                    "InvalidParameter",
                    &msg,
                ))
            }
        };

        if let Err(response) = Self::authorize(req, auth_config) {
            return Ok(response);
        }

        if parts.is_empty() {
            return Ok(Self::handle_namespace_request(req, &namespace));
        }

        if parts[0] == "b" && parts.len() == 1 {
            return Self::handle_bucket_collection(storage, req, &namespace);
        }

        if parts.len() == 2 && parts[0] == "b" {
            return Self::handle_bucket_request(storage, req, &namespace, &parts[1]);
        }

        if parts.len() >= 3 && parts[0] == "b" && parts[2] == "u" {
            return Self::handle_multipart_request(storage, req, &namespace, &parts);
        }

        if parts.len() >= 3 && parts[0] == "b" && parts[2] == "o" {
            return Self::handle_object_request(storage, req, &parts);
        }

        Ok(Self::error_response(
            StatusCode::BAD_REQUEST,
            "InvalidParameter",
            "Unsupported OCI path",
        ))
    }

    fn handle_namespace_request(req: &Request, namespace: &str) -> Response<Body> {
        if req.method() == Method::GET {
            return Self::text_response(StatusCode::OK, namespace);
        }
        Self::error_response(
            StatusCode::METHOD_NOT_ALLOWED,
            "MethodNotAllowed",
            "Unsupported OCI namespace operation",
        )
    }

    fn handle_bucket_collection(
        storage: &Arc<dyn Storage>,
        req: &Request,
        namespace: &str,
    ) -> Result<Response<Body>, String> {
        if req.method() != Method::POST {
            return Ok(Self::error_response(
                StatusCode::METHOD_NOT_ALLOWED,
                "MethodNotAllowed",
                "Unsupported OCI bucket collection operation",
            ));
        }
        let payload: serde_json::Value =
            serde_json::from_slice(&req.body).map_err(|err| err.to_string())?;
        let bucket = payload
            .get("name")
            .and_then(|value| value.as_str())
            .ok_or_else(|| "Missing OCI bucket name".to_string())?;
        storage
            .as_ref()
            .create_namespace(bucket.to_string())
            .map_err(|err| err.to_string())?;
        Ok(Self::json_response(
            StatusCode::OK,
            &format!("{{\"name\":\"{bucket}\",\"namespace\":\"{namespace}\"}}"),
        ))
    }

    fn handle_bucket_request(
        storage: &Arc<dyn Storage>,
        req: &Request,
        namespace: &str,
        bucket: &str,
    ) -> Result<Response<Body>, String> {
        match *req.method() {
            Method::PUT => {
                storage
                    .as_ref()
                    .create_namespace(bucket.to_string())
                    .map_err(|err| err.to_string())?;
                Ok(Self::json_response(
                    StatusCode::OK,
                    "{\"etag\":\"created\"}",
                ))
            }
            Method::DELETE => {
                storage
                    .as_ref()
                    .delete_namespace(bucket)
                    .map_err(|err| err.to_string())?;
                Ok(Self::json_response(StatusCode::NO_CONTENT, ""))
            }
            Method::GET => {
                storage
                    .as_ref()
                    .get_namespace(bucket)
                    .map_err(|err| err.to_string())?;
                Ok(Self::json_response(
                    StatusCode::OK,
                    &format!("{{\"name\":\"{bucket}\",\"namespace\":\"{namespace}\"}}"),
                ))
            }
            _ => Ok(Self::error_response(
                StatusCode::METHOD_NOT_ALLOWED,
                "MethodNotAllowed",
                "Unsupported OCI bucket operation",
            )),
        }
    }

    fn handle_multipart_request(
        storage: &Arc<dyn Storage>,
        req: &Request,
        namespace: &str,
        parts: &[String],
    ) -> Result<Response<Body>, String> {
        let bucket = parts[1].as_str();
        if parts.len() == 3 {
            return Self::handle_multipart_collection(storage, req, namespace, bucket);
        }

        let object = Self::decode_object_path(&parts[3..].join("/"))?;
        let upload_id = req
            .query_param("uploadId")
            .ok_or_else(|| "Missing uploadId query parameter".to_string())?;
        match *req.method() {
            Method::PUT => Self::upload_multipart_part(storage, req, bucket, upload_id),
            Method::POST => Self::commit_multipart_upload(storage, req, bucket, &object, upload_id),
            Method::DELETE => {
                storage
                    .abort_multipart_upload(bucket, upload_id)
                    .map_err(|err| err.to_string())?;
                Ok(Self::response(StatusCode::NO_CONTENT).empty())
            }
            _ => Ok(Self::error_response(
                StatusCode::METHOD_NOT_ALLOWED,
                "MethodNotAllowed",
                "Unsupported OCI multipart operation",
            )),
        }
    }

    fn handle_multipart_collection(
        storage: &Arc<dyn Storage>,
        req: &Request,
        namespace: &str,
        bucket: &str,
    ) -> Result<Response<Body>, String> {
        if req.method() != Method::POST {
            return Ok(Self::error_response(
                StatusCode::METHOD_NOT_ALLOWED,
                "MethodNotAllowed",
                "Unsupported OCI multipart collection operation",
            ));
        }
        let payload: serde_json::Value =
            serde_json::from_slice(&req.body).map_err(|err| err.to_string())?;
        let upload = Self::create_multipart_session(storage, bucket, &payload)?;
        let storage_tier = payload
            .get("storageTier")
            .and_then(|value| value.as_str())
            .unwrap_or("Standard");
        Ok(Self::json_response(
            StatusCode::OK,
            &serde_json::json!({
                "namespace": namespace,
                "bucket": bucket,
                "object": upload.key,
                "uploadId": upload.upload_id,
                "timeCreated": upload.initiated.to_rfc3339(),
                "storageTier": storage_tier,
            })
            .to_string(),
        ))
    }

    fn create_multipart_session(
        storage: &Arc<dyn Storage>,
        bucket: &str,
        payload: &serde_json::Value,
    ) -> Result<crate::models::MultipartUpload, String> {
        let object = payload
            .get("object")
            .and_then(|value| value.as_str())
            .ok_or_else(|| "Missing OCI multipart object name".to_string())?;
        let content_type = payload
            .get("contentType")
            .and_then(|value| value.as_str())
            .map(std::string::ToString::to_string);
        let metadata = Self::metadata_from_json(payload.get("metadata"));
        let storage_tier = payload
            .get("storageTier")
            .and_then(|value| value.as_str())
            .unwrap_or("Standard")
            .to_string();
        storage
            .as_ref()
            .create_upload_session(CreateUploadSessionRequest {
                namespace: bucket.to_string(),
                key: object.to_string(),
                content_type,
                metadata,
                provider_metadata: HashMap::from([
                    ("storage_tier".to_string(), storage_tier.clone()),
                    ("storage_class".to_string(), storage_tier),
                ]),
            })
            .map_err(|err| err.to_string())
    }

    fn upload_multipart_part(
        storage: &Arc<dyn Storage>,
        req: &Request,
        bucket: &str,
        upload_id: &str,
    ) -> Result<Response<Body>, String> {
        let part_number = req
            .query_param("uploadPartNum")
            .ok_or_else(|| "Missing uploadPartNum query parameter".to_string())?
            .parse::<u32>()
            .map_err(|_| "Invalid uploadPartNum query parameter".to_string())?;
        let etag = storage
            .as_ref()
            .upload_session_part(bucket, upload_id, part_number, req.body.to_vec())
            .map_err(|err| err.to_string())?;
        Ok(Self::response(StatusCode::OK).header("etag", &etag).empty())
    }

    fn commit_multipart_upload(
        storage: &Arc<dyn Storage>,
        req: &Request,
        bucket: &str,
        object: &str,
        upload_id: &str,
    ) -> Result<Response<Body>, String> {
        let payload: serde_json::Value =
            serde_json::from_slice(&req.body).map_err(|err| err.to_string())?;
        let upload = storage
            .get_multipart_upload(bucket, upload_id)
            .map_err(|err| err.to_string())?;
        if upload.key != object {
            return Ok(Self::error_response(
                StatusCode::BAD_REQUEST,
                "InvalidParameter",
                "Multipart upload object did not match upload session",
            ));
        }
        if let Some(response) = Self::validate_parts_to_commit(&payload, &upload)? {
            return Ok(response);
        }
        let etag = storage
            .as_ref()
            .complete_upload_session(bucket, upload_id)
            .map_err(|err| err.to_string())?;
        Ok(Self::response(StatusCode::OK).header("etag", &etag).empty())
    }

    fn validate_parts_to_commit(
        payload: &serde_json::Value,
        upload: &crate::models::MultipartUpload,
    ) -> Result<Option<Response<Body>>, String> {
        let Some(parts_to_commit) = payload
            .get("partsToCommit")
            .and_then(serde_json::Value::as_array)
        else {
            return Ok(None);
        };
        for part in parts_to_commit {
            let part_num = Self::part_num_from_json(part)?;
            let etag = part
                .get("etag")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| "Missing etag in partsToCommit".to_string())?;
            let stored_part = upload
                .parts
                .iter()
                .find(|stored| stored.part_number == part_num)
                .ok_or_else(|| format!("Missing uploaded part {part_num}"))?;
            if stored_part.etag != etag {
                return Ok(Some(Self::error_response(
                    StatusCode::BAD_REQUEST,
                    "InvalidPart",
                    "Multipart commit etag did not match uploaded part",
                )));
            }
        }
        Ok(None)
    }

    fn part_num_from_json(part: &serde_json::Value) -> Result<u32, String> {
        part.get("partNum")
            .and_then(serde_json::Value::as_u64)
            .ok_or_else(|| "Missing partNum in partsToCommit".to_string())
            .and_then(|value| {
                u32::try_from(value)
                    .map_err(|_| "partNum in partsToCommit is too large".to_string())
            })
    }

    fn handle_object_request(
        storage: &Arc<dyn Storage>,
        req: &Request,
        parts: &[String],
    ) -> Result<Response<Body>, String> {
        let bucket = parts[1].as_str();
        if parts.len() == 3 {
            return Self::list_objects(storage, req, bucket);
        }

        let object = Self::decode_object_path(&parts[3..].join("/"))?;
        match *req.method() {
            Method::PUT => Self::put_object(storage, req, bucket, &object),
            Method::GET => Self::get_object(storage, req, bucket, &object),
            Method::HEAD => Self::head_object(storage, bucket, &object),
            Method::DELETE => {
                storage
                    .as_ref()
                    .delete_blob(bucket, &object)
                    .map_err(|err| err.to_string())?;
                Ok(Self::json_response(StatusCode::NO_CONTENT, ""))
            }
            _ => Ok(Self::error_response(
                StatusCode::METHOD_NOT_ALLOWED,
                "MethodNotAllowed",
                "Unsupported OCI object operation",
            )),
        }
    }

    fn list_objects(
        storage: &Arc<dyn Storage>,
        req: &Request,
        bucket: &str,
    ) -> Result<Response<Body>, String> {
        if req.method() != Method::GET {
            return Ok(Self::error_response(
                StatusCode::METHOD_NOT_ALLOWED,
                "MethodNotAllowed",
                "Unsupported OCI object list operation",
            ));
        }
        let objects = storage
            .as_ref()
            .list_blobs(bucket, req.query_param("prefix"), None, None, None)
            .map_err(|err| err.to_string())?;
        let items = objects
            .iter()
            .map(|blob| {
                format!(
                    "{{\"name\":\"{}\",\"size\":{},\"etag\":\"{}\",\"timeCreated\":\"{}\"}}",
                    blob.key,
                    blob.size,
                    blob.etag,
                    blob.last_modified.to_rfc3339()
                )
            })
            .collect::<Vec<_>>()
            .join(",");
        Ok(Self::json_response(
            StatusCode::OK,
            &format!("{{\"objects\":[{items}]}}"),
        ))
    }

    fn put_object(
        storage: &Arc<dyn Storage>,
        req: &Request,
        bucket: &str,
        object: &str,
    ) -> Result<Response<Body>, String> {
        let stored = storage
            .as_ref()
            .put_blob(PutBlobRequest {
                namespace: bucket.to_string(),
                key: object.to_string(),
                data: req.body.to_vec(),
                content_type: req
                    .header("content-type")
                    .unwrap_or("application/octet-stream")
                    .to_string(),
                metadata: Self::metadata_from_headers(req),
                tags: HashMap::new(),
            })
            .map_err(|err| err.to_string())?;
        Ok(Self::json_response(
            StatusCode::OK,
            &format!(
                "{{\"etag\":\"{}\",\"name\":\"{}\"}}",
                stored.etag, stored.key
            ),
        ))
    }

    fn get_object(
        storage: &Arc<dyn Storage>,
        req: &Request,
        bucket: &str,
        object: &str,
    ) -> Result<Response<Body>, String> {
        let blob = storage
            .as_ref()
            .get_blob(bucket, object)
            .map_err(|err| err.to_string())?;
        if let Some(range_header) = req.header("range") {
            return Self::object_range_response(storage, bucket, object, &blob, range_header);
        }
        Ok(Self::object_response(StatusCode::OK, &blob)
            .body(blob.data)
            .build())
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
            return Ok(
                Self::object_response(StatusCode::PARTIAL_CONTENT, &payload.blob)
                    .header("content-length", &payload.data.len().to_string())
                    .header(
                        "content-range",
                        &format!("bytes {start}-{end}/{}", blob.size),
                    )
                    .body(payload.data)
                    .build(),
            );
        }
        Ok(Self::error_response(
            StatusCode::RANGE_NOT_SATISFIABLE,
            "InvalidRange",
            "The requested range is not satisfiable",
        ))
    }

    fn head_object(
        storage: &Arc<dyn Storage>,
        bucket: &str,
        object: &str,
    ) -> Result<Response<Body>, String> {
        let blob = storage
            .as_ref()
            .get_blob(bucket, object)
            .map_err(|err| err.to_string())?;
        Ok(Self::object_response(StatusCode::OK, &blob).empty())
    }
}

impl ProviderAdapter for OciAdapter {
    fn name(&self) -> &'static str {
        "oci-object"
    }

    fn matches(&self, req: &Request) -> bool {
        req.path().starts_with("/n/")
            || req
                .header("authorization")
                .is_some_and(|value| value.starts_with("Signature "))
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::storage::FilesystemStorage;
    use http_body_util::BodyExt;
    use hyper::Request as HyperRequest;
    use std::fs;

    fn temp_storage() -> Arc<dyn Storage> {
        let dir = std::env::temp_dir().join(format!("sqrzl-oci-test-{}", uuid::Uuid::new_v4()));
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

    fn oci_auth() -> Arc<AuthConfig> {
        Arc::new(Config {
            access_key_id: Some("oci-key".to_string()),
            secret_access_key: Some("oci-secret".to_string()),
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

    fn authorization(req: &Request) -> String {
        type HmacSha256 = Hmac<Sha256>;
        let mut mac = HmacSha256::new_from_slice(b"oci-secret").expect("key");
        mac.update(OciAdapter::signing_string(req).as_bytes());
        let signature = BASE64.encode(mac.finalize().into_bytes());
        format!("Signature keyId=\"oci-key\",algorithm=\"hmac-sha256\",headers=\"date (request-target) host\",signature=\"{signature}\"")
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn should_support_oci_namespace_bucket_and_object_flows() {
        let adapter = OciAdapter::new();
        let storage = temp_storage();

        let response = adapter
            .handle_request(
                &storage.clone(),
                &auth_disabled(),
                &parsed_request("GET", "http://localhost/n/tenant", &[], b"").await,
            )
            .expect("namespace lookup should succeed");
        assert_eq!(response.status(), StatusCode::OK);

        adapter
            .handle_request(
                &storage.clone(),
                &auth_disabled(),
                &parsed_request(
                    "POST",
                    "http://localhost/n/tenant/b",
                    &[("content-type", "application/json")],
                    br#"{"name":"archive","compartmentId":"ignored"}"#,
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
                    "http://localhost/n/tenant/b/archive/o/report.txt",
                    &[("content-type", "text/plain")],
                    b"oci data",
                )
                .await,
            )
            .expect("object put should succeed");

        let response = adapter
            .handle_request(
                &storage.clone(),
                &auth_disabled(),
                &parsed_request("GET", "http://localhost/n/tenant/b/archive/o", &[], b"").await,
            )
            .expect("object list should succeed");
        let body = response
            .into_body()
            .collect()
            .await
            .expect("body should read")
            .to_bytes();
        assert!(String::from_utf8(body.to_vec())
            .expect("json")
            .contains("report.txt"));

        let response = adapter
            .handle_request(
                &storage,
                &auth_disabled(),
                &parsed_request(
                    "GET",
                    "http://localhost/n/tenant/b/archive/o/report.txt",
                    &[],
                    b"",
                )
                .await,
            )
            .expect("object get should succeed");
        let body = response
            .into_body()
            .collect()
            .await
            .expect("body should read")
            .to_bytes();
        assert_eq!(body.as_ref(), b"oci data");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn should_validate_oci_signature_authorization() {
        let adapter = OciAdapter::new();
        let storage = temp_storage();

        let mut request = parsed_request(
            "GET",
            "http://localhost/n/tenant",
            &[
                ("date", "Sat, 01 Jan 2024 00:00:00 +0000"),
                ("host", "objectstorage.localhost"),
            ],
            b"",
        )
        .await;
        let auth = authorization(&request);
        request
            .headers
            .insert("authorization", auth.parse().expect("header should parse"));

        let response = adapter
            .handle_request(&storage, &oci_auth(), &request)
            .expect("oci auth request should complete");
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn should_round_trip_oci_metadata_and_prefix_listing() {
        let adapter = OciAdapter::new();
        let storage = temp_storage();

        adapter
            .handle_request(
                &storage.clone(),
                &auth_disabled(),
                &parsed_request(
                    "POST",
                    "http://localhost/n/tenant/b",
                    &[("content-type", "application/json")],
                    br#"{"name":"archive","compartmentId":"ignored"}"#,
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
                    "http://localhost/n/tenant/b/archive/o/folder/report.txt",
                    &[("content-type", "text/plain"), ("opc-meta-owner", "casey")],
                    b"oci metadata",
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
                    "http://localhost/n/tenant/b/archive/o?prefix=folder/",
                    &[],
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
        let json = String::from_utf8(body.to_vec()).expect("json");
        assert!(json.contains("folder/report.txt"));
        assert!(json.contains("timeCreated"));

        let response = adapter
            .handle_request(
                &storage,
                &auth_disabled(),
                &parsed_request(
                    "HEAD",
                    "http://localhost/n/tenant/b/archive/o/folder/report.txt",
                    &[],
                    b"",
                )
                .await,
            )
            .expect("head should succeed");
        assert_eq!(
            response
                .headers()
                .get("opc-meta-owner")
                .and_then(|value| value.to_str().ok()),
            Some("casey")
        );
        assert_eq!(
            response
                .headers()
                .get("accept-ranges")
                .and_then(|value| value.to_str().ok()),
            Some("bytes")
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn should_support_oci_range_reads() {
        let adapter = OciAdapter::new();
        let storage = temp_storage();

        adapter
            .handle_request(
                &storage.clone(),
                &auth_disabled(),
                &parsed_request(
                    "POST",
                    "http://localhost/n/tenant/b",
                    &[("content-type", "application/json")],
                    br#"{"name":"range-bucket","compartmentId":"ignored"}"#,
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
                    "http://localhost/n/tenant/b/range-bucket/o/hello.txt",
                    &[("content-type", "text/plain")],
                    b"oci smoke",
                )
                .await,
            )
            .expect("object put should succeed");

        let response = adapter
            .handle_request(
                &storage,
                &auth_disabled(),
                &parsed_request(
                    "GET",
                    "http://localhost/n/tenant/b/range-bucket/o/hello.txt",
                    &[("range", "bytes=0-2")],
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
            Some("bytes 0-2/9")
        );
        let body = response
            .into_body()
            .collect()
            .await
            .expect("body should read")
            .to_bytes();
        assert_eq!(body.as_ref(), b"oci");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn should_support_official_oci_namespace_and_bucket_shapes() {
        let adapter = OciAdapter::new();
        let storage = temp_storage();

        let response = adapter
            .handle_request(
                &storage.clone(),
                &auth_disabled(),
                &parsed_request("GET", "http://localhost/n/tenant", &[], b"").await,
            )
            .expect("namespace lookup should succeed");
        let body = response
            .into_body()
            .collect()
            .await
            .expect("body should read")
            .to_bytes();
        assert_eq!(String::from_utf8(body.to_vec()).expect("text"), "tenant");

        let response = adapter
            .handle_request(
                &storage.clone(),
                &auth_disabled(),
                &parsed_request(
                    "POST",
                    "http://localhost/n/tenant/b",
                    &[("content-type", "application/json")],
                    br#"{"name":"sdk-bucket","compartmentId":"ignored"}"#,
                )
                .await,
            )
            .expect("bucket create should succeed");
        assert_eq!(response.status(), StatusCode::OK);

        let response = adapter
            .handle_request(
                &storage,
                &auth_disabled(),
                &parsed_request("GET", "http://localhost/n/tenant/b/sdk-bucket", &[], b"").await,
            )
            .expect("bucket get should succeed");
        let body = response
            .into_body()
            .collect()
            .await
            .expect("body should read")
            .to_bytes();
        assert!(String::from_utf8(body.to_vec())
            .expect("json")
            .contains("\"sdk-bucket\""));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn should_support_oci_multipart_upload_lifecycle() {
        let adapter = OciAdapter::new();
        let storage = temp_storage();

        create_oci_multipart_bucket(&adapter, &storage).await;
        let upload_id = create_oci_multipart_upload(&adapter, &storage).await;
        let part_one_etag = upload_oci_part(&adapter, &storage, &upload_id, 1, b"multi").await;
        let part_two_etag = upload_oci_part(&adapter, &storage, &upload_id, 2, b"part").await;
        commit_oci_multipart_upload(
            &adapter,
            &storage,
            &upload_id,
            &part_one_etag,
            &part_two_etag,
        )
        .await;
        verify_oci_multipart_metadata(&adapter, &storage).await;
    }

    async fn create_oci_multipart_bucket(adapter: &OciAdapter, storage: &Arc<dyn Storage>) {
        adapter
            .handle_request(
                &storage.clone(),
                &auth_disabled(),
                &parsed_request(
                    "POST",
                    "http://localhost/n/tenant/b",
                    &[("content-type", "application/json")],
                    br#"{"name":"multipart-bucket","compartmentId":"ignored"}"#,
                )
                .await,
            )
            .expect("bucket create should succeed");
    }

    async fn create_oci_multipart_upload(
        adapter: &OciAdapter,
        storage: &Arc<dyn Storage>,
    ) -> String {
        let response = adapter
            .handle_request(
                &storage.clone(),
                &auth_disabled(),
                &parsed_request(
                    "POST",
                    "http://localhost/n/tenant/b/multipart-bucket/u",
                    &[("content-type", "application/json")],
                    br#"{"object":"multi.txt","contentType":"text/plain","metadata":{"owner":"sdk"},"storageTier":"InfrequentAccess"}"#,
                )
                .await,
            )
            .expect("multipart create should succeed");
        let json: serde_json::Value =
            serde_json::from_slice(&read_test_body(response).await).expect("json should parse");
        json.get("uploadId")
            .and_then(serde_json::Value::as_str)
            .expect("upload id should exist")
            .to_string()
    }

    async fn upload_oci_part(
        adapter: &OciAdapter,
        storage: &Arc<dyn Storage>,
        upload_id: &str,
        part_number: u32,
        body: &[u8],
    ) -> String {
        let response = adapter
            .handle_request(
                &storage.clone(),
                &auth_disabled(),
                &parsed_request(
                    "PUT",
                    &format!(
                        "http://localhost/n/tenant/b/multipart-bucket/u/multi.txt?uploadId={upload_id}&uploadPartNum={part_number}"
                    ),
                    &[],
                    body,
                )
                .await,
            )
            .expect("part upload should succeed");
        response
            .headers()
            .get("etag")
            .and_then(|value| value.to_str().ok())
            .expect("etag should exist")
            .to_string()
    }

    async fn commit_oci_multipart_upload(
        adapter: &OciAdapter,
        storage: &Arc<dyn Storage>,
        upload_id: &str,
        part_one_etag: &str,
        part_two_etag: &str,
    ) {
        let response = adapter
            .handle_request(
                &storage.clone(),
                &auth_disabled(),
                &parsed_request(
                    "POST",
                    &format!(
                        "http://localhost/n/tenant/b/multipart-bucket/u/multi.txt?uploadId={upload_id}"
                    ),
                    &[("content-type", "application/json")],
                    format!(
                        "{{\"partsToCommit\":[{{\"partNum\":1,\"etag\":\"{part_one_etag}\"}},{{\"partNum\":2,\"etag\":\"{part_two_etag}\"}}]}}"
                    )
                    .as_bytes(),
                )
                .await,
            )
            .expect("multipart commit should succeed");
        assert_eq!(response.status(), StatusCode::OK);
    }

    async fn verify_oci_multipart_metadata(adapter: &OciAdapter, storage: &Arc<dyn Storage>) {
        let response = adapter
            .handle_request(
                &storage.clone(),
                &auth_disabled(),
                &parsed_request(
                    "HEAD",
                    "http://localhost/n/tenant/b/multipart-bucket/o/multi.txt",
                    &[],
                    b"",
                )
                .await,
            )
            .expect("head should succeed");
        assert_eq!(
            response
                .headers()
                .get("opc-meta-owner")
                .and_then(|value| value.to_str().ok()),
            Some("sdk")
        );
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
}
