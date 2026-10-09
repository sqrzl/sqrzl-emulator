use super::{state, ProviderAdapter};
use crate::auth::{AuthConfig, HttpRequestLike};
use crate::blob::{BlobBackend, CreateUploadSessionRequest};
use crate::body::Body;
use crate::server::{RequestExt as Request, ResponseBuilder};
use crate::storage::{
    ObjectCondition, Storage, MULTIPART_MAX_OBJECT_SIZE_KEY, MULTIPART_MAX_PART_SIZE_KEY,
    MULTIPART_MIN_NON_FINAL_PART_SIZE_KEY,
};
use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use http::{HeaderMap, HeaderValue, Method, StatusCode, Uri};
use hyper::Response;
use rsa::pkcs1::DecodeRsaPublicKey;
use rsa::pkcs1v15::{Signature as RsaSignature, VerifyingKey};
use rsa::pkcs8::DecodePublicKey;
use rsa::sha2::Sha256 as RsaSha256;
use rsa::signature::Verifier;
use rsa::RsaPublicKey;
use std::collections::{BTreeSet, HashMap};
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, OnceLock};

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
const OCI_BUCKET_COMPARTMENT_KEY: &str = "oci-compartment-id";
const OCI_BUCKET_METADATA_KEY: &str = "oci-bucket-user-metadata";
const OCI_NAMESPACE: &str = "sqrzl-emulator";
const OCI_MAX_OBJECT_SIZE: u64 = 10 * 1024 * 1024 * 1024 * 1024;
const OCI_MAX_PART_SIZE: u64 = 50 * 1024 * 1024 * 1024;
const OCI_MIN_NON_FINAL_PART_SIZE: u64 = 10 * 1024 * 1024;
const OCI_PAR_STATE: &str = "oci-preauthenticated-request-v2";
const OCI_PAR_INDEX_STATE: &str = "oci-preauthenticated-request-index-v2";
const S3_VERSIONING_STATUS_KEY: &str = "s3_versioning_status";
const S3_OBJECT_LOCK_ENABLED_KEY: &str = "s3_object_lock_enabled";
const GCS_SOFT_DELETE_SECONDS_KEY: &str = "gcs_soft_delete_seconds";
const GCS_RETENTION_SECONDS_KEY: &str = "gcs_retention_seconds";

#[derive(Clone, Copy)]
enum JsonFieldKind {
    String,
    Boolean,
    StringMap,
    NestedObjectMap,
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct OciPreauthenticatedRequest {
    id: String,
    token: String,
    name: String,
    bucket: String,
    object_name: Option<String>,
    access_type: String,
    time_created: chrono::DateTime<chrono::Utc>,
    time_expires: chrono::DateTime<chrono::Utc>,
}

static OCI_PAR_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
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

        uri.path().starts_with("/n/")
            || uri.path().starts_with("/p/")
            || authorization.starts_with("Signature ")
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

    fn validate_document(
        payload: &serde_json::Value,
        schema: &[(&str, JsonFieldKind)],
    ) -> Option<Response<Body>> {
        let Some(fields) = payload.as_object() else {
            return Some(Self::invalid_parameter(
                "The request body must be a JSON object.",
            ));
        };
        for (name, value) in fields {
            let Some((_, kind)) = schema.iter().find(|(field, _)| name == field) else {
                return Some(Self::invalid_parameter(&format!(
                    "Unknown request field: {name}."
                )));
            };
            let valid = match kind {
                JsonFieldKind::String => value.is_string(),
                JsonFieldKind::Boolean => value.is_boolean(),
                JsonFieldKind::StringMap => value
                    .as_object()
                    .is_some_and(|map| map.values().all(serde_json::Value::is_string)),
                JsonFieldKind::NestedObjectMap => value
                    .as_object()
                    .is_some_and(|map| map.values().all(serde_json::Value::is_object)),
            };
            if !valid {
                return Some(Self::invalid_parameter(&format!(
                    "Invalid JSON type for field {name}."
                )));
            }
        }
        None
    }

    fn validate_bucket_document(payload: &serde_json::Value) -> Option<Response<Body>> {
        use JsonFieldKind::{Boolean, NestedObjectMap, String, StringMap};
        // CreateBucketDetails native fields. Controls lacking local semantics
        // remain explicit errors, including requests for their default values.
        let schema = [
            ("name", String),
            ("compartmentId", String),
            ("storageTier", String),
            ("metadata", StringMap),
            ("publicAccessType", String),
            ("objectEventsEnabled", Boolean),
            ("freeformTags", StringMap),
            ("definedTags", NestedObjectMap),
            ("kmsKeyId", String),
            ("isBucketKeyEnabled", Boolean),
            ("versioning", String),
            ("autoTiering", String),
            ("bucketScope", String),
        ];
        if let Some(response) = Self::validate_document(payload, &schema) {
            return Some(response);
        }
        if let Some(field) = payload.as_object().unwrap().keys().find(|field| {
            !matches!(
                field.as_str(),
                "name" | "compartmentId" | "storageTier" | "metadata"
            )
        }) {
            return Some(Self::error_response(
                StatusCode::NOT_IMPLEMENTED,
                "NotImplemented",
                &format!("Bucket field {field} is not supported by this emulator."),
            ));
        }
        if payload
            .get("compartmentId")
            .is_some_and(|value| value.as_str() == Some(""))
        {
            return Some(Self::invalid_parameter(
                "The compartmentId cannot be empty.",
            ));
        }
        if payload
            .get("metadata")
            .and_then(serde_json::Value::as_object)
            .is_some_and(|map| {
                map.iter()
                    .map(|(key, value)| key.len() + value.as_str().unwrap().len())
                    .sum::<usize>()
                    > 4096
            })
        {
            return Some(Self::invalid_parameter(
                "Bucket user metadata may not exceed 4096 bytes.",
            ));
        }
        None
    }

    fn validate_multipart_document(payload: &serde_json::Value) -> Option<Response<Body>> {
        use JsonFieldKind::{String, StringMap};
        let schema = [
            ("object", String),
            ("storageTier", String),
            ("contentType", String),
            ("metadata", StringMap),
            ("cacheControl", String),
            ("contentDisposition", String),
            ("contentEncoding", String),
            ("contentLanguage", String),
        ];
        if let Some(response) = Self::validate_document(payload, &schema) {
            return Some(response);
        }
        for (field, value) in payload.as_object().unwrap() {
            if matches!(
                field.as_str(),
                "contentType"
                    | "cacheControl"
                    | "contentDisposition"
                    | "contentEncoding"
                    | "contentLanguage"
            ) && http::HeaderValue::from_str(value.as_str().unwrap()).is_err()
            {
                return Some(Self::invalid_parameter(&format!(
                    "Field {field} must be a valid HTTP header value."
                )));
            }
        }
        if let Some(metadata) = payload
            .get("metadata")
            .and_then(serde_json::Value::as_object)
        {
            for (key, value) in metadata {
                if http::HeaderName::from_bytes(format!("opc-meta-{key}").as_bytes()).is_err()
                    || http::HeaderValue::from_str(value.as_str().unwrap()).is_err()
                {
                    return Some(Self::invalid_parameter(
                        "Multipart metadata must contain valid HTTP metadata names and values.",
                    ));
                }
            }
        }
        None
    }

