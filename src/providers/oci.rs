use super::ProviderAdapter;
use crate::auth::{AuthConfig, HttpRequestLike};
use crate::blob::{BlobBackend, BlobRange, CreateUploadSessionRequest};
use crate::body::Body;
use crate::server::{RequestExt as Request, ResponseBuilder};
use crate::storage::{ObjectCondition, Storage};
use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use http::{HeaderMap, HeaderValue, Method, StatusCode, Uri};
use hyper::Response;
use sha2::{Digest, Sha256, Sha384};
use std::collections::{BTreeSet, HashMap};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

pub struct OciAdapter;

const OCI_CONTENT_MD5_KEY: &str = "oci-content-md5";
const OCI_CONTENT_CRC32C_KEY: &str = "oci-content-crc32c";
const OCI_CONTENT_SHA256_KEY: &str = "oci-content-sha256";
const OCI_CONTENT_SHA384_KEY: &str = "oci-content-sha384";
const OCI_CONTENT_LANGUAGE_KEY: &str = "oci-content-language";
const OCI_CONTENT_ENCODING_KEY: &str = "oci-content-encoding";
const OCI_CACHE_CONTROL_KEY: &str = "oci-cache-control";
const OCI_CONTENT_DISPOSITION_KEY: &str = "oci-content-disposition";
const OCI_BUCKET_STORAGE_TIER_KEY: &str = "oci-storage-tier";
const S3_VERSIONING_STATUS_KEY: &str = "s3_versioning_status";
const S3_OBJECT_LOCK_ENABLED_KEY: &str = "s3_object_lock_enabled";
const GCS_SOFT_DELETE_SECONDS_KEY: &str = "gcs_soft_delete_seconds";
const GCS_RETENTION_SECONDS_KEY: &str = "gcs_retention_seconds";
const AZURE_VERSIONING_KEY: &str = "azure_versioning_enabled";
const AZURE_SOFT_DELETE_DAYS_KEY: &str = "azure_soft_delete_days";
const OCI_VALID_LIST_FIELDS: [&str; 8] = [
    "name",
    "size",
    "etag",
    "md5",
    "timecreated",
    "timemodified",
    "storagetier",
    "archivalstate",
];

#[derive(Clone)]
enum OciListEntry {
    Object(Box<crate::models::Object>),
    Prefix(String),
}