    fn valid_bucket_name(name: &str) -> bool {
        !name.is_empty()
            && name.len() <= 256
            && name
                .as_bytes()
                .iter()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(*byte, b'-' | b'_' | b'.'))
    }

    fn foreign_protection_active(storage: &Arc<dyn Storage>, bucket: &str) -> bool {
        super::azure_object_protection_active(storage.as_ref(), bucket)
            || storage.get_bucket(bucket).is_ok_and(|bucket| {
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
        payload: &serde_json::Value,
    ) -> Result<(), crate::error::Error> {
        storage.create_namespace(bucket.to_string())?;
        let mut metadata = HashMap::from([(
            OCI_BUCKET_STORAGE_TIER_KEY.to_string(),
            storage_tier.to_string(),
        )]);
        if let Some(compartment) = payload
            .get("compartmentId")
            .and_then(serde_json::Value::as_str)
        {
            metadata.insert(
                OCI_BUCKET_COMPARTMENT_KEY.to_string(),
                compartment.to_string(),
            );
        }
        if let Some(user_metadata) = payload.get("metadata") {
            metadata.insert(
                OCI_BUCKET_METADATA_KEY.to_string(),
                user_metadata.to_string(),
            );
        }
        if let Err(error) = storage.update_bucket_metadata(bucket, metadata) {
            let rollback = storage.delete_namespace(bucket);
            return Err(crate::error::Error::InternalError(match rollback {
                Ok(()) => error.to_string(),
                Err(rollback_error) => {
                    format!("{error}; failed to roll back OCI bucket creation: {rollback_error}")
                }
            }));
        }
        Ok(())
    }

    fn parse_path(req: &Request) -> Result<(String, Vec<String>, bool), String> {
        let path = req.path().strip_prefix('/').unwrap_or(req.path());
        if path == "n" || path == "n/" {
            return Ok((OCI_NAMESPACE.to_string(), Vec::new(), false));
        }
        let Some(path) = path.strip_prefix("n/") else {
            return Err("OCI requests must start with /n".to_string());
        };
        if path.is_empty() {
            return Ok((OCI_NAMESPACE.to_string(), Vec::new(), false));
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

    #[allow(clippy::result_large_err)]
    fn put_provider_metadata(req: &Request) -> Result<HashMap<String, String>, Response<Body>> {
        let content_md5 = BASE64.encode(req.payload_md5());
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
                    BASE64.encode(req.payload_crc32c().to_be_bytes()),
                ),
                "SHA256" => (
                    OCI_CONTENT_SHA256_KEY,
                    "opc-content-sha256",
                    "SHA256",
                    BASE64.encode(req.payload_sha256()),
                ),
                "SHA384" => (
                    OCI_CONTENT_SHA384_KEY,
                    "opc-content-sha384",
                    "SHA384",
                    BASE64.encode(req.payload_sha384()),
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
    #[allow(clippy::too_many_lines)]
    fn authorize(req: &Request, config: &AuthConfig) -> Result<(), Response<Body>> {
        if !config.oci_auth_enforced() {
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
        let Some(expected_key_id) = config.oci_key_id() else {
            return Err(Self::error_response(
                StatusCode::UNAUTHORIZED,
                "NotAuthenticated",
                "OCI authentication is enabled but its RSA identity is incomplete.",
            ));
        };
        if parameters.get("keyId") != Some(&expected_key_id.as_str()) {
            return Err(Self::error_response(
                StatusCode::UNAUTHORIZED,
                "NotAuthenticated",
                "The OCI signing key ID is not configured.",
            ));
        }
        let signed_headers = parameters["headers"].split_whitespace().collect::<Vec<_>>();
        if !signed_headers.contains(&"(request-target)")
            || !signed_headers.contains(&"host")
            || (!signed_headers.contains(&"date") && !signed_headers.contains(&"x-date"))
        {
            return Err(malformed());
        }
        let request_date = req
            .header("x-date")
            .or_else(|| req.header("date"))
            .and_then(|value| chrono::DateTime::parse_from_rfc2822(value).ok())
            .map(|value| value.with_timezone(&chrono::Utc));
        if request_date.is_none_or(|date| {
            chrono::Utc::now().signed_duration_since(date).abs() > chrono::Duration::minutes(5)
        }) {
            return Err(Self::error_response(
                StatusCode::UNAUTHORIZED,
                "NotAuthenticated",
                "The OCI request date is outside the permitted clock skew.",
            ));
        }
        let mut signing_lines = Vec::with_capacity(signed_headers.len());
        for name in signed_headers {
            if name == "(request-target)" {
                let target = req
                    .uri
                    .path_and_query()
                    .map_or(req.path(), http::uri::PathAndQuery::as_str);
                signing_lines.push(format!(
                    "(request-target): {} {target}",
                    req.method().as_str().to_ascii_lowercase()
                ));
                continue;
            }
            if name != name.to_ascii_lowercase() {
                return Err(malformed());
            }
            let Some(value) = req.header(name) else {
                return Err(malformed());
            };
            signing_lines.push(format!(
                "{name}: {}",
                value.split_whitespace().collect::<Vec<_>>().join(" ")
            ));
        }
        let Some(public_key_path) = config.vendor_credentials.oci_public_key_path.as_deref() else {
            return Err(malformed());
        };
        let pem = std::fs::read_to_string(public_key_path).map_err(|_| malformed())?;
        let public_key = RsaPublicKey::from_public_key_pem(&pem)
            .or_else(|_| RsaPublicKey::from_pkcs1_pem(&pem))
            .map_err(|_| malformed())?;
        let signature_bytes = BASE64
            .decode(parameters["signature"])
            .map_err(|_| malformed())?;
        let signature =
            RsaSignature::try_from(signature_bytes.as_slice()).map_err(|_| malformed())?;
        VerifyingKey::<RsaSha256>::new(public_key)
            .verify(signing_lines.join("\n").as_bytes(), &signature)
            .map_err(|_| {
                Self::error_response(
                    StatusCode::UNAUTHORIZED,
                    "NotAuthenticated",
                    "OCI request signature verification failed.",
                )
            })
    }

    fn handle_request(
        &self,
        storage: &Arc<dyn Storage>,
        auth_config: &Arc<AuthConfig>,
        req: &Request,
    ) -> Result<Response<Body>, String> {
        let _ = self.name();
        if req.path().starts_with("/p/") {
            return Self::handle_par_request(storage, req);
        }
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

        if explicit_namespace && namespace != OCI_NAMESPACE {
            return Ok(Self::error_response(
                StatusCode::NOT_FOUND,
                "NotAuthorizedOrNotFound",
                "The requested namespace does not exist or is not authorized.",
            ));
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

        if parts.len() >= 3 && parts[0] == "b" && parts[2] == "p" {
            return Self::handle_par_control(storage, req, &namespace, &parts);
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

    #[allow(clippy::too_many_lines)]
    fn handle_par_control(
        storage: &Arc<dyn Storage>,
        req: &Request,
        namespace: &str,
        parts: &[String],
    ) -> Result<Response<Body>, String> {
        let bucket = &parts[1];
        if storage.get_bucket(bucket).is_err() {
            return Ok(Self::bucket_not_found());
        }
        let lock = OCI_PAR_LOCK.get_or_init(|| Mutex::new(()));
        let _guard = lock
            .lock()
            .map_err(|_| "Failed to lock OCI PAR state".to_string())?;
        if parts.len() == 3 && req.method() == Method::POST {
            let payload: serde_json::Value = serde_json::from_slice(&req.body)
                .map_err(|_| "The OCI PAR request body is not valid JSON".to_string())?;
            let name = payload
                .get("name")
                .and_then(serde_json::Value::as_str)
                .filter(|value| !value.is_empty())
                .ok_or_else(|| "The OCI PAR name is required".to_string())?;
            let access_type = payload
                .get("accessType")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| "The OCI PAR accessType is required".to_string())?;
            if !matches!(
                access_type,
                "ObjectRead"
                    | "ObjectWrite"
                    | "ObjectReadWrite"
                    | "AnyObjectRead"
                    | "AnyObjectWrite"
                    | "AnyObjectReadWrite"
            ) {
                return Ok(Self::invalid_parameter("The PAR accessType is invalid."));
            }
            let object_name = payload
                .get("objectName")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string);
            if access_type.starts_with("Object") && object_name.is_none() {
                return Ok(Self::invalid_parameter(
                    "Object-scoped PARs require objectName.",
                ));
            }
            let expires = payload
                .get("timeExpires")
                .and_then(serde_json::Value::as_str)
                .and_then(|value| chrono::DateTime::parse_from_rfc3339(value).ok())
                .map(|value| value.with_timezone(&chrono::Utc));
            let Some(time_expires) = expires.filter(|value| *value > chrono::Utc::now()) else {
                return Ok(Self::invalid_parameter(
                    "timeExpires must be a future RFC3339 timestamp.",
                ));
            };
            let id = uuid::Uuid::new_v4().simple().to_string();
            let par = OciPreauthenticatedRequest {
                token: id.clone(),
                id: id.clone(),
                name: name.to_string(),
                bucket: bucket.clone(),
                object_name,
                access_type: access_type.to_string(),
                time_created: chrono::Utc::now(),
                time_expires,
            };
            state::save_json(storage.as_ref(), OCI_PAR_STATE, &id, &par)?;
            let mut index: Vec<String> =
                state::load_json(storage.as_ref(), OCI_PAR_INDEX_STATE, bucket)?
                    .unwrap_or_default();
            index.push(id);
            state::save_json(storage.as_ref(), OCI_PAR_INDEX_STATE, bucket, &index)?;
            return Ok(Self::json_response(
                StatusCode::OK,
                &Self::par_json(namespace, &par).to_string(),
            ));
        }
        if parts.len() == 3 && req.method() == Method::GET {
            let index: Vec<String> =
                state::load_json(storage.as_ref(), OCI_PAR_INDEX_STATE, bucket)?
                    .unwrap_or_default();
            let preauthenticated_requests = index
                .into_iter()
                .filter_map(|id| {
                    state::load_json::<OciPreauthenticatedRequest>(
                        storage.as_ref(),
                        OCI_PAR_STATE,
                        &id,
                    )
                    .ok()
                    .flatten()
                })
                .filter(|par| par.time_expires > chrono::Utc::now())
                .map(|par| Self::par_json(namespace, &par))
                .collect::<Vec<_>>();
            return Ok(Self::json_response(
                StatusCode::OK,
                &serde_json::Value::Array(preauthenticated_requests).to_string(),
            ));
        }
        let Some(id) = parts.get(3) else {
            return Ok(Self::invalid_parameter("The PAR ID is required."));
        };
        let Some(par) =
            state::load_json::<OciPreauthenticatedRequest>(storage.as_ref(), OCI_PAR_STATE, id)?
        else {
            return Ok(Self::error_response(
                StatusCode::NOT_FOUND,
                "NotAuthorizedOrNotFound",
                "The pre-authenticated request does not exist.",
            ));
        };
        match *req.method() {
            Method::GET => Ok(Self::json_response(
                StatusCode::OK,
                &Self::par_json(namespace, &par).to_string(),
            )),
            Method::DELETE => {
                storage
                    .delete_provider_state(OCI_PAR_STATE, id)
                    .map_err(|error| error.to_string())?;
                let mut index: Vec<String> =
                    state::load_json(storage.as_ref(), OCI_PAR_INDEX_STATE, bucket)?
                        .unwrap_or_default();
                index.retain(|entry| entry != id);
                state::save_json(storage.as_ref(), OCI_PAR_INDEX_STATE, bucket, &index)?;
                Ok(Self::response(StatusCode::NO_CONTENT).empty())
            }
            _ => Ok(Self::error_response(
                StatusCode::METHOD_NOT_ALLOWED,
                "MethodNotAllowed",
                "Unsupported PAR control operation.",
            )),
        }
    }

    fn par_json(namespace: &str, par: &OciPreauthenticatedRequest) -> serde_json::Value {
        let object_suffix = par.object_name.as_ref().map_or_else(String::new, |object| {
            format!("/{}", Self::encode_object_path(object))
        });
        serde_json::json!({
            "id": par.id,
            "name": par.name,
            "accessType": par.access_type,
            "objectName": par.object_name,
            "timeCreated": par.time_created.to_rfc3339(),
            "timeExpires": par.time_expires.to_rfc3339(),
            "accessUri": format!(
                "/p/{}/n/{namespace}/b/{}/o{object_suffix}",
                par.token, par.bucket
            ),
        })
    }

    fn encode_object_path(object: &str) -> String {
        object
            .split('/')
            .map(|segment| urlencoding::encode(segment).into_owned())
            .collect::<Vec<_>>()
            .join("/")
    }

    #[allow(clippy::too_many_lines)]
    fn handle_par_request(
        storage: &Arc<dyn Storage>,
        req: &Request,
    ) -> Result<Response<Body>, String> {
        let path = req.path().trim_start_matches('/');
        let mut path_parts = path.split('/');
        if path_parts.next() != Some("p") {
            return Ok(Self::invalid_parameter("Invalid PAR access path."));
        }
        let Some(token) = path_parts.next().filter(|value| !value.is_empty()) else {
            return Ok(Self::invalid_parameter("Invalid PAR access path."));
        };
        let Some(par) =
            state::load_json::<OciPreauthenticatedRequest>(storage.as_ref(), OCI_PAR_STATE, token)?
        else {
            return Ok(Self::error_response(
                StatusCode::NOT_FOUND,
                "NotAuthorizedOrNotFound",
                "The pre-authenticated request does not exist.",
            ));
        };
        if par.time_expires <= chrono::Utc::now() {
            return Ok(Self::error_response(
                StatusCode::UNAUTHORIZED,
                "NotAuthenticated",
                "The pre-authenticated request has expired.",
            ));
        }
        let suffix = format!("/{}", path_parts.collect::<Vec<_>>().join("/"));
        let multipart_prefix = format!("/n/{OCI_NAMESPACE}/b/{}/u/", par.bucket);
        if let Some(multipart_suffix) = suffix.strip_prefix(&multipart_prefix) {
            if !par.access_type.contains("Write") {
                return Ok(Self::error_response(
                    StatusCode::FORBIDDEN,
                    "NotAuthorizedOrNotFound",
                    "The pre-authenticated request does not allow writes.",
                ));
            }
            let Some((raw_object, upload_suffix)) = multipart_suffix.split_once("/id/") else {
                return Ok(Self::invalid_parameter(
                    "The multipart PAR access path is invalid.",
                ));
            };
            let Ok(requested_object) = Self::decode_object_path(raw_object) else {
                return Ok(Self::invalid_parameter("The object name is invalid."));
            };
            let mut multipart = upload_suffix.split('/');
            let Some(upload_id) = multipart.next().filter(|value| !value.is_empty()) else {
                return Ok(Self::invalid_parameter(
                    "The multipart upload ID is required.",
                ));
            };
            let upload = match storage.get_multipart_upload(&par.bucket, upload_id) {
                Ok(upload) => upload,
                Err(crate::error::Error::NoSuchUpload | crate::error::Error::InvalidUploadId) => {
                    return Ok(Self::multipart_upload_not_found())
                }
                Err(error) => return Err(error.to_string()),
            };
            if requested_object != upload.key
                || par
                    .object_name
                    .as_deref()
                    .is_some_and(|object| object != upload.key)
            {
                return Ok(Self::error_response(
                    StatusCode::FORBIDDEN,
                    "NotAuthorizedOrNotFound",
                    "The multipart upload is outside the PAR object scope.",
                ));
            }
            return match *req.method() {
                Method::PUT => {
                    let Some(part_number) = multipart
                        .next()
                        .or_else(|| req.query_param("uploadPartNum"))
                    else {
                        return Ok(Self::invalid_parameter(
                            "The upload part number is required.",
                        ));
                    };
                    let mut part_request = req.clone();
                    part_request
                        .query_params
                        .insert("uploadPartNum".to_string(), part_number.to_string());
                    Self::upload_multipart_part(storage, &part_request, &par.bucket, upload_id)
                }
                Method::POST => {
                    Self::commit_multipart_upload(storage, req, &par.bucket, &upload.key, upload_id)
                }
                Method::DELETE => storage
                    .abort_multipart_upload(&par.bucket, upload_id)
                    .map(|()| Self::response(StatusCode::NO_CONTENT).empty())
                    .map_err(|error| error.to_string()),
                _ => Ok(Self::error_response(
                    StatusCode::METHOD_NOT_ALLOWED,
                    "MethodNotAllowed",
                    "Unsupported PAR multipart operation.",
                )),
            };
        }

        if let Some(object) = &par.object_name {
            let expected = format!(
                "/n/{OCI_NAMESPACE}/b/{}/o/{}",
                par.bucket,
                Self::encode_object_path(object)
            );
            if suffix != expected {
                return Ok(Self::error_response(
                    StatusCode::FORBIDDEN,
                    "NotAuthorizedOrNotFound",
                    "The requested object is outside the PAR object scope.",
                ));
            }
        }

        let object = if let Some(object) = &par.object_name {
            object.clone()
        } else {
            let marker = format!("/n/{}/b/{}/o/", OCI_NAMESPACE, par.bucket);
            let Some(raw_object) = suffix.strip_prefix(&marker) else {
                return Ok(Self::error_response(
                    StatusCode::FORBIDDEN,
                    "NotAuthorizedOrNotFound",
                    "The requested object is outside the PAR scope.",
                ));
            };
            Self::decode_object_path(raw_object)?
        };
        if req.method() == Method::PUT && req.header("opc-multipart") == Some("true") {
            if !par.access_type.contains("Write") {
                return Ok(Self::error_response(
                    StatusCode::FORBIDDEN,
                    "NotAuthorizedOrNotFound",
                    "The pre-authenticated request does not allow writes.",
                ));
            }
            let upload = storage
                .as_ref()
                .create_upload_session(CreateUploadSessionRequest {
                    namespace: par.bucket.clone(),
                    key: object.clone(),
                    content_type: req.header("content-type").map(str::to_string),
                    metadata: Self::metadata_from_headers(req),
                    provider_metadata: Self::multipart_provider_metadata("Standard"),
                })
                .map_err(|error| error.to_string())?;
            return Ok(Self::json_response(
                StatusCode::OK,
                &serde_json::json!({
                    "namespace": OCI_NAMESPACE,
                    "bucket": par.bucket,
                    "object": object,
                    "uploadId": upload.upload_id,
                    "timeCreated": upload.initiated.to_rfc3339(),
                    "storageTier": "Standard",
                    "accessUri": format!(
                        "/p/{token}/n/{OCI_NAMESPACE}/b/{}/u/{}/id/{}/",
                        par.bucket,
                        Self::encode_object_path(&object),
                        upload.upload_id
                    ),
                })
                .to_string(),
            ));
        }
        match *req.method() {
            Method::PUT if par.access_type.contains("Write") => {
                Self::put_object(storage, req, &par.bucket, &object)
            }
            Method::GET if par.access_type.contains("Read") => {
                Self::get_object(storage, req, &par.bucket, &object)
            }
            Method::HEAD if par.access_type.contains("Read") => {
                Self::head_object(storage, req, &par.bucket, &object)
            }
            _ => Ok(Self::error_response(
                StatusCode::FORBIDDEN,
                "NotAuthorizedOrNotFound",
                "The pre-authenticated request does not allow this operation.",
            )),
        }
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
        if let Some(response) = Self::validate_bucket_document(&payload) {
            return Ok(response);
        }
        let Some(bucket) = payload.get("name").and_then(|value| value.as_str()) else {
            return Ok(Self::invalid_parameter("The bucket name is required."));
        };
        if !Self::valid_bucket_name(bucket) {
            return Ok(Self::invalid_parameter(
                "The bucket name may contain only letters, numbers, dashes, underscores, and periods.",
            ));
        }
        let storage_tier = payload
            .get("storageTier")
            .and_then(|value| value.as_str())
            .unwrap_or("Standard");
        if !Self::valid_bucket_storage_tier(storage_tier) {
            return Ok(Self::invalid_parameter(
                "The storageTier value must be Standard or Archive.",
            ));
        }
        if let Err(error) = Self::create_bucket(storage, bucket, storage_tier, &payload) {
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
                "compartmentId": payload.get("compartmentId"),
                "metadata": payload.get("metadata").cloned().unwrap_or_else(|| serde_json::json!({})),
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
                        "compartmentId": namespace_record.metadata.get(OCI_BUCKET_COMPARTMENT_KEY),
                        "metadata": namespace_record.metadata.get(OCI_BUCKET_METADATA_KEY).map(|value| serde_json::from_str::<serde_json::Value>(value)).transpose().map_err(|error| error.to_string())?.unwrap_or_else(|| serde_json::json!({})),
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
        if let Some(response) = Self::validate_multipart_document(&payload) {
            return Ok(response);
        }
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
        let metadata = payload
            .get("metadata")
            .map(|value| serde_json::from_value::<HashMap<String, String>>(value.clone()))
            .transpose()
            .map_err(|error| crate::error::Error::InvalidRequest(error.to_string()))?
            .unwrap_or_default();
        let mut provider_metadata = Self::multipart_provider_metadata(storage_tier);
        for (field, key) in [
            ("cacheControl", OCI_CACHE_CONTROL_KEY),
            ("contentDisposition", OCI_CONTENT_DISPOSITION_KEY),
            ("contentEncoding", OCI_CONTENT_ENCODING_KEY),
            ("contentLanguage", OCI_CONTENT_LANGUAGE_KEY),
        ] {
            if let Some(value) = payload.get(field).and_then(serde_json::Value::as_str) {
                provider_metadata.insert(key.to_string(), value.to_string());
            }
        }
        storage
            .as_ref()
            .create_upload_session(CreateUploadSessionRequest {
                namespace: bucket.to_string(),
                key: object.to_string(),
                content_type,
                metadata,
                provider_metadata,
            })
    }

    fn multipart_provider_metadata(storage_tier: &str) -> HashMap<String, String> {
        HashMap::from([
            ("storage_tier".to_string(), storage_tier.to_string()),
            ("storage_class".to_string(), storage_tier.to_string()),
            (
                MULTIPART_MAX_OBJECT_SIZE_KEY.to_string(),
                OCI_MAX_OBJECT_SIZE.to_string(),
            ),
            (
                MULTIPART_MAX_PART_SIZE_KEY.to_string(),
                OCI_MAX_PART_SIZE.to_string(),
            ),
            (
                MULTIPART_MIN_NON_FINAL_PART_SIZE_KEY.to_string(),
                OCI_MIN_NON_FINAL_PART_SIZE.to_string(),
            ),
        ])
    }

    fn upload_multipart_part(
        storage: &Arc<dyn Storage>,
        req: &Request,
        bucket: &str,
        upload_id: &str,
    ) -> Result<Response<Body>, String> {
        if req.payload_len() > 50 * 1024 * 1024 * 1024 {
            return Ok(Self::payload_too_large_response(50 * 1024 * 1024 * 1024));
        }
        if let Err(response) = Self::put_provider_metadata(req) {
            return Ok(response);
        }
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
        let content_md5 = BASE64.encode(req.payload_md5());
        let upload_result = if let Some(payload) = &req.spooled_body {
            storage.upload_part_streamed(
                bucket,
                upload_id,
                part_number,
                &payload.path,
                payload.len,
                hex::encode(payload.md5),
            )
        } else {
            storage
                .as_ref()
                .upload_session_part(bucket, upload_id, part_number, req.body.to_vec())
        };
        let etag = match upload_result {
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
        Ok(Self::response(StatusCode::OK)
            .header("etag", &etag)
            .header("opc-content-md5", &content_md5)
            .empty())
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

    #[allow(clippy::too_many_lines)]
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
        if req.payload_len() > 50 * 1024 * 1024 * 1024 {
            return Ok(Self::payload_too_large_response(50 * 1024 * 1024 * 1024));
        }
        let condition = match Self::put_condition(req) {
            Ok(condition) => condition,
            Err(response) => return Ok(response),
        };
        let mut value = if let Some(payload) = &req.spooled_body {
            let mut value = crate::models::Object::new_with_metadata_and_etag(
                object.to_string(),
                Vec::new(),
                req.header("content-type")
                    .unwrap_or("application/octet-stream")
                    .to_string(),
                Self::metadata_from_headers(req),
                hex::encode(payload.md5),
            );
            value.size = payload.len;
            value
        } else {
            crate::models::Object::new_with_metadata(
                object.to_string(),
                req.body.to_vec(),
                req.header("content-type")
                    .unwrap_or("application/octet-stream")
                    .to_string(),
                Self::metadata_from_headers(req),
            )
        };
        value.provider_metadata = provider_metadata;
        value.storage_class = storage_tier;
        let written = if let Some(payload) = &req.spooled_body {
            let result = if let Some(condition) = condition.as_ref() {
                storage.put_object_streamed_if(
                    bucket,
                    object.to_string(),
                    value,
                    &payload.path,
                    condition,
                )
            } else {
                storage
                    .put_object_streamed(bucket, object.to_string(), value, &payload.path)
                    .map(|()| true)
            };
            match result {
                Ok(written) => written,
                Err(crate::error::Error::BucketNotFound) => return Ok(Self::bucket_not_found()),
                Err(error) => return Err(error.to_string()),
            }
        } else if let Some(condition) = condition {
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
        let stored = match storage.get_object_metadata(bucket, object) {
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
        let metadata = match storage.get_object_metadata(bucket, object) {
            Ok(blob) => blob,
            Err(crate::error::Error::KeyNotFound) => return Ok(Self::object_not_found()),
            Err(crate::error::Error::BucketNotFound) => return Ok(Self::bucket_not_found()),
            Err(err) => return Err(err.to_string()),
        };
        match Self::read_condition(req, &metadata) {
            Ok(Some(response)) | Err(response) => return Ok(response),
            Ok(None) => {}
        }
        if let Some(range_header) = req.header("range") {
            return Self::object_range_response(storage, req, bucket, object, range_header);
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
        Ok(Self::object_response(StatusCode::OK, &blob)
            .body(blob.data)
            .build())
    }

    fn object_range_response(
        storage: &Arc<dyn Storage>,
        req: &Request,
        bucket: &str,
        object: &str,
        range_header: &str,
    ) -> Result<Response<Body>, String> {
        if let Some((start, end)) = crate::utils::request::parse_byte_range(range_header) {
            let (blob, data) = match storage.get_object_range(bucket, object, start, end) {
                Ok(payload) => payload,
                Err(crate::error::Error::InvalidRequest(_)) => {
                    return Self::object_range_error_response(storage, req, bucket, object)
                }
                Err(crate::error::Error::KeyNotFound) => return Ok(Self::object_not_found()),
                Err(error) => return Err(error.to_string()),
            };
            match Self::read_condition(req, &blob) {
                Ok(Some(response)) | Err(response) => return Ok(response),
                Ok(None) => {}
            }
            let end = start + data.len() as u64 - 1;
            return Ok(Self::object_response(StatusCode::PARTIAL_CONTENT, &blob)
                .header("content-length", &data.len().to_string())
                .header(
                    "content-range",
                    &format!("bytes {start}-{end}/{}", blob.size),
                )
                .body(data)
                .build());
        }
        Self::object_range_error_response(storage, req, bucket, object)
    }

    fn object_range_error_response(
        storage: &Arc<dyn Storage>,
        req: &Request,
        bucket: &str,
        object: &str,
    ) -> Result<Response<Body>, String> {
        // Error responses have no payload; use a metadata-only snapshot to
        // preserve resource and condition precedence without loading the blob.
        let blob = match storage.get_object_metadata(bucket, object) {
            Ok(blob) => blob,
            Err(crate::error::Error::KeyNotFound) => return Ok(Self::object_not_found()),
            Err(crate::error::Error::BucketNotFound) => return Ok(Self::bucket_not_found()),
            Err(error) => return Err(error.to_string()),
        };
        match Self::read_condition(req, &blob) {
            Ok(Some(response)) | Err(response) => Ok(response),
            Ok(None) => Ok(Self::invalid_range_response()),
        }
    }

    fn invalid_range_response() -> Response<Body> {
        Self::error_response(
            StatusCode::RANGE_NOT_SATISFIABLE,
            "InvalidRange",
            "The requested range is not satisfiable",
        )
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
        let blob = match storage.get_object_metadata(bucket, object) {
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
            || req.path().starts_with("/p/")
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

    #[test]
    fn should_apply_oci_bucket_naming_rules() {
        // Arrange
        let valid_names = ["A", "Bucket_name-01", "bucket.example"];
        let invalid_names = ["", "bad bucket", "bad/bucket", "bucket!"];

        // Act
        let valid_results = valid_names.map(OciAdapter::valid_bucket_name);
        let invalid_results = invalid_names.map(OciAdapter::valid_bucket_name);

        // Assert
        assert_eq!(valid_results, [true; 3]);
        assert_eq!(invalid_results, [false; 4]);
    }

    #[tokio::test]
    async fn should_reject_invalid_oci_bucket_name_without_mutation() {
        let adapter = OciAdapter::new();
        let storage = temp_storage();
        let request = parsed_request(
            "POST",
            "http://localhost/n/sqrzl-emulator/b",
            &[("content-type", "application/json")],
            br#"{"name":"bad bucket!","compartmentId":"ignored"}"#,
        )
        .await;

        let response = adapter
            .handle_request(&storage, &auth_disabled(), &request)
            .expect("invalid bucket request should complete");

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert!(storage.get_namespace("bad bucket!").is_err());
    }

    #[tokio::test]
    async fn should_reject_unsupported_oci_bucket_configuration_before_mutation() {
        let storage = temp_storage();
        for (field, value) in [
            ("versioning", serde_json::json!("Enabled")),
            ("kmsKeyId", serde_json::json!("key")),
            ("isBucketKeyEnabled", serde_json::json!(true)),
            ("publicAccessType", serde_json::json!("ObjectRead")),
            ("freeformTags", serde_json::json!({"owner": "test"})),
            (
                "definedTags",
                serde_json::json!({"operations": {"owner": "test"}}),
            ),
            ("objectEventsEnabled", serde_json::json!(true)),
            ("autoTiering", serde_json::json!("InfrequentAccess")),
            ("bucketScope", serde_json::json!("REGION")),
            ("lifecycle", serde_json::json!({})),
            ("retentionRules", serde_json::json!([])),
        ] {
            let mut payload = serde_json::json!({"name": "unsupported-config", "compartmentId": "local-compartment"});
            payload[field] = value;
            let request = parsed_request(
                "POST",
                "http://localhost/n/sqrzl-emulator/b",
                &[],
                &serde_json::to_vec(&payload).unwrap(),
            )
            .await;
            let response = OciAdapter::new()
                .handle_request(&storage, &auth_disabled(), &request)
                .unwrap();
            let unknown = matches!(field, "lifecycle" | "retentionRules");
            assert_oci_error_response(
                response,
                if unknown {
                    StatusCode::BAD_REQUEST
                } else {
                    StatusCode::NOT_IMPLEMENTED
                },
                if unknown {
                    "InvalidParameter"
                } else {
                    "NotImplemented"
                },
            )
            .await;
            assert!(
                !storage.bucket_exists("unsupported-config").unwrap(),
                "{field}"
            );
        }
    }

    #[tokio::test]
    async fn should_reject_malformed_oci_bucket_documents_before_defaults_or_mutation() {
        let storage = temp_storage();
        for field in ["storageTier", "compartmentId", "metadata"] {
            for value in [
                serde_json::Value::Null,
                serde_json::json!(123),
                serde_json::json!([]),
            ] {
                let mut payload = serde_json::json!({"name": "invalid-config"});
                payload[field] = value;
                let request = parsed_request(
                    "POST",
                    "http://localhost/n/sqrzl-emulator/b",
                    &[],
                    &serde_json::to_vec(&payload).unwrap(),
                )
                .await;
                let response = OciAdapter::new()
                    .handle_request(&storage, &auth_disabled(), &request)
                    .unwrap();
                assert_oci_error_response(response, StatusCode::BAD_REQUEST, "InvalidParameter")
                    .await;
                assert!(!storage.bucket_exists("invalid-config").unwrap());
            }
        }
        for payload in [
            serde_json::json!([]),
            serde_json::json!({"name": "invalid-config", "unexpected": true}),
            serde_json::json!({"name": "invalid-config", "metadata": {"valid": "yes", "invalid": 123}}),
        ] {
            let request = parsed_request(
                "POST",
                "http://localhost/n/sqrzl-emulator/b",
                &[],
                &serde_json::to_vec(&payload).unwrap(),
            )
            .await;
            let response = OciAdapter::new()
                .handle_request(&storage, &auth_disabled(), &request)
                .unwrap();
            assert_oci_error_response(response, StatusCode::BAD_REQUEST, "InvalidParameter").await;
            assert!(!storage.bucket_exists("invalid-config").unwrap());
        }
    }

    #[tokio::test]
    async fn should_reject_malformed_oci_multipart_documents_before_sessions() {
        let storage = temp_storage();
        storage
            .create_bucket("typed-multipart".to_string())
            .unwrap();
        for field in [
            "contentType",
            "storageTier",
            "metadata",
            "cacheControl",
            "contentDisposition",
            "contentEncoding",
            "contentLanguage",
        ] {
            for value in [
                serde_json::Value::Null,
                serde_json::json!(123),
                serde_json::json!([]),
            ] {
                let mut payload = serde_json::json!({"object": "test.bin"});
                payload[field] = value;
                let request = parsed_request(
                    "POST",
                    "http://localhost/n/sqrzl-emulator/b/typed-multipart/u",
                    &[],
                    &serde_json::to_vec(&payload).unwrap(),
                )
                .await;
                let response = OciAdapter::new()
                    .handle_request(&storage, &auth_disabled(), &request)
                    .unwrap();
                assert_oci_error_response(response, StatusCode::BAD_REQUEST, "InvalidParameter")
                    .await;
                assert!(storage
                    .list_multipart_uploads("typed-multipart")
                    .unwrap()
                    .is_empty());
            }
        }
        for payload in [
            serde_json::json!({"object":"test.bin", "metadata":{"valid":"yes", "invalid":123}}),
            serde_json::json!({"object":"test.bin", "unexpected":true}),
        ] {
            let request = parsed_request(
                "POST",
                "http://localhost/n/sqrzl-emulator/b/typed-multipart/u",
                &[],
                &serde_json::to_vec(&payload).unwrap(),
            )
            .await;
            let response = OciAdapter::new()
                .handle_request(&storage, &auth_disabled(), &request)
                .unwrap();
            assert_oci_error_response(response, StatusCode::BAD_REQUEST, "InvalidParameter").await;
            assert!(storage
                .list_multipart_uploads("typed-multipart")
                .unwrap()
                .is_empty());
        }
        for payload in [
            serde_json::json!({"object":"test.bin", "contentType":"text/plain\r\nInjected: yes"}),
            serde_json::json!({"object":"test.bin", "metadata":{"owner":"test\r\nInjected: yes"}}),
            serde_json::json!({"object":"test.bin", "metadata":{"bad key":"test"}}),
        ] {
            let request = parsed_request(
                "POST",
                "http://localhost/n/sqrzl-emulator/b/typed-multipart/u",
                &[],
                &serde_json::to_vec(&payload).unwrap(),
            )
            .await;
            let response = OciAdapter::new()
                .handle_request(&storage, &auth_disabled(), &request)
                .unwrap();
            assert_oci_error_response(response, StatusCode::BAD_REQUEST, "InvalidParameter").await;
            assert!(storage
                .list_multipart_uploads("typed-multipart")
                .unwrap()
                .is_empty());
        }
    }

    #[tokio::test]
    async fn should_preserve_oci_multipart_content_properties_through_completion_and_restart() {
        let base =
            std::env::temp_dir().join(format!("sqrzl-oci-properties-{}", uuid::Uuid::new_v4()));
        let storage: Arc<dyn Storage> = Arc::new(FilesystemStorage::new(&base));
        storage
            .create_bucket("property-bucket".to_string())
            .unwrap();
        let request = parsed_request("POST", "http://localhost/n/sqrzl-emulator/b/property-bucket/u", &[], br#"{"object":"test.bin","contentType":"text/plain","cacheControl":"no-cache","contentDisposition":"inline","contentEncoding":"gzip","contentLanguage":"en","metadata":{"owner":"sdk"}}"#).await;
        let response = OciAdapter::new()
            .handle_request(&storage, &auth_disabled(), &request)
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body: serde_json::Value =
            serde_json::from_slice(&read_test_body(response).await).unwrap();
        let id = body["uploadId"].as_str().unwrap();
        let etag = storage
            .upload_part("property-bucket", id, 1, b"data".to_vec())
            .unwrap();
        // Restart both before and after completion; properties are session state,
        // then durable object metadata, never inferred from the completion request.
        drop(storage);
        let storage: Arc<dyn Storage> = Arc::new(FilesystemStorage::new(&base));
        let commit = serde_json::json!({"partsToCommit":[{"partNum":1,"etag":etag}]});
        let request = parsed_request(
            "POST",
            &format!(
                "http://localhost/n/sqrzl-emulator/b/property-bucket/u/test.bin?uploadId={id}"
            ),
            &[],
            &serde_json::to_vec(&commit).unwrap(),
        )
        .await;
        let response = OciAdapter::new()
            .handle_request(&storage, &auth_disabled(), &request)
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        drop(storage);
        let storage: Arc<dyn Storage> = Arc::new(FilesystemStorage::new(&base));
        for method in ["GET", "HEAD"] {
            let request = parsed_request(
                method,
                "http://localhost/n/sqrzl-emulator/b/property-bucket/o/test.bin",
                &[],
                b"",
            )
            .await;
            let response = OciAdapter::new()
                .handle_request(&storage, &auth_disabled(), &request)
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            for (header, value) in [
                ("content-type", "text/plain"),
                ("cache-control", "no-cache"),
                ("content-disposition", "inline"),
                ("content-encoding", "gzip"),
                ("content-language", "en"),
                ("opc-meta-owner", "sdk"),
            ] {
                assert_eq!(
                    response
                        .headers()
                        .get(header)
                        .and_then(|value| value.to_str().ok()),
                    Some(value)
                );
            }
        }
        drop(storage);
        fs::remove_dir_all(base).unwrap();
    }

    #[tokio::test]
    async fn should_round_trip_typed_oci_bucket_metadata_and_compartment_after_restart() {
        let base =
            std::env::temp_dir().join(format!("sqrzl-oci-bucket-fields-{}", uuid::Uuid::new_v4()));
        let storage: Arc<dyn Storage> = Arc::new(FilesystemStorage::new(&base));
        let request = parsed_request("POST", "http://localhost/n/sqrzl-emulator/b", &[], br#"{"name":"roundtrip-bucket","compartmentId":"local-compartment","metadata":{"owner":"sdk"},"storageTier":"Archive"}"#).await;
        let response = OciAdapter::new()
            .handle_request(&storage, &auth_disabled(), &request)
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        drop(storage);
        let storage: Arc<dyn Storage> = Arc::new(FilesystemStorage::new(&base));
        let request = parsed_request(
            "GET",
            "http://localhost/n/sqrzl-emulator/b/roundtrip-bucket",
            &[],
            b"",
        )
        .await;
        let response = OciAdapter::new()
            .handle_request(&storage, &auth_disabled(), &request)
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body: serde_json::Value =
            serde_json::from_slice(&read_test_body(response).await).unwrap();
        assert_eq!(body["compartmentId"], "local-compartment");
        assert_eq!(body["metadata"], serde_json::json!({"owner":"sdk"}));
        assert_eq!(body["storageTier"], "Archive");
        drop(storage);
        fs::remove_dir_all(base).unwrap();
    }

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
            vendor_credentials: crate::config::VendorCredentials::default(),
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
            vendor_credentials: crate::config::VendorCredentials::default(),
        })
    }

    fn oci_rsa_auth(public_key_path: &std::path::Path) -> Arc<AuthConfig> {
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
            vendor_credentials: crate::config::VendorCredentials {
                oci_tenancy_ocid: Some("ocid1.tenancy".to_string()),
                oci_user_ocid: Some("ocid1.user".to_string()),
                oci_key_fingerprint: Some("fingerprint".to_string()),
                oci_public_key_path: Some(public_key_path.display().to_string()),
                ..Default::default()
            },
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
                    "http://localhost/n/sqrzl-emulator/b",
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
                    "http://localhost/n/sqrzl-emulator/b/archive/o/report.txt",
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
                &parsed_request(
                    "GET",
                    "http://localhost/n/sqrzl-emulator/b/archive/o",
                    &[],
                    b"",
                )
                .await,
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
                    "http://localhost/n/sqrzl-emulator/b/archive/o/report.txt",
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
            "http://localhost/n/sqrzl-emulator",
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
    async fn should_verify_oci_rsa_signatures_and_reject_a_tampered_target() {
        // Arrange
        use rsa::pkcs1v15::SigningKey;
        use rsa::pkcs8::{EncodePublicKey, LineEnding};
        use rsa::signature::{SignatureEncoding, Signer};

        let adapter = OciAdapter::new();
        let storage = temp_storage();
        let private_key = rsa::RsaPrivateKey::new(&mut rsa::rand_core::OsRng, 2048)
            .expect("test RSA key should generate");
        let public_key_path =
            std::env::temp_dir().join(format!("sqrzl-oci-public-key-{}.pem", uuid::Uuid::new_v4()));
        std::fs::write(
            &public_key_path,
            private_key
                .to_public_key()
                .to_public_key_pem(LineEnding::LF)
                .expect("public key PEM should encode"),
        )
        .expect("public key PEM should write");
        let date = chrono::Utc::now().to_rfc2822();
        let signed_headers = "(request-target) host date";
        let signing_text =
            format!("(request-target): get /n/\nhost: objectstorage.localhost\ndate: {date}");
        let signature = SigningKey::<RsaSha256>::new(private_key).sign(signing_text.as_bytes());
        let authorization = format!(
            "Signature version=\"1\",keyId=\"ocid1.tenancy/ocid1.user/fingerprint\",algorithm=\"rsa-sha256\",headers=\"{signed_headers}\",signature=\"{}\"",
            BASE64.encode(signature.to_bytes())
        );
        let auth = oci_rsa_auth(&public_key_path);
        let mut valid = parsed_request(
            "GET",
            "http://localhost/n/",
            &[("date", &date), ("host", "objectstorage.localhost")],
            b"",
        )
        .await;
        valid.headers.insert(
            "authorization",
            authorization.parse().expect("authorization should parse"),
        );
        let mut tampered = valid.clone();
        tampered.uri = "http://localhost/n/?changed=true"
            .parse()
            .expect("tampered URI should parse");

        // Act
        let accepted = adapter
            .handle_request(&storage, &auth, &valid)
            .expect("valid signed request should complete");
        let rejected = adapter
            .handle_request(&storage, &auth, &tampered)
            .expect("tampered request should complete");

        // Assert
        assert_eq!(accepted.status(), StatusCode::OK);
        assert_eq!(rejected.status(), StatusCode::UNAUTHORIZED);
        let _ = std::fs::remove_file(public_key_path);
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
                    "http://localhost/n/sqrzl-emulator/b",
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
                    "http://localhost/n/sqrzl-emulator/b/archive/o/folder/report.txt",
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
                    "http://localhost/n/sqrzl-emulator/b/archive/o?prefix=folder/&fields=name,timeCreated",
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
                    "http://localhost/n/sqrzl-emulator/b/archive/o/folder/report.txt",
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
                    "http://localhost/n/sqrzl-emulator/b",
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
                    "http://localhost/n/sqrzl-emulator/b/range-bucket/o/hello.txt",
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
                    "http://localhost/n/sqrzl-emulator/b/range-bucket/o/hello.txt",
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
            &Uri::from_static("http://localhost/n/sqrzl-emulator/b/bucket/o/object"),
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
                &parsed_request("GET", "http://localhost/n/wrong-namespace/b", &[], b"").await,
            )
            .expect("wrong namespace should return an OCI response");
        assert_oci_error_response(response, StatusCode::NOT_FOUND, "NotAuthorizedOrNotFound").await;

        let response = adapter
            .handle_request(
                &storage.clone(),
                &auth_disabled(),
                &parsed_request("GET", "http://localhost/n/sqrzl-emulator", &[], b"").await,
            )
            .expect("namespace metadata request should return an OCI response");
        assert_oci_error_response(response, StatusCode::NOT_IMPLEMENTED, "NotImplemented").await;

        let response = adapter
            .handle_request(
                &storage.clone(),
                &auth_disabled(),
                &parsed_request(
                    "POST",
                    "http://localhost/n/sqrzl-emulator/b",
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
                &parsed_request(
                    "GET",
                    "http://localhost/n/sqrzl-emulator/b/sdk-bucket",
                    &[],
                    b"",
                )
                .await,
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
                    "http://localhost/n/sqrzl-emulator/b/put-alias",
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
                    "http://localhost/n/sqrzl-emulator/b",
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
                    "http://localhost/n/sqrzl-emulator/b/put-alias",
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
            "http://localhost/n/sqrzl-emulator/b/multipart-bucket/o/%FF",
            "http://localhost/n/sqrzl-emulator/b/multipart-bucket/o/%ZZ",
            "http://localhost/n/sqrzl-emulator/b/multipart-bucket/u/%FF?uploadId=missing&uploadPartNum=1",
            "http://localhost/n/sqrzl-emulator/b/multipart-bucket/u/%ZZ?uploadId=missing&uploadPartNum=1",
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
                        &format!(
                            "http://localhost/n/sqrzl-emulator/b/multipart-bucket/o/{encoded}"
                        ),
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
                    "http://localhost/n/sqrzl-emulator/b/multipart-bucket/o/a%20b",
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
                    "http://localhost/n/sqrzl-emulator/b/multipart-bucket/o/a%20b",
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
                    "http://localhost/n/sqrzl-emulator/b/multipart-bucket/o",
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
                    "http://localhost/n/sqrzl-emulator/b/multipart-bucket/o/a%20b",
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
                            "http://localhost/n/sqrzl-emulator/b/multipart-bucket/o/current?versionId=old",
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
                    "http://localhost/n/sqrzl-emulator/b/multipart-bucket/u",
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
                        "http://localhost/n/sqrzl-emulator/b/multipart-bucket/u/a//b?uploadId={upload_id}&uploadPartNum=1"
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
                        "http://localhost/n/sqrzl-emulator/b/multipart-bucket/u/a//b?uploadId={upload_id}"
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
                    "http://localhost/n/sqrzl-emulator/b/multipart-bucket/u/multi.txt?uploadPartNum=1",
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
                "http://localhost/n/sqrzl-emulator/b/multipart-bucket/u/multi.txt?uploadId={upload_id}"
            ),
            format!(
                "http://localhost/n/sqrzl-emulator/b/multipart-bucket/u/multi.txt?uploadId={upload_id}&uploadPartNum=0"
            ),
            format!(
                "http://localhost/n/sqrzl-emulator/b/multipart-bucket/u/multi.txt?uploadId={upload_id}&uploadPartNum=not-a-number"
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
                    "http://localhost/n/sqrzl-emulator/b/multipart-bucket/u/multi.txt?uploadId=missing&uploadPartNum=1",
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
                            "http://localhost/n/sqrzl-emulator/b/multipart-bucket/u/multi.txt?uploadId={upload_id}"
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
        let part_one = minimum_non_final_part(b'm');
        let part_one_etag = upload_oci_part(&adapter, &storage, &upload_id, 1, &part_one).await;
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
                        "http://localhost/n/sqrzl-emulator/b/multipart-bucket/u/multi.txt?uploadId={upload_id}"
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
                        "http://localhost/n/sqrzl-emulator/b/multipart-bucket/u/multi.txt?uploadId={upload_id}"
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
        let object = storage
            .get_object("multipart-bucket", "multi.txt")
            .expect("complete manifest should commit after rejected attempts");
        assert_eq!(object.data.len(), part_one.len() + b"part".len());
        assert!(object.data.starts_with(&part_one));
        assert!(object.data.ends_with(b"part"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn should_reject_unsupported_oci_multipart_conditions_without_consuming_session() {
        let adapter = OciAdapter::new();
        let storage = temp_storage();
        create_oci_multipart_bucket(&adapter, &storage).await;
        let upload_id = create_oci_multipart_upload(&adapter, &storage).await;
        let part_one = minimum_non_final_part(b'm');
        let part_one_etag = upload_oci_part(&adapter, &storage, &upload_id, 1, &part_one).await;
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
                            "http://localhost/n/sqrzl-emulator/b/multipart-bucket/u/multi.txt?uploadId={upload_id}"
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
        let object = storage
            .get_object("multipart-bucket", "multi.txt")
            .expect("unconditional retry should commit the object");
        assert_eq!(object.data.len(), part_one.len() + b"part".len());
        assert!(object.data.starts_with(&part_one));
        assert!(object.data.ends_with(b"part"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn should_support_oci_multipart_upload_lifecycle() {
        let adapter = OciAdapter::new();
        let storage = temp_storage();

        create_oci_multipart_bucket(&adapter, &storage).await;
        let upload_id = create_oci_multipart_upload(&adapter, &storage).await;
        let part_one = minimum_non_final_part(b'm');
        let part_one_etag = upload_oci_part(&adapter, &storage, &upload_id, 1, &part_one).await;
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

    #[tokio::test(flavor = "multi_thread")]
    async fn should_reject_an_object_par_used_for_a_different_path() {
        // Arrange
        let adapter = OciAdapter::new();
        let storage = temp_storage();
        create_oci_multipart_bucket(&adapter, &storage).await;
        let expires = (chrono::Utc::now() + chrono::Duration::hours(1)).to_rfc3339();
        let created = adapter
            .handle_request(
                &storage,
                &auth_disabled(),
                &parsed_request(
                    "POST",
                    "http://localhost/n/sqrzl-emulator/b/multipart-bucket/p/",
                    &[("content-type", "application/json")],
                    serde_json::json!({
                        "name": "write-safe",
                        "accessType": "ObjectWrite",
                        "objectName": "safe.txt",
                        "timeExpires": expires,
                    })
                    .to_string()
                    .as_bytes(),
                )
                .await,
            )
            .expect("PAR creation should complete");
        let created: serde_json::Value =
            serde_json::from_slice(&read_test_body(created).await).expect("PAR JSON should parse");
        let access_uri = created["accessUri"]
            .as_str()
            .expect("PAR access URI should exist");
        let wrong_uri = format!(
            "http://localhost{}",
            access_uri.replace("safe.txt", "other.txt")
        );

        // Act
        let response = adapter
            .handle_request(
                &storage,
                &auth_disabled(),
                &parsed_request("PUT", &wrong_uri, &[], b"must not be written").await,
            )
            .expect("PAR request should complete");

        // Assert
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert!(storage.get_object("multipart-bucket", "safe.txt").is_err());
        assert!(storage.get_object("multipart-bucket", "other.txt").is_err());
    }

    #[tokio::test(flavor = "multi_thread")]
    #[allow(clippy::too_many_lines)]
    async fn should_upload_directly_and_in_parts_through_an_object_par() {
        // Arrange
        let adapter = OciAdapter::new();
        let storage = temp_storage();
        create_oci_multipart_bucket(&adapter, &storage).await;
        let expires = (chrono::Utc::now() + chrono::Duration::hours(1)).to_rfc3339();
        let created = adapter
            .handle_request(
                &storage,
                &auth_disabled(),
                &parsed_request(
                    "POST",
                    "http://localhost/n/sqrzl-emulator/b/multipart-bucket/p/",
                    &[("content-type", "application/json")],
                    serde_json::json!({
                        "name": "read-write-object",
                        "accessType": "ObjectReadWrite",
                        "objectName": "large.bin",
                        "timeExpires": expires,
                    })
                    .to_string()
                    .as_bytes(),
                )
                .await,
            )
            .expect("PAR creation should complete");
        let created: serde_json::Value =
            serde_json::from_slice(&read_test_body(created).await).expect("PAR JSON should parse");
        let access_uri = format!(
            "http://localhost{}",
            created["accessUri"]
                .as_str()
                .expect("PAR access URI should exist")
        );

        // Act
        let direct = adapter
            .handle_request(
                &storage,
                &auth_disabled(),
                &parsed_request("PUT", &access_uri, &[], b"direct").await,
            )
            .expect("direct PAR upload should complete");
        let read = adapter
            .handle_request(
                &storage,
                &auth_disabled(),
                &parsed_request("GET", &access_uri, &[], b"").await,
            )
            .expect("PAR read should complete");
        let initiated = adapter
            .handle_request(
                &storage,
                &auth_disabled(),
                &parsed_request("PUT", &access_uri, &[("opc-multipart", "true")], b"").await,
            )
            .expect("PAR multipart initiation should complete");
        let initiated: serde_json::Value = serde_json::from_slice(&read_test_body(initiated).await)
            .expect("multipart initiation JSON should parse");
        let multipart_uri = format!(
            "http://localhost{}",
            initiated["accessUri"]
                .as_str()
                .expect("multipart PAR URI should exist")
        );
        assert!(multipart_uri.contains("/n/sqrzl-emulator/b/multipart-bucket/u/large.bin/id/"));
        let first_part = minimum_non_final_part(b'l');
        let first = adapter
            .handle_request(
                &storage,
                &auth_disabled(),
                &parsed_request("PUT", &format!("{multipart_uri}1"), &[], &first_part).await,
            )
            .expect("first PAR part should complete");
        let second = adapter
            .handle_request(
                &storage,
                &auth_disabled(),
                &parsed_request("PUT", &format!("{multipart_uri}2"), &[], b"file").await,
            )
            .expect("second PAR part should complete");
        let manifest = serde_json::json!({
            "partsToCommit": [
                {"partNum": 1, "etag": first.headers()["etag"].to_str().unwrap()},
                {"partNum": 2, "etag": second.headers()["etag"].to_str().unwrap()},
            ]
        });
        let committed = adapter
            .handle_request(
                &storage,
                &auth_disabled(),
                &parsed_request(
                    "POST",
                    &multipart_uri,
                    &[("content-type", "application/json")],
                    manifest.to_string().as_bytes(),
                )
                .await,
            )
            .expect("PAR multipart commit should complete");

        // Assert
        assert_eq!(direct.status(), StatusCode::OK);
        assert_eq!(read.status(), StatusCode::OK);
        assert_eq!(read_test_body(read).await, b"direct");
        assert_eq!(committed.status(), StatusCode::OK);
        let object = storage
            .get_object("multipart-bucket", "large.bin")
            .expect("committed PAR object should exist");
        assert_eq!(object.data.len(), first_part.len() + b"file".len());
        assert!(object.data.starts_with(&first_part));
        assert!(object.data.ends_with(b"file"));
    }

    async fn create_oci_multipart_bucket(adapter: &OciAdapter, storage: &Arc<dyn Storage>) {
        adapter
            .handle_request(
                &storage.clone(),
                &auth_disabled(),
                &parsed_request(
                    "POST",
                    "http://localhost/n/sqrzl-emulator/b",
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
                    "http://localhost/n/sqrzl-emulator/b/multipart-bucket/u",
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
                        "http://localhost/n/sqrzl-emulator/b/multipart-bucket/u/multi.txt?uploadId={upload_id}&uploadPartNum={part_number}"
                    ),
                    &[],
                    body,
                )
                .await,
            )
            .expect("part upload should succeed");
        let expected_md5 = BASE64.encode(md5::compute(body).0);
        assert_eq!(
            response
                .headers()
                .get("opc-content-md5")
                .and_then(|value| value.to_str().ok()),
            Some(expected_md5.as_str())
        );
        response
            .headers()
            .get("etag")
            .and_then(|value| value.to_str().ok())
            .expect("etag should exist")
            .to_string()
    }

    fn minimum_non_final_part(byte: u8) -> Vec<u8> {
        vec![
            byte;
            usize::try_from(OCI_MIN_NON_FINAL_PART_SIZE)
                .expect("OCI minimum part size should fit in memory")
        ]
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
                        "http://localhost/n/sqrzl-emulator/b/multipart-bucket/u/multi.txt?uploadId={upload_id}"
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
                    "http://localhost/n/sqrzl-emulator/b/multipart-bucket/o/multi.txt",
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