impl OciListEntry {
    fn name(&self) -> &str {
        match self {
            Self::Object(object) => &object.key,
            Self::Prefix(prefix) => prefix,
        }
    }
}

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

    fn bucket_not_found() -> Response<Body> {
        Self::error_response(
            StatusCode::NOT_FOUND,
            "BucketNotFound",
            "The bucket does not exist.",
        )
    }

    fn object_not_found() -> Response<Body> {
        Self::error_response(
            StatusCode::NOT_FOUND,
            "ObjectNotFound",
            "The object does not exist.",
        )
    }

    fn multipart_upload_not_found() -> Response<Body> {
        Self::error_response(
            StatusCode::NOT_FOUND,
            "MultipartUploadNotFound",
            "The multipart upload does not exist.",
        )
    }

    fn selective_multipart_commit_not_implemented() -> Response<Body> {
        Self::error_response(
            StatusCode::NOT_IMPLEMENTED,
            "NotImplemented",
            "Selective OCI multipart completion is not implemented.",
        )
    }

    fn conditional_multipart_commit_not_implemented() -> Response<Body> {
        Self::error_response(
            StatusCode::NOT_IMPLEMENTED,
            "NotImplemented",
            "Conditional OCI multipart completion is not implemented.",
        )
    }

    fn with_client_request_id(
        client_request_id: Option<&str>,
        mut response: Response<Body>,
    ) -> Response<Body> {
        if let Some(value) = client_request_id.and_then(|value| HeaderValue::from_str(value).ok()) {
            response
                .headers_mut()
                .insert("opc-client-request-id", value);
        }
        response
    }

    fn invalid_parameter(message: &str) -> Response<Body> {
        Self::error_response(StatusCode::BAD_REQUEST, "InvalidParameter", message)
    }

    fn foreign_protection_active(storage: &Arc<dyn Storage>, bucket: &str) -> bool {
        storage.get_bucket(bucket).ok().is_some_and(|bucket| {
            bucket
                .metadata
                .get(S3_VERSIONING_STATUS_KEY)
                .is_some_and(|status| matches!(status.as_str(), "Enabled" | "Suspended"))
                || bucket
                    .metadata
                    .get(S3_OBJECT_LOCK_ENABLED_KEY)
                    .is_some_and(|value| value == "true")
                || bucket
                    .metadata
                    .get(GCS_SOFT_DELETE_SECONDS_KEY)
                    .and_then(|value| value.parse::<u64>().ok())
                    .is_some_and(|seconds| seconds > 0)
                || bucket
                    .metadata
                    .get(GCS_RETENTION_SECONDS_KEY)
                    .and_then(|value| value.parse::<u64>().ok())
                    .is_some_and(|seconds| seconds > 0)
                || bucket
                    .metadata
                    .get(AZURE_VERSIONING_KEY)
                    .is_some_and(|value| value == "true")
                || bucket
                    .metadata
                    .get(AZURE_SOFT_DELETE_DAYS_KEY)
                    .and_then(|value| value.parse::<u64>().ok())
                    .is_some_and(|days| days > 0)
        })
    }

    fn incorrect_state() -> Response<Body> {
        Self::error_response(
            StatusCode::CONFLICT,
            "IncorrectState",
            "The bucket data-protection mode is not compatible with this OCI operation.",
        )
    }

    fn valid_bucket_storage_tier(value: &str) -> bool {
        matches!(value, "Standard" | "Archive")
    }

    fn valid_object_storage_tier(value: &str) -> bool {
        matches!(value, "Standard" | "InfrequentAccess" | "Archive")
    }

    #[allow(clippy::result_large_err)]
    fn bucket_storage_tier(
        storage: &Arc<dyn Storage>,
        bucket: &str,
    ) -> Result<String, Response<Body>> {
        match storage.get_namespace(bucket) {
            Ok(namespace) => Ok(namespace
                .metadata
                .get(OCI_BUCKET_STORAGE_TIER_KEY)
                .cloned()
                .unwrap_or_else(|| "Standard".to_string())),
            Err(crate::error::Error::BucketNotFound) => Err(Self::bucket_not_found()),
            Err(error) => Err(Self::error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "InternalError",
                &error.to_string(),
            )),
        }
    }

    #[allow(clippy::result_large_err)]
    fn resolve_object_storage_tier(
        storage: &Arc<dyn Storage>,
        bucket: &str,
        requested: Option<&str>,
    ) -> Result<String, Response<Body>> {
        let bucket_tier = Self::bucket_storage_tier(storage, bucket)?;
        let storage_tier = requested.unwrap_or(&bucket_tier);
        if !Self::valid_object_storage_tier(storage_tier) {
            return Err(Self::invalid_parameter(
                "The storage-tier value must be Standard, InfrequentAccess, or Archive.",
            ));
        }
        if bucket_tier == "Archive" && storage_tier != "Archive" {
            return Err(Self::invalid_parameter(
                "Objects in an Archive tier bucket must use the Archive storage tier.",
            ));
        }
        Ok(storage_tier.to_string())
    }

    fn create_bucket(
        storage: &Arc<dyn Storage>,
        bucket: &str,
        storage_tier: &str,
    ) -> Result<(), crate::error::Error> {
        storage.create_namespace(bucket.to_string())?;
        storage.update_bucket_metadata(
            bucket,
            HashMap::from([(
                OCI_BUCKET_STORAGE_TIER_KEY.to_string(),
                storage_tier.to_string(),
            )]),
        )?;
        Ok(())
    }

    fn parse_path(req: &Request) -> Result<(String, Vec<String>, bool), String> {
        let path = req.path().strip_prefix('/').unwrap_or(req.path());
        if path == "n" || path == "n/" {
            return Ok(("sqrzl-emulator".to_string(), Vec::new(), false));
        }
        let Some(path) = path.strip_prefix("n/") else {
            return Err("OCI requests must start with /n".to_string());
        };
        if path.is_empty() {
            return Ok(("sqrzl-emulator".to_string(), Vec::new(), false));
        }
        let (namespace, route) = path
            .split_once('/')
            .map_or((path, None), |(namespace, route)| (namespace, Some(route)));
        if namespace.is_empty() {
            return Err("OCI requests must include a namespace".to_string());
        }
        let Some(route) = route.filter(|route| !route.is_empty()) else {
            return Ok((namespace.to_string(), Vec::new(), true));
        };
        if route == "b" || route == "b/" {
            return Ok((namespace.to_string(), vec!["b".to_string()], true));
        }
        let Some(bucket_route) = route.strip_prefix("b/") else {
            return Ok((namespace.to_string(), vec![route.to_string()], true));
        };
        let (bucket, resource) = bucket_route
            .split_once('/')
            .map_or((bucket_route, None), |(bucket, resource)| {
                (bucket, Some(resource))
            });
        if bucket.is_empty() {
            return Err("OCI requests must include a bucket name".to_string());
        }
        let mut parts = vec!["b".to_string(), bucket.to_string()];
        let Some(resource) = resource.filter(|resource| !resource.is_empty()) else {
            return Ok((namespace.to_string(), parts, true));
        };
        let (kind, object) = resource
            .split_once('/')
            .map_or((resource, None), |(kind, object)| (kind, Some(object)));
        parts.push(kind.to_string());
        if let Some(object) = object.filter(|object| !object.is_empty()) {
            parts.push(object.to_string());
        }
        Ok((namespace.to_string(), parts, true))
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

    fn normalize_etag(value: &str) -> &str {
        value.trim().trim_start_matches("W/").trim_matches('"')
    }

    fn strong_etag(value: &str) -> Option<&str> {
        let value = value.trim();
        (!value.starts_with("W/")).then(|| value.trim_matches('"'))
    }

    fn precondition_failed() -> Response<Body> {
        Self::error_response(
            StatusCode::PRECONDITION_FAILED,
            "NoEtagMatch",
            "The specified entity tag does not match the current entity tag.",
        )
    }

    #[allow(clippy::result_large_err)]
    fn put_condition(req: &Request) -> Result<Option<ObjectCondition>, Response<Body>> {
        if req.header("if-match").is_some() && req.header("if-none-match").is_some() {
            return Err(Self::error_response(
                StatusCode::BAD_REQUEST,
                "InvalidParameter",
                "If-Match and If-None-Match cannot be used together.",
            ));
        }
        if let Some(if_none_match) = req.header("if-none-match") {
            if if_none_match != "*" {
                return Err(Self::error_response(
                    StatusCode::BAD_REQUEST,
                    "InvalidParameter",
                    "The only valid If-None-Match value for PutObject is '*'.",
                ));
            }
            return Ok(Some(ObjectCondition::Missing));
        }
        Ok(req.header("if-match").map(|value| {
            if value.trim() == "*" {
                ObjectCondition::EtagNotIn(Vec::new())
            } else if let Some(etag) = Self::strong_etag(value) {
                ObjectCondition::Etag(etag.to_string())
            } else {
                ObjectCondition::Etag("__sqrzl_weak_etag_never_matches__".to_string())
            }
        }))
    }

    #[allow(clippy::result_large_err)]
    fn read_condition(
        req: &Request,
        blob: &crate::models::Object,
    ) -> Result<Option<Response<Body>>, Response<Body>> {
        if let Some(if_match) = req.header("if-match") {
            let Some(expected) = Self::strong_etag(if_match) else {
                return Ok(Some(Self::precondition_failed()));
            };
            if expected != "*" && expected != blob.etag {
                return Ok(Some(Self::precondition_failed()));
            }
        }
        if let Some(if_none_match) = req.header("if-none-match") {
            if if_none_match.trim() == "*" {
                return Err(Self::error_response(
                    StatusCode::BAD_REQUEST,
                    "InvalidParameter",
                    "Wildcards are not valid for If-None-Match on GetObject or HeadObject.",
                ));
            }
            if Self::normalize_etag(if_none_match) == blob.etag {
                return Ok(Some(Self::response(StatusCode::NOT_MODIFIED).empty()));
            }
        }
        Ok(None)
    }

    fn crc32c(data: &[u8]) -> u32 {
        let mut crc = !0_u32;
        for byte in data {
            crc ^= u32::from(*byte);
            for _ in 0..8 {
                crc = (crc >> 1) ^ (0x82f6_3b78_u32 & (0_u32.wrapping_sub(crc & 1)));
            }
        }
        !crc
    }

    #[allow(clippy::result_large_err)]
    fn put_provider_metadata(req: &Request) -> Result<HashMap<String, String>, Response<Body>> {
        let content_md5 = BASE64.encode(md5::compute(&req.body).0);
        if let Some(provided) = req
            .header("content-md5")
            .filter(|provided| *provided != content_md5)
        {
            return Err(Self::error_response(
                StatusCode::BAD_REQUEST,
                "UnmatchedContentMD5",
                &format!(
                    "The computed MD5 of the request body ({content_md5}) does not match the Content-MD5 header ({provided})"
                ),
            ));
        }

        let mut metadata = HashMap::from([(OCI_CONTENT_MD5_KEY.to_string(), content_md5)]);
        if let Some(algorithm) = req.header("opc-checksum-algorithm") {
            let (key, header, label, computed) = match algorithm.to_ascii_uppercase().as_str() {
                "CRC32C" => (
                    OCI_CONTENT_CRC32C_KEY,
                    "opc-content-crc32c",
                    "CRC32C",
                    BASE64.encode(Self::crc32c(&req.body).to_be_bytes()),
                ),
                "SHA256" => (
                    OCI_CONTENT_SHA256_KEY,
                    "opc-content-sha256",
                    "SHA256",
                    BASE64.encode(Sha256::digest(&req.body)),
                ),
                "SHA384" => (
                    OCI_CONTENT_SHA384_KEY,
                    "opc-content-sha384",
                    "SHA384",
                    BASE64.encode(Sha384::digest(&req.body)),
                ),
                _ => {
                    return Err(Self::error_response(
                        StatusCode::BAD_REQUEST,
                        "InvalidParameter",
                        "The opc-checksum-algorithm value is invalid.",
                    ))
                }
            };
            if let Some(provided) = req.header(header).filter(|provided| *provided != computed) {
                return Err(Self::error_response(
                    StatusCode::BAD_REQUEST,
                    &format!("UnmatchedContent{label}"),
                    &format!(
                        "The computed {label} of the request body ({computed}) does not match the {header} header ({provided})"
                    ),
                ));
            }
            metadata.insert(key.to_string(), computed);
        }
        for (header, key) in [
            ("content-language", OCI_CONTENT_LANGUAGE_KEY),
            ("content-encoding", OCI_CONTENT_ENCODING_KEY),
            ("cache-control", OCI_CACHE_CONTROL_KEY),
            ("content-disposition", OCI_CONTENT_DISPOSITION_KEY),
        ] {
            if let Some(value) = req.header(header) {
                metadata.insert(key.to_string(), value.to_string());
            }
        }
        Ok(metadata)
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
        crate::utils::request::decode_uri_path(path)
            .map_err(|err| format!("Invalid encoded OCI object path: {err}"))
    }

    fn object_response(status: StatusCode, blob: &crate::models::Object) -> ResponseBuilder {
        let mut builder = Self::response(status)
            .header("accept-ranges", "bytes")
            .header("content-length", &blob.size.to_string())
            .header("content-type", &blob.content_type)
            .header("etag", &blob.etag)
            .header(
                "last-modified",
                &crate::utils::headers::format_last_modified_at(&blob.last_modified),
            );
        for (metadata_key, header) in [
            (OCI_CONTENT_MD5_KEY, "content-md5"),
            (OCI_CONTENT_CRC32C_KEY, "opc-content-crc32c"),
            (OCI_CONTENT_SHA256_KEY, "opc-content-sha256"),
            (OCI_CONTENT_SHA384_KEY, "opc-content-sha384"),
            (OCI_CONTENT_LANGUAGE_KEY, "content-language"),
            (OCI_CONTENT_ENCODING_KEY, "content-encoding"),
            (OCI_CACHE_CONTROL_KEY, "cache-control"),
            (OCI_CONTENT_DISPOSITION_KEY, "content-disposition"),
        ] {
            if let Some(value) = blob.provider_metadata.get(metadata_key) {
                builder = builder.header(header, value);
            }
        }
        builder = builder.header("storage-tier", &blob.storage_class);
        for (key, value) in &blob.metadata {
            builder = builder.header(&format!("opc-meta-{key}"), value);
        }
        builder
    }

    #[allow(clippy::result_large_err)]
    fn authorize(req: &Request, config: &AuthConfig) -> Result<(), Response<Body>> {
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
        let malformed = || {
            Self::error_response(
                StatusCode::UNAUTHORIZED,
                "NotAuthenticated",
                "The required information to complete authentication was not provided.",
            )
        };
        let mut parameters = HashMap::new();
        for parameter in auth["Signature ".len()..].split(',') {
            let Some((name, value)) = parameter.trim().split_once('=') else {
                return Err(malformed());
            };
            let value = value.trim();
            let Some(value) = value
                .strip_prefix('"')
                .and_then(|value| value.strip_suffix('"'))
            else {
                return Err(malformed());
            };
            if name.trim().is_empty()
                || value.is_empty()
                || parameters.insert(name.trim(), value).is_some()
            {
                return Err(malformed());
            }
        }
        if parameters.get("algorithm") != Some(&"rsa-sha256")
            || parameters.get("keyId").is_none_or(|value| value.is_empty())
            || parameters
                .get("headers")
                .is_none_or(|value| value.is_empty())
            || parameters
                .get("signature")
                .is_none_or(|value| value.is_empty())
            || parameters
                .get("version")
                .is_some_and(|version| *version != "1")
        {
            return Err(malformed());
        }
        Err(Self::error_response(
            StatusCode::NOT_IMPLEMENTED,
            "NotImplemented",
            "OCI RSA-SHA256 request-signature verification is not implemented.",
        ))
    }

    fn handle_request(
        &self,
        storage: &Arc<dyn Storage>,
        auth_config: &Arc<AuthConfig>,
        req: &Request,
    ) -> Result<Response<Body>, String> {
        let _ = self.name();
        let (namespace, parts, explicit_namespace) = match Self::parse_path(req) {
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
            return Ok(Self::handle_namespace_request(
                req,
                &namespace,
                explicit_namespace,
            ));
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

    fn handle_namespace_request(
        req: &Request,
        namespace: &str,
        explicit_namespace: bool,
    ) -> Response<Body> {
        if req.method() == Method::GET {
            if explicit_namespace {
                return Self::error_response(
                    StatusCode::NOT_IMPLEMENTED,
                    "NotImplemented",
                    "OCI namespace metadata is not implemented by this emulator.",
                );
            }
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
        let payload: serde_json::Value = match serde_json::from_slice(&req.body) {
            Ok(payload) => payload,
            Err(_) => {
                return Ok(Self::invalid_parameter(
                    "The request body is not valid JSON.",
                ))
            }
        };
        let Some(bucket) = payload.get("name").and_then(|value| value.as_str()) else {
            return Ok(Self::invalid_parameter("The bucket name is required."));
        };
        let storage_tier = payload
            .get("storageTier")
            .and_then(|value| value.as_str())
            .unwrap_or("Standard");
        if !Self::valid_bucket_storage_tier(storage_tier) {
            return Ok(Self::invalid_parameter(
                "The storageTier value must be Standard or Archive.",
            ));
        }
        if let Err(error) = Self::create_bucket(storage, bucket, storage_tier) {
            if matches!(error, crate::error::Error::BucketAlreadyExists) {
                return Ok(Self::error_response(
                    StatusCode::CONFLICT,
                    "BucketAlreadyExists",
                    "The bucket already exists",
                ));
            }
            return Err(error.to_string());
        }
        Ok(Self::json_response(
            StatusCode::OK,
            &serde_json::json!({
                "name": bucket,
                "namespace": namespace,
                "storageTier": storage_tier,
            })
            .to_string(),
        ))
    }

    fn handle_bucket_request(
        storage: &Arc<dyn Storage>,
        req: &Request,
        namespace: &str,
        bucket: &str,
    ) -> Result<Response<Body>, String> {
        match *req.method() {
            Method::POST => Ok(Self::error_response(
                StatusCode::NOT_IMPLEMENTED,
                "NotImplemented",
                "OCI bucket updates are not implemented by this emulator.",
            )),
            Method::DELETE => {
                if Self::foreign_protection_active(storage, bucket) {
                    return Ok(Self::incorrect_state());
                }
                if let Err(error) = storage.as_ref().delete_namespace(bucket) {
                    if matches!(error, crate::error::Error::BucketNotEmpty) {
                        return Ok(Self::error_response(
                            StatusCode::CONFLICT,
                            "BucketNotEmpty",
                            "The bucket is not empty",
                        ));
                    }
                    if matches!(error, crate::error::Error::BucketNotFound) {
                        return Ok(Self::bucket_not_found());
                    }
                    return Err(error.to_string());
                }
                Ok(Self::response(StatusCode::NO_CONTENT).empty())
            }
            Method::GET => {
                let namespace_record = match storage.as_ref().get_namespace(bucket) {
                    Ok(namespace) => namespace,
                    Err(crate::error::Error::BucketNotFound) => return Ok(Self::bucket_not_found()),
                    Err(error) => return Err(error.to_string()),
                };
                let storage_tier = namespace_record
                    .metadata
                    .get(OCI_BUCKET_STORAGE_TIER_KEY)
                    .map_or("Standard", String::as_str);
                Ok(Self::json_response(
                    StatusCode::OK,
                    &serde_json::json!({
                        "name": bucket,
                        "namespace": namespace,
                        "storageTier": storage_tier,
                        "timeCreated": namespace_record.created_at.to_rfc3339(),
                    })
                    .to_string(),
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

        let Ok(object) = Self::decode_object_path(&parts[3..].join("/")) else {
            return Ok(Self::invalid_parameter("The object name is invalid."));
        };
        let Some(upload_id) = req
            .query_param("uploadId")
            .filter(|value| !value.is_empty())
        else {
            return Ok(Self::invalid_parameter(
                "The uploadId query parameter is required.",
            ));
        };
        match *req.method() {
            Method::PUT => Self::upload_multipart_part(storage, req, bucket, upload_id),
            Method::POST => Self::commit_multipart_upload(storage, req, bucket, &object, upload_id),
            Method::DELETE => match storage.abort_multipart_upload(bucket, upload_id) {
                Ok(()) => Ok(Self::response(StatusCode::NO_CONTENT).empty()),
                Err(crate::error::Error::BucketNotFound) => Ok(Self::bucket_not_found()),
                Err(crate::error::Error::InvalidUploadId | crate::error::Error::NoSuchUpload) => {
                    Ok(Self::multipart_upload_not_found())
                }
                Err(error) => Err(error.to_string()),
            },
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
        let payload: serde_json::Value = match serde_json::from_slice(&req.body) {
            Ok(payload) => payload,
            Err(_) => {
                return Ok(Self::invalid_parameter(
                    "The request body is not valid JSON.",
                ))
            }
        };
        let Some(object) = payload.get("object").and_then(|value| value.as_str()) else {
            return Ok(Self::invalid_parameter("The object name is required."));
        };
        let requested_storage_tier = payload.get("storageTier").and_then(|value| value.as_str());
        let storage_tier =
            match Self::resolve_object_storage_tier(storage, bucket, requested_storage_tier) {
                Ok(storage_tier) => storage_tier,
                Err(response) => return Ok(response),
            };
        let upload = match Self::create_multipart_session(
            storage,
            bucket,
            object,
            &payload,
            &storage_tier,
        ) {
            Ok(upload) => upload,
            Err(crate::error::Error::BucketNotFound) => return Ok(Self::bucket_not_found()),
            Err(error) => return Err(error.to_string()),
        };
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
        object: &str,
        payload: &serde_json::Value,
        storage_tier: &str,
    ) -> Result<crate::models::MultipartUpload, crate::error::Error> {
        let content_type = payload
            .get("contentType")
            .and_then(|value| value.as_str())
            .map(std::string::ToString::to_string);
        let metadata = Self::metadata_from_json(payload.get("metadata"));
        storage
            .as_ref()
            .create_upload_session(CreateUploadSessionRequest {
                namespace: bucket.to_string(),
                key: object.to_string(),
                content_type,
                metadata,
                provider_metadata: HashMap::from([
                    ("storage_tier".to_string(), storage_tier.to_string()),
                    ("storage_class".to_string(), storage_tier.to_string()),
                ]),
            })
    }

    fn upload_multipart_part(
        storage: &Arc<dyn Storage>,
        req: &Request,
        bucket: &str,
        upload_id: &str,
    ) -> Result<Response<Body>, String> {
        let Some(raw_part_number) = req.query_param("uploadPartNum") else {
            return Ok(Self::invalid_parameter(
                "The uploadPartNum query parameter is required.",
            ));
        };
        let part_number = match raw_part_number.parse::<u32>() {
            Ok(part_number) if (1..=10_000).contains(&part_number) => part_number,
            _ => {
                return Ok(Self::invalid_parameter(
                    "The uploadPartNum query parameter must be between 1 and 10000.",
                ))
            }
        };
        let etag = match storage.as_ref().upload_session_part(
            bucket,
            upload_id,
            part_number,
            req.body.to_vec(),
        ) {
            Ok(etag) => etag,
            Err(crate::error::Error::BucketNotFound) => return Ok(Self::bucket_not_found()),
            Err(crate::error::Error::InvalidUploadId | crate::error::Error::NoSuchUpload) => {
                return Ok(Self::multipart_upload_not_found())
            }
            Err(crate::error::Error::InvalidPartNumber) => {
                return Ok(Self::invalid_parameter(
                    "The uploadPartNum query parameter must be between 1 and 10000.",
                ))
            }
            Err(error) => return Err(error.to_string()),
        };
        Ok(Self::response(StatusCode::OK).header("etag", &etag).empty())
    }

    fn commit_multipart_upload(
        storage: &Arc<dyn Storage>,
        req: &Request,
        bucket: &str,
        object: &str,
        upload_id: &str,
    ) -> Result<Response<Body>, String> {
        if req.header("if-match").is_some() || req.header("if-none-match").is_some() {
            return Ok(Self::conditional_multipart_commit_not_implemented());
        }
        if Self::foreign_protection_active(storage, bucket) {
            return Ok(Self::incorrect_state());
        }
        let payload: serde_json::Value = match serde_json::from_slice(&req.body) {
            Ok(payload) => payload,
            Err(_) => {
                return Ok(Self::invalid_parameter(
                    "The request body is not valid JSON.",
                ))
            }
        };
        let upload = match storage.get_multipart_upload(bucket, upload_id) {
            Ok(upload) => upload,
            Err(crate::error::Error::BucketNotFound) => return Ok(Self::bucket_not_found()),
            Err(crate::error::Error::InvalidUploadId | crate::error::Error::NoSuchUpload) => {
                return Ok(Self::multipart_upload_not_found())
            }
            Err(error) => return Err(error.to_string()),
        };
        if upload.key != object {
            return Ok(Self::error_response(
                StatusCode::BAD_REQUEST,
                "InvalidParameter",
                "Multipart upload object did not match upload session",
            ));
        }
        if let Some(response) = Self::validate_parts_to_commit(&payload, &upload) {
            return Ok(response);
        }
        let etag = match storage.as_ref().complete_upload_session(bucket, upload_id) {
            Ok(etag) => etag,
            Err(crate::error::Error::BucketNotFound) => return Ok(Self::bucket_not_found()),
            Err(crate::error::Error::InvalidUploadId | crate::error::Error::NoSuchUpload) => {
                return Ok(Self::multipart_upload_not_found())
            }
            Err(
                crate::error::Error::InvalidPartNumber
                | crate::error::Error::InvalidPartOrder
                | crate::error::Error::IncompleteMultipartUpload
                | crate::error::Error::EntityTooSmall,
            ) => {
                return Ok(Self::error_response(
                    StatusCode::BAD_REQUEST,
                    "InvalidPart",
                    "The multipart upload parts are invalid.",
                ))
            }
            Err(error) => return Err(error.to_string()),
        };
        Ok(Self::response(StatusCode::OK).header("etag", &etag).empty())
    }

    // Keep the complete provider manifest validation sequence together so each
    // failure can be audited as precommit and session-preserving.
    #[allow(clippy::too_many_lines)]
    fn validate_parts_to_commit(
        payload: &serde_json::Value,
        upload: &crate::models::MultipartUpload,
    ) -> Option<Response<Body>> {
        let Some(parts_to_commit) = payload.get("partsToCommit") else {
            return Some(Self::invalid_parameter(
                "The partsToCommit field is required.",
            ));
        };
        let Some(parts_to_commit) = parts_to_commit.as_array() else {
            return Some(Self::invalid_parameter(
                "The partsToCommit field must be an array.",
            ));
        };
        let uploaded_parts = upload
            .parts
            .iter()
            .map(|part| part.part_number)
            .collect::<BTreeSet<_>>();
        if uploaded_parts.is_empty() {
            return Some(Self::invalid_parameter(
                "At least one multipart upload part must be committed.",
            ));
        }
        let mut committed_parts = BTreeSet::new();
        for part in parts_to_commit {
            let Some(part_num) = Self::part_num_from_json(part) else {
                return Some(Self::invalid_parameter(
                    "Each partsToCommit entry must contain a partNum between 1 and 10000.",
                ));
            };
            if !committed_parts.insert(part_num) {
                return Some(Self::invalid_parameter(
                    "A multipart upload part cannot be committed more than once.",
                ));
            }
            let Some(etag) = part.get("etag").and_then(serde_json::Value::as_str) else {
                return Some(Self::invalid_parameter(
                    "Each partsToCommit entry must contain an etag.",
                ));
            };
            let Some(stored_part) = upload
                .parts
                .iter()
                .find(|stored| stored.part_number == part_num)
            else {
                return Some(Self::error_response(
                    StatusCode::BAD_REQUEST,
                    "InvalidPart",
                    "A part selected for commit was not uploaded.",
                ));
            };
            if stored_part.etag != etag {
                return Some(Self::error_response(
                    StatusCode::BAD_REQUEST,
                    "InvalidPart",
                    "Multipart commit etag did not match uploaded part",
                ));
            }
        }

        let mut excluded_parts = BTreeSet::new();
        if let Some(parts_to_exclude) = payload.get("partsToExclude") {
            let Some(parts_to_exclude) = parts_to_exclude.as_array() else {
                return Some(Self::invalid_parameter(
                    "The partsToExclude field must be an array.",
                ));
            };
            for part in parts_to_exclude {
                let Some(part_num) = Self::part_num_value(part) else {
                    return Some(Self::invalid_parameter(
                        "Each partsToExclude entry must be a part number between 1 and 10000.",
                    ));
                };
                if !excluded_parts.insert(part_num) {
                    return Some(Self::invalid_parameter(
                        "A multipart upload part cannot be excluded more than once.",
                    ));
                }
            }
        }

        if !committed_parts.is_disjoint(&excluded_parts) {
            return Some(Self::invalid_parameter(
                "A multipart upload part cannot be both committed and excluded.",
            ));
        }
        if !committed_parts.is_subset(&uploaded_parts) || !excluded_parts.is_subset(&uploaded_parts)
        {
            return Some(Self::error_response(
                StatusCode::BAD_REQUEST,
                "InvalidPart",
                "The multipart commit references a part that was not uploaded.",
            ));
        }
        let classified_parts = committed_parts
            .union(&excluded_parts)
            .copied()
            .collect::<BTreeSet<_>>();
        if classified_parts != uploaded_parts {
            return Some(Self::invalid_parameter(
                "Every uploaded part must be included in partsToCommit or partsToExclude.",
            ));
        }
        if !excluded_parts.is_empty() || committed_parts != uploaded_parts {
            return Some(Self::selective_multipart_commit_not_implemented());
        }

        None
    }

    fn part_num_from_json(part: &serde_json::Value) -> Option<u32> {
        part.get("partNum").and_then(Self::part_num_value)
    }

    fn part_num_value(value: &serde_json::Value) -> Option<u32> {
        value
            .as_u64()
            .and_then(|value| u32::try_from(value).ok())
            .filter(|value| (1..=10_000).contains(value))
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
        if req.query_param("versionId").is_some() {
            return Ok(Self::error_response(
                StatusCode::NOT_IMPLEMENTED,
                "NotImplemented",
                "OCI version-scoped object operations are not implemented by this emulator.",
            ));
        }

        let Ok(object) = Self::decode_object_path(&parts[3..].join("/")) else {
            return Ok(Self::invalid_parameter("The object name is invalid."));
        };
        match *req.method() {
            Method::PUT => Self::put_object(storage, req, bucket, &object),
            Method::GET => Self::get_object(storage, req, bucket, &object),
            Method::HEAD => Self::head_object(storage, req, bucket, &object),
            Method::DELETE => Self::delete_object(storage, req, bucket, &object),
            _ => Ok(Self::error_response(
                StatusCode::METHOD_NOT_ALLOWED,
                "MethodNotAllowed",
                "Unsupported OCI object operation",
            )),
        }
    }

    // OCI listing deliberately keeps filtering, delimiter grouping, and page
    // token selection in provider order so the next-unreturned-name rule is
    // reviewable as one operation.
    #[allow(clippy::too_many_lines)]
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
        let limit = match req.query_param("limit") {
            None => 1_000,
            Some(value) => match value.parse::<usize>() {
                Ok(value) if (1..=1_000).contains(&value) => value,
                _ => {
                    return Ok(Self::error_response(
                        StatusCode::BAD_REQUEST,
                        "InvalidParameter",
                        "The limit must be between 1 and 1000.",
                    ))
                }
            },
        };
        if req.query_param("start").is_some() && req.query_param("startAfter").is_some() {
            return Ok(Self::error_response(
                StatusCode::BAD_REQUEST,
                "InvalidParameter",
                "The start and startAfter parameters cannot be combined.",
            ));
        }
        let delimiter = req
            .query_param("delimiter")
            .filter(|value| !value.is_empty());
        let prefix = req.query_param("prefix").unwrap_or("");
        let mut objects = Vec::new();
        let mut backend_marker: Option<String> = None;
        let mut seen_markers = BTreeSet::new();
        loop {
            let result = match storage.list_objects(
                bucket,
                Some(prefix),
                None,
                backend_marker.as_deref(),
                Some(1_000),
            ) {
                Ok(result) => result,
                Err(crate::error::Error::BucketNotFound) => return Ok(Self::bucket_not_found()),
                Err(error) => return Err(error.to_string()),
            };
            objects.extend(result.objects);
            let Some(next_marker) = result.next_marker else {
                break;
            };
            if !seen_markers.insert(next_marker.clone()) {
                return Ok(Self::error_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "InternalError",
                    "Object listing returned a repeated continuation marker.",
                ));
            }
            backend_marker = Some(next_marker);
        }

        if let Some(start) = req.query_param("start") {
            objects.retain(|object| object.key.as_str() >= start);
        }
        if let Some(start_after) = req.query_param("startAfter") {
            objects.retain(|object| object.key.as_str() > start_after);
        }
        if let Some(end) = req.query_param("end") {
            objects.retain(|object| object.key.as_str() < end);
        }

        let mut common_prefixes = BTreeSet::new();
        let mut entries = Vec::new();
        for object in objects {
            let grouped_prefix = delimiter.and_then(|delimiter| {
                object.key.strip_prefix(prefix).and_then(|suffix| {
                    suffix.find(delimiter).map(|position| {
                        let end = prefix.len() + position + delimiter.len();
                        object.key[..end].to_string()
                    })
                })
            });
            if let Some(grouped_prefix) = grouped_prefix {
                common_prefixes.insert(grouped_prefix);
            } else {
                entries.push(OciListEntry::Object(Box::new(object)));
            }
        }
        entries.extend(common_prefixes.into_iter().map(OciListEntry::Prefix));
        entries.sort_by(|left, right| left.name().cmp(right.name()));

        let next_start_with = entries.get(limit).map(|entry| entry.name().to_string());
        entries.truncate(limit);
        let mut page_objects = Vec::new();
        let mut page_prefixes = Vec::new();
        for entry in entries {
            match entry {
                OciListEntry::Object(object) => page_objects.push(*object),
                OciListEntry::Prefix(prefix) => page_prefixes.push(prefix),
            }
        }

        let fields = req
            .query_param("fields")
            .map(|fields| {
                fields
                    .split(',')
                    .map(|field| field.trim().to_ascii_lowercase())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        if fields
            .iter()
            .any(|field| !OCI_VALID_LIST_FIELDS.contains(&field.as_str()))
        {
            return Ok(Self::invalid_parameter(
                "The fields parameter contains an unsupported field.",
            ));
        }
        let include = |field: &str| fields.iter().any(|selected| selected == field);
        let objects = page_objects
            .iter()
            .map(|object| {
                let mut summary = serde_json::Map::from_iter([(
                    "name".to_string(),
                    serde_json::Value::String(object.key.clone()),
                )]);
                if include("size") {
                    summary.insert("size".to_string(), object.size.into());
                }
                if include("etag") {
                    summary.insert("etag".to_string(), object.etag.clone().into());
                }
                if include("md5") {
                    if let Some(md5) = object.provider_metadata.get(OCI_CONTENT_MD5_KEY) {
                        summary.insert("md5".to_string(), md5.clone().into());
                    }
                }
                if include("timecreated") {
                    summary.insert(
                        "timeCreated".to_string(),
                        object.last_modified.to_rfc3339().into(),
                    );
                }
                if include("timemodified") {
                    summary.insert(
                        "timeModified".to_string(),
                        object.last_modified.to_rfc3339().into(),
                    );
                }
                if include("storagetier") {
                    summary.insert(
                        "storageTier".to_string(),
                        object.storage_class.clone().into(),
                    );
                }
                if include("archivalstate") && object.storage_class == "Archive" {
                    summary.insert("archivalState".to_string(), "Archived".into());
                }
                serde_json::Value::Object(summary)
            })
            .collect::<Vec<_>>();
        let mut body = serde_json::Map::from_iter([(
            "objects".to_string(),
            serde_json::Value::Array(objects),
        )]);
        if !page_prefixes.is_empty() {
            body.insert("prefixes".to_string(), serde_json::json!(page_prefixes));
        }
        if let Some(next_start_with) = next_start_with {
            body.insert("nextStartWith".to_string(), next_start_with.into());
        }
        Ok(Self::json_response(
            StatusCode::OK,
            &serde_json::Value::Object(body).to_string(),
        ))
    }

    fn put_object(
        storage: &Arc<dyn Storage>,
        req: &Request,
        bucket: &str,
        object: &str,
    ) -> Result<Response<Body>, String> {
        if Self::foreign_protection_active(storage, bucket) {
            return Ok(Self::incorrect_state());
        }
        let storage_tier =
            match Self::resolve_object_storage_tier(storage, bucket, req.header("storage-tier")) {
                Ok(storage_tier) => storage_tier,
                Err(response) => return Ok(response),
            };
        let provider_metadata = match Self::put_provider_metadata(req) {
            Ok(metadata) => metadata,
            Err(response) => return Ok(response),
        };
        let condition = match Self::put_condition(req) {
            Ok(condition) => condition,
            Err(response) => return Ok(response),
        };
        let mut value = crate::models::Object::new_with_metadata(
            object.to_string(),
            req.body.to_vec(),
            req.header("content-type")
                .unwrap_or("application/octet-stream")
                .to_string(),
            Self::metadata_from_headers(req),
        );
        value.provider_metadata = provider_metadata;
        value.storage_class = storage_tier;
        let written = if let Some(condition) = condition {
            match storage.put_object_if(bucket, object.to_string(), value, &condition) {
                Ok(written) => written,
                Err(crate::error::Error::BucketNotFound) => return Ok(Self::bucket_not_found()),
                Err(error) => return Err(error.to_string()),
            }
        } else {
            match storage.put_object(bucket, object.to_string(), value) {
                Ok(()) => {}
                Err(crate::error::Error::BucketNotFound) => return Ok(Self::bucket_not_found()),
                Err(error) => return Err(error.to_string()),
            }
            true
        };
        if !written {
            return Ok(Self::precondition_failed());
        }
        let stored = match storage.get_object(bucket, object) {
            Ok(stored) => stored,
            Err(crate::error::Error::BucketNotFound) => return Ok(Self::bucket_not_found()),
            Err(error) => return Err(error.to_string()),
        };
        let mut response = Self::response(StatusCode::OK)
            .header("etag", &stored.etag)
            .header(
                "last-modified",
                &crate::utils::headers::format_last_modified_at(&stored.last_modified),
            )
            .header(
                "opc-content-md5",
                stored
                    .provider_metadata
                    .get(OCI_CONTENT_MD5_KEY)
                    .map_or("", String::as_str),
            );
        for (key, header) in [
            (OCI_CONTENT_CRC32C_KEY, "opc-content-crc32c"),
            (OCI_CONTENT_SHA256_KEY, "opc-content-sha256"),
            (OCI_CONTENT_SHA384_KEY, "opc-content-sha384"),
        ] {
            if let Some(value) = stored.provider_metadata.get(key) {
                response = response.header(header, value);
            }
        }
        Ok(response.empty())
    }

    fn get_object(
        storage: &Arc<dyn Storage>,
        req: &Request,
        bucket: &str,
        object: &str,
    ) -> Result<Response<Body>, String> {
        match storage.get_namespace(bucket) {
            Ok(_) => {}
            Err(crate::error::Error::BucketNotFound) => return Ok(Self::bucket_not_found()),
            Err(error) => return Err(error.to_string()),
        }
        let blob = match storage.as_ref().get_blob(bucket, object) {
            Ok(blob) => blob,
            Err(crate::error::Error::KeyNotFound) => return Ok(Self::object_not_found()),
            Err(crate::error::Error::BucketNotFound) => return Ok(Self::bucket_not_found()),
            Err(err) => return Err(err.to_string()),
        };
        match Self::read_condition(req, &blob) {
            Ok(Some(response)) | Err(response) => return Ok(response),
            Ok(None) => {}
        }
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
        req: &Request,
        bucket: &str,
        object: &str,
    ) -> Result<Response<Body>, String> {
        match storage.get_namespace(bucket) {
            Ok(_) => {}
            Err(crate::error::Error::BucketNotFound) => return Ok(Self::bucket_not_found()),
            Err(error) => return Err(error.to_string()),
        }
        let blob = match storage.as_ref().get_blob(bucket, object) {
            Ok(blob) => blob,
            Err(crate::error::Error::KeyNotFound) => return Ok(Self::object_not_found()),
            Err(crate::error::Error::BucketNotFound) => return Ok(Self::bucket_not_found()),
            Err(err) => return Err(err.to_string()),
        };
        match Self::read_condition(req, &blob) {
            Ok(Some(response)) | Err(response) => return Ok(response),
            Ok(None) => {}
        }
        Ok(Self::object_response(StatusCode::OK, &blob).empty())
    }

    fn delete_object(
        storage: &Arc<dyn Storage>,
        req: &Request,
        bucket: &str,
        object: &str,
    ) -> Result<Response<Body>, String> {
        match storage.get_namespace(bucket) {
            Ok(_) => {}
            Err(crate::error::Error::BucketNotFound) => return Ok(Self::bucket_not_found()),
            Err(error) => return Err(error.to_string()),
        }
        if Self::foreign_protection_active(storage, bucket) {
            return Ok(Self::incorrect_state());
        }
        let deleted = if let Some(if_match) = req.header("if-match") {
            let condition = if if_match.trim() == "*" {
                ObjectCondition::EtagNotIn(Vec::new())
            } else if let Some(etag) = Self::strong_etag(if_match) {
                ObjectCondition::Etag(etag.to_string())
            } else {
                return Ok(Self::precondition_failed());
            };
            match storage.delete_object_if(bucket, object, &condition) {
                Ok(deleted) => deleted,
                Err(crate::error::Error::BucketNotFound) => return Ok(Self::bucket_not_found()),
                Err(error) => return Err(error.to_string()),
            }
        } else {
            match storage.delete_object(bucket, object) {
                Ok(()) => {}
                Err(crate::error::Error::KeyNotFound) => return Ok(Self::object_not_found()),
                Err(crate::error::Error::BucketNotFound) => return Ok(Self::bucket_not_found()),
                Err(error) => return Err(error.to_string()),
            }
            true
        };
        if !deleted {
            return Ok(Self::precondition_failed());
        }
        Ok(Self::response(StatusCode::NO_CONTENT).empty())
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
        headers: &HeaderMap,
        max_request_bytes: usize,
    ) -> Response<Body> {
        Self::with_client_request_id(
            headers
                .get("opc-client-request-id")
                .and_then(|value| value.to_str().ok()),
            Self::payload_too_large_response(max_request_bytes),
        )
    }

    fn render_incomplete_body(
        &self,
        _method: &Method,
        _uri: &Uri,
        headers: &HeaderMap,
    ) -> Response<Body> {
        Self::with_client_request_id(
            headers
                .get("opc-client-request-id")
                .and_then(|value| value.to_str().ok()),
            Self::error_response(
                StatusCode::BAD_REQUEST,
                "InvalidParameter",
                "The request body ended before the declared Content-Length was received.",
            ),
        )
    }

    fn validate_request_framing(&self, req: &Request) -> Option<Response<Body>> {
        super::content_length_mismatch(req).then(|| {
            Self::with_client_request_id(
                req.header("opc-client-request-id"),
                Self::error_response(
                    StatusCode::BAD_REQUEST,
                    "InvalidParameter",
                    "Content-Length does not match the request body",
                ),
            )
        })
    }

    fn handle<'a>(
        &'a self,
        storage: Arc<dyn Storage>,
        auth_config: Arc<AuthConfig>,
        req: Request,
    ) -> Pin<Box<dyn Future<Output = Result<Response<Body>, String>> + Send + 'a>> {
        let client_request_id = req.header("opc-client-request-id").map(str::to_string);
        let result = self
            .handle_request(&storage, &auth_config, &req)
            .map(|response| Self::with_client_request_id(client_request_id.as_deref(), response));
        Box::pin(std::future::ready(result))
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
            smtp_port: crate::config::DEFAULT_SQRZL_SMTP_PORT,
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
            smtp_port: crate::config::DEFAULT_SQRZL_SMTP_PORT,
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
    async fn should_support_oci_namespace_bucket_and_object_flows() {
        let adapter = OciAdapter::new();
        let storage = temp_storage();

        let response = adapter
            .handle_request(
                &storage.clone(),
                &auth_disabled(),
                &parsed_request("GET", "http://localhost/n/", &[], b"").await,
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
    async fn should_reject_malformed_or_wrong_algorithm_signature_authorization() {
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
        request.headers.insert(
            "authorization",
            "Signature keyId=\"oci-key\",algorithm=\"hmac-sha256\",signature=\"fake\""
                .parse()
                .expect("header should parse"),
        );

        let response = adapter
            .handle_request(&storage, &oci_auth(), &request)
            .expect("oci auth request should complete");
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn should_report_valid_oci_signature_shape_as_explicitly_unsupported() {
        let adapter = OciAdapter::new();
        let storage = temp_storage();

        let mut request = parsed_request(
            "GET",
            "http://localhost/n/tenant",
            &[
                ("date", "Mon, 01 Jan 2024 00:00:00 GMT"),
                ("host", "objectstorage.localhost"),
            ],
            b"",
        )
        .await;
        request.headers.insert(
            "authorization",
            "Signature version=\"1\",keyId=\"ocid1.tenancy/key/fingerprint\",algorithm=\"rsa-sha256\",headers=\"(request-target) host date\",signature=\"ZmFrZQ==\""
                .parse()
                .expect("header should parse"),
        );

        let response = adapter
            .handle_request(&storage, &oci_auth(), &request)
            .expect("oci auth request should complete");
        assert_eq!(response.status(), StatusCode::NOT_IMPLEMENTED);
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
                    "http://localhost/n/tenant/b/archive/o?prefix=folder/&fields=name,timeCreated",
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
    async fn should_render_native_oci_incomplete_body_error() {
        let adapter = OciAdapter::new();
        let mut headers = HeaderMap::new();
        headers.insert(
            "opc-client-request-id",
            HeaderValue::from_static("oci-short-body"),
        );

        let response = adapter.render_incomplete_body(
            &Method::PUT,
            &Uri::from_static("http://localhost/n/tenant/b/bucket/o/object"),
            &headers,
        );

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            response
                .headers()
                .get("opc-client-request-id")
                .and_then(|value| value.to_str().ok()),
            Some("oci-short-body")
        );
        let body = response
            .into_body()
            .collect()
            .await
            .expect("body should read")
            .to_bytes();
        let body: serde_json::Value =
            serde_json::from_slice(&body).expect("error body should be json");
        assert_eq!(body["code"], "InvalidParameter");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn should_support_official_oci_namespace_and_bucket_shapes() {
        let adapter = OciAdapter::new();
        let storage = temp_storage();

        let response = adapter
            .handle_request(
                &storage.clone(),
                &auth_disabled(),
                &parsed_request("GET", "http://localhost/n/", &[], b"").await,
            )
            .expect("namespace lookup should succeed");
        let body = response
            .into_body()
            .collect()
            .await
            .expect("body should read")
            .to_bytes();
        assert_eq!(
            String::from_utf8(body.to_vec()).expect("text"),
            "sqrzl-emulator"
        );

        let response = adapter
            .handle_request(
                &storage.clone(),
                &auth_disabled(),
                &parsed_request("GET", "http://localhost/n/tenant", &[], b"").await,
            )
            .expect("namespace metadata request should return an OCI response");
        assert_oci_error_response(response, StatusCode::NOT_IMPLEMENTED, "NotImplemented").await;

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
    async fn should_reject_invented_oci_bucket_put_alias_without_creating_bucket() {
        let adapter = OciAdapter::new();
        let storage = temp_storage();

        let response = adapter
            .handle_request(
                &storage.clone(),
                &auth_disabled(),
                &parsed_request(
                    "PUT",
                    "http://localhost/n/tenant/b/put-alias",
                    &[("content-type", "application/json")],
                    br#"{"name":"put-alias","compartmentId":"ignored"}"#,
                )
                .await,
            )
            .expect("unsupported bucket PUT should return an OCI response");
        assert_oci_error_response(response, StatusCode::METHOD_NOT_ALLOWED, "MethodNotAllowed")
            .await;
        assert!(matches!(
            storage.get_bucket("put-alias"),
            Err(crate::error::Error::BucketNotFound)
        ));

        let response = adapter
            .handle_request(
                &storage.clone(),
                &auth_disabled(),
                &parsed_request(
                    "POST",
                    "http://localhost/n/tenant/b",
                    &[("content-type", "application/json")],
                    br#"{"name":"put-alias","compartmentId":"ignored"}"#,
                )
                .await,
            )
            .expect("official bucket collection POST should succeed");
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            storage
                .get_bucket("put-alias")
                .expect("collection POST should create the bucket")
                .name,
            "put-alias"
        );

        let response = adapter
            .handle_request(
                &storage.clone(),
                &auth_disabled(),
                &parsed_request(
                    "POST",
                    "http://localhost/n/tenant/b/put-alias",
                    &[("content-type", "application/json")],
                    br#"{"storageTier":"Archive"}"#,
                )
                .await,
            )
            .expect("unsupported bucket update should return an OCI response");
        assert_oci_error_response(response, StatusCode::NOT_IMPLEMENTED, "NotImplemented").await;
        assert_eq!(
            storage
                .get_bucket("put-alias")
                .expect("unsupported update must preserve the bucket")
                .metadata
                .get(OCI_BUCKET_STORAGE_TIER_KEY)
                .map(String::as_str),
            Some("Standard")
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn should_reject_invalid_oci_object_encoding_without_mutating() {
        let adapter = OciAdapter::new();
        let storage = temp_storage();
        create_oci_multipart_bucket(&adapter, &storage).await;

        for uri in [
            "http://localhost/n/tenant/b/multipart-bucket/o/%FF",
            "http://localhost/n/tenant/b/multipart-bucket/o/%ZZ",
            "http://localhost/n/tenant/b/multipart-bucket/u/%FF?uploadId=missing&uploadPartNum=1",
            "http://localhost/n/tenant/b/multipart-bucket/u/%ZZ?uploadId=missing&uploadPartNum=1",
        ] {
            let response = adapter
                .handle_request(
                    &storage.clone(),
                    &auth_disabled(),
                    &parsed_request("PUT", uri, &[], b"must-not-write").await,
                )
                .expect("invalid object encoding should return an OCI response");
            assert_oci_error_response(response, StatusCode::BAD_REQUEST, "InvalidParameter").await;
        }

        assert!(storage
            .list_objects("multipart-bucket", None, None, None, None)
            .expect("bucket listing should succeed")
            .objects
            .is_empty());
        assert!(storage
            .list_multipart_uploads("multipart-bucket")
            .expect("multipart listing should succeed")
            .is_empty());
    }

    // Exercise one decoded key through the complete verb set while also
    // checking every path spelling that previously collapsed or double-decoded.
    #[allow(clippy::too_many_lines)]
    #[tokio::test(flavor = "multi_thread")]
    async fn should_decode_oci_object_paths_once_without_collapsing_key_components() {
        // Arrange
        let adapter = OciAdapter::new();
        let storage = temp_storage();
        create_oci_multipart_bucket(&adapter, &storage).await;
        let cases = [
            ("a%20b", "a b"),
            ("percent%25key", "percent%key"),
            ("unicode-%E2%98%83", "unicode-☃"),
            ("dir%2Fchild", "dir/child"),
            ("a//b", "a//b"),
            ("dir/", "dir/"),
            ("/leading", "/leading"),
        ];

        // Act
        for (encoded, decoded) in cases {
            let response = adapter
                .handle_request(
                    &storage,
                    &auth_disabled(),
                    &parsed_request(
                        "PUT",
                        &format!("http://localhost/n/tenant/b/multipart-bucket/o/{encoded}"),
                        &[],
                        b"payload",
                    )
                    .await,
                )
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            assert_eq!(
                storage
                    .get_object("multipart-bucket", decoded)
                    .unwrap()
                    .data,
                b"payload"
            );
        }
        let get = adapter
            .handle_request(
                &storage,
                &auth_disabled(),
                &parsed_request(
                    "GET",
                    "http://localhost/n/tenant/b/multipart-bucket/o/a%20b",
                    &[],
                    b"",
                )
                .await,
            )
            .unwrap();
        let head = adapter
            .handle_request(
                &storage,
                &auth_disabled(),
                &parsed_request(
                    "HEAD",
                    "http://localhost/n/tenant/b/multipart-bucket/o/a%20b",
                    &[],
                    b"",
                )
                .await,
            )
            .unwrap();
        let list = adapter
            .handle_request(
                &storage,
                &auth_disabled(),
                &parsed_request(
                    "GET",
                    "http://localhost/n/tenant/b/multipart-bucket/o",
                    &[],
                    b"",
                )
                .await,
            )
            .unwrap();
        let delete = adapter
            .handle_request(
                &storage,
                &auth_disabled(),
                &parsed_request(
                    "DELETE",
                    "http://localhost/n/tenant/b/multipart-bucket/o/a%20b",
                    &[],
                    b"",
                )
                .await,
            )
            .unwrap();

        // Assert
        assert_eq!(get.status(), StatusCode::OK);
        assert_eq!(read_test_body(get).await, b"payload");
        assert_eq!(head.status(), StatusCode::OK);
        assert_eq!(delete.status(), StatusCode::NO_CONTENT);
        assert!(storage.get_object("multipart-bucket", "a b").is_err());
        let listed: serde_json::Value =
            serde_json::from_slice(&read_test_body(list).await).unwrap();
        let names = listed["objects"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|object| object["name"].as_str())
            .collect::<BTreeSet<_>>();
        for (_, decoded) in cases.into_iter().skip(1) {
            assert!(names.contains(decoded), "missing decoded OCI key {decoded}");
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn should_reject_oci_version_scoped_operations_without_touching_current_object() {
        // Arrange
        let adapter = OciAdapter::new();
        let storage = temp_storage();
        create_oci_multipart_bucket(&adapter, &storage).await;
        storage
            .put_object(
                "multipart-bucket",
                "current".to_string(),
                crate::models::Object::new(
                    "current".to_string(),
                    b"current".to_vec(),
                    "application/octet-stream".to_string(),
                ),
            )
            .unwrap();

        // Act
        let mut responses = Vec::new();
        for method in ["GET", "HEAD", "DELETE"] {
            responses.push(
                adapter
                    .handle_request(
                        &storage,
                        &auth_disabled(),
                        &parsed_request(
                            method,
                            "http://localhost/n/tenant/b/multipart-bucket/o/current?versionId=old",
                            &[],
                            b"",
                        )
                        .await,
                    )
                    .unwrap(),
            );
        }

        // Assert
        for response in responses {
            assert_oci_error_response(response, StatusCode::NOT_IMPLEMENTED, "NotImplemented")
                .await;
        }
        assert_eq!(
            storage
                .get_object("multipart-bucket", "current")
                .unwrap()
                .data,
            b"current"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn should_preserve_oci_multipart_object_path_components() {
        // Arrange
        let adapter = OciAdapter::new();
        let storage = temp_storage();
        create_oci_multipart_bucket(&adapter, &storage).await;
        let created = adapter
            .handle_request(
                &storage,
                &auth_disabled(),
                &parsed_request(
                    "POST",
                    "http://localhost/n/tenant/b/multipart-bucket/u",
                    &[("content-type", "application/json")],
                    br#"{"object":"a//b"}"#,
                )
                .await,
            )
            .unwrap();
        let created: serde_json::Value =
            serde_json::from_slice(&read_test_body(created).await).unwrap();
        let upload_id = created["uploadId"].as_str().unwrap();

        // Act
        let uploaded = adapter
            .handle_request(
                &storage,
                &auth_disabled(),
                &parsed_request(
                    "PUT",
                    &format!(
                        "http://localhost/n/tenant/b/multipart-bucket/u/a//b?uploadId={upload_id}&uploadPartNum=1"
                    ),
                    &[],
                    b"payload",
                )
                .await,
            )
            .unwrap();
        let etag = uploaded.headers()["etag"].to_str().unwrap().to_string();
        let manifest = serde_json::json!({
            "partsToCommit": [{"partNum": 1, "etag": etag}]
        })
        .to_string();
        let committed = adapter
            .handle_request(
                &storage,
                &auth_disabled(),
                &parsed_request(
                    "POST",
                    &format!(
                        "http://localhost/n/tenant/b/multipart-bucket/u/a//b?uploadId={upload_id}"
                    ),
                    &[("content-type", "application/json")],
                    manifest.as_bytes(),
                )
                .await,
            )
            .unwrap();

        // Assert
        assert_eq!(uploaded.status(), StatusCode::OK);
        assert_eq!(committed.status(), StatusCode::OK);
        assert_eq!(
            storage.get_object("multipart-bucket", "a//b").unwrap().data,
            b"payload"
        );
        assert!(storage.get_object("multipart-bucket", "a/b").is_err());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn should_return_native_oci_errors_for_invalid_multipart_requests() {
        let adapter = OciAdapter::new();
        let storage = temp_storage();
        create_oci_multipart_bucket(&adapter, &storage).await;

        let response = adapter
            .handle_request(
                &storage.clone(),
                &auth_disabled(),
                &parsed_request(
                    "PUT",
                    "http://localhost/n/tenant/b/multipart-bucket/u/multi.txt?uploadPartNum=1",
                    &[],
                    b"must-not-upload",
                )
                .await,
            )
            .expect("missing upload ID should return an OCI response");
        assert_oci_error_response(response, StatusCode::BAD_REQUEST, "InvalidParameter").await;

        let upload_id = create_oci_multipart_upload(&adapter, &storage).await;
        for uri in [
            format!(
                "http://localhost/n/tenant/b/multipart-bucket/u/multi.txt?uploadId={upload_id}"
            ),
            format!(
                "http://localhost/n/tenant/b/multipart-bucket/u/multi.txt?uploadId={upload_id}&uploadPartNum=0"
            ),
            format!(
                "http://localhost/n/tenant/b/multipart-bucket/u/multi.txt?uploadId={upload_id}&uploadPartNum=not-a-number"
            ),
        ] {
            let response = adapter
                .handle_request(
                    &storage.clone(),
                    &auth_disabled(),
                    &parsed_request("PUT", &uri, &[], b"must-not-upload").await,
                )
                .expect("invalid part number should return an OCI response");
            assert_oci_error_response(response, StatusCode::BAD_REQUEST, "InvalidParameter").await;
        }

        let response = adapter
            .handle_request(
                &storage.clone(),
                &auth_disabled(),
                &parsed_request(
                    "PUT",
                    "http://localhost/n/tenant/b/multipart-bucket/u/multi.txt?uploadId=missing&uploadPartNum=1",
                    &[],
                    b"must-not-upload",
                )
                .await,
            )
            .expect("missing session should return an OCI response");
        assert_oci_error_response(response, StatusCode::NOT_FOUND, "MultipartUploadNotFound").await;

        for body in [b"{".as_slice(), br"{}".as_slice()] {
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
                        body,
                    )
                    .await,
                )
                .expect("invalid commit document should return an OCI response");
            assert_oci_error_response(response, StatusCode::BAD_REQUEST, "InvalidParameter").await;
        }

        let upload = storage
            .get_multipart_upload("multipart-bucket", &upload_id)
            .expect("invalid requests must preserve the upload session");
        assert!(upload.parts.is_empty());
        assert!(matches!(
            storage.get_object("multipart-bucket", "multi.txt"),
            Err(crate::error::Error::KeyNotFound)
        ));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn should_never_commit_unlisted_or_excluded_oci_multipart_parts() {
        let adapter = OciAdapter::new();
        let storage = temp_storage();
        create_oci_multipart_bucket(&adapter, &storage).await;
        let upload_id = create_oci_multipart_upload(&adapter, &storage).await;
        let part_one_etag = upload_oci_part(&adapter, &storage, &upload_id, 1, b"multi").await;
        let part_two_etag = upload_oci_part(&adapter, &storage, &upload_id, 2, b"part").await;

        let incomplete_manifest =
            format!("{{\"partsToCommit\":[{{\"partNum\":1,\"etag\":\"{part_one_etag}\"}}]}}");
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
                    incomplete_manifest.as_bytes(),
                )
                .await,
            )
            .expect("unclassified part should return an OCI response");
        assert_oci_error_response(response, StatusCode::BAD_REQUEST, "InvalidParameter").await;

        let selective_manifest = format!(
            "{{\"partsToCommit\":[{{\"partNum\":1,\"etag\":\"{part_one_etag}\"}}],\"partsToExclude\":[2]}}"
        );
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
                    selective_manifest.as_bytes(),
                )
                .await,
            )
            .expect("selective commit should return an OCI response");
        assert_oci_error_response(response, StatusCode::NOT_IMPLEMENTED, "NotImplemented").await;

        let upload = storage
            .get_multipart_upload("multipart-bucket", &upload_id)
            .expect("rejected selection must preserve the upload session");
        assert_eq!(upload.parts.len(), 2);
        assert!(matches!(
            storage.get_object("multipart-bucket", "multi.txt"),
            Err(crate::error::Error::KeyNotFound)
        ));

        commit_oci_multipart_upload(
            &adapter,
            &storage,
            &upload_id,
            &part_one_etag,
            &part_two_etag,
        )
        .await;
        assert_eq!(
            storage
                .get_object("multipart-bucket", "multi.txt")
                .expect("complete manifest should commit after rejected attempts")
                .data,
            b"multipart"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn should_reject_unsupported_oci_multipart_conditions_without_consuming_session() {
        let adapter = OciAdapter::new();
        let storage = temp_storage();
        create_oci_multipart_bucket(&adapter, &storage).await;
        let upload_id = create_oci_multipart_upload(&adapter, &storage).await;
        let part_one_etag = upload_oci_part(&adapter, &storage, &upload_id, 1, b"multi").await;
        let part_two_etag = upload_oci_part(&adapter, &storage, &upload_id, 2, b"part").await;
        let manifest = format!(
            "{{\"partsToCommit\":[{{\"partNum\":1,\"etag\":\"{part_one_etag}\"}},{{\"partNum\":2,\"etag\":\"{part_two_etag}\"}}]}}"
        );

        for condition in [("if-match", "stale"), ("if-none-match", "*")] {
            let response = adapter
                .handle_request(
                    &storage.clone(),
                    &auth_disabled(),
                    &parsed_request(
                        "POST",
                        &format!(
                            "http://localhost/n/tenant/b/multipart-bucket/u/multi.txt?uploadId={upload_id}"
                        ),
                        &[("content-type", "application/json"), condition],
                        manifest.as_bytes(),
                    )
                    .await,
                )
                .expect("conditional commit should return an OCI response");
            assert_oci_error_response(response, StatusCode::NOT_IMPLEMENTED, "NotImplemented")
                .await;
            assert_eq!(
                storage
                    .get_multipart_upload("multipart-bucket", &upload_id)
                    .expect("unsupported condition must preserve the upload session")
                    .parts
                    .len(),
                2
            );
            assert!(matches!(
                storage.get_object("multipart-bucket", "multi.txt"),
                Err(crate::error::Error::KeyNotFound)
            ));
        }

        commit_oci_multipart_upload(
            &adapter,
            &storage,
            &upload_id,
            &part_one_etag,
            &part_two_etag,
        )
        .await;
        assert_eq!(
            storage
                .get_object("multipart-bucket", "multi.txt")
                .expect("unconditional retry should commit the object")
                .data,
            b"multipart"
        );
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

    async fn assert_oci_error_response(response: Response<Body>, status: StatusCode, code: &str) {
        assert_eq!(response.status(), status);
        let body: serde_json::Value = serde_json::from_slice(&read_test_body(response).await)
            .expect("OCI error body should be JSON");
        assert_eq!(body["code"], code);
    }
}
