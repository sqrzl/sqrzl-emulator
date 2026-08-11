use super::{data_protection_activation_lock, state, ProviderAdapter};
use crate::auth::{AuthConfig, HttpRequestLike};
use crate::blob::{BlobBackend, BlobRange};
use crate::body::Body;
use crate::server::{RequestExt as Request, ResponseBuilder};
use crate::storage::ObjectCondition;
use crate::storage::Storage;
use crate::utils::request_origin;
use crate::utils::xml::push_escaped_xml;
use base64::{
    engine::general_purpose::{STANDARD as BASE64, URL_SAFE_NO_PAD},
    Engine as _,
};
use hmac::{Hmac, KeyInit, Mac};
use http::{HeaderMap, Method, StatusCode, Uri};
use hyper::Response;
use serde::{Deserialize, Serialize};
use sha1::Sha1;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap};
use std::fmt::Write as _;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, Weak};

const GCS_GENERATION_KEY: &str = "__sqrzl_gcs_generation";
const GCS_METAGENERATION_KEY: &str = "__sqrzl_gcs_metageneration";
const GCS_UPDATED_KEY: &str = "__sqrzl_gcs_updated";
const GCS_CRC32C_KEY: &str = "__sqrzl_gcs_crc32c";
const GCS_RESUMABLE_SESSION_STATE: &str = "gcs-resumable-session";
const GCS_SOFT_DELETE_SECONDS_KEY: &str = "gcs_soft_delete_seconds";
const GCS_RETENTION_SECONDS_KEY: &str = "gcs_retention_seconds";
const S3_VERSIONING_STATUS_KEY: &str = "s3_versioning_status";
const S3_OBJECT_LOCK_ENABLED_KEY: &str = "s3_object_lock_enabled";
const AZURE_VERSIONING_KEY: &str = "azure_versioning_enabled";
const AZURE_SOFT_DELETE_DAYS_KEY: &str = "azure_soft_delete_days";
const GCS_MIN_SOFT_DELETE_SECONDS: u64 = 604_800;
const GCS_MAX_SOFT_DELETE_SECONDS_EXCLUSIVE: u64 = 7_776_000;
const GCS_MAX_RETENTION_SECONDS_EXCLUSIVE: u64 = 3_155_760_000;

#[derive(Clone, Serialize, Deserialize)]
struct ResumableSession {
    bucket: String,
    key: String,
    content_type: String,
    metadata: HashMap<String, String>,
    #[serde(default)]
    crc32c: Option<u32>,
    #[serde(default)]
    if_generation_match: Option<String>,
    #[serde(default)]
    if_generation_not_match: Option<String>,
}

pub struct GcsAdapter {
    resumable_sessions: Mutex<HashMap<String, ResumableSession>>,
    object_mutation_locks: Mutex<ObjectMutationLocks>,
}

impl Default for GcsAdapter {
    fn default() -> Self {
        Self::new()
    }
}

type MultipartUploadParts = (ParsedUploadMetadata, String, Vec<u8>);
type ObjectMutationLocks = HashMap<(String, String), Weak<Mutex<()>>>;

#[derive(Default)]
struct ParsedUploadMetadata {
    name: Option<String>,
    content_type: Option<String>,
    metadata: HashMap<String, String>,
    crc32c: Option<u32>,
}

enum UploadMetadataError {
    Invalid(String),
    ChecksumMismatch(String),
    Unsupported(String),
}

#[derive(Clone, Copy)]
struct GenerationPreconditions<'a> {
    expected: Option<&'a str>,
    rejected: Option<&'a str>,
}

struct BlobWrite<'a> {
    data: Vec<u8>,
    content_type: String,
    metadata: HashMap<String, String>,
    preconditions: GenerationPreconditions<'a>,
}

enum BlobWriteOutcome {
    Stored(Box<crate::blob::BlobRecord>),
    PreconditionFailed,
    RetentionPolicyNotMet,
}

impl GcsAdapter {
    #[must_use]
    pub fn new() -> Self {
        Self {
            resumable_sessions: Mutex::new(HashMap::new()),
            object_mutation_locks: Mutex::new(HashMap::new()),
        }
    }

    fn object_mutation_lock(&self, bucket: &str, key: &str) -> Result<Arc<Mutex<()>>, String> {
        let mut locks = self
            .object_mutation_locks
            .lock()
            .map_err(|_| "Failed to lock GCS object-mutation lock registry".to_string())?;
        locks.retain(|_, lock| lock.strong_count() > 0);
        let lock_key = (bucket.to_string(), key.to_string());
        if let Some(lock) = locks.get(&lock_key).and_then(Weak::upgrade) {
            return Ok(lock);
        }
        let lock = Arc::new(Mutex::new(()));
        locks.insert(lock_key, Arc::downgrade(&lock));
        Ok(lock)
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
            .unwrap_or("");
        let authorization = headers
            .get("authorization")
            .and_then(|value| value.to_str().ok())
            .unwrap_or("");
        let query = uri.query().unwrap_or("");

        Self::is_gcs_endpoint_host(host)
            || authorization.starts_with("GOOG1 ")
            || query.contains("GoogleAccessId=")
            || path.starts_with("/upload/storage/v1/")
            || path.starts_with("/upload/resumable/")
            || path.starts_with("/storage/v1/")
            || path.starts_with("/download/storage/v1/")
    }

    fn is_json_api_path(path: &str) -> bool {
        path.starts_with("/storage/v1/")
            || path.starts_with("/upload/storage/v1/")
            || path.starts_with("/upload/resumable/")
            || path.starts_with("/download/storage/v1/")
    }

    fn is_json_api_request(req: &Request) -> bool {
        Self::is_json_api_path(req.path())
            && req.host().and_then(Self::xml_virtual_host_bucket).is_none()
    }

    fn is_json_api_head(uri: &Uri, headers: &HeaderMap) -> bool {
        let virtual_hosted = headers
            .get("host")
            .and_then(|value| value.to_str().ok())
            .and_then(Self::xml_virtual_host_bucket)
            .is_some();
        Self::is_json_api_path(uri.path()) && !virtual_hosted
    }

    fn payload_too_large_response(json_api: bool, max_request_bytes: usize) -> Response<Body> {
        let message =
            format!("Request body exceeds SQRZL_MAX_REQUEST_BYTES ({max_request_bytes} bytes)");
        if json_api {
            return Self::json_upload_error(
                StatusCode::PAYLOAD_TOO_LARGE,
                "REQUEST_ENTITY_TOO_LARGE",
                &message,
            );
        }
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

    fn json_error(status: StatusCode, reason: &str, message: &str) -> Response<Body> {
        let domain = if reason == "rateLimitExceeded" {
            "usageLimits"
        } else {
            "global"
        };
        Self::json_response(
            status,
            &serde_json::json!({
                "error": {
                    "errors": [{
                        "domain": domain,
                        "reason": reason,
                        "message": message
                    }],
                    "code": status.as_u16(),
                    "message": message
                }
            })
            .to_string(),
        )
    }

    fn default_json_error_reason(status: StatusCode, message: &str) -> &'static str {
        match status {
            StatusCode::BAD_REQUEST if message.starts_with("Required parameter") => "required",
            StatusCode::BAD_REQUEST if message.contains("JSON") => "parseError",
            StatusCode::BAD_REQUEST if message.contains("parameter") => "invalidParameter",
            StatusCode::BAD_REQUEST => "invalidArgument",
            StatusCode::UNAUTHORIZED => "authError",
            StatusCode::FORBIDDEN => "forbidden",
            StatusCode::NOT_FOUND => "notFound",
            StatusCode::METHOD_NOT_ALLOWED => "methodNotAllowed",
            StatusCode::CONFLICT => "conflict",
            StatusCode::PRECONDITION_FAILED => "conditionNotMet",
            StatusCode::PAYLOAD_TOO_LARGE => "uploadTooLarge",
            StatusCode::TOO_MANY_REQUESTS => "rateLimitExceeded",
            StatusCode::INTERNAL_SERVER_ERROR => "internalError",
            StatusCode::SERVICE_UNAVAILABLE => "backendError",
            StatusCode::NOT_IMPLEMENTED => "notImplemented",
            _ => "unknown",
        }
    }

    fn error_response(status: StatusCode, code: &str, message: &str) -> Response<Body> {
        let body = format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?><Error><Code>{code}</Code><Message>{message}</Message></Error>"
        );
        Self::xml_response(status, body)
    }

    fn is_gcs_host(req: &Request) -> bool {
        req.host().is_some_and(Self::is_gcs_endpoint_host)
    }

    fn is_gcs_endpoint_host(host: &str) -> bool {
        let host = host.split(':').next().unwrap_or(host);
        host.eq_ignore_ascii_case("storage.googleapis.com")
            || host.eq_ignore_ascii_case("storage.localhost")
            || Self::xml_virtual_host_bucket(host).is_some()
    }

    fn xml_virtual_host_bucket(host: &str) -> Option<String> {
        let host = host.split(':').next().unwrap_or(host);
        let lowercase = host.to_ascii_lowercase();
        [".storage.googleapis.com", ".storage.localhost"]
            .into_iter()
            .find_map(|suffix| {
                let bucket = lowercase.strip_suffix(suffix)?;
                (!bucket.is_empty()).then(|| host[..bucket.len()].to_string())
            })
    }

    fn parse_path(req: &Request) -> Result<(Option<String>, Option<String>), String> {
        let path = req.path().strip_prefix('/').unwrap_or(req.path());
        if let Some(bucket) = req.host().and_then(Self::xml_virtual_host_bucket) {
            return if path.is_empty() {
                Ok((Some(bucket), None))
            } else {
                Ok((Some(bucket), Some(Self::decode_object_path(path)?)))
            };
        }
        if path.is_empty() {
            return Ok((None, None));
        }
        match path.split_once('/') {
            Some(("", _)) => Err("GCS XML paths must include a bucket name".to_string()),
            Some((bucket, "")) => Ok((Some(bucket.to_string()), None)),
            Some((bucket, encoded_object)) => Ok((
                Some(bucket.to_string()),
                Some(Self::decode_object_path(encoded_object)?),
            )),
            None => Ok((Some(path.to_string()), None)),
        }
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
                !matches!(
                    key.as_str(),
                    GCS_GENERATION_KEY | GCS_METAGENERATION_KEY | GCS_UPDATED_KEY
                )
            })
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect()
    }

    fn decode_object_path(path: &str) -> Result<String, String> {
        crate::utils::request::decode_uri_path(path)
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
        previous_updated: Option<&str>,
    ) -> HashMap<String, String> {
        metadata.retain(|key, _| {
            !matches!(
                key.as_str(),
                GCS_GENERATION_KEY | GCS_METAGENERATION_KEY | GCS_UPDATED_KEY
            )
        });
        metadata.insert(GCS_GENERATION_KEY.to_string(), generation);
        metadata.insert(GCS_METAGENERATION_KEY.to_string(), metageneration);
        metadata.insert(
            GCS_UPDATED_KEY.to_string(),
            Self::next_gcs_updated(previous_updated),
        );
        metadata
    }

    fn next_gcs_updated(previous: Option<&str>) -> String {
        let now = chrono::Utc::now();
        let updated = previous
            .and_then(|value| chrono::DateTime::parse_from_rfc3339(value).ok())
            .map(|previous| previous.with_timezone(&chrono::Utc))
            .map_or(now, |previous| {
                if now <= previous {
                    previous + chrono::Duration::milliseconds(1)
                } else {
                    now
                }
            });
        updated.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
    }

    fn gcs_updated(
        metadata: &HashMap<String, String>,
        fallback: chrono::DateTime<chrono::Utc>,
    ) -> String {
        metadata
            .get(GCS_UPDATED_KEY)
            .cloned()
            .unwrap_or_else(|| fallback.to_rfc3339_opts(chrono::SecondsFormat::Millis, true))
    }

    fn json_etag(metadata: &HashMap<String, String>, fallback: &str) -> String {
        match (
            metadata.get(GCS_GENERATION_KEY),
            metadata.get(GCS_METAGENERATION_KEY),
        ) {
            (Some(generation), Some(metageneration)) => {
                BASE64.encode(md5::compute(format!("{generation}:{metageneration}").as_bytes()).0)
            }
            _ => fallback.to_string(),
        }
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

    fn encoded_crc32c(data: &[u8]) -> String {
        BASE64.encode(Self::crc32c(data).to_be_bytes())
    }

    fn parse_crc32c(value: &serde_json::Value) -> Result<u32, UploadMetadataError> {
        let value = value.as_str().ok_or_else(|| {
            UploadMetadataError::Invalid(
                "Object metadata crc32c must be a base64-encoded string".to_string(),
            )
        })?;
        let decoded = BASE64.decode(value).map_err(|_| {
            UploadMetadataError::Invalid(
                "Object metadata crc32c must be base64-encoded big-endian bytes".to_string(),
            )
        })?;
        let bytes: [u8; 4] = decoded.try_into().map_err(|_| {
            UploadMetadataError::Invalid(
                "Object metadata crc32c must decode to exactly four bytes".to_string(),
            )
        })?;
        Ok(u32::from_be_bytes(bytes))
    }

    fn request_crc32c(req: &Request) -> Result<Option<u32>, UploadMetadataError> {
        let mut expected = None;
        for (_, value) in req
            .headers()
            .into_iter()
            .filter(|(name, _)| name.eq_ignore_ascii_case("x-goog-hash"))
        {
            if value.trim().is_empty() {
                return Err(UploadMetadataError::Invalid(
                    "X-Goog-Hash must contain a checksum token".to_string(),
                ));
            }
            for token in value.split(',').map(str::trim) {
                let (algorithm, encoded) = token.split_once('=').ok_or_else(|| {
                    UploadMetadataError::Invalid(
                        "X-Goog-Hash checksum tokens must use algorithm=value syntax".to_string(),
                    )
                })?;
                if !algorithm.eq_ignore_ascii_case("crc32c") {
                    return Err(UploadMetadataError::Unsupported(format!(
                        "X-Goog-Hash algorithm {algorithm} is not supported by this emulator surface"
                    )));
                }
                let decoded = BASE64.decode(encoded).map_err(|_| {
                    UploadMetadataError::Invalid(
                        "X-Goog-Hash crc32c must be base64-encoded big-endian bytes".to_string(),
                    )
                })?;
                let bytes: [u8; 4] = decoded.try_into().map_err(|_| {
                    UploadMetadataError::Invalid(
                        "X-Goog-Hash crc32c must decode to exactly four bytes".to_string(),
                    )
                })?;
                let parsed = u32::from_be_bytes(bytes);
                if expected.is_some_and(|expected| expected != parsed) {
                    return Err(UploadMetadataError::Invalid(
                        "X-Goog-Hash contains conflicting CRC32C values".to_string(),
                    ));
                }
                expected = Some(parsed);
            }
        }
        Ok(expected)
    }

    fn combined_crc32c(
        metadata_crc32c: Option<u32>,
        header_crc32c: Option<u32>,
    ) -> Result<Option<u32>, UploadMetadataError> {
        match (metadata_crc32c, header_crc32c) {
            (Some(metadata), Some(header)) if metadata != header => {
                Err(UploadMetadataError::Invalid(
                    "Object metadata and X-Goog-Hash contain conflicting CRC32C values".to_string(),
                ))
            }
            (Some(expected), _) | (_, Some(expected)) => Ok(Some(expected)),
            (None, None) => Ok(None),
        }
    }

    fn validate_crc32c(expected: Option<u32>, data: &[u8]) -> Result<(), UploadMetadataError> {
        let calculated = Self::crc32c(data);
        if let Some(expected) = expected.filter(|expected| *expected != calculated) {
            return Err(UploadMetadataError::ChecksumMismatch(format!(
                "Provided CRC32C \"{}\" does not match calculated CRC32C \"{}\"",
                BASE64.encode(expected.to_be_bytes()),
                BASE64.encode(calculated.to_be_bytes())
            )));
        }
        Ok(())
    }

    fn put_blob_with_generation_match(
        &self,
        storage: &Arc<dyn Storage>,
        bucket: &str,
        key: &str,
        write: BlobWrite<'_>,
    ) -> Result<BlobWriteOutcome, String> {
        let mutation_lock = self.object_mutation_lock(bucket, key)?;
        let _guard = mutation_lock
            .lock()
            .map_err(|_| "Failed to lock GCS object mutation".to_string())?;
        let existing = storage.get_object(bucket, key).ok();
        if existing
            .as_ref()
            .is_some_and(|object| Self::object_is_retained(storage, bucket, object))
        {
            return Ok(BlobWriteOutcome::RetentionPolicyNotMet);
        }
        let generation = Self::next_generation(existing.as_ref());
        let metadata =
            Self::metadata_with_gcs_state(write.metadata, generation, "1".to_string(), None);
        let crc32c = Self::encoded_crc32c(&write.data);
        let mut object = crate::models::Object::new_with_metadata(
            key.to_string(),
            write.data,
            write.content_type,
            metadata,
        );
        object
            .provider_metadata
            .insert(GCS_CRC32C_KEY.to_string(), crc32c);
        let written = match (write.preconditions.expected, write.preconditions.rejected) {
            (Some("0"), None) => {
                storage.put_object_if(bucket, key.to_string(), object, &ObjectCondition::Missing)
            }
            (Some(expected), None) => storage.put_object_if(
                bucket,
                key.to_string(),
                object,
                &ObjectCondition::Metadata {
                    key: GCS_GENERATION_KEY.to_string(),
                    value: expected.to_string(),
                },
            ),
            (None, Some(rejected)) if existing.is_some() => storage.put_object_if(
                bucket,
                key.to_string(),
                object,
                &ObjectCondition::MetadataNot {
                    key: GCS_GENERATION_KEY.to_string(),
                    value: rejected.to_string(),
                },
            ),
            (None | Some(_), Some(_)) => Ok(false),
            (None, None) => storage
                .put_object(bucket, key.to_string(), object)
                .map(|()| true),
        }
        .map_err(|err| err.to_string())?;
        if !written {
            return Ok(BlobWriteOutcome::PreconditionFailed);
        }
        let stored = storage
            .get_object(bucket, key)
            .map_err(|err| err.to_string())?;
        Ok(BlobWriteOutcome::Stored(Box::new(
            crate::blob::BlobRecord::from_object(bucket, &stored),
        )))
    }

    fn generation_precondition_failed() -> Response<Body> {
        Self::json_error(
            StatusCode::PRECONDITION_FAILED,
            "conditionNotMet",
            "At least one of the pre-conditions you specified did not hold.",
        )
    }

    fn generation_not_match_precondition_failed() -> Response<Body> {
        Self::response(StatusCode::NOT_MODIFIED).empty()
    }

    fn upload_precondition_failed(preconditions: GenerationPreconditions<'_>) -> Response<Body> {
        if preconditions.rejected.is_some() {
            Self::generation_not_match_precondition_failed()
        } else {
            Self::generation_precondition_failed()
        }
    }

    fn invalid_preconditions(req: &Request) -> Option<Response<Body>> {
        const PAIRS: [(&str, &str); 2] = [
            ("ifGenerationMatch", "ifGenerationNotMatch"),
            ("ifMetagenerationMatch", "ifMetagenerationNotMatch"),
        ];
        let invalid = PAIRS.iter().any(|(matched, not_matched)| {
            req.query_param(matched).is_some() && req.query_param(not_matched).is_some()
        }) || PAIRS
            .iter()
            .flat_map(|(matched, not_matched)| [matched, not_matched])
            .any(|name| {
                req.query_param(name)
                    .is_some_and(|value| value.parse::<u64>().is_err())
            });
        invalid.then(|| {
            Self::json_error(
                StatusCode::BAD_REQUEST,
                "invalidParameter",
                "Generation preconditions must be unsigned integers and match/not-match cannot be combined",
            )
        })
    }

    fn invalid_upload_preconditions(req: &Request) -> Option<Response<Body>> {
        Self::invalid_preconditions(req).or_else(|| {
            (req.query_param("ifMetagenerationMatch").is_some()
                || req.query_param("ifMetagenerationNotMatch").is_some())
            .then(|| {
                Self::json_error(
                    StatusCode::BAD_REQUEST,
                    "invalidParameter",
                    "Object uploads do not support metageneration preconditions",
                )
            })
        })
    }

    #[allow(clippy::result_large_err)]
    fn xml_mutation_condition(
        req: &Request,
        require_generation_for_metageneration: bool,
    ) -> Result<Option<ObjectCondition>, Response<Body>> {
        let generation = req.header("x-goog-if-generation-match");
        let metageneration = req.header("x-goog-if-metageneration-match");
        if require_generation_for_metageneration && metageneration.is_some() && generation.is_none()
        {
            return Err(Self::error_response(
                StatusCode::BAD_REQUEST,
                "InvalidArgument",
                "x-goog-if-metageneration-match requires x-goog-if-generation-match",
            ));
        }
        if generation.is_some_and(|value| value.parse::<u64>().is_err())
            || metageneration
                .is_some_and(|value| value.parse::<u64>().map_or(true, |parsed| parsed == 0))
        {
            return Err(Self::error_response(
                StatusCode::BAD_REQUEST,
                "InvalidArgument",
                "Generation preconditions must be unsigned integers and metageneration must be positive",
            ));
        }

        let mut conditions = Vec::new();
        if let Some(generation) = generation {
            conditions.push(if generation == "0" {
                ObjectCondition::Missing
            } else {
                ObjectCondition::Metadata {
                    key: GCS_GENERATION_KEY.to_string(),
                    value: generation.to_string(),
                }
            });
        }
        if let Some(metageneration) = metageneration {
            conditions.push(ObjectCondition::Metadata {
                key: GCS_METAGENERATION_KEY.to_string(),
                value: metageneration.to_string(),
            });
        }
        Ok(match conditions.len() {
            0 => None,
            1 => conditions.pop(),
            _ => Some(ObjectCondition::All(conditions)),
        })
    }

    fn xml_precondition_failed_response() -> Response<Body> {
        Self::error_response(
            StatusCode::PRECONDITION_FAILED,
            "PreconditionFailed",
            "At least one of the preconditions you specified did not hold",
        )
    }

    fn invalid_json_mutation_headers(req: &Request) -> Option<Response<Body>> {
        (req.header("if-match").is_some() || req.header("if-none-match").is_some()).then(|| {
            Self::json_error(
                StatusCode::BAD_REQUEST,
                "invalidParameter",
                "GCS JSON mutations require generation query preconditions; ETag mutation headers are unsupported",
            )
        })
    }

    fn json_not_found(object: &str) -> Response<Body> {
        Self::json_error(
            StatusCode::NOT_FOUND,
            "notFound",
            &format!("No such object: {object}"),
        )
    }

    #[allow(clippy::result_large_err)]
    fn check_gcs_preconditions(
        req: &Request,
        blob: &crate::models::Object,
    ) -> Result<(), Response<Body>> {
        if let Some(response) = Self::invalid_preconditions(req) {
            return Err(response);
        }
        let generation = Self::generation(blob);
        let metageneration = Self::metageneration(blob);

        if let Some(expected) = req.query_param("ifGenerationMatch") {
            if generation != expected {
                return Err(Self::json_error(
                    StatusCode::PRECONDITION_FAILED,
                    "conditionNotMet",
                    "Generation precondition failed",
                ));
            }
        }
        if let Some(expected) = req.query_param("ifGenerationNotMatch") {
            if generation == expected {
                return Err(Self::response(StatusCode::NOT_MODIFIED).empty());
            }
        }
        if let Some(expected) = req.query_param("ifMetagenerationMatch") {
            if metageneration != expected {
                return Err(Self::json_error(
                    StatusCode::PRECONDITION_FAILED,
                    "conditionNotMet",
                    "Metageneration precondition failed",
                ));
            }
        }
        if let Some(expected) = req.query_param("ifMetagenerationNotMatch") {
            if metageneration == expected {
                return Err(Self::response(StatusCode::NOT_MODIFIED).empty());
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
    ) -> Result<MultipartUploadParts, UploadMetadataError> {
        let boundary = Self::multipart_boundary(content_type).ok_or_else(|| {
            UploadMetadataError::Invalid("Missing multipart boundary".to_string())
        })?;
        let marker = [b"--".as_slice(), boundary.as_bytes()].concat();
        let delimiter = [b"\r\n".as_slice(), marker.as_slice()].concat();
        if !body.starts_with(&marker) {
            return Err(UploadMetadataError::Invalid(
                "Multipart body does not start with its boundary".to_string(),
            ));
        }

        let mut parsed_metadata = None;
        let mut media_content_type = None;
        let mut data = None;
        let mut cursor = 0;

        loop {
            if !body[cursor..].starts_with(&marker) {
                return Err(UploadMetadataError::Invalid(
                    "Malformed multipart boundary".to_string(),
                ));
            }
            cursor += marker.len();
            if body[cursor..].starts_with(b"--") {
                break;
            }
            if !body[cursor..].starts_with(b"\r\n") {
                return Err(UploadMetadataError::Invalid(
                    "Multipart boundary must be followed by CRLF".to_string(),
                ));
            }
            cursor += 2;

            let headers_end = Self::find_bytes(body, b"\r\n\r\n", cursor).ok_or_else(|| {
                UploadMetadataError::Invalid("Missing multipart part headers".to_string())
            })?;
            let headers = std::str::from_utf8(&body[cursor..headers_end]).map_err(|_| {
                UploadMetadataError::Invalid("Multipart part headers must be UTF-8".to_string())
            })?;
            let payload_start = headers_end + 4;
            let next_boundary =
                Self::find_bytes(body, &delimiter, payload_start).ok_or_else(|| {
                    UploadMetadataError::Invalid("Missing closing multipart boundary".to_string())
                })?;
            let raw_body = &body[payload_start..next_boundary];
            cursor = next_boundary + 2;

            let part_content_type = headers.lines().find_map(|header| {
                let (name, value) = header.split_once(':')?;
                name.trim()
                    .eq_ignore_ascii_case("content-type")
                    .then(|| value.trim().to_string())
            });

            if part_content_type.as_deref().is_some_and(|content_type| {
                content_type
                    .to_ascii_lowercase()
                    .contains("application/json")
            }) {
                if parsed_metadata.is_some() {
                    return Err(UploadMetadataError::Invalid(
                        "Multipart upload contains multiple metadata parts".to_string(),
                    ));
                }
                parsed_metadata = Some(Self::parse_json_upload_metadata(raw_body)?);
            } else {
                if data.is_some() {
                    return Err(UploadMetadataError::Invalid(
                        "Multipart upload contains multiple media parts".to_string(),
                    ));
                }
                media_content_type = part_content_type;
                data = Some(raw_body.to_vec());
            }
        }

        let metadata = parsed_metadata.ok_or_else(|| {
            UploadMetadataError::Invalid("Missing multipart object metadata".to_string())
        })?;
        let content_type = media_content_type
            .or_else(|| metadata.content_type.clone())
            .unwrap_or_else(|| "application/octet-stream".to_string());

        Ok((
            metadata,
            content_type,
            data.ok_or_else(|| {
                UploadMetadataError::Invalid("Missing multipart object data".to_string())
            })?,
        ))
    }

    fn find_bytes(haystack: &[u8], needle: &[u8], start: usize) -> Option<usize> {
        if needle.is_empty() || start > haystack.len() {
            return None;
        }
        haystack[start..]
            .windows(needle.len())
            .position(|window| window == needle)
            .map(|offset| start + offset)
    }

    fn parse_json_upload_metadata(
        body: &[u8],
    ) -> Result<ParsedUploadMetadata, UploadMetadataError> {
        if body.is_empty() {
            return Ok(ParsedUploadMetadata::default());
        }
        let value: serde_json::Value = serde_json::from_slice(body).map_err(|error| {
            UploadMetadataError::Invalid(format!("Invalid object metadata JSON: {error}"))
        })?;
        let object = value.as_object().ok_or_else(|| {
            UploadMetadataError::Invalid("Object metadata must be a JSON object".to_string())
        })?;
        for field in object.keys() {
            if !matches!(
                field.as_str(),
                "name" | "bucket" | "contentType" | "metadata" | "crc32c"
            ) {
                return Err(UploadMetadataError::Unsupported(format!(
                    "Object metadata field {field} is not supported by this emulator surface"
                )));
            }
        }
        if object.get("bucket").is_some_and(|value| !value.is_string()) {
            return Err(UploadMetadataError::Invalid(
                "Object metadata bucket must be a string".to_string(),
            ));
        }
        let name = match object.get("name") {
            Some(value) => Some(
                value
                    .as_str()
                    .ok_or_else(|| {
                        UploadMetadataError::Invalid(
                            "Object metadata name must be a string".to_string(),
                        )
                    })?
                    .to_string(),
            ),
            None => None,
        };
        let content_type = match object.get("contentType") {
            Some(value) => Some(
                value
                    .as_str()
                    .filter(|value| !value.is_empty())
                    .ok_or_else(|| {
                        UploadMetadataError::Invalid(
                            "Object metadata contentType must be a non-empty string".to_string(),
                        )
                    })?
                    .to_string(),
            ),
            None => None,
        };
        let metadata = match object.get("metadata") {
            Some(serde_json::Value::Object(values)) => values
                .iter()
                .map(|(key, value)| {
                    value
                        .as_str()
                        .map(|value| (key.clone(), value.to_string()))
                        .ok_or_else(|| {
                            UploadMetadataError::Invalid(format!(
                                "Object metadata value {key} must be a string"
                            ))
                        })
                })
                .collect::<Result<HashMap<_, _>, _>>()?,
            Some(_) => {
                return Err(UploadMetadataError::Invalid(
                    "Object metadata metadata must be a JSON object".to_string(),
                ));
            }
            None => HashMap::new(),
        };
        let crc32c = object.get("crc32c").map(Self::parse_crc32c).transpose()?;
        Ok(ParsedUploadMetadata {
            name,
            content_type,
            metadata,
            crc32c,
        })
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

    fn canonicalized_extension_headers(req: &Request) -> Result<String, String> {
        let mut headers = BTreeMap::<String, Vec<String>>::new();
        for (name, value) in &req.headers {
            let name = name.as_str().to_ascii_lowercase();
            if !name.starts_with("x-goog-")
                || matches!(
                    name.as_str(),
                    "x-goog-encryption-key" | "x-goog-encryption-key-sha256"
                )
            {
                continue;
            }
            let value = value
                .to_str()
                .map_err(|_| format!("Invalid non-text GCS extension header {name}"))?;
            headers
                .entry(name)
                .or_default()
                .push(value.trim().to_string());
        }

        let mut canonical = String::new();
        for (name, values) in headers {
            canonical.push_str(&name);
            canonical.push(':');
            canonical.push_str(&values.join(","));
            canonical.push('\n');
        }
        Ok(canonical)
    }

    fn string_to_sign(
        req: &Request,
        bucket: &str,
        object: Option<&str>,
        expires: &str,
    ) -> Result<String, String> {
        let resource = if let Some(object) = object {
            format!("/{bucket}/{object}")
        } else {
            format!("/{bucket}")
        };
        let canonicalized_extension_headers = Self::canonicalized_extension_headers(req)?;

        Ok(format!(
            "{}\n{}\n{}\n{}\n{}{}",
            req.method(),
            req.header("content-md5").unwrap_or(""),
            req.header("content-type").unwrap_or(""),
            expires,
            canonicalized_extension_headers,
            resource
        ))
    }

    fn encode_page_token(kind: &str, marker: &str) -> String {
        let digest = Sha256::digest(format!("sqrzl-gcs-{kind}:{marker}").as_bytes());
        URL_SAFE_NO_PAD.encode(format!("{kind}\0{marker}\0{}", hex::encode(digest)))
    }

    fn decode_page_token(kind: &str, token: &str) -> Option<String> {
        let decoded = String::from_utf8(URL_SAFE_NO_PAD.decode(token).ok()?).ok()?;
        let mut fields = decoded.split('\0');
        let supplied_kind = fields.next()?;
        let marker = fields.next()?;
        let supplied_digest = fields.next()?;
        if fields.next().is_some() || supplied_kind != kind {
            return None;
        }
        let expected = Sha256::digest(format!("sqrzl-gcs-{kind}:{marker}").as_bytes());
        (supplied_digest == hex::encode(expected)).then(|| marker.to_string())
    }

    fn authorization_error(
        req: &Request,
        status: StatusCode,
        xml_code: &str,
        json_reason: &str,
        message: &str,
    ) -> Response<Body> {
        if Self::is_json_api_request(req) {
            Self::json_error(status, json_reason, message)
        } else {
            Self::error_response(status, xml_code, message)
        }
    }

    #[allow(
        clippy::result_large_err,
        clippy::too_many_lines,
        reason = "keeping the mutually exclusive signed-URL, bearer, and HMAC branches together makes the provider authentication contract auditable"
    )]
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
            let expires_at = expires.parse::<i64>().ok();
            if expires_at.is_none_or(|timestamp| timestamp <= chrono::Utc::now().timestamp()) {
                return Err(Self::authorization_error(
                    req,
                    StatusCode::FORBIDDEN,
                    "AccessDenied",
                    "forbidden",
                    "Request has expired",
                ));
            }
            if config.access_key() != Some(access_id) {
                return Err(Self::authorization_error(
                    req,
                    StatusCode::FORBIDDEN,
                    "AccessDenied",
                    "forbidden",
                    "Invalid access id",
                ));
            }
            let string_to_sign =
                Self::string_to_sign(req, bucket, object, expires).map_err(|msg| {
                    Self::authorization_error(
                        req,
                        StatusCode::FORBIDDEN,
                        "SignatureDoesNotMatch",
                        "forbidden",
                        &msg,
                    )
                })?;
            let expected = Self::sign(config, &string_to_sign).map_err(|msg| {
                Self::authorization_error(
                    req,
                    StatusCode::FORBIDDEN,
                    "SignatureDoesNotMatch",
                    "forbidden",
                    &msg,
                )
            })?;
            if expected == signature {
                return Ok(());
            }
            return Err(Self::authorization_error(
                req,
                StatusCode::FORBIDDEN,
                "SignatureDoesNotMatch",
                "forbidden",
                "GCS signed URL signature mismatch",
            ));
        }

        let Some(authorization) = req.header("authorization") else {
            let status = if Self::is_json_api_request(req) {
                StatusCode::UNAUTHORIZED
            } else {
                StatusCode::FORBIDDEN
            };
            return Err(Self::authorization_error(
                req,
                status,
                "AccessDenied",
                "authError",
                "Missing authorization",
            ));
        };
        if let Some(token) = authorization.strip_prefix("Bearer ") {
            if config.secret_key() == Some(token) || config.access_key() == Some(token) {
                return Ok(());
            }
            return Err(Self::authorization_error(
                req,
                StatusCode::UNAUTHORIZED,
                "AccessDenied",
                "authError",
                "Invalid bearer token",
            ));
        }
        let prefix = format!("GOOG1 {}:", config.access_key().unwrap_or_default());
        let Some(signature) = authorization.strip_prefix(&prefix) else {
            let status = if Self::is_json_api_request(req) {
                StatusCode::UNAUTHORIZED
            } else {
                StatusCode::FORBIDDEN
            };
            return Err(Self::authorization_error(
                req,
                status,
                "AccessDenied",
                "authError",
                "Unsupported authorization",
            ));
        };
        let date = req.header("date").unwrap_or("");
        let string_to_sign = Self::string_to_sign(req, bucket, object, date).map_err(|msg| {
            Self::authorization_error(
                req,
                StatusCode::FORBIDDEN,
                "SignatureDoesNotMatch",
                "forbidden",
                &msg,
            )
        })?;
        let expected = Self::sign(config, &string_to_sign).map_err(|msg| {
            Self::authorization_error(
                req,
                StatusCode::FORBIDDEN,
                "SignatureDoesNotMatch",
                "forbidden",
                &msg,
            )
        })?;
        if expected == signature {
            Ok(())
        } else {
            Err(Self::authorization_error(
                req,
                StatusCode::FORBIDDEN,
                "SignatureDoesNotMatch",
                "forbidden",
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
    use crate::server::handle_s3_request;
    use crate::storage::FilesystemStorage;
    use http_body_util::BodyExt;
    use hyper::Request as HyperRequest;
    use std::fs;

    #[test]
    fn should_apply_gcs_bucket_naming_rules() {
        // Arrange
        let valid_names = ["abc", "bucket_name-01", "test.example.com"];
        let invalid_names = [
            "ab",
            "Upper",
            "192.168.5.4",
            "goog-data",
            "my-google-bucket",
            "bad..name",
        ];

        // Act
        let valid_results = valid_names.map(GcsAdapter::valid_bucket_name);
        let invalid_results = invalid_names.map(GcsAdapter::valid_bucket_name);

        // Assert
        assert_eq!(valid_results, [true; 3]);
        assert_eq!(invalid_results, [false; 6]);
    }

    #[tokio::test]
    async fn should_reject_invalid_gcs_bucket_name_without_mutation() {
        let adapter = GcsAdapter::new();
        let storage = temp_storage();
        let request = parsed_request(
            "POST",
            "http://localhost/storage/v1/b?project=test-project",
            &[("content-type", "application/json")],
            br#"{"name":"A"}"#,
        )
        .await;

        let response = adapter
            .handle_request(&storage, &auth_disabled(), &request)
            .expect("invalid bucket request should complete");

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert!(storage.get_namespace("A").is_err());
    }

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
            smtp_port: crate::config::DEFAULT_SQRZL_SMTP_PORT,
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
        if matches!(method, "POST" | "PUT" | "PATCH")
            && !headers
                .iter()
                .any(|(name, _)| name.eq_ignore_ascii_case("content-length"))
            && !headers
                .iter()
                .any(|(name, _)| name.eq_ignore_ascii_case("transfer-encoding"))
        {
            builder = builder.header("content-length", body.len().to_string());
        }
        Request::from_hyper(
            builder
                .body(Body::from(body.to_vec()))
                .expect("request should build"),
        )
        .await
        .expect("request should parse")
    }

    #[tokio::test]
    async fn should_sort_merge_and_filter_gcs_v2_extension_headers_in_string_to_sign() {
        // Arrange
        let request = parsed_request(
            "GET",
            "http://localhost/private/item.txt",
            &[
                ("x-goog-meta-reviewer", "jane"),
                ("X-Goog-Acl", "public-read"),
                ("x-goog-meta-reviewer", "john"),
                ("x-goog-encryption-key", "sensitive-key"),
                ("x-goog-encryption-key-sha256", "sensitive-key-hash"),
                ("x-unrelated", "ignored"),
            ],
            b"",
        )
        .await;

        // Act
        let actual = GcsAdapter::string_to_sign(
            &request,
            "private",
            Some("item.txt"),
            "Tue, 11 Aug 2026 12:00:00 GMT",
        )
        .expect("string to sign should build");

        // Assert
        assert_eq!(
            actual,
            "GET\n\n\nTue, 11 Aug 2026 12:00:00 GMT\nx-goog-acl:public-read\nx-goog-meta-reviewer:jane,john\n/private/item.txt"
        );
    }

    #[tokio::test]
    async fn should_authorize_gcs_v2_hmac_request_with_canonicalized_extension_headers() {
        // Arrange
        let adapter = GcsAdapter::new();
        let storage = temp_storage();
        storage.create_bucket("private".to_string()).unwrap();
        storage
            .put_object(
                "private",
                "item.txt".to_string(),
                crate::models::Object::new(
                    "item.txt".to_string(),
                    b"authenticated".to_vec(),
                    "text/plain".to_string(),
                ),
            )
            .unwrap();
        let date = "Tue, 11 Aug 2026 12:00:00 GMT";
        let canonical = format!(
            "GET\n\n\n{date}\nx-goog-acl:public-read\nx-goog-meta-reviewer:jane,john\n/private/item.txt"
        );
        let signature = GcsAdapter::sign(&gcs_auth(), &canonical).expect("signature should build");
        let authorization = format!("GOOG1 test-access:{signature}");
        let request = parsed_request(
            "GET",
            "http://localhost/private/item.txt",
            &[
                ("date", date),
                ("authorization", &authorization),
                ("x-goog-meta-reviewer", "jane"),
                ("x-goog-acl", "public-read"),
                ("x-goog-meta-reviewer", "john"),
                ("x-goog-encryption-key", "sensitive-key"),
                ("x-goog-encryption-key-sha256", "sensitive-key-hash"),
            ],
            b"",
        )
        .await;

        // Act
        let response = adapter
            .handle_request(&storage, &gcs_auth(), &request)
            .expect("signed request should respond");

        // Assert
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(read_test_body(response).await, b"authenticated");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn should_render_json_and_xml_auth_failures_with_their_native_envelopes() {
        // Arrange
        let adapter = GcsAdapter::new();
        let storage = temp_storage();
        storage.create_bucket("private".to_string()).unwrap();
        let json_request = parsed_request(
            "GET",
            "http://localhost/storage/v1/b/private/o/object",
            &[],
            b"",
        )
        .await;
        let xml_request = parsed_request(
            "GET",
            "http://localhost/private/object",
            &[("host", "storage.googleapis.com")],
            b"",
        )
        .await;

        // Act
        let json = adapter
            .handle_request(&storage, &gcs_auth(), &json_request)
            .unwrap();
        let xml = adapter
            .handle_request(&storage, &gcs_auth(), &xml_request)
            .unwrap();

        // Assert
        assert_eq!(json.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(json.headers()["content-type"], "application/json");
        assert_eq!(
            parse_json_body(json).await["error"]["errors"][0]["reason"],
            "authError"
        );
        assert_eq!(xml.status(), StatusCode::FORBIDDEN);
        assert_eq!(xml.headers()["content-type"], "application/xml");
        assert!(String::from_utf8(read_test_body(xml).await)
            .unwrap()
            .contains("<Code>AccessDenied</Code>"));
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
    async fn should_decode_gcs_xml_object_paths_once_without_collapsing_key_components() {
        // Arrange
        let adapter = GcsAdapter::new();
        let storage = temp_storage();
        storage.create_bucket("encoded-paths".to_string()).unwrap();
        let cases = [
            ("space%20name.txt", "space name.txt"),
            ("percent%25name.txt", "percent%name.txt"),
            ("nested%2Fname.txt", "nested/name.txt"),
            ("snowman-%E2%98%83.txt", "snowman-☃.txt"),
            ("literal%252Fescape.txt", "literal%2Fescape.txt"),
            ("dir/", "dir/"),
            ("a//b", "a//b"),
            ("/leading-slash", "/leading-slash"),
        ];

        // Act
        // Assert
        for (index, (encoded, expected)) in cases.into_iter().enumerate() {
            let payload = format!("payload-{index}");
            let response = adapter
                .handle_request(
                    &storage,
                    &auth_disabled(),
                    &parsed_request(
                        "PUT",
                        &format!("http://localhost/encoded-paths/{encoded}"),
                        &[("host", "storage.googleapis.com")],
                        payload.as_bytes(),
                    )
                    .await,
                )
                .expect("encoded object PUT should respond");

            assert_eq!(response.status(), StatusCode::OK, "encoded path {encoded}");
            assert_eq!(
                storage
                    .get_object("encoded-paths", expected)
                    .expect("decoded object should exist")
                    .data,
                payload.as_bytes(),
                "encoded path {encoded}"
            );
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn should_paginate_gcs_xml_list_type_two_without_duplicates() {
        // Arrange
        let adapter = GcsAdapter::new();
        let storage = temp_storage();
        storage.create_bucket("xml-v2".to_string()).unwrap();
        for key in ["alpha", "beta", "gamma"] {
            storage
                .put_object(
                    "xml-v2",
                    key.to_string(),
                    crate::models::Object::new(
                        key.to_string(),
                        b"x".to_vec(),
                        "text/plain".to_string(),
                    ),
                )
                .unwrap();
        }

        // Act
        let first = adapter
            .handle_request(
                &storage,
                &auth_disabled(),
                &parsed_request(
                    "GET",
                    "http://localhost/xml-v2?list-type=2&max-keys=1&start-after=alpha",
                    &[("host", "storage.googleapis.com"), ("content-length", "0")],
                    b"",
                )
                .await,
            )
            .expect("first XML v2 page should respond");
        let first = String::from_utf8(read_test_body(first).await).expect("response should be XML");
        let token = first
            .split_once("<NextContinuationToken>")
            .and_then(|(_, rest)| rest.split_once("</NextContinuationToken>"))
            .map(|(token, _)| token.to_string())
            .expect("truncated page should include a continuation token");
        let second = adapter
            .handle_request(
                &storage,
                &auth_disabled(),
                &parsed_request(
                    "GET",
                    &format!(
                        "http://localhost/xml-v2?list-type=2&max-keys=1&continuation-token={token}"
                    ),
                    &[("host", "storage.googleapis.com"), ("content-length", "0")],
                    b"",
                )
                .await,
            )
            .expect("last XML v2 page should respond");
        let second =
            String::from_utf8(read_test_body(second).await).expect("response should be XML");

        // Assert
        assert!(first.contains("<StartAfter>alpha</StartAfter>"));
        assert!(first.contains("<KeyCount>1</KeyCount>"));
        assert!(first.contains("<MaxKeys>1</MaxKeys>"));
        assert!(first.contains("<IsTruncated>true</IsTruncated>"));
        assert!(first.contains("<Key>beta</Key>"));
        assert!(!first.contains("<Key>alpha</Key>"));
        assert!(!first.contains("<Key>gamma</Key>"));
        assert!(second.contains(&format!("<ContinuationToken>{token}</ContinuationToken>")));
        assert!(second.contains("<KeyCount>1</KeyCount>"));
        assert!(second.contains("<IsTruncated>false</IsTruncated>"));
        assert!(!second.contains("<NextContinuationToken>"));
        assert!(second.contains("<Key>gamma</Key>"));
        assert!(!second.contains("<Key>beta</Key>"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn should_reject_invalid_gcs_xml_list_pagination_arguments() {
        // Arrange
        let adapter = GcsAdapter::new();
        let storage = temp_storage();
        storage
            .create_bucket("xml-invalid-list".to_string())
            .unwrap();

        // Act
        // Assert
        for max_keys in ["alpha", "-1", "184467440737095516160", "0"] {
            let response = adapter
                .handle_request(
                    &storage,
                    &auth_disabled(),
                    &parsed_request(
                        "GET",
                        &format!(
                            "http://localhost/xml-invalid-list?list-type=2&max-keys={max_keys}"
                        ),
                        &[("host", "storage.googleapis.com"), ("content-length", "0")],
                        b"",
                    )
                    .await,
                )
                .expect("invalid max-keys should respond");
            assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{max_keys}");
            assert!(String::from_utf8(read_test_body(response).await)
                .expect("response should be XML")
                .contains("<Code>InvalidArgument</Code>"));
        }

        let response = adapter
            .handle_request(
                &storage,
                &auth_disabled(),
                &parsed_request(
                    "GET",
                    "http://localhost/xml-invalid-list?list-type=2&continuation-token=not-issued",
                    &[("host", "storage.googleapis.com"), ("content-length", "0")],
                    b"",
                )
                .await,
            )
            .expect("invalid continuation token should respond");
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert!(String::from_utf8(read_test_body(response).await)
            .expect("response should be XML")
            .contains("<Code>InvalidArgument</Code>"));
    }

    #[allow(
        clippy::too_many_lines,
        reason = "one table-driven regression covers every rejected JSON and XML history selector and verifies the shared no-mutation postcondition"
    )]
    #[tokio::test(flavor = "multi_thread")]
    async fn should_reject_unsupported_gcs_history_operations_without_mutation() {
        // Arrange
        let adapter = GcsAdapter::new();
        let storage = temp_storage();
        storage
            .create_bucket("unsupported-history".to_string())
            .unwrap();
        storage
            .put_object(
                "unsupported-history",
                "object".to_string(),
                crate::models::Object::new_with_metadata(
                    "object".to_string(),
                    b"current bytes".to_vec(),
                    "text/plain".to_string(),
                    HashMap::from([
                        (GCS_GENERATION_KEY.to_string(), "2".to_string()),
                        (GCS_METAGENERATION_KEY.to_string(), "1".to_string()),
                    ]),
                ),
            )
            .unwrap();
        let json_cases = [
            (
                "GET",
                "http://localhost/storage/v1/b/unsupported-history/o/object?generation=1",
                b"".as_slice(),
            ),
            (
                "PATCH",
                "http://localhost/storage/v1/b/unsupported-history/o/object?generation=1",
                br#"{"metadata":{"owner":"old"}}"#.as_slice(),
            ),
            (
                "DELETE",
                "http://localhost/storage/v1/b/unsupported-history/o/object?generation=1",
                b"".as_slice(),
            ),
            (
                "GET",
                "http://localhost/download/storage/v1/b/unsupported-history/o/object?generation=1",
                b"".as_slice(),
            ),
        ];
        let xml_cases = ["GET", "PATCH", "DELETE"];

        // Act
        // Assert
        for (method, uri, body) in json_cases {
            let response = adapter
                .handle_request(
                    &storage,
                    &auth_disabled(),
                    &parsed_request(method, uri, &[], body).await,
                )
                .expect("generation-scoped JSON request should respond");
            assert_eq!(response.status(), StatusCode::NOT_IMPLEMENTED, "{method}");
            assert_eq!(
                parse_json_body(response).await["error"]["errors"][0]["reason"],
                "notImplemented"
            );
        }
        for method in xml_cases {
            let response = adapter
                .handle_request(
                    &storage,
                    &auth_disabled(),
                    &parsed_request(
                        method,
                        "http://localhost/unsupported-history/object?generation=1",
                        &[("host", "storage.googleapis.com"), ("content-length", "0")],
                        b"",
                    )
                    .await,
                )
                .expect("generation-scoped XML request should respond");
            assert_eq!(response.status(), StatusCode::NOT_IMPLEMENTED, "{method}");
            assert!(String::from_utf8(read_test_body(response).await)
                .expect("response should be XML")
                .contains("<Code>NotImplemented</Code>"));
        }
        for parameter in ["softDeleted", "versions"] {
            let response = adapter
                .handle_request(
                    &storage,
                    &auth_disabled(),
                    &parsed_request(
                        "GET",
                        &format!(
                            "http://localhost/storage/v1/b/unsupported-history/o?{parameter}=true"
                        ),
                        &[],
                        b"",
                    )
                    .await,
                )
                .expect("unsupported JSON history listing should respond");
            assert_eq!(
                response.status(),
                StatusCode::NOT_IMPLEMENTED,
                "{parameter}"
            );
        }
        let versions = adapter
            .handle_request(
                &storage,
                &auth_disabled(),
                &parsed_request(
                    "GET",
                    "http://localhost/unsupported-history?versions",
                    &[("host", "storage.googleapis.com"), ("content-length", "0")],
                    b"",
                )
                .await,
            )
            .expect("unsupported XML history listing should respond");
        assert_eq!(versions.status(), StatusCode::NOT_IMPLEMENTED);
        assert!(String::from_utf8(read_test_body(versions).await)
            .expect("response should be XML")
            .contains("<Code>NotImplemented</Code>"));
        let current = storage
            .get_object("unsupported-history", "object")
            .expect("current object must be preserved");
        assert_eq!(current.data, b"current bytes");
        assert_eq!(
            current.metadata.get(GCS_GENERATION_KEY),
            Some(&"2".to_string())
        );
        assert!(!current.metadata.contains_key("owner"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn should_reject_gcs_xml_object_subresources_without_mutating_bytes() {
        // Arrange
        let adapter = GcsAdapter::new();
        let storage = temp_storage();
        storage
            .create_bucket("xml-object-subresources".to_string())
            .unwrap();
        storage
            .put_object(
                "xml-object-subresources",
                "object".to_string(),
                crate::models::Object::new(
                    "object".to_string(),
                    b"original".to_vec(),
                    "text/plain".to_string(),
                ),
            )
            .unwrap();

        // Act
        // Assert
        for subresource in ["acl", "retention", "legal-hold", "tagging"] {
            for method in ["GET", "PUT", "DELETE"] {
                let payload = if method == "PUT" {
                    b"must-not-overwrite".as_slice()
                } else {
                    b"".as_slice()
                };
                let content_length = payload.len().to_string();
                let response = adapter
                    .handle_request(
                        &storage,
                        &auth_disabled(),
                        &parsed_request(
                            method,
                            &format!(
                                "http://localhost/xml-object-subresources/object?{subresource}"
                            ),
                            &[
                                ("host", "storage.googleapis.com"),
                                ("content-length", content_length.as_str()),
                            ],
                            payload,
                        )
                        .await,
                    )
                    .expect("unsupported XML object subresource should respond");
                assert_eq!(response.status(), StatusCode::NOT_IMPLEMENTED);
                assert!(String::from_utf8(read_test_body(response).await)
                    .expect("response should be XML")
                    .contains("<Code>NotImplemented</Code>"));
                assert_eq!(
                    storage
                        .get_object("xml-object-subresources", "object")
                        .unwrap()
                        .data,
                    b"original"
                );
            }
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn should_reject_gcs_xml_bucket_subresources_before_crud_dispatch() {
        // Arrange
        let adapter = GcsAdapter::new();
        let storage = temp_storage();
        storage
            .create_bucket("xml-bucket-subresources".to_string())
            .unwrap();
        let subresources = [
            "acl",
            "cors",
            "lifecycle",
            "logging",
            "storageClass",
            "tagging",
            "versioning",
        ];

        // Act
        // Assert
        for subresource in subresources {
            let response = adapter
                .handle_request(
                    &storage,
                    &auth_disabled(),
                    &parsed_request(
                        "GET",
                        &format!("http://localhost/xml-bucket-subresources?{subresource}"),
                        &[("host", "storage.googleapis.com"), ("content-length", "0")],
                        b"",
                    )
                    .await,
                )
                .expect("unsupported XML bucket subresource should respond");
            assert_eq!(response.status(), StatusCode::NOT_IMPLEMENTED);
            assert!(String::from_utf8(read_test_body(response).await)
                .expect("response should be XML")
                .contains("<Code>NotImplemented</Code>"));
            assert!(storage.get_bucket("xml-bucket-subresources").is_ok());
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn should_assign_distinct_generations_to_concurrent_json_and_xml_writes() {
        // Arrange
        const WRITES: usize = 12;
        let adapter = Arc::new(GcsAdapter::new());
        let storage = temp_storage();
        storage
            .create_bucket("generation-race".to_string())
            .unwrap();
        let barrier = Arc::new(tokio::sync::Barrier::new(WRITES));
        let mut handles = Vec::new();

        // Act
        for index in 0..WRITES {
            let adapter = adapter.clone();
            let storage = storage.clone();
            let barrier = barrier.clone();
            handles.push(tokio::spawn(async move {
                let payload = format!("writer-{index}");
                let json = index % 2 == 0;
                let uri = if json {
                    "http://localhost/upload/storage/v1/b/generation-race/o?uploadType=media&name=object"
                } else {
                    "http://localhost/generation-race/object"
                };
                let content_length = payload.len().to_string();
                let headers = if json {
                    vec![("content-length", content_length.as_str())]
                } else {
                    vec![
                        ("host", "storage.googleapis.com"),
                        ("content-length", content_length.as_str()),
                    ]
                };
                let request =
                    parsed_request(if json { "POST" } else { "PUT" }, uri, &headers, payload.as_bytes())
                        .await;
                barrier.wait().await;
                let response = adapter
                    .handle_request(&storage, &auth_disabled(), &request)
                    .expect("concurrent write should respond");
                assert_eq!(response.status(), StatusCode::OK);
                let generation = if json {
                    parse_json_body(response).await["generation"]
                        .as_str()
                        .expect("JSON generation")
                        .to_string()
                } else {
                    header_value(&response, "x-goog-generation")
                        .expect("XML generation")
                        .to_string()
                };
                (payload.into_bytes(), generation)
            }));
        }
        let mut writes = Vec::new();
        for handle in handles {
            writes.push(handle.await.expect("write task should join"));
        }

        // Assert
        let unique = writes
            .iter()
            .map(|(_, generation)| generation)
            .collect::<std::collections::HashSet<_>>();
        assert_eq!(unique.len(), WRITES);
        let current = storage.get_object("generation-race", "object").unwrap();
        let current_generation = current.metadata[GCS_GENERATION_KEY].as_str();
        assert!(writes.iter().any(|(payload, generation)| {
            payload == &current.data && generation == current_generation
        }));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn should_apply_retention_inside_the_serialized_json_write() {
        // Arrange
        let adapter = Arc::new(GcsAdapter::new());
        let storage = temp_storage();
        storage.create_bucket("retention-race".to_string()).unwrap();
        storage
            .update_bucket_metadata(
                "retention-race",
                HashMap::from([(GCS_RETENTION_SECONDS_KEY.to_string(), "3600".to_string())]),
            )
            .unwrap();
        let barrier = Arc::new(tokio::sync::Barrier::new(2));
        let mut writes = Vec::new();

        // Act
        for payload in [b"first".as_slice(), b"second".as_slice()] {
            let adapter = adapter.clone();
            let storage = storage.clone();
            let barrier = barrier.clone();
            writes.push(tokio::spawn(async move {
                let request = parsed_request(
                    "POST",
                    "http://localhost/upload/storage/v1/b/retention-race/o?uploadType=media&name=object",
                    &[],
                    payload,
                )
                .await;
                barrier.wait().await;
                adapter
                    .handle_request(&storage, &auth_disabled(), &request)
                    .expect("concurrent retained write should respond")
                    .status()
            }));
        }
        let statuses = [
            writes.remove(0).await.unwrap(),
            writes.remove(0).await.unwrap(),
        ];

        // Assert
        assert_eq!(
            statuses
                .iter()
                .filter(|status| **status == StatusCode::OK)
                .count(),
            1
        );
        assert_eq!(
            statuses
                .iter()
                .filter(|status| **status == StatusCode::FORBIDDEN)
                .count(),
            1
        );
        assert!(matches!(
            storage
                .get_object("retention-race", "object")
                .unwrap()
                .data
                .as_slice(),
            b"first" | b"second"
        ));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn should_serialize_unconditional_json_patch_and_delete_without_spurious_412() {
        // Arrange
        const PATCHES: usize = 8;
        let adapter = Arc::new(GcsAdapter::new());
        let storage = temp_storage();
        storage.create_bucket("mutation-race".to_string()).unwrap();
        adapter
            .handle_request(
                &storage,
                &auth_disabled(),
                &parsed_request(
                    "POST",
                    "http://localhost/upload/storage/v1/b/mutation-race/o?uploadType=media&name=object",
                    &[("content-length", "7")],
                    b"payload",
                )
                .await,
            )
            .unwrap();
        let barrier = Arc::new(tokio::sync::Barrier::new(PATCHES));
        let mut handles = Vec::new();

        // Act
        for index in 0..PATCHES {
            let adapter = adapter.clone();
            let storage = storage.clone();
            let barrier = barrier.clone();
            handles.push(tokio::spawn(async move {
                let payload = format!(r#"{{"metadata":{{"writer":"{index}"}}}}"#);
                let request = parsed_request(
                    "PATCH",
                    "http://localhost/storage/v1/b/mutation-race/o/object",
                    &[("content-type", "application/json")],
                    payload.as_bytes(),
                )
                .await;
                barrier.wait().await;
                let response = adapter
                    .handle_request(&storage, &auth_disabled(), &request)
                    .unwrap();
                let status = response.status();
                let metadata = parse_json_body(response).await;
                (
                    status,
                    metadata["metageneration"].as_str().map(str::to_string),
                )
            }));
        }
        let mut metagenerations = std::collections::HashSet::new();
        for handle in handles {
            let (status, metageneration) = handle.await.unwrap();
            assert_eq!(status, StatusCode::OK);
            metagenerations.insert(metageneration.expect("metageneration"));
        }

        // Assert
        assert_eq!(metagenerations.len(), PATCHES);
        let delete_barrier = Arc::new(tokio::sync::Barrier::new(2));
        let mut deletes = Vec::new();
        for _ in 0..2 {
            let adapter = adapter.clone();
            let storage = storage.clone();
            let barrier = delete_barrier.clone();
            deletes.push(tokio::spawn(async move {
                let request = parsed_request(
                    "DELETE",
                    "http://localhost/storage/v1/b/mutation-race/o/object",
                    &[],
                    b"",
                )
                .await;
                barrier.wait().await;
                adapter
                    .handle_request(&storage, &auth_disabled(), &request)
                    .unwrap()
                    .status()
            }));
        }
        let statuses = [
            deletes.remove(0).await.unwrap(),
            deletes.remove(0).await.unwrap(),
        ];
        assert!(statuses.contains(&StatusCode::NO_CONTENT));
        assert!(statuses.contains(&StatusCode::NOT_FOUND));
        assert!(!statuses.contains(&StatusCode::PRECONDITION_FAILED));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn should_reject_malformed_gcs_xml_object_path_without_mutation() {
        // Arrange
        let adapter = GcsAdapter::new();
        let storage = temp_storage();
        storage.create_bucket("encoded-paths".to_string()).unwrap();
        let request = parsed_request(
            "PUT",
            "http://localhost/encoded-paths/bad%2",
            &[("host", "storage.googleapis.com")],
            b"must-not-commit",
        )
        .await;

        // Act
        let response = adapter
            .handle_request(&storage, &auth_disabled(), &request)
            .expect("malformed path should respond");

        // Assert
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert!(String::from_utf8(read_test_body(response).await)
            .expect("response should be XML")
            .contains("<Code>InvalidURI</Code>"));
        assert!(storage.get_object("encoded-paths", "bad%2").is_err());
        assert!(storage
            .list_object_versions_for_key("encoded-paths", "bad%2")
            .expect("version list should succeed")
            .is_empty());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn should_route_virtual_hosted_gcs_xml_object_crud_and_decode_the_key_once() {
        // Arrange
        let adapter = GcsAdapter::new();
        let storage = temp_storage();
        storage.create_bucket("virtual.gcs".to_string()).unwrap();
        let host = "virtual.gcs.storage.googleapis.com";
        let uri = "http://localhost/dir%2Fitem%20%E2%98%83";
        let put = parsed_request("PUT", uri, &[("host", host)], b"virtual payload").await;

        // Act
        assert!(adapter.matches(&put));
        let created = adapter
            .handle_request(&storage, &auth_disabled(), &put)
            .expect("virtual-hosted PUT should respond");
        let get = adapter
            .handle_request(
                &storage,
                &auth_disabled(),
                &parsed_request("GET", uri, &[("host", host)], b"").await,
            )
            .expect("virtual-hosted GET should respond");
        let head = adapter
            .handle_request(
                &storage,
                &auth_disabled(),
                &parsed_request("HEAD", uri, &[("host", host)], b"").await,
            )
            .expect("virtual-hosted HEAD should respond");

        // Assert
        assert_eq!(created.status(), StatusCode::OK);
        assert_eq!(get.status(), StatusCode::OK);
        assert_eq!(read_test_body(get).await, b"virtual payload");
        assert_eq!(head.status(), StatusCode::OK);
        assert_eq!(header_value(&head, "content-length"), Some("15"));
        assert!(read_test_body(head).await.is_empty());
        assert_eq!(
            storage
                .get_object("virtual.gcs", "dir/item ☃")
                .expect("decoded virtual-hosted object should exist")
                .data,
            b"virtual payload"
        );

        let deleted = adapter
            .handle_request(
                &storage,
                &auth_disabled(),
                &parsed_request(
                    "DELETE",
                    uri,
                    &[("host", host), ("content-length", "0")],
                    b"",
                )
                .await,
            )
            .expect("virtual-hosted DELETE should respond");
        assert_eq!(deleted.status(), StatusCode::NO_CONTENT);
        assert!(storage.get_object("virtual.gcs", "dir/item ☃").is_err());

        let reserved_uri = "http://localhost/storage/v1/key";
        let reserved_put =
            parsed_request("PUT", reserved_uri, &[("host", host)], b"xml, not json").await;
        assert!(adapter.validate_request_framing(&reserved_put).is_none());
        let reserved_created = adapter
            .handle_request(&storage, &auth_disabled(), &reserved_put)
            .expect("reserved-prefix XML key PUT should respond");
        assert_eq!(reserved_created.status(), StatusCode::OK);
        let reserved_get = parsed_request(
            "GET",
            reserved_uri,
            &[("host", host), ("content-length", "0")],
            b"",
        )
        .await;
        assert!(adapter.validate_request_framing(&reserved_get).is_none());
        let reserved_read = adapter
            .handle_request(&storage, &auth_disabled(), &reserved_get)
            .expect("reserved-prefix XML key GET should respond");
        assert_eq!(reserved_read.status(), StatusCode::OK);
        assert_eq!(read_test_body(reserved_read).await, b"xml, not json");
        assert_eq!(
            storage
                .get_object("virtual.gcs", "storage/v1/key")
                .expect("reserved-prefix key should stay in virtual-host bucket")
                .data,
            b"xml, not json"
        );

        let malformed = parsed_request(
            "PUT",
            "http://localhost/bad%2",
            &[("host", host)],
            b"must-not-commit",
        )
        .await;
        let response = adapter
            .handle_request(&storage, &auth_disabled(), &malformed)
            .expect("malformed virtual-hosted path should respond");
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert!(String::from_utf8(read_test_body(response).await)
            .expect("response should be XML")
            .contains("<Code>InvalidURI</Code>"));
        assert!(storage.get_object("virtual.gcs", "bad%2").is_err());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn should_require_gcs_xml_delete_object_framing_before_mutation() {
        // Arrange
        let adapter = GcsAdapter::new();
        let storage = temp_storage();
        storage.create_bucket("delete-framing".to_string()).unwrap();
        for key in ["path-style", "virtual-hosted", "chunked"] {
            storage
                .put_object(
                    "delete-framing",
                    key.to_string(),
                    crate::models::Object::new(
                        key.to_string(),
                        b"preserve until framed".to_vec(),
                        "text/plain".to_string(),
                    ),
                )
                .unwrap();
        }
        let cases = [
            (
                "http://localhost/delete-framing/path-style",
                "storage.googleapis.com",
                "path-style",
            ),
            (
                "http://localhost/virtual-hosted",
                "delete-framing.storage.googleapis.com",
                "virtual-hosted",
            ),
        ];

        // Act
        // Assert
        for (uri, host, key) in cases {
            let missing = parsed_request("DELETE", uri, &[("host", host)], b"").await;
            let rejected = adapter
                .validate_request_framing(&missing)
                .expect("unframed XML DELETE Object should be rejected");
            assert_eq!(rejected.status(), StatusCode::LENGTH_REQUIRED);
            assert!(String::from_utf8(read_test_body(rejected).await)
                .expect("response should be XML")
                .contains("<Code>MissingContentLength</Code>"));
            assert!(storage.get_object("delete-framing", key).is_ok());

            let explicit = parsed_request(
                "DELETE",
                uri,
                &[("host", host), ("content-length", "0")],
                b"",
            )
            .await;
            assert!(adapter.validate_request_framing(&explicit).is_none());
            let deleted = adapter
                .handle_request(&storage, &auth_disabled(), &explicit)
                .expect("framed XML DELETE Object should respond");
            assert_eq!(deleted.status(), StatusCode::NO_CONTENT);
            assert!(storage.get_object("delete-framing", key).is_err());
        }

        let chunked = parsed_request(
            "DELETE",
            "http://localhost/chunked",
            &[
                ("host", "delete-framing.storage.googleapis.com"),
                ("transfer-encoding", "chunked"),
            ],
            b"",
        )
        .await;
        assert!(adapter.validate_request_framing(&chunked).is_none());
        let deleted = adapter
            .handle_request(&storage, &auth_disabled(), &chunked)
            .expect("chunked XML DELETE Object should respond");
        assert_eq!(deleted.status(), StatusCode::NO_CONTENT);
        assert!(storage.get_object("delete-framing", "chunked").is_err());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn should_return_xml_no_such_key_without_mutation_for_missing_object_get_and_delete() {
        // Arrange
        let adapter = GcsAdapter::new();
        let storage = temp_storage();
        storage
            .create_bucket("missing-objects".to_string())
            .unwrap();

        // Act
        let get = adapter
            .handle_request(
                &storage,
                &auth_disabled(),
                &parsed_request(
                    "GET",
                    "http://localhost/missing-objects/absent.txt",
                    &[("host", "storage.googleapis.com")],
                    b"",
                )
                .await,
            )
            .expect("missing GET should produce a response");
        let delete = adapter
            .handle_request(
                &storage,
                &auth_disabled(),
                &parsed_request(
                    "DELETE",
                    "http://localhost/missing-objects/absent.txt",
                    &[("host", "storage.googleapis.com")],
                    b"",
                )
                .await,
            )
            .expect("missing DELETE should produce a response");

        // Assert
        assert_eq!(get.status(), StatusCode::NOT_FOUND);
        assert!(String::from_utf8(
            get.into_body()
                .collect()
                .await
                .expect("GET body should read")
                .to_bytes()
                .to_vec(),
        )
        .expect("GET response should be XML")
        .contains("<Code>NoSuchKey</Code>"));
        assert_eq!(delete.status(), StatusCode::NOT_FOUND);
        assert!(String::from_utf8(
            delete
                .into_body()
                .collect()
                .await
                .expect("DELETE body should read")
                .to_bytes()
                .to_vec(),
        )
        .expect("DELETE response should be XML")
        .contains("<Code>NoSuchKey</Code>"));
        assert!(matches!(
            storage.get_object("missing-objects", "absent.txt"),
            Err(crate::error::Error::KeyNotFound)
        ));
    }

    #[tokio::test]
    async fn should_render_json_incomplete_body_for_gcs_json_upload() {
        // Arrange
        let adapter = GcsAdapter::new();
        let uri: Uri = "/upload/storage/v1/b/bucket/o?uploadType=media"
            .parse()
            .expect("URI should parse");

        // Act
        let response = adapter.render_incomplete_body(&Method::POST, &uri, &HeaderMap::new());

        // Assert
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            response
                .headers()
                .get("content-type")
                .and_then(|value| value.to_str().ok()),
            Some("application/json")
        );
        assert_eq!(parse_json_body(response).await["error"]["code"], 400);
    }

    #[tokio::test]
    async fn should_render_xml_incomplete_body_for_gcs_xml_upload() {
        // Arrange
        let adapter = GcsAdapter::new();
        let uri: Uri = "/bucket/object".parse().expect("URI should parse");

        // Act
        let response = adapter.render_incomplete_body(&Method::PUT, &uri, &HeaderMap::new());

        // Assert
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            response
                .headers()
                .get("content-type")
                .and_then(|value| value.to_str().ok()),
            Some("application/xml")
        );
        assert!(String::from_utf8(read_test_body(response).await)
            .expect("response should be XML")
            .contains("<Code>IncompleteBody</Code>"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn should_reject_missing_or_invalid_gcs_upload_type_without_creating_session() {
        // Arrange
        let adapter = GcsAdapter::new();
        let storage = temp_storage();
        storage.create_bucket("uploads".to_string()).unwrap();
        let requests = [
            (
                "http://localhost/upload/storage/v1/b/uploads/o?name=item.txt",
                "required",
            ),
            (
                "http://localhost/upload/storage/v1/b/uploads/o?uploadType=unknown&name=item.txt",
                "invalidArgument",
            ),
            (
                "http://localhost/upload/storage/v1/b/uploads/o?uploadType=resumable",
                "required",
            ),
        ];

        // Act
        // Assert
        for (uri, expected_reason) in requests {
            let response = adapter
                .handle_request(
                    &storage,
                    &auth_disabled(),
                    &parsed_request("POST", uri, &[], b"").await,
                )
                .expect("invalid upload request should respond");
            assert_eq!(response.status(), StatusCode::BAD_REQUEST);
            assert_eq!(
                parse_json_body(response).await["error"]["errors"][0]["reason"],
                expected_reason
            );
        }
        assert!(adapter.resumable_sessions.lock().unwrap().is_empty());
        assert!(storage.get_object("uploads", "item.txt").is_err());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn should_shape_missing_bucket_malformed_multipart_and_unknown_session_errors() {
        // Arrange
        let adapter = GcsAdapter::new();
        let storage = temp_storage();
        storage.create_bucket("uploads".to_string()).unwrap();
        let missing_bucket = parsed_request(
            "POST",
            "http://localhost/upload/storage/v1/b/absent/o?uploadType=media&name=item.txt",
            &[],
            b"data",
        )
        .await;
        let malformed = parsed_request(
            "POST",
            "http://localhost/upload/storage/v1/b/uploads/o?uploadType=multipart",
            &[("content-type", "multipart/related; boundary=sqrzl")],
            b"not-a-multipart-document",
        )
        .await;
        let unknown_session = parsed_request(
            "PUT",
            "http://localhost/upload/resumable/missing-session",
            &[],
            b"data",
        )
        .await;

        // Act
        let missing_bucket = adapter
            .handle_request(&storage, &auth_disabled(), &missing_bucket)
            .expect("missing bucket upload should respond");
        let malformed = adapter
            .handle_request(&storage, &auth_disabled(), &malformed)
            .expect("malformed multipart upload should respond");
        let unknown_session = adapter
            .handle_request(&storage, &auth_disabled(), &unknown_session)
            .expect("unknown resumable session should respond");

        // Assert
        assert_eq!(missing_bucket.status(), StatusCode::NOT_FOUND);
        assert_eq!(malformed.status(), StatusCode::BAD_REQUEST);
        assert_eq!(unknown_session.status(), StatusCode::NOT_FOUND);
        for response in [missing_bucket, malformed, unknown_session] {
            assert_eq!(response.headers()["content-type"], "application/json");
            assert!(parse_json_body(response).await.get("error").is_some());
        }
        assert!(storage.get_object("uploads", "item.txt").is_err());
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
            &GcsAdapter::string_to_sign(&request, "videos", Some("movie.txt"), expires)
                .expect("string to sign should build"),
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
    async fn should_preserve_binary_multipart_media_bytes_exactly() {
        // Arrange
        let adapter = GcsAdapter::new();
        let storage = temp_storage();
        storage
            .create_bucket("multipart-binary".to_string())
            .unwrap();
        let media = b" \xff\x00 ";
        let mut body = b"--sqrzl-boundary\r\nContent-Type: application/json; charset=UTF-8\r\n\r\n{\"name\":\"binary.dat\",\"contentType\":\"application/x-sqrzl\",\"metadata\":{\"owner\":\"alice\"}}\r\n--sqrzl-boundary\r\nContent-Transfer-Encoding: binary\r\n\r\n".to_vec();
        body.extend_from_slice(media);
        body.extend_from_slice(b"\r\n--sqrzl-boundary--\r\n");
        let upload = parsed_request(
            "POST",
            "http://localhost/upload/storage/v1/b/multipart-binary/o?uploadType=multipart",
            &[
                ("host", "storage.googleapis.com"),
                ("content-type", "multipart/related; boundary=sqrzl-boundary"),
            ],
            &body,
        )
        .await;

        // Act
        let response = adapter
            .handle_request(&storage, &auth_disabled(), &upload)
            .expect("binary multipart upload should respond");

        // Assert
        assert_eq!(response.status(), StatusCode::OK);
        let stored = storage
            .get_object("multipart-binary", "binary.dat")
            .expect("multipart object should be stored");
        assert_eq!(stored.data, media);
        assert_eq!(stored.content_type, "application/x-sqrzl");
        assert_eq!(
            stored.metadata.get("owner").map(String::as_str),
            Some("alice")
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn should_apply_resumable_json_metadata_and_body_name_without_silent_fields() {
        // Arrange
        let adapter = GcsAdapter::new();
        let storage = temp_storage();
        storage
            .create_bucket("resumable-metadata".to_string())
            .unwrap();
        let initiate = parsed_request(
            "POST",
            "http://localhost/upload/storage/v1/b/resumable-metadata/o?uploadType=resumable",
            &[
                ("host", "storage.googleapis.com"),
                ("content-type", "application/json"),
            ],
            br#"{"name":"body-name.txt","contentType":"text/plain","metadata":{"owner":"alice"}}"#,
        )
        .await;

        // Act
        let response = adapter
            .handle_request(&storage, &auth_disabled(), &initiate)
            .expect("resumable metadata initiation should respond");
        assert_eq!(response.status(), StatusCode::OK);
        let location = header_value(&response, "location")
            .expect("resumable session location should exist")
            .to_string();
        let completion = adapter
            .handle_request(
                &storage,
                &auth_disabled(),
                &parsed_request(
                    "PUT",
                    &location,
                    &[
                        ("host", "storage.googleapis.com"),
                        ("x-goog-meta-stage", "complete"),
                    ],
                    b"payload",
                )
                .await,
            )
            .expect("resumable metadata completion should respond");

        // Assert
        assert_eq!(completion.status(), StatusCode::OK);
        let stored = storage
            .get_object("resumable-metadata", "body-name.txt")
            .expect("resumable object should be stored");
        assert_eq!(stored.content_type, "text/plain");
        assert_eq!(
            stored.metadata.get("owner").map(String::as_str),
            Some("alice")
        );
        assert_eq!(
            stored.metadata.get("stage").map(String::as_str),
            Some("complete")
        );

        let unsupported = adapter
            .handle_request(
                &storage,
                &auth_disabled(),
                &parsed_request(
                    "POST",
                    "http://localhost/upload/storage/v1/b/resumable-metadata/o?uploadType=resumable&name=unsupported.txt",
                    &[("content-type", "application/json")],
                    br#"{"cacheControl":"no-cache"}"#,
                )
                .await,
            )
            .expect("unsupported resumable metadata should respond");
        assert_eq!(unsupported.status(), StatusCode::NOT_IMPLEMENTED);
        assert!(storage
            .get_object("resumable-metadata", "unsupported.txt")
            .is_err());
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
    async fn should_reject_resumable_etag_headers_without_consuming_the_session() {
        let adapter = GcsAdapter::new();
        let storage = temp_storage();
        storage.create_bucket("videos".to_string()).unwrap();
        let initiated = adapter
            .handle_request(
                &storage,
                &auth_disabled(),
                &parsed_request(
                    "POST",
                    "http://localhost/upload/storage/v1/b/videos/o?uploadType=resumable&name=guarded.txt",
                    &[("host", "storage.googleapis.com")],
                    b"",
                )
                .await,
            )
            .expect("resumable initiation should respond");
        let location = header_value(&initiated, "location")
            .expect("resumable session location should exist")
            .to_string();

        let rejected = adapter
            .handle_request(
                &storage,
                &auth_disabled(),
                &parsed_request(
                    "PUT",
                    &location,
                    &[
                        ("host", "storage.googleapis.com"),
                        ("if-match", "\"unsupported\""),
                    ],
                    b"payload",
                )
                .await,
            )
            .expect("unsupported header should respond");
        assert_eq!(rejected.status(), StatusCode::BAD_REQUEST);
        assert!(storage.get_object("videos", "guarded.txt").is_err());

        let retried = adapter
            .handle_request(
                &storage,
                &auth_disabled(),
                &parsed_request(
                    "PUT",
                    &location,
                    &[("host", "storage.googleapis.com")],
                    b"payload",
                )
                .await,
            )
            .expect("the retained resumable session should still complete");
        assert_eq!(retried.status(), StatusCode::OK);
        assert_eq!(
            storage
                .get_object("videos", "guarded.txt")
                .expect("retry should commit the object")
                .data,
            b"payload"
        );
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
    async fn should_reject_gcs_json_object_head_with_json_error_without_mutation() {
        // Arrange
        let adapter = GcsAdapter::new();
        let storage = temp_storage();
        storage.create_bucket("json-head".to_string()).unwrap();
        let upload = adapter
            .handle_request(
                &storage,
                &auth_disabled(),
                &parsed_request(
                    "POST",
                    "http://localhost/upload/storage/v1/b/json-head/o?uploadType=media&name=empty.txt",
                    &[("host", "storage.googleapis.com"), ("content-length", "0")],
                    b"",
                )
                .await,
            )
            .expect("empty media upload should produce a response");
        assert_eq!(upload.status(), StatusCode::OK);

        // Act
        let response = adapter
            .handle_request(
                &storage,
                &auth_disabled(),
                &parsed_request(
                    "HEAD",
                    "http://localhost/storage/v1/b/json-head/o/empty.txt",
                    &[("host", "storage.googleapis.com")],
                    b"",
                )
                .await,
            )
            .expect("JSON HEAD should produce a response");

        // Assert
        assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
        assert_eq!(
            response
                .headers()
                .get("content-type")
                .and_then(|value| value.to_str().ok()),
            Some("application/json")
        );
        let json: serde_json::Value = serde_json::from_slice(&read_test_body(response).await)
            .expect("JSON HEAD rejection should use a JSON error envelope");
        assert_eq!(json["error"]["code"], 405);
        assert_eq!(
            storage.get_object("json-head", "empty.txt").unwrap().size,
            0
        );
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
    async fn should_validate_matching_gcs_json_multipart_crc32c() {
        // Arrange
        let adapter = GcsAdapter::new();
        let storage = temp_storage();
        storage
            .create_bucket("multipart-crc32c".to_string())
            .unwrap();
        let boundary = "sqrzl-crc32c";
        let body = format!(
            "--{boundary}\r\nContent-Type: application/json; charset=UTF-8\r\n\r\n{{\"name\":\"checked.txt\",\"crc32c\":\"KGE25g==\"}}\r\n--{boundary}\r\nContent-Type: text/plain\r\n\r\nmultipart body\r\n--{boundary}--\r\n"
        );
        let request = parsed_request(
            "POST",
            "http://localhost/upload/storage/v1/b/multipart-crc32c/o?uploadType=multipart",
            &[
                ("host", "storage.googleapis.com"),
                ("content-type", "multipart/related; boundary=sqrzl-crc32c"),
            ],
            body.as_bytes(),
        )
        .await;

        // Act
        let response = adapter
            .handle_request(&storage, &auth_disabled(), &request)
            .expect("matching CRC32C upload should respond");

        // Assert
        assert_eq!(response.status(), StatusCode::OK);
        let response_json: serde_json::Value =
            serde_json::from_slice(&read_test_body(response).await)
                .expect("upload response should contain JSON metadata");
        assert_eq!(response_json["crc32c"], "KGE25g==");
        assert_eq!(
            storage
                .get_object("multipart-crc32c", "checked.txt")
                .expect("matching CRC32C upload should persist")
                .data,
            b"multipart body"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn should_reject_mismatched_gcs_json_multipart_crc32c_without_mutation() {
        // Arrange
        let adapter = GcsAdapter::new();
        let storage = temp_storage();
        storage
            .create_bucket("multipart-crc32c".to_string())
            .unwrap();
        storage
            .put_object(
                "multipart-crc32c",
                "checked.txt".to_string(),
                crate::models::Object::new(
                    "checked.txt".to_string(),
                    b"preserved".to_vec(),
                    "text/plain".to_string(),
                ),
            )
            .unwrap();
        let before = storage
            .get_object("multipart-crc32c", "checked.txt")
            .unwrap();
        let boundary = "sqrzl-crc32c";
        let body = format!(
            "--{boundary}\r\nContent-Type: application/json; charset=UTF-8\r\n\r\n{{\"name\":\"checked.txt\",\"crc32c\":\"AAAAAA==\"}}\r\n--{boundary}\r\nContent-Type: text/plain\r\n\r\nmultipart body\r\n--{boundary}--\r\n"
        );
        let request = parsed_request(
            "POST",
            "http://localhost/upload/storage/v1/b/multipart-crc32c/o?uploadType=multipart",
            &[
                ("host", "storage.googleapis.com"),
                ("content-type", "multipart/related; boundary=sqrzl-crc32c"),
            ],
            body.as_bytes(),
        )
        .await;

        // Act
        let response = adapter
            .handle_request(&storage, &auth_disabled(), &request)
            .expect("mismatched CRC32C upload should respond");

        // Assert
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let response_json: serde_json::Value =
            serde_json::from_slice(&read_test_body(response).await)
                .expect("checksum rejection should use the JSON error envelope");
        assert_eq!(response_json["error"]["code"], 400);
        assert_eq!(response_json["error"]["errors"][0]["reason"], "invalid");
        let after = storage
            .get_object("multipart-crc32c", "checked.txt")
            .expect("failed checksum must preserve the existing object");
        assert_eq!(after.data, before.data);
        assert_eq!(after.etag, before.etag);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn should_reject_mismatched_resumable_metadata_crc32c_without_mutation() {
        // Arrange
        let adapter = GcsAdapter::new();
        let storage = temp_storage();
        storage
            .create_bucket("resumable-crc32c".to_string())
            .unwrap();
        let initiation = adapter
            .handle_request(
                &storage,
                &auth_disabled(),
                &parsed_request(
                    "POST",
                    "http://localhost/upload/storage/v1/b/resumable-crc32c/o?uploadType=resumable",
                    &[("content-type", "application/json")],
                    br#"{"name":"checked.txt","crc32c":"9ONpcA=="}"#,
                )
                .await,
            )
            .expect("resumable checksum initiation should respond");
        let location = header_value(&initiation, "location")
            .expect("resumable checksum session should have a location")
            .to_string();

        // Act
        let response = adapter
            .handle_request(
                &storage,
                &auth_disabled(),
                &parsed_request("PUT", &location, &[], b"wrong").await,
            )
            .expect("mismatched resumable checksum should respond");

        // Assert
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let response_json: serde_json::Value =
            serde_json::from_slice(&read_test_body(response).await)
                .expect("resumable checksum rejection should use JSON");
        assert_eq!(response_json["error"]["errors"][0]["reason"], "invalid");
        assert!(storage
            .get_object("resumable-crc32c", "checked.txt")
            .is_err());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn should_validate_matching_gcs_json_media_x_goog_hash_crc32c() {
        // Arrange
        let adapter = GcsAdapter::new();
        let storage = temp_storage();
        storage.create_bucket("media-crc32c".to_string()).unwrap();
        let request = parsed_request(
            "POST",
            "http://localhost/upload/storage/v1/b/media-crc32c/o?uploadType=media&name=checked.txt",
            &[("x-goog-hash", "crc32c=9ONpcA==, crc32c=9ONpcA==")],
            b"payload",
        )
        .await;

        // Act
        let response = adapter
            .handle_request(&storage, &auth_disabled(), &request)
            .expect("matching media checksum should respond");

        // Assert
        assert_eq!(response.status(), StatusCode::OK);
        let response_json: serde_json::Value =
            serde_json::from_slice(&read_test_body(response).await)
                .expect("matching media response should contain JSON metadata");
        assert_eq!(response_json["crc32c"], "9ONpcA==");
        assert_eq!(
            storage
                .get_object("media-crc32c", "checked.txt")
                .expect("matching media checksum should persist")
                .data,
            b"payload"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn should_reject_invalid_gcs_json_media_x_goog_hash_without_mutation() {
        // Arrange
        let adapter = GcsAdapter::new();
        let storage = temp_storage();
        storage.create_bucket("media-crc32c".to_string()).unwrap();
        storage
            .put_object(
                "media-crc32c",
                "checked.txt".to_string(),
                crate::models::Object::new(
                    "checked.txt".to_string(),
                    b"preserved".to_vec(),
                    "text/plain".to_string(),
                ),
            )
            .unwrap();
        let cases = [
            ("crc32c=AAAAAA==", StatusCode::BAD_REQUEST),
            ("crc32c=not-base64", StatusCode::BAD_REQUEST),
            ("crc32c", StatusCode::BAD_REQUEST),
            ("md5=XUFAKrxLKna5cZ2REBfFkg==", StatusCode::NOT_IMPLEMENTED),
        ];

        // Act
        // Assert
        for (hash, expected_status) in cases {
            let response = adapter
                .handle_request(
                    &storage,
                    &auth_disabled(),
                    &parsed_request(
                        "POST",
                        "http://localhost/upload/storage/v1/b/media-crc32c/o?uploadType=media&name=checked.txt",
                        &[("x-goog-hash", hash)],
                        b"payload",
                    )
                    .await,
                )
                .expect("invalid media hash should respond");
            assert_eq!(response.status(), expected_status, "{hash}");
            assert_eq!(
                storage
                    .get_object("media-crc32c", "checked.txt")
                    .expect("invalid media hash must preserve the object")
                    .data,
                b"preserved",
                "{hash}"
            );
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn should_retain_resumable_session_after_x_goog_hash_crc32c_mismatch() {
        // Arrange
        let adapter = GcsAdapter::new();
        let storage = temp_storage();
        storage
            .create_bucket("resumable-header-crc32c".to_string())
            .unwrap();
        let initiation = adapter
            .handle_request(
                &storage,
                &auth_disabled(),
                &parsed_request(
                    "POST",
                    "http://localhost/upload/storage/v1/b/resumable-header-crc32c/o?uploadType=resumable&name=checked.txt",
                    &[],
                    b"",
                )
                .await,
            )
            .expect("resumable session initiation should respond");
        let location = header_value(&initiation, "location")
            .expect("resumable session should have a location")
            .to_string();

        // Act
        let rejected = adapter
            .handle_request(
                &storage,
                &auth_disabled(),
                &parsed_request(
                    "PUT",
                    &location,
                    &[("x-goog-hash", "crc32c=AAAAAA==")],
                    b"payload",
                )
                .await,
            )
            .expect("mismatched resumable checksum should respond");
        let absent_after_rejection = storage
            .get_object("resumable-header-crc32c", "checked.txt")
            .is_err();
        let response = adapter
            .handle_request(
                &storage,
                &auth_disabled(),
                &parsed_request(
                    "PUT",
                    &location,
                    &[("x-goog-hash", "crc32c=9ONpcA==")],
                    b"payload",
                )
                .await,
            )
            .expect("checksum retry should respond");

        // Assert
        assert_eq!(rejected.status(), StatusCode::BAD_REQUEST);
        assert!(absent_after_rejection);
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            storage
                .get_object("resumable-header-crc32c", "checked.txt")
                .expect("checksum retry should persist the object")
                .data,
            b"payload"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn should_persist_gcs_json_media_uploads_with_generation_preconditions() {
        let adapter = GcsAdapter::new();
        let storage = temp_storage();
        storage.create_bucket("media-bucket".to_string()).unwrap();

        let create_uri = "http://localhost/upload/storage/v1/b/media-bucket/o?uploadType=media&name=wal%2Fpublication-catalog.v1.json&ifGenerationMatch=0";
        let response = upload_gcs_json_media(
            &adapter,
            &storage,
            create_uri,
            br#"{"version":1}"#,
            &[
                ("content-type", "application/json"),
                ("x-goog-meta-owner", "midge"),
            ],
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let created = parse_json_body(response).await;
        assert_eq!(created["name"], "wal/publication-catalog.v1.json");
        let generation = created["generation"]
            .as_str()
            .expect("generation should exist")
            .to_string();
        assert!(created["etag"]
            .as_str()
            .is_some_and(|etag| !etag.is_empty()));

        let metadata = adapter
            .handle_request(
                &storage.clone(),
                &auth_disabled(),
                &parsed_request(
                    "GET",
                    "http://localhost/storage/v1/b/media-bucket/o/wal%2Fpublication-catalog.v1.json",
                    &[("host", "storage.googleapis.com")],
                    b"",
                )
                .await,
            )
            .expect("metadata read should succeed");
        assert_eq!(metadata.status(), StatusCode::OK);
        let metadata = parse_json_body(metadata).await;
        assert_eq!(metadata["generation"], generation);
        assert_eq!(metadata["metadata"]["owner"], "midge");

        let media = read_gcs_json_media(&adapter, &storage).await;
        assert_eq!(media.status(), StatusCode::OK);
        assert_eq!(read_test_body(media).await, br#"{"version":1}"#);

        let matching_uri = format!(
            "http://localhost/upload/storage/v1/b/media-bucket/o?uploadType=media&name=wal%2Fpublication-catalog.v1.json&ifGenerationMatch={generation}"
        );
        let overwritten =
            upload_gcs_json_media(&adapter, &storage, &matching_uri, br#"{"version":2}"#, &[])
                .await;
        assert_eq!(overwritten.status(), StatusCode::OK);

        let stale =
            upload_gcs_json_media(&adapter, &storage, &matching_uri, br#"{"version":3}"#, &[])
                .await;
        assert_eq!(stale.status(), StatusCode::PRECONDITION_FAILED);

        let media = read_gcs_json_media(&adapter, &storage).await;
        assert_eq!(read_test_body(media).await, br#"{"version":2}"#);
    }

    async fn upload_gcs_json_media(
        adapter: &GcsAdapter,
        storage: &Arc<dyn Storage>,
        uri: &str,
        body: &[u8],
        extra_headers: &[(&str, &str)],
    ) -> Response<Body> {
        let mut headers = vec![("host", "storage.googleapis.com")];
        headers.extend_from_slice(extra_headers);
        adapter
            .handle_request(
                &storage.clone(),
                &auth_disabled(),
                &parsed_request("POST", uri, &headers, body).await,
            )
            .expect("media upload should return a response")
    }

    async fn read_gcs_json_media(
        adapter: &GcsAdapter,
        storage: &Arc<dyn Storage>,
    ) -> Response<Body> {
        adapter
            .handle_request(
                &storage.clone(),
                &auth_disabled(),
                &parsed_request(
                    "GET",
                    "http://localhost/storage/v1/b/media-bucket/o/wal%2Fpublication-catalog.v1.json?alt=media",
                    &[("host", "storage.googleapis.com")],
                    b"",
                )
                .await,
            )
            .expect("media read should return a response")
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

    #[tokio::test(flavor = "multi_thread")]
    async fn should_patch_protected_object_metadata_without_creating_a_new_version() {
        // Arrange
        let adapter = GcsAdapter::new();
        let storage = temp_storage();
        let create = parsed_request(
            "POST",
            "http://localhost/storage/v1/b?project=test-project",
            &[
                ("host", "storage.googleapis.com"),
                ("content-type", "application/json"),
            ],
            br#"{"name":"protected-metadata","softDeletePolicy":{"retentionDurationSeconds":"604800"}}"#,
        )
        .await;
        assert_eq!(
            adapter
                .handle_request(&storage, &auth_disabled(), &create)
                .unwrap()
                .status(),
            StatusCode::OK
        );
        let upload = parsed_request(
            "POST",
            "http://localhost/upload/storage/v1/b/protected-metadata/o?uploadType=media&name=object",
            &[("content-type", "text/plain")],
            b"preserved bytes",
        )
        .await;
        assert_eq!(
            adapter
                .handle_request(&storage, &auth_disabled(), &upload)
                .unwrap()
                .status(),
            StatusCode::OK
        );
        let observed = storage.get_object("protected-metadata", "object").unwrap();
        let observed_metadata = GcsAdapter::json_object_metadata("protected-metadata", &observed);
        let generation = observed.metadata[GCS_GENERATION_KEY].clone();
        let versions_before = storage
            .list_object_versions_for_key("protected-metadata", "object")
            .unwrap();
        let patch = parsed_request(
            "PATCH",
            &format!(
                "http://localhost/storage/v1/b/protected-metadata/o/object?ifGenerationMatch={generation}&ifMetagenerationMatch=1"
            ),
            &[("content-type", "application/json")],
            br#"{"contentType":"application/sqrzl-test","metadata":{"owner":"gcs"}}"#,
        )
        .await;

        // Act
        let response = adapter
            .handle_request(&storage, &auth_disabled(), &patch)
            .expect("metadata patch should respond");

        // Assert
        assert_eq!(response.status(), StatusCode::OK);
        let metadata_response = parse_json_body(response).await;
        assert_eq!(metadata_response["generation"], generation);
        assert_eq!(metadata_response["metageneration"], "2");
        assert_ne!(metadata_response["etag"], observed_metadata["etag"]);
        assert_ne!(metadata_response["updated"], observed_metadata["updated"]);
        let updated = storage.get_object("protected-metadata", "object").unwrap();
        assert_eq!(updated.data, observed.data);
        assert_eq!(updated.etag, observed.etag);
        assert_eq!(updated.last_modified, observed.last_modified);
        assert_eq!(updated.version_id, observed.version_id);
        assert_eq!(updated.metadata[GCS_GENERATION_KEY], generation);
        assert_eq!(updated.metadata[GCS_METAGENERATION_KEY], "2");
        assert_eq!(
            updated.metadata.get("owner").map(String::as_str),
            Some("gcs")
        );
        assert_eq!(updated.content_type, "application/sqrzl-test");
        let versions_after = storage
            .list_object_versions_for_key("protected-metadata", "object")
            .unwrap();
        assert_eq!(versions_after.len(), versions_before.len());
        assert_eq!(
            versions_after
                .iter()
                .map(|version| version.version_id.as_deref())
                .collect::<Vec<_>>(),
            versions_before
                .iter()
                .map(|version| version.version_id.as_deref())
                .collect::<Vec<_>>()
        );
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
                        ("content-type", "application/json"),
                    ],
                    br#"{"metadata":{"owner":"jules"}}"#,
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

    #[tokio::test(flavor = "multi_thread")]
    async fn should_require_auth_and_project_for_json_bucket_collection() {
        // Arrange
        let adapter = GcsAdapter::new();
        let storage = temp_storage();
        let missing_auth = parsed_request(
            "GET",
            "http://localhost/storage/v1/b?project=test-project",
            &[("host", "storage.googleapis.com")],
            b"",
        )
        .await;
        let missing_project = parsed_request(
            "GET",
            "http://localhost/storage/v1/b",
            &[("host", "storage.googleapis.com")],
            b"",
        )
        .await;

        // Act
        let unauthorized = adapter
            .handle_request(&storage, &gcs_auth(), &missing_auth)
            .expect("missing auth should produce a response");
        let invalid = adapter
            .handle_request(&storage, &auth_disabled(), &missing_project)
            .expect("missing project should produce a response");

        // Assert
        assert_eq!(unauthorized.status(), StatusCode::UNAUTHORIZED);
        let unauthorized_body = unauthorized
            .into_body()
            .collect()
            .await
            .expect("authorization body should read")
            .to_bytes();
        let unauthorized_body: serde_json::Value =
            serde_json::from_slice(&unauthorized_body).expect("authorization error should be JSON");
        assert_eq!(unauthorized_body["error"]["code"], 401);
        assert_eq!(
            unauthorized_body["error"]["errors"][0]["reason"],
            "authError"
        );
        assert_eq!(invalid.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn should_return_prefixes_and_opaque_page_tokens_for_json_object_list() {
        // Arrange
        let adapter = GcsAdapter::new();
        let storage = temp_storage();
        storage.create_bucket("listed".to_string()).unwrap();
        for key in ["docs/a.txt", "docs/archive/b.txt", "docs/z.txt"] {
            storage
                .put_object(
                    "listed",
                    key.to_string(),
                    crate::models::Object::new(
                        key.to_string(),
                        b"x".to_vec(),
                        "text/plain".to_string(),
                    ),
                )
                .unwrap();
        }
        let request = parsed_request(
            "GET",
            "http://localhost/storage/v1/b/listed/o?prefix=docs%2F&delimiter=%2F&maxResults=2",
            &[("host", "storage.googleapis.com")],
            b"",
        )
        .await;

        // Act
        let response = adapter
            .handle_request(&storage, &auth_disabled(), &request)
            .expect("object list should respond");
        let json = parse_json_body(response).await;

        // Assert
        assert_eq!(json["prefixes"][0], "docs/archive/");
        assert!(json["nextPageToken"]
            .as_str()
            .is_some_and(|token| token != "docs/a.txt"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn should_only_include_gcs_json_next_page_tokens_on_truncated_lists() {
        // Arrange
        let adapter = GcsAdapter::new();
        let storage = temp_storage();
        for bucket in ["page-a", "page-b"] {
            storage.create_bucket(bucket.to_string()).unwrap();
        }
        for key in ["a.txt", "b.txt"] {
            storage
                .put_object(
                    "page-a",
                    key.to_string(),
                    crate::models::Object::new(
                        key.to_string(),
                        b"x".to_vec(),
                        "text/plain".to_string(),
                    ),
                )
                .unwrap();
        }

        // Act
        let bucket_first = adapter
            .handle_request(
                &storage,
                &auth_disabled(),
                &parsed_request(
                    "GET",
                    "http://localhost/storage/v1/b?project=test&maxResults=1",
                    &[("host", "storage.googleapis.com")],
                    b"",
                )
                .await,
            )
            .expect("first bucket page should respond");
        let bucket_first = parse_json_body(bucket_first).await;
        let bucket_token = bucket_first["nextPageToken"]
            .as_str()
            .expect("truncated bucket page should contain a token");
        let bucket_last = adapter
            .handle_request(
                &storage,
                &auth_disabled(),
                &parsed_request(
                    "GET",
                    &format!(
                        "http://localhost/storage/v1/b?project=test&maxResults=1&pageToken={bucket_token}"
                    ),
                    &[("host", "storage.googleapis.com")],
                    b"",
                )
                .await,
            )
            .expect("last bucket page should respond");
        let bucket_last = parse_json_body(bucket_last).await;

        let object_first = adapter
            .handle_request(
                &storage,
                &auth_disabled(),
                &parsed_request(
                    "GET",
                    "http://localhost/storage/v1/b/page-a/o?maxResults=1",
                    &[("host", "storage.googleapis.com")],
                    b"",
                )
                .await,
            )
            .expect("first object page should respond");
        let object_first = parse_json_body(object_first).await;
        let object_token = object_first["nextPageToken"]
            .as_str()
            .expect("truncated object page should contain a token");
        let object_last = adapter
            .handle_request(
                &storage,
                &auth_disabled(),
                &parsed_request(
                    "GET",
                    &format!(
                        "http://localhost/storage/v1/b/page-a/o?maxResults=1&pageToken={object_token}"
                    ),
                    &[("host", "storage.googleapis.com")],
                    b"",
                )
                .await,
            )
            .expect("last object page should respond");
        let object_last = parse_json_body(object_last).await;

        // Assert
        assert!(bucket_first.get("nextPageToken").is_some());
        assert_eq!(bucket_last["items"][0]["name"], "page-b");
        assert!(bucket_last.get("nextPageToken").is_none());
        assert!(object_first.get("nextPageToken").is_some());
        assert_eq!(object_last["items"][0]["name"], "b.txt");
        assert!(object_last.get("nextPageToken").is_none());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn should_preserve_first_gcs_object_after_zero_sized_page() {
        // Arrange
        let adapter = GcsAdapter::new();
        let storage = temp_storage();
        storage.create_bucket("zero-page".to_string()).unwrap();
        storage
            .put_object(
                "zero-page",
                "first.txt".to_string(),
                crate::models::Object::new(
                    "first.txt".to_string(),
                    b"x".to_vec(),
                    "text/plain".to_string(),
                ),
            )
            .unwrap();

        // Act
        let empty_page = adapter
            .handle_request(
                &storage,
                &auth_disabled(),
                &parsed_request(
                    "GET",
                    "http://localhost/storage/v1/b/zero-page/o?maxResults=0",
                    &[("host", "storage.googleapis.com")],
                    b"",
                )
                .await,
            )
            .expect("zero-sized page should respond");
        let empty_page = parse_json_body(empty_page).await;
        let token = empty_page["nextPageToken"]
            .as_str()
            .expect("nonempty listing should return a continuation token");
        let next_page = adapter
            .handle_request(
                &storage,
                &auth_disabled(),
                &parsed_request(
                    "GET",
                    &format!(
                        "http://localhost/storage/v1/b/zero-page/o?maxResults=1&pageToken={token}"
                    ),
                    &[("host", "storage.googleapis.com")],
                    b"",
                )
                .await,
            )
            .expect("continuation page should respond");
        // Assert
        assert!(empty_page["items"].as_array().is_some_and(Vec::is_empty));
        let next_page = parse_json_body(next_page).await;
        assert_eq!(next_page["items"][0]["name"], "first.txt");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn should_reject_nonnumeric_gcs_json_page_size() {
        // Arrange
        let adapter = GcsAdapter::new();
        let storage = temp_storage();
        storage
            .create_bucket("invalid-page-size".to_string())
            .unwrap();

        // Act
        let response = adapter
            .handle_request(
                &storage,
                &auth_disabled(),
                &parsed_request(
                    "GET",
                    "http://localhost/storage/v1/b/invalid-page-size/o?maxResults=not-a-number",
                    &[("host", "storage.googleapis.com")],
                    b"",
                )
                .await,
            )
            .expect("invalid page size should respond");

        // Assert
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(parse_json_body(response).await["error"]["code"], 400);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn should_reject_retained_overwrite_and_invalid_generation_preconditions() {
        // Arrange
        let adapter = GcsAdapter::new();
        let storage = temp_storage();
        let create = parsed_request(
            "POST",
            "http://localhost/storage/v1/b?project=test-project",
            &[
                ("host", "storage.googleapis.com"),
                ("content-type", "application/json"),
            ],
            br#"{"name":"retained","retentionPolicy":{"retentionPeriod":"3600"}}"#,
        )
        .await;
        assert_eq!(
            adapter
                .handle_request(&storage, &auth_disabled(), &create)
                .expect("bucket create should respond")
                .status(),
            StatusCode::OK
        );
        let create_object = parsed_request(
            "POST",
            "http://localhost/upload/storage/v1/b/retained/o?uploadType=media&name=item.txt",
            &[("host", "storage.googleapis.com")],
            b"first",
        )
        .await;
        assert_eq!(
            adapter
                .handle_request(&storage, &auth_disabled(), &create_object)
                .expect("initial upload should respond")
                .status(),
            StatusCode::OK
        );
        let overwrite = parsed_request(
            "POST",
            "http://localhost/upload/storage/v1/b/retained/o?uploadType=media&name=item.txt",
            &[("host", "storage.googleapis.com")],
            b"second",
        )
        .await;
        let invalid = parsed_request(
            "POST",
            "http://localhost/upload/storage/v1/b/retained/o?uploadType=media&name=other.txt&ifGenerationMatch=0&ifGenerationNotMatch=1",
            &[("host", "storage.googleapis.com")],
            b"other",
        )
        .await;
        let xml_delete = parsed_request(
            "DELETE",
            "http://localhost/retained/item.txt",
            &[("host", "storage.googleapis.com")],
            b"",
        )
        .await;

        // Act
        let retained = adapter
            .handle_request(&storage, &auth_disabled(), &overwrite)
            .expect("retained overwrite should respond");
        let invalid = adapter
            .handle_request(&storage, &auth_disabled(), &invalid)
            .expect("invalid preconditions should respond");
        let xml_delete = adapter
            .handle_request(&storage, &auth_disabled(), &xml_delete)
            .expect("retained XML delete should respond");

        // Assert
        assert_eq!(retained.status(), StatusCode::FORBIDDEN);
        assert!(String::from_utf8_lossy(&read_test_body(retained).await)
            .contains("retentionPolicyNotMet"));
        assert_eq!(invalid.status(), StatusCode::BAD_REQUEST);
        assert_eq!(xml_delete.status(), StatusCode::FORBIDDEN);
        assert!(String::from_utf8_lossy(&read_test_body(xml_delete).await)
            .contains("<Code>RetentionPolicyNotMet</Code>"));
        assert_eq!(
            storage.get_object("retained", "item.txt").unwrap().data,
            b"first"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn should_reject_unsupported_or_invalid_bucket_retention_configuration_without_mutation()
    {
        // Arrange
        let adapter = GcsAdapter::new();
        let storage = temp_storage();
        let invalid_soft_delete = parsed_request(
            "POST",
            "http://localhost/storage/v1/b?project=test-project",
            &[
                ("host", "storage.googleapis.com"),
                ("content-type", "application/json"),
            ],
            br#"{"name":"invalid-soft-delete","softDeletePolicy":{"retentionDurationSeconds":"604799"}}"#,
        )
        .await;
        let locked_create = parsed_request(
            "POST",
            "http://localhost/storage/v1/b?project=test-project",
            &[
                ("host", "storage.googleapis.com"),
                ("content-type", "application/json"),
            ],
            br#"{"name":"locked-create","retentionPolicy":{"retentionPeriod":"3600","isLocked":true}}"#,
        )
        .await;

        // Act
        let invalid_soft_delete = adapter
            .handle_request(&storage, &auth_disabled(), &invalid_soft_delete)
            .expect("invalid soft-delete policy should respond");
        let locked_create = adapter
            .handle_request(&storage, &auth_disabled(), &locked_create)
            .expect("unsupported locked policy should respond");

        // Assert
        assert_eq!(invalid_soft_delete.status(), StatusCode::BAD_REQUEST);
        assert_eq!(locked_create.status(), StatusCode::NOT_IMPLEMENTED);
        assert!(!storage.bucket_exists("invalid-soft-delete").unwrap());
        assert!(!storage.bucket_exists("locked-create").unwrap());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn should_reject_unsupported_retention_updates_without_changing_valid_configuration() {
        // Arrange
        let adapter = GcsAdapter::new();
        let storage = temp_storage();
        let valid_create = parsed_request(
            "POST",
            "http://localhost/storage/v1/b?project=test-project",
            &[
                ("host", "storage.googleapis.com"),
                ("content-type", "application/json"),
            ],
            br#"{"name":"retention-config","retentionPolicy":{"retentionPeriod":"3600"}}"#,
        )
        .await;
        assert_eq!(
            adapter
                .handle_request(&storage, &auth_disabled(), &valid_create)
                .expect("valid retention policy should respond")
                .status(),
            StatusCode::OK
        );
        let disable_soft_delete = parsed_request(
            "PATCH",
            "http://localhost/storage/v1/b/retention-config",
            &[
                ("host", "storage.googleapis.com"),
                ("content-type", "application/json"),
            ],
            br#"{"softDeletePolicy":{"retentionDurationSeconds":"0"}}"#,
        )
        .await;
        let invalid_retention = parsed_request(
            "PATCH",
            "http://localhost/storage/v1/b/retention-config",
            &[
                ("host", "storage.googleapis.com"),
                ("content-type", "application/json"),
            ],
            br#"{"retentionPolicy":{"retentionPeriod":"3155760000"}}"#,
        )
        .await;
        let lock = parsed_request(
            "POST",
            "http://localhost/storage/v1/b/retention-config/lockRetentionPolicy?ifMetagenerationMatch=1",
            &[("host", "storage.googleapis.com")],
            b"",
        )
        .await;

        // Act and assert
        assert_eq!(
            adapter
                .handle_request(&storage, &auth_disabled(), &disable_soft_delete)
                .expect("unsupported soft-delete disable should respond")
                .status(),
            StatusCode::NOT_IMPLEMENTED
        );
        assert_eq!(
            adapter
                .handle_request(&storage, &auth_disabled(), &invalid_retention)
                .expect("invalid retention duration should respond")
                .status(),
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            adapter
                .handle_request(&storage, &auth_disabled(), &lock)
                .expect("unsupported retention lock should respond")
                .status(),
            StatusCode::NOT_IMPLEMENTED
        );
        let bucket = storage.get_bucket("retention-config").unwrap();
        assert_eq!(
            bucket.metadata.get(GCS_RETENTION_SECONDS_KEY),
            Some(&"3600".to_string())
        );
        assert!(!bucket.metadata.contains_key(GCS_SOFT_DELETE_SECONDS_KEY));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn should_serialize_conflicting_s3_and_gcs_data_protection_activation() {
        for iteration in 0..16 {
            // Arrange
            let storage = temp_storage();
            let bucket = format!(
                "activation-race-{iteration}-{}",
                &uuid::Uuid::new_v4().simple().to_string()[..8]
            );
            storage.create_bucket(bucket.clone()).unwrap();
            let s3_request = parsed_request(
                "PUT",
                &format!("http://localhost/{bucket}?versioning"),
                &[],
                br#"<VersioningConfiguration xmlns="http://s3.amazonaws.com/doc/2006-03-01/"><Status>Enabled</Status></VersioningConfiguration>"#,
            )
            .await;
            let gcs_request = parsed_request(
                "PATCH",
                &format!("http://localhost/storage/v1/b/{bucket}"),
                &[
                    ("host", "storage.googleapis.com"),
                    ("content-type", "application/json"),
                ],
                br#"{"softDeletePolicy":{"retentionDurationSeconds":"604800"}}"#,
            )
            .await;
            let start = Arc::new(tokio::sync::Barrier::new(3));

            let s3_storage = storage.clone();
            let s3_start = start.clone();
            let s3 = tokio::spawn(async move {
                s3_start.wait().await;
                handle_s3_request(s3_storage, auth_disabled(), s3_request)
                    .await
                    .expect("S3 activation should respond")
            });

            let gcs_storage = storage.clone();
            let gcs_start = start.clone();
            let gcs = tokio::spawn(async move {
                gcs_start.wait().await;
                GcsAdapter::new()
                    .handle_request(&gcs_storage, &auth_disabled(), &gcs_request)
                    .expect("GCS activation should respond")
            });

            // Act
            start.wait().await;
            let s3_response = s3.await.expect("S3 activation task should complete");
            let gcs_response = gcs.await.expect("GCS activation task should complete");

            // Assert
            let s3_status = s3_response.status();
            let gcs_status = gcs_response.status();
            assert!(
                matches!(
                    (s3_status, gcs_status),
                    (StatusCode::OK, StatusCode::CONFLICT) | (StatusCode::CONFLICT, StatusCode::OK)
                ),
                "one provider must win activation: S3={s3_status}, GCS={gcs_status}"
            );
            if s3_status == StatusCode::CONFLICT {
                assert!(String::from_utf8_lossy(&read_test_body(s3_response).await)
                    .contains("<Code>InvalidBucketState</Code>"));
            }
            if gcs_status == StatusCode::CONFLICT {
                assert_eq!(
                    parse_json_body(gcs_response).await["error"]["errors"][0]["reason"],
                    "conflict"
                );
            }

            let metadata = storage.get_bucket(&bucket).unwrap().metadata;
            let s3_owns_history = metadata
                .get(S3_VERSIONING_STATUS_KEY)
                .is_some_and(|value| value == "Enabled");
            let gcs_owns_history = metadata
                .get(GCS_SOFT_DELETE_SECONDS_KEY)
                .is_some_and(|value| value == "604800");
            assert_ne!(
                s3_owns_history, gcs_owns_history,
                "the shared namespace must have exactly one protection owner"
            );
            assert_eq!(s3_status == StatusCode::OK, s3_owns_history);
            assert_eq!(gcs_status == StatusCode::OK, gcs_owns_history);
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn should_reject_expired_v2_signed_url() {
        // Arrange
        let adapter = GcsAdapter::new();
        let storage = temp_storage();
        storage.create_bucket("signed".to_string()).unwrap();
        storage
            .put_object(
                "signed",
                "item.txt".to_string(),
                crate::models::Object::new(
                    "item.txt".to_string(),
                    b"secret".to_vec(),
                    "text/plain".to_string(),
                ),
            )
            .unwrap();
        let unsigned = parsed_request(
            "GET",
            "http://localhost/signed/item.txt?GoogleAccessId=test-access&Expires=1",
            &[("host", "storage.googleapis.com")],
            b"",
        )
        .await;
        let signature = GcsAdapter::sign(
            &gcs_auth(),
            &GcsAdapter::string_to_sign(&unsigned, "signed", Some("item.txt"), "1")
                .expect("string to sign should build"),
        )
        .expect("signature should build");
        let request = parsed_request(
            "GET",
            &format!(
                "http://localhost/signed/item.txt?GoogleAccessId=test-access&Expires=1&Signature={}",
                urlencoding::encode(&signature)
            ),
            &[("host", "storage.googleapis.com")],
            b"",
        )
        .await;

        // Act
        let response = adapter
            .handle_request(&storage, &gcs_auth(), &request)
            .expect("expired request should respond");

        // Assert
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn should_reject_partial_resumable_chunk_without_consuming_session() {
        // Arrange
        let adapter = GcsAdapter::new();
        let storage = temp_storage();
        storage.create_bucket("chunks".to_string()).unwrap();
        let init = parsed_request(
            "POST",
            "http://localhost/upload/storage/v1/b/chunks/o?uploadType=resumable&name=item.txt",
            &[("host", "storage.googleapis.com")],
            b"",
        )
        .await;
        let init = adapter
            .handle_request(&storage, &auth_disabled(), &init)
            .expect("session create should respond");
        let location = header_value(&init, "location")
            .expect("session location should exist")
            .to_string();
        let chunk = parsed_request(
            "PUT",
            &location,
            &[
                ("host", "storage.googleapis.com"),
                ("content-range", "bytes 0-2/6"),
            ],
            b"abc",
        )
        .await;

        // Act
        let response = adapter
            .handle_request(&storage, &auth_disabled(), &chunk)
            .expect("partial chunk should respond");

        // Assert
        assert_eq!(response.status(), StatusCode::NOT_IMPLEMENTED);
        assert!(storage.get_object("chunks", "item.txt").is_err());
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
            || req.path().starts_with("/upload/resumable/")
            || req.path().starts_with("/storage/v1/")
            || req.path().starts_with("/download/storage/v1/")
    }

    fn matches_request_head(&self, _method: &Method, uri: &Uri, headers: &HeaderMap) -> bool {
        Self::matches_head(uri, headers)
    }

    fn render_payload_too_large(
        &self,
        _method: &Method,
        uri: &Uri,
        headers: &HeaderMap,
        max_request_bytes: usize,
    ) -> Response<Body> {
        Self::payload_too_large_response(Self::is_json_api_head(uri, headers), max_request_bytes)
    }

    fn render_incomplete_body(
        &self,
        _method: &Method,
        uri: &Uri,
        headers: &HeaderMap,
    ) -> Response<Body> {
        if Self::is_json_api_head(uri, headers) {
            Self::json_error(
                StatusCode::BAD_REQUEST,
                "invalidArgument",
                "The request body did not contain the declared number of bytes.",
            )
        } else {
            Self::error_response(
                StatusCode::BAD_REQUEST,
                "IncompleteBody",
                "The request body did not contain the declared number of bytes.",
            )
        }
    }

    fn validate_request_framing(&self, req: &Request) -> Option<Response<Body>> {
        let json_api = Self::is_json_api_request(req);
        let json_upload = json_api
            && ((*req.method() == Method::POST && req.path().starts_with("/upload/storage/v1/"))
                || (*req.method() == Method::PUT && req.path().starts_with("/upload/resumable/")));
        let xml_request = !json_api;
        let content_length_or_chunked = req.header("content-length").is_some()
            || req
                .header("transfer-encoding")
                .is_some_and(|value| value.eq_ignore_ascii_case("chunked"));
        if (json_upload || xml_request) && !content_length_or_chunked {
            return Some(if json_upload {
                // The GCS JSON status table explicitly defines 411 as a bodyless response.
                Self::response(StatusCode::LENGTH_REQUIRED).empty()
            } else {
                Self::error_response(
                    StatusCode::LENGTH_REQUIRED,
                    "MissingContentLength",
                    "Content-Length is required unless Transfer-Encoding is chunked.",
                )
            });
        }
        super::content_length_mismatch(req).then(|| {
            if json_api {
                Self::json_error(
                    StatusCode::BAD_REQUEST,
                    "invalidArgument",
                    "Content-Length does not match the request body",
                )
            } else {
                Self::error_response(
                    StatusCode::BAD_REQUEST,
                    "IncompleteBody",
                    "Content-Length does not match the request body",
                )
            }
        })
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
        if Self::is_json_api_request(req)
            && (req.path().starts_with("/storage/v1/")
                || req.path().starts_with("/download/storage/v1/"))
        {
            return self.handle_json_api(storage, auth_config, req);
        }

        if Self::is_json_api_request(req)
            && (req.path().starts_with("/upload/storage/v1/b/")
                || req.path().starts_with("/upload/resumable/"))
        {
            return self.handle_resumable(storage, auth_config, req);
        }

        let (bucket, object) = match Self::parse_path(req) {
            Ok(path) => path,
            Err(message) => {
                return Ok(Self::error_response(
                    StatusCode::BAD_REQUEST,
                    "InvalidURI",
                    &message,
                ));
            }
        };
        let Some(bucket) = bucket else {
            return Ok(Self::handle_xml_root_request(storage, req));
        };

        if let Err(response) = Self::authorize(req, auth_config, &bucket, object.as_deref()) {
            return Ok(response);
        }

        if let Some(object) = object {
            if Self::foreign_data_protection_active(storage, &bucket) {
                return Ok(Self::foreign_data_protection_xml_response());
            }
            self.handle_xml_object_request(storage, req, &bucket, &object)
        } else {
            Ok(Self::handle_xml_bucket_request(storage, req, &bucket))
        }
    }

    fn handle_xml_root_request(storage: &Arc<dyn Storage>, req: &Request) -> Response<Body> {
        if req.method() != Method::GET {
            return Self::error_response(StatusCode::BAD_REQUEST, "InvalidURI", "Missing bucket");
        }

        let buckets = match storage.as_ref().list_namespaces() {
            Ok(buckets) => buckets,
            Err(error) => {
                return Self::error_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "InternalError",
                    &error.to_string(),
                );
            }
        };
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
        Self::xml_response(StatusCode::OK, body)
    }

    fn handle_xml_bucket_request(
        storage: &Arc<dyn Storage>,
        req: &Request,
        bucket: &str,
    ) -> Response<Body> {
        if let Some(parameter) = Self::unsupported_xml_subresource(req, false) {
            return Self::unsupported_xml_subresource_response(parameter);
        }
        if matches!(*req.method(), Method::GET | Method::DELETE)
            && Self::foreign_data_protection_active(storage, bucket)
        {
            return Self::foreign_data_protection_xml_response();
        }

        match *req.method() {
            Method::PUT => {
                if !Self::valid_bucket_name(bucket) {
                    return Self::error_response(
                        StatusCode::BAD_REQUEST,
                        "InvalidBucketName",
                        "The specified bucket name is not valid.",
                    );
                }
                if let Err(error) = storage.as_ref().create_namespace(bucket.to_string()) {
                    if matches!(error, crate::error::Error::BucketAlreadyExists) {
                        return Self::error_response(
                            StatusCode::CONFLICT,
                            "BucketAlreadyOwnedByYou",
                            "Your previous request to create the named bucket succeeded",
                        );
                    }
                    return Self::error_response(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "InternalError",
                        &error.to_string(),
                    );
                }
                Self::empty_response(StatusCode::OK)
            }
            Method::DELETE => {
                if let Err(error) = storage.as_ref().delete_namespace(bucket) {
                    if matches!(error, crate::error::Error::BucketNotEmpty) {
                        return Self::error_response(
                            StatusCode::CONFLICT,
                            "BucketNotEmpty",
                            "The bucket you tried to delete is not empty",
                        );
                    }
                    if matches!(error, crate::error::Error::BucketNotFound) {
                        return Self::xml_bucket_not_found(bucket);
                    }
                    return Self::error_response(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "InternalError",
                        &error.to_string(),
                    );
                }
                Self::empty_response(StatusCode::NO_CONTENT)
            }
            Method::GET => Self::list_xml_bucket(storage, req, bucket),
            _ => Self::unsupported_xml_operation(),
        }
    }

    fn list_xml_bucket(storage: &Arc<dyn Storage>, req: &Request, bucket: &str) -> Response<Body> {
        if req.has_query_param("versions") {
            return Self::error_response(
                StatusCode::NOT_IMPLEMENTED,
                "NotImplemented",
                "GCS XML object version listings are not supported by this emulator surface.",
            );
        }
        let max_results = match Self::xml_max_keys(req) {
            Ok(value) => value,
            Err(response) => return *response,
        };
        match req.query_param("list-type") {
            Some("2") => {
                return Self::list_xml_bucket_v2(storage, req, bucket, max_results);
            }
            None | Some("1") => {}
            Some(_) => {
                return Self::error_response(
                    StatusCode::BAD_REQUEST,
                    "InvalidArgument",
                    "list-type must be 1 or 2.",
                );
            }
        }
        let mut blobs = match storage.as_ref().list_blobs(
            bucket,
            req.query_param("prefix"),
            req.query_param("delimiter"),
            req.query_param("marker"),
            Some(max_results.saturating_add(1)),
        ) {
            Ok(blobs) => blobs,
            Err(crate::error::Error::BucketNotFound) => {
                return Self::xml_bucket_not_found(bucket);
            }
            Err(error) => {
                return Self::error_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "InternalError",
                    &error.to_string(),
                );
            }
        };
        let next_marker = (blobs.len() > max_results).then(|| {
            blobs.truncate(max_results);
            blobs
                .last()
                .map(|blob| blob.key.clone())
                .unwrap_or_default()
        });
        let mut body = String::with_capacity(128 + blobs.len() * 128);
        body.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?><ListBucketResult><Name>");
        push_escaped_xml(&mut body, bucket);
        body.push_str("</Name>");
        for blob in blobs {
            Self::append_xml_bucket_item(&mut body, &blob);
        }
        body.push_str("<IsTruncated>");
        body.push_str(if next_marker.is_some() {
            "true"
        } else {
            "false"
        });
        body.push_str("</IsTruncated>");
        if let Some(next_marker) = next_marker {
            body.push_str("<NextMarker>");
            push_escaped_xml(&mut body, &next_marker);
            body.push_str("</NextMarker>");
        }
        body.push_str("</ListBucketResult>");
        Self::xml_response(StatusCode::OK, body)
    }

    fn xml_max_keys(req: &Request) -> Result<usize, Box<Response<Body>>> {
        let Some(raw) = req.query_param("max-keys") else {
            return Ok(1_000);
        };
        raw.parse::<u32>()
            .ok()
            .filter(|value| *value > 0)
            .map(|value| value as usize)
            .ok_or_else(|| {
                Box::new(Self::error_response(
                    StatusCode::BAD_REQUEST,
                    "InvalidArgument",
                    "max-keys must be a positive integer.",
                ))
            })
    }

    fn list_xml_bucket_v2(
        storage: &Arc<dyn Storage>,
        req: &Request,
        bucket: &str,
        max_results: usize,
    ) -> Response<Body> {
        let continuation_token = req.query_param("continuation-token");
        let continuation_marker = match continuation_token {
            Some(token) => match Self::decode_page_token("xml-objects", token) {
                Some(marker) => Some(marker),
                None => {
                    return Self::error_response(
                        StatusCode::BAD_REQUEST,
                        "InvalidArgument",
                        "The continuation token provided is incorrect.",
                    );
                }
            },
            None => None,
        };
        let start_after = req.query_param("start-after");
        let marker = continuation_marker.as_deref().or(start_after);
        let mut blobs = match storage.as_ref().list_blobs(
            bucket,
            req.query_param("prefix"),
            req.query_param("delimiter"),
            marker,
            Some(max_results.saturating_add(1)),
        ) {
            Ok(blobs) => blobs,
            Err(crate::error::Error::BucketNotFound) => {
                return Self::xml_bucket_not_found(bucket);
            }
            Err(error) => {
                return Self::error_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "InternalError",
                    &error.to_string(),
                );
            }
        };
        let truncated = blobs.len() > max_results;
        blobs.truncate(max_results);
        let next_continuation_token = truncated.then(|| {
            Self::encode_page_token(
                "xml-objects",
                blobs.last().map_or("", |blob| blob.key.as_str()),
            )
        });

        let mut body = String::with_capacity(256 + blobs.len() * 128);
        body.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?><ListBucketResult><Name>");
        push_escaped_xml(&mut body, bucket);
        body.push_str("</Name><Prefix>");
        push_escaped_xml(&mut body, req.query_param("prefix").unwrap_or(""));
        body.push_str("</Prefix><KeyCount>");
        write!(body, "{}", blobs.len()).unwrap();
        body.push_str("</KeyCount><MaxKeys>");
        write!(body, "{max_results}").unwrap();
        body.push_str("</MaxKeys><IsTruncated>");
        body.push_str(if truncated { "true" } else { "false" });
        body.push_str("</IsTruncated>");
        if let Some(continuation_token) = continuation_token {
            body.push_str("<ContinuationToken>");
            push_escaped_xml(&mut body, continuation_token);
            body.push_str("</ContinuationToken>");
        }
        if let Some(next_continuation_token) = next_continuation_token {
            body.push_str("<NextContinuationToken>");
            push_escaped_xml(&mut body, &next_continuation_token);
            body.push_str("</NextContinuationToken>");
        }
        if let Some(start_after) = start_after {
            body.push_str("<StartAfter>");
            push_escaped_xml(&mut body, start_after);
            body.push_str("</StartAfter>");
        }
        for blob in blobs {
            Self::append_xml_bucket_item(&mut body, &blob);
        }
        body.push_str("</ListBucketResult>");
        Self::xml_response(StatusCode::OK, body)
    }

    fn append_xml_bucket_item(body: &mut String, blob: &crate::blob::BlobRecord) {
        let generation = blob.metadata.get(GCS_GENERATION_KEY).map_or_else(
            || blob.last_modified.timestamp_millis().max(1).to_string(),
            Clone::clone,
        );
        body.push_str("<Contents><Key>");
        push_escaped_xml(body, &blob.key);
        body.push_str("</Key><Size>");
        write!(body, "{}", blob.size).unwrap();
        body.push_str("</Size><ETag>");
        push_escaped_xml(body, &format!("\"{}\"", blob.etag.trim_matches('"')));
        body.push_str("</ETag><Generation>");
        push_escaped_xml(body, &generation);
        body.push_str("</Generation></Contents>");
    }

    fn xml_bucket_not_found(bucket: &str) -> Response<Body> {
        Self::error_response(
            StatusCode::NOT_FOUND,
            "NoSuchBucket",
            &format!("The specified bucket {bucket} does not exist."),
        )
    }

    fn handle_xml_object_request(
        &self,
        storage: &Arc<dyn Storage>,
        req: &Request,
        bucket: &str,
        object: &str,
    ) -> Result<Response<Body>, String> {
        match storage.get_bucket(bucket) {
            Ok(_) => {}
            Err(crate::error::Error::BucketNotFound) if *req.method() == Method::HEAD => {
                return Ok(Self::empty_response(StatusCode::NOT_FOUND));
            }
            Err(crate::error::Error::BucketNotFound) => {
                return Ok(Self::xml_bucket_not_found(bucket));
            }
            Err(error) => {
                return Ok(Self::error_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "InternalError",
                    &error.to_string(),
                ));
            }
        }
        if let Some(parameter) = Self::unsupported_xml_subresource(req, true) {
            return Ok(Self::unsupported_xml_subresource_response(parameter));
        }
        if matches!(*req.method(), Method::GET | Method::PATCH | Method::DELETE)
            && req.has_query_param("generation")
        {
            return Ok(Self::error_response(
                StatusCode::NOT_IMPLEMENTED,
                "NotImplemented",
                "Generation-scoped GCS XML object operations are not supported by this emulator surface.",
            ));
        }
        match *req.method() {
            Method::PUT => {
                let mutation_lock = self.object_mutation_lock(bucket, object)?;
                let _guard = mutation_lock
                    .lock()
                    .map_err(|_| "Failed to lock GCS object mutation".to_string())?;
                Ok(Self::put_xml_object(storage, req, bucket, object))
            }
            Method::GET => Self::object_media_response(storage, req, bucket, object),
            Method::HEAD => Self::object_head_response(storage, bucket, object),
            Method::DELETE => {
                let mutation_lock = self.object_mutation_lock(bucket, object)?;
                let _guard = mutation_lock
                    .lock()
                    .map_err(|_| "Failed to lock GCS object mutation".to_string())?;
                if Self::existing_object_is_retained(storage, bucket, object) {
                    return Ok(Self::error_response(
                        StatusCode::FORBIDDEN,
                        "RetentionPolicyNotMet",
                        "Object is subject to an active retention policy",
                    ));
                }
                let condition = match Self::xml_mutation_condition(req, false) {
                    Ok(condition) => condition,
                    Err(response) => return Ok(response),
                };
                let deleted = match condition {
                    Some(condition) => storage.delete_object_if(bucket, object, &condition),
                    None => storage.as_ref().delete_blob(bucket, object).map(|()| true),
                };
                match deleted {
                    Ok(true) => Ok(Self::empty_response(StatusCode::NO_CONTENT)),
                    Ok(false) => Ok(Self::xml_precondition_failed_response()),
                    Err(crate::error::Error::KeyNotFound) => Ok(Self::error_response(
                        StatusCode::NOT_FOUND,
                        "NoSuchKey",
                        "The specified key does not exist.",
                    )),
                    Err(crate::error::Error::BucketNotFound) => {
                        Ok(Self::xml_bucket_not_found(bucket))
                    }
                    Err(error) => Ok(Self::error_response(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "InternalError",
                        &error.to_string(),
                    )),
                }
            }
            _ => Ok(Self::unsupported_xml_operation()),
        }
    }

    fn put_xml_object(
        storage: &Arc<dyn Storage>,
        req: &Request,
        bucket: &str,
        object: &str,
    ) -> Response<Body> {
        if Self::existing_object_is_retained(storage, bucket, object) {
            return Self::error_response(
                StatusCode::FORBIDDEN,
                "RetentionPolicyNotMet",
                "Object is subject to an active retention policy",
            );
        }
        let condition = match Self::xml_mutation_condition(req, true) {
            Ok(condition) => condition,
            Err(response) => return response,
        };
        let existing = storage.get_object(bucket, object).ok();
        let metadata = Self::metadata_with_gcs_state(
            Self::metadata_from_headers(req),
            Self::next_generation(existing.as_ref()),
            "1".to_string(),
            None,
        );
        let mut object_record = crate::models::Object::new_with_metadata(
            object.to_string(),
            req.body.to_vec(),
            req.header("content-type")
                .unwrap_or("application/octet-stream")
                .to_string(),
            metadata,
        );
        object_record
            .provider_metadata
            .insert(GCS_CRC32C_KEY.to_string(), Self::encoded_crc32c(&req.body));
        let written = if let Some(condition) = condition {
            match storage.put_object_if(bucket, object.to_string(), object_record, &condition) {
                Ok(written) => written,
                Err(crate::error::Error::BucketNotFound) => {
                    return Self::xml_bucket_not_found(bucket);
                }
                Err(error) => {
                    return Self::error_response(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "InternalError",
                        &error.to_string(),
                    );
                }
            }
        } else {
            if let Err(error) = storage.put_object(bucket, object.to_string(), object_record) {
                if matches!(error, crate::error::Error::BucketNotFound) {
                    return Self::xml_bucket_not_found(bucket);
                }
                return Self::error_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "InternalError",
                    &error.to_string(),
                );
            }
            true
        };
        if !written {
            return Self::xml_precondition_failed_response();
        }
        let stored_object = match storage.get_object(bucket, object) {
            Ok(object) => object,
            Err(crate::error::Error::BucketNotFound) => {
                return Self::xml_bucket_not_found(bucket);
            }
            Err(error) => {
                return Self::error_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "InternalError",
                    &error.to_string(),
                );
            }
        };
        let stored = crate::blob::BlobRecord::from_object(bucket, &stored_object);
        Self::response(StatusCode::OK)
            .header("etag", &format!("\"{}\"", stored.etag))
            .header(
                "x-goog-generation",
                &Self::generation_from_metadata(&stored.metadata),
            )
            .header(
                "x-goog-metageneration",
                &Self::metageneration_from_metadata(&stored.metadata),
            )
            .empty()
    }

    fn object_media_response(
        storage: &Arc<dyn Storage>,
        req: &Request,
        bucket: &str,
        object: &str,
    ) -> Result<Response<Body>, String> {
        let blob = match storage.as_ref().get_blob(bucket, object) {
            Ok(blob) => blob,
            Err(crate::error::Error::KeyNotFound) => {
                return Ok(Self::error_response(
                    StatusCode::NOT_FOUND,
                    "NoSuchKey",
                    "The specified key does not exist.",
                ));
            }
            Err(crate::error::Error::BucketNotFound) => {
                return Ok(Self::xml_bucket_not_found(bucket));
            }
            Err(err) => {
                return Ok(Self::error_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "InternalError",
                    &err.to_string(),
                ));
            }
        };
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
        let blob = match storage.as_ref().get_blob(bucket, object) {
            Ok(blob) => blob,
            Err(crate::error::Error::KeyNotFound | crate::error::Error::BucketNotFound) => {
                return Ok(Self::empty_response(StatusCode::NOT_FOUND));
            }
            Err(err) => {
                return Ok(Self::error_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "InternalError",
                    &err.to_string(),
                ));
            }
        };
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

    fn unsupported_xml_subresource(req: &Request, object: bool) -> Option<&str> {
        const BUCKET_SUBRESOURCES: [&str; 15] = [
            "acl",
            "cors",
            "customPlacementConfig",
            "defaultObjectAcl",
            "encryptionConfig",
            "lifecycle",
            "location",
            "logging",
            "object-lock",
            "retention",
            "storageClass",
            "tagging",
            "versioning",
            "website",
            "websiteConfig",
        ];
        const OBJECT_SUBRESOURCES: [&str; 12] = [
            "acl",
            "compose",
            "encryption",
            "legal-hold",
            "object-lock",
            "partNumber",
            "response-content-disposition",
            "response-content-type",
            "retention",
            "tagging",
            "uploadId",
            "uploads",
        ];
        let candidates = if object {
            OBJECT_SUBRESOURCES.as_slice()
        } else {
            BUCKET_SUBRESOURCES.as_slice()
        };
        candidates
            .iter()
            .copied()
            .find(|parameter| req.has_query_param(parameter))
    }

    fn unsupported_xml_subresource_response(parameter: &str) -> Response<Body> {
        Self::error_response(
            StatusCode::NOT_IMPLEMENTED,
            "NotImplemented",
            &format!(
                "The GCS XML {parameter} subresource is not supported by this emulator surface."
            ),
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
        if req.method() != Method::POST {
            return Ok(Self::json_upload_error(
                StatusCode::METHOD_NOT_ALLOWED,
                "METHOD_NOT_ALLOWED",
                "GCS JSON uploads require POST.",
            ));
        }
        if let Err(response) = Self::authorize(req, auth_config, bucket, None) {
            return Ok(response);
        }
        match storage.get_bucket(bucket) {
            Ok(_) => {}
            Err(crate::error::Error::BucketNotFound) => {
                return Ok(Self::json_upload_error(
                    StatusCode::NOT_FOUND,
                    "NOT_FOUND",
                    "The specified bucket does not exist.",
                ));
            }
            Err(error) => return Err(error.to_string()),
        }
        if Self::foreign_data_protection_active(storage, bucket) {
            return Ok(Self::foreign_data_protection_json_response());
        }
        match req.query_param("uploadType") {
            Some("media") => self.handle_media_upload(storage, req, bucket),
            Some("multipart") => self.handle_multipart_upload(storage, req, bucket),
            Some("resumable") => self.create_resumable_session(storage, req, bucket),
            Some(value) => Ok(Self::json_upload_error(
                StatusCode::BAD_REQUEST,
                "INVALID_ARGUMENT",
                &format!("Unsupported uploadType: {value}"),
            )),
            None => Ok(Self::json_upload_error(
                StatusCode::BAD_REQUEST,
                "INVALID_ARGUMENT",
                "Required parameter: uploadType",
            )),
        }
    }

    fn json_upload_error(status: StatusCode, _status_name: &str, message: &str) -> Response<Body> {
        Self::json_error(
            status,
            Self::default_json_error_reason(status, message),
            message,
        )
    }

    fn upload_metadata_error_response(error: UploadMetadataError) -> Response<Body> {
        match error {
            UploadMetadataError::Invalid(message) => {
                Self::json_upload_error(StatusCode::BAD_REQUEST, "INVALID_ARGUMENT", &message)
            }
            UploadMetadataError::ChecksumMismatch(message) => {
                Self::json_error(StatusCode::BAD_REQUEST, "invalid", &message)
            }
            UploadMetadataError::Unsupported(message) => {
                Self::json_upload_error(StatusCode::NOT_IMPLEMENTED, "UNIMPLEMENTED", &message)
            }
        }
    }

    fn handle_media_upload(
        &self,
        storage: &Arc<dyn Storage>,
        req: &Request,
        bucket: &str,
    ) -> Result<Response<Body>, String> {
        if let Some(response) = Self::invalid_json_mutation_headers(req) {
            return Ok(response);
        }
        if let Some(response) = Self::invalid_upload_preconditions(req) {
            return Ok(response);
        }
        let expected_crc32c = match Self::request_crc32c(req) {
            Ok(expected) => expected,
            Err(error) => return Ok(Self::upload_metadata_error_response(error)),
        };
        if let Err(error) = Self::validate_crc32c(expected_crc32c, &req.body) {
            return Ok(Self::upload_metadata_error_response(error));
        }
        let Some(key) = req.query_param("name") else {
            return Ok(Self::json_upload_error(
                StatusCode::BAD_REQUEST,
                "INVALID_ARGUMENT",
                "Required parameter: name",
            ));
        };
        if Self::existing_object_is_retained(storage, bucket, key) {
            return Ok(Self::retention_policy_not_met_response());
        }
        let content_type = req
            .header("content-type")
            .unwrap_or("application/octet-stream")
            .to_string();
        let preconditions = GenerationPreconditions {
            expected: req.query_param("ifGenerationMatch"),
            rejected: req.query_param("ifGenerationNotMatch"),
        };
        let stored = self.put_blob_with_generation_match(
            storage,
            bucket,
            key,
            BlobWrite {
                data: req.body.to_vec(),
                content_type,
                metadata: Self::metadata_from_headers(req),
                preconditions,
            },
        )?;
        let stored = match stored {
            BlobWriteOutcome::Stored(stored) => stored,
            BlobWriteOutcome::PreconditionFailed => {
                return Ok(Self::upload_precondition_failed(preconditions));
            }
            BlobWriteOutcome::RetentionPolicyNotMet => {
                return Ok(Self::retention_policy_not_met_response());
            }
        };
        Ok(Self::gcs_object_json_response(StatusCode::OK, &stored))
    }

    fn handle_multipart_upload(
        &self,
        storage: &Arc<dyn Storage>,
        req: &Request,
        bucket: &str,
    ) -> Result<Response<Body>, String> {
        if let Some(response) = Self::invalid_json_mutation_headers(req) {
            return Ok(response);
        }
        if let Some(response) = Self::invalid_upload_preconditions(req) {
            return Ok(response);
        }
        if req.header("x-goog-hash").is_some() {
            return Ok(Self::upload_metadata_error_response(
                UploadMetadataError::Unsupported(
                    "Multipart uploads accept CRC32C in the JSON object metadata, not X-Goog-Hash"
                        .to_string(),
                ),
            ));
        }
        let content_type = req.header("content-type").unwrap_or("multipart/related");
        let (upload_metadata, object_content_type, data) =
            match Self::parse_multipart_upload(content_type, &req.body) {
                Ok(upload) => upload,
                Err(error) => return Ok(Self::upload_metadata_error_response(error)),
            };
        if let Err(error) = Self::validate_crc32c(upload_metadata.crc32c, &data) {
            return Ok(Self::upload_metadata_error_response(error));
        }
        let Some(key) = req
            .query_param("name")
            .map(str::to_string)
            .or(upload_metadata.name)
        else {
            return Ok(Self::json_upload_error(
                StatusCode::BAD_REQUEST,
                "INVALID_ARGUMENT",
                "Required parameter: name",
            ));
        };
        if Self::existing_object_is_retained(storage, bucket, &key) {
            return Ok(Self::retention_policy_not_met_response());
        }
        let preconditions = GenerationPreconditions {
            expected: req.query_param("ifGenerationMatch"),
            rejected: req.query_param("ifGenerationNotMatch"),
        };
        let stored = self.put_blob_with_generation_match(
            storage,
            bucket,
            &key,
            BlobWrite {
                data,
                content_type: object_content_type,
                metadata: upload_metadata.metadata,
                preconditions,
            },
        )?;
        let stored = match stored {
            BlobWriteOutcome::Stored(stored) => stored,
            BlobWriteOutcome::PreconditionFailed => {
                return Ok(Self::upload_precondition_failed(preconditions));
            }
            BlobWriteOutcome::RetentionPolicyNotMet => {
                return Ok(Self::retention_policy_not_met_response());
            }
        };
        Ok(Self::gcs_object_json_response(StatusCode::OK, &stored))
    }

    fn create_resumable_session(
        &self,
        storage: &Arc<dyn Storage>,
        req: &Request,
        bucket: &str,
    ) -> Result<Response<Body>, String> {
        if let Some(response) = Self::invalid_json_mutation_headers(req) {
            return Ok(response);
        }
        if let Some(response) = Self::invalid_upload_preconditions(req) {
            return Ok(response);
        }
        if req.header("x-goog-hash").is_some() {
            return Ok(Self::upload_metadata_error_response(
                UploadMetadataError::Unsupported(
                    "X-Goog-Hash is supported on the final resumable upload request, not session initiation"
                        .to_string(),
                ),
            ));
        }
        let upload_metadata = match Self::parse_json_upload_metadata(&req.body) {
            Ok(metadata) => metadata,
            Err(error) => return Ok(Self::upload_metadata_error_response(error)),
        };
        let Some(key) = req
            .query_param("name")
            .map(str::to_string)
            .or(upload_metadata.name)
        else {
            return Ok(Self::json_upload_error(
                StatusCode::BAD_REQUEST,
                "INVALID_ARGUMENT",
                "Required parameter: name",
            ));
        };
        let session_id = uuid::Uuid::new_v4().to_string();
        let session = ResumableSession {
            bucket: bucket.to_string(),
            key,
            content_type: upload_metadata.content_type.unwrap_or_else(|| {
                req.header("x-upload-content-type")
                    .unwrap_or("application/octet-stream")
                    .to_string()
            }),
            metadata: upload_metadata.metadata,
            crc32c: upload_metadata.crc32c,
            if_generation_match: req.query_param("ifGenerationMatch").map(str::to_string),
            if_generation_not_match: req.query_param("ifGenerationNotMatch").map(str::to_string),
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
        if req.method() != Method::PUT {
            return Ok(Self::json_upload_error(
                StatusCode::METHOD_NOT_ALLOWED,
                "METHOD_NOT_ALLOWED",
                "Resumable upload sessions require PUT.",
            ));
        }
        if let Some(response) = Self::invalid_json_mutation_headers(req) {
            return Ok(response);
        }
        if let Some(content_range) = req.header("content-range") {
            if !Self::is_complete_resumable_range(content_range, req.body.len()) {
                return Ok(Self::json_error(
                    StatusCode::NOT_IMPLEMENTED,
                    "notImplemented",
                    "Chunked resumable uploads are not supported by this emulator surface",
                ));
            }
        }
        let header_crc32c = match Self::request_crc32c(req) {
            Ok(expected) => expected,
            Err(error) => return Ok(Self::upload_metadata_error_response(error)),
        };
        let Some(session) = self.take_resumable_session(storage, session_id)? else {
            return Ok(Self::json_upload_error(
                StatusCode::NOT_FOUND,
                "NOT_FOUND",
                "The resumable upload session does not exist.",
            ));
        };
        let expected_crc32c = match Self::combined_crc32c(session.crc32c, header_crc32c) {
            Ok(expected) => expected,
            Err(error) => return Ok(Self::upload_metadata_error_response(error)),
        };
        if let Err(error) = Self::validate_crc32c(expected_crc32c, &req.body) {
            return Ok(Self::upload_metadata_error_response(error));
        }
        if Self::foreign_data_protection_active(storage, &session.bucket) {
            return Ok(Self::foreign_data_protection_json_response());
        }
        if Self::existing_object_is_retained(storage, &session.bucket, &session.key) {
            return Ok(Self::retention_policy_not_met_response());
        }
        let preconditions = GenerationPreconditions {
            expected: session.if_generation_match.as_deref(),
            rejected: session.if_generation_not_match.as_deref(),
        };
        let mut metadata = session.metadata;
        metadata.extend(Self::metadata_from_headers(req));
        let stored = self.put_blob_with_generation_match(
            storage,
            &session.bucket,
            &session.key,
            BlobWrite {
                data: req.body.to_vec(),
                content_type: session.content_type,
                metadata,
                preconditions,
            },
        )?;
        let stored = match stored {
            BlobWriteOutcome::Stored(stored) => stored,
            BlobWriteOutcome::PreconditionFailed => {
                return Ok(Self::upload_precondition_failed(preconditions));
            }
            BlobWriteOutcome::RetentionPolicyNotMet => {
                return Ok(Self::retention_policy_not_met_response());
            }
        };
        storage
            .delete_provider_state(GCS_RESUMABLE_SESSION_STATE, session_id)
            .map_err(|err| err.to_string())?;
        Ok(Self::gcs_object_json_response(StatusCode::OK, &stored))
    }

    fn is_complete_resumable_range(value: &str, body_len: usize) -> bool {
        let Some((range, total)) = value.strip_prefix("bytes ").and_then(|v| v.split_once('/'))
        else {
            return false;
        };
        let Some((start, end)) = range.split_once('-') else {
            return false;
        };
        let Ok(start) = start.parse::<usize>() else {
            return false;
        };
        let Ok(end) = end.parse::<usize>() else {
            return false;
        };
        let Ok(total) = total.parse::<usize>() else {
            return false;
        };
        start == 0 && end.checked_add(1) == Some(total) && total == body_len
    }

    fn take_resumable_session(
        &self,
        storage: &Arc<dyn Storage>,
        session_id: &str,
    ) -> Result<Option<ResumableSession>, String> {
        Ok({
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
        )?))
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
                "bucket": stored.namespace,
                "size": stored.size.to_string(),
                "crc32c": stored.provider_metadata.get(GCS_CRC32C_KEY),
                "etag": Self::json_etag(&stored.metadata, &stored.etag),
                "generation": Self::generation_from_metadata(&stored.metadata),
                "metageneration": Self::metageneration_from_metadata(&stored.metadata),
                "updated": Self::gcs_updated(&stored.metadata, stored.last_modified),
                "contentType": stored.content_type,
                "metadata": Self::public_metadata(&stored.metadata),
            })
            .to_string(),
        )
    }

    fn handle_json_api(
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

        if parts.starts_with(&["storage", "v1", "b"]) {
            return self.handle_json_bucket_api(storage, auth_config, req, &parts);
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
        &self,
        storage: &Arc<dyn Storage>,
        auth_config: &Arc<AuthConfig>,
        req: &Request,
        parts: &[&str],
    ) -> Result<Response<Body>, String> {
        if parts.len() == 3 {
            if let Err(response) = Self::authorize(req, auth_config, "", None) {
                return Ok(response);
            }
            return Self::handle_json_bucket_collection(storage, req);
        }

        let bucket = parts.get(3).copied().unwrap_or_default();
        if let Err(response) = Self::authorize(req, auth_config, bucket, None) {
            return Ok(response);
        }

        if parts.len() == 4 {
            return Self::handle_json_bucket_item(storage, req, bucket);
        }
        if parts.get(4) == Some(&"lockRetentionPolicy") {
            return Ok(Self::unimplemented_data_protection_response(
                "Bucket retention-policy locking is not supported by this emulator surface",
            ));
        }
        if parts.get(4) == Some(&"o") {
            return self.handle_json_object_api(storage, auth_config, req, parts, bucket);
        }
        Ok(Self::json_upload_error(
            StatusCode::BAD_REQUEST,
            "INVALID_ARGUMENT",
            "Unsupported GCS JSON API path",
        ))
    }

    fn handle_json_bucket_collection(
        storage: &Arc<dyn Storage>,
        req: &Request,
    ) -> Result<Response<Body>, String> {
        if req.query_param("project").is_none() {
            return Ok(Self::json_error(
                StatusCode::BAD_REQUEST,
                "required",
                "Required parameter: project",
            ));
        }
        match *req.method() {
            Method::GET => Self::list_json_buckets(storage, req),
            Method::POST => Self::create_json_bucket(storage, req),
            _ => Ok(Self::json_upload_error(
                StatusCode::METHOD_NOT_ALLOWED,
                "METHOD_NOT_ALLOWED",
                "Unsupported GCS JSON API bucket collection operation",
            )),
        }
    }

    fn list_json_buckets(
        storage: &Arc<dyn Storage>,
        req: &Request,
    ) -> Result<Response<Body>, String> {
        let marker = match req.query_param("pageToken") {
            Some(token) => match Self::decode_page_token("buckets", token) {
                Some(marker) => Some(marker),
                None => return Ok(Self::invalid_page_token_response()),
            },
            None => None,
        };
        let max_results = match Self::json_max_results(req) {
            Ok(value) => value,
            Err(response) => return Ok(response),
        };
        let mut buckets = storage
            .as_ref()
            .list_namespaces()
            .map_err(|err| err.to_string())?;
        let prefix = req.query_param("prefix").unwrap_or("");
        buckets.retain(|bucket| {
            bucket.name.starts_with(prefix)
                && marker.as_ref().is_none_or(|marker| bucket.name > *marker)
        });
        buckets.sort_unstable_by(|left, right| left.name.cmp(&right.name));
        let truncated = buckets.len() > max_results;
        buckets.truncate(max_results);
        let next_page_token = truncated.then(|| {
            Self::encode_page_token(
                "buckets",
                buckets.last().map_or("", |bucket| bucket.name.as_str()),
            )
        });
        let mut body = serde_json::json!({
            "kind": "storage#buckets",
            "items": buckets.into_iter().map(|bucket| serde_json::json!({
                "name": bucket.name,
                "timeCreated": bucket.created_at.to_rfc3339(),
            })).collect::<Vec<_>>(),
        });
        if let Some(next_page_token) = next_page_token {
            body["nextPageToken"] = serde_json::Value::String(next_page_token);
        }
        Ok(Self::json_response(StatusCode::OK, &body.to_string()))
    }

    fn invalid_page_token_response() -> Response<Body> {
        Self::json_error(
            StatusCode::BAD_REQUEST,
            "invalidParameter",
            "Invalid pageToken",
        )
    }

    #[allow(clippy::result_large_err)]
    fn json_max_results(req: &Request) -> Result<usize, Response<Body>> {
        let Some(raw) = req.query_param("maxResults") else {
            return Ok(1_000);
        };
        raw.parse::<u32>().map_or_else(
            |_| {
                Err(Self::json_error(
                    StatusCode::BAD_REQUEST,
                    "invalidParameter",
                    "Invalid value for maxResults",
                ))
            },
            |value| Ok((value as usize).min(1_000)),
        )
    }

    fn valid_bucket_name(name: &str) -> bool {
        let max_len = if name.contains('.') { 222 } else { 63 };
        if name.len() < 3 || name.len() > max_len {
            return false;
        }
        let bytes = name.as_bytes();
        bytes.first().is_some_and(u8::is_ascii_alphanumeric)
            && bytes.last().is_some_and(u8::is_ascii_alphanumeric)
            && bytes.iter().all(|byte| {
                byte.is_ascii_lowercase()
                    || byte.is_ascii_digit()
                    || matches!(*byte, b'-' | b'_' | b'.')
            })
            && name
                .split('.')
                .all(|component| !component.is_empty() && component.len() <= 63)
            && name.parse::<std::net::Ipv4Addr>().is_err()
            && !name.starts_with("goog")
            && !name.contains("google")
            && !name.contains("g00gle")
    }

    fn create_json_bucket(
        storage: &Arc<dyn Storage>,
        req: &Request,
    ) -> Result<Response<Body>, String> {
        let payload: serde_json::Value = match serde_json::from_slice(&req.body) {
            Ok(payload) => payload,
            Err(error) => {
                return Ok(Self::json_upload_error(
                    StatusCode::BAD_REQUEST,
                    "INVALID_ARGUMENT",
                    &format!("Invalid bucket metadata JSON: {error}"),
                ));
            }
        };
        if let Some(response) = Self::validate_bucket_data_protection(req, &payload) {
            return Ok(response);
        }
        let Some(bucket) = payload.get("name").and_then(|value| value.as_str()) else {
            return Ok(Self::json_upload_error(
                StatusCode::BAD_REQUEST,
                "INVALID_ARGUMENT",
                "Required parameter: name",
            ));
        };
        if !Self::valid_bucket_name(bucket) {
            return Ok(Self::json_error(
                StatusCode::BAD_REQUEST,
                "invalidParameter",
                "Invalid bucket name",
            ));
        }
        let activation_lock = data_protection_activation_lock(bucket)?;
        let _activation_guard = activation_lock
            .lock()
            .map_err(|_| "Failed to lock GCS data-protection activation".to_string())?;
        if let Err(error) = storage.as_ref().create_namespace(bucket.to_string()) {
            if matches!(error, crate::error::Error::BucketAlreadyExists) {
                return Ok(Self::json_error(
                    StatusCode::CONFLICT,
                    "conflict",
                    "The requested bucket already exists",
                ));
            }
            return Ok(Self::json_upload_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "INTERNAL",
                &error.to_string(),
            ));
        }
        if let Err(error) = Self::apply_bucket_data_protection(storage, bucket, &payload) {
            let rollback = storage.delete_namespace(bucket);
            let message = match rollback {
                Ok(()) => error,
                Err(rollback_error) => {
                    format!("{error}; failed to roll back GCS bucket creation: {rollback_error}")
                }
            };
            return Ok(Self::json_upload_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "INTERNAL",
                &message,
            ));
        }
        let namespace = match storage.as_ref().get_namespace(bucket) {
            Ok(namespace) => namespace,
            Err(error) => {
                let rollback = storage.delete_namespace(bucket);
                let message = match rollback {
                    Ok(()) => error.to_string(),
                    Err(rollback_error) => format!(
                        "{error}; failed to roll back GCS bucket creation: {rollback_error}"
                    ),
                };
                return Ok(Self::json_upload_error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "INTERNAL",
                    &message,
                ));
            }
        };
        Ok(Self::json_bucket_response(StatusCode::OK, &namespace))
    }

    fn handle_json_bucket_item(
        storage: &Arc<dyn Storage>,
        req: &Request,
        bucket: &str,
    ) -> Result<Response<Body>, String> {
        match *req.method() {
            Method::GET => match storage.as_ref().get_namespace(bucket) {
                Ok(namespace) => Ok(Self::json_bucket_response(StatusCode::OK, &namespace)),
                Err(crate::error::Error::BucketNotFound) => Ok(Self::json_bucket_not_found(bucket)),
                Err(error) => Ok(Self::json_upload_error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "INTERNAL",
                    &error.to_string(),
                )),
            },
            Method::DELETE => {
                if Self::foreign_data_protection_active(storage, bucket) {
                    return Ok(Self::foreign_data_protection_json_response());
                }
                if let Err(error) = storage.as_ref().delete_namespace(bucket) {
                    if matches!(error, crate::error::Error::BucketNotEmpty) {
                        return Ok(Self::json_error(
                            StatusCode::CONFLICT,
                            "conflict",
                            "The bucket is not empty",
                        ));
                    }
                    if matches!(error, crate::error::Error::BucketNotFound) {
                        return Ok(Self::json_bucket_not_found(bucket));
                    }
                    return Ok(Self::json_upload_error(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "INTERNAL",
                        &error.to_string(),
                    ));
                }
                Ok(Self::empty_response(StatusCode::NO_CONTENT))
            }
            Method::PATCH => {
                if matches!(
                    storage.as_ref().get_namespace(bucket),
                    Err(crate::error::Error::BucketNotFound)
                ) {
                    return Ok(Self::json_bucket_not_found(bucket));
                }
                let payload: serde_json::Value = match serde_json::from_slice(&req.body) {
                    Ok(payload) => payload,
                    Err(error) => {
                        return Ok(Self::json_upload_error(
                            StatusCode::BAD_REQUEST,
                            "INVALID_ARGUMENT",
                            &format!("Invalid bucket metadata JSON: {error}"),
                        ));
                    }
                };
                if let Some(response) = Self::validate_bucket_data_protection(req, &payload) {
                    return Ok(response);
                }
                let activation_lock = data_protection_activation_lock(bucket)?;
                let _activation_guard = activation_lock
                    .lock()
                    .map_err(|_| "Failed to lock GCS data-protection activation".to_string())?;
                if Self::bucket_data_protection_requested(&payload)
                    && Self::foreign_data_protection_active(storage, bucket)
                {
                    return Ok(Self::foreign_data_protection_json_response());
                }
                if let Err(error) = Self::apply_bucket_data_protection(storage, bucket, &payload) {
                    return Ok(Self::json_upload_error(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "INTERNAL",
                        &error,
                    ));
                }
                let namespace = match storage.as_ref().get_namespace(bucket) {
                    Ok(namespace) => namespace,
                    Err(crate::error::Error::BucketNotFound) => {
                        return Ok(Self::json_bucket_not_found(bucket));
                    }
                    Err(error) => {
                        return Ok(Self::json_upload_error(
                            StatusCode::INTERNAL_SERVER_ERROR,
                            "INTERNAL",
                            &error.to_string(),
                        ));
                    }
                };
                Ok(Self::json_bucket_response(StatusCode::OK, &namespace))
            }
            _ => Ok(Self::json_upload_error(
                StatusCode::METHOD_NOT_ALLOWED,
                "METHOD_NOT_ALLOWED",
                "Unsupported GCS JSON API bucket operation",
            )),
        }
    }

    fn json_bucket_not_found(bucket: &str) -> Response<Body> {
        Self::json_upload_error(
            StatusCode::NOT_FOUND,
            "NOT_FOUND",
            &format!("The specified bucket {bucket} does not exist."),
        )
    }

    fn unimplemented_data_protection_response(message: &str) -> Response<Body> {
        Self::json_error(StatusCode::NOT_IMPLEMENTED, "notImplemented", message)
    }

    fn invalid_data_protection_response(message: &str) -> Response<Body> {
        Self::json_error(StatusCode::BAD_REQUEST, "invalidArgument", message)
    }

    fn foreign_data_protection_json_response() -> Response<Body> {
        Self::json_error(
            StatusCode::CONFLICT,
            "conflict",
            "GCS object history and data protection are unavailable while another provider owns this bucket's retention or versioning mode",
        )
    }

    fn foreign_data_protection_xml_response() -> Response<Body> {
        Self::error_response(
            StatusCode::CONFLICT,
            "InvalidBucketState",
            "GCS object history is unavailable while another provider owns this bucket's retention or versioning mode",
        )
    }

    fn bucket_data_protection_requested(payload: &serde_json::Value) -> bool {
        payload.get("softDeletePolicy").is_some()
            || payload
                .get("retentionPolicy")
                .is_some_and(|policy| !policy.is_null())
    }

    fn foreign_data_protection_active(storage: &Arc<dyn Storage>, bucket: &str) -> bool {
        storage.get_bucket(bucket).ok().is_some_and(|bucket| {
            let s3_mode = bucket
                .metadata
                .get(S3_VERSIONING_STATUS_KEY)
                .is_some_and(|status| matches!(status.as_str(), "Enabled" | "Suspended"))
                || bucket
                    .metadata
                    .get(S3_OBJECT_LOCK_ENABLED_KEY)
                    .is_some_and(|enabled| enabled == "true");
            let azure_mode = bucket
                .metadata
                .get(AZURE_VERSIONING_KEY)
                .is_some_and(|enabled| enabled == "true")
                || bucket
                    .metadata
                    .get(AZURE_SOFT_DELETE_DAYS_KEY)
                    .and_then(|days| days.parse::<u64>().ok())
                    .is_some_and(|days| days > 0);
            let gcs_owns_shared_history = bucket
                .metadata
                .get(GCS_SOFT_DELETE_SECONDS_KEY)
                .and_then(|seconds| seconds.parse::<u64>().ok())
                .is_some_and(|seconds| seconds > 0);
            s3_mode || azure_mode || bucket.versioning_enabled && !gcs_owns_shared_history
        })
    }

    fn json_unsigned_integer(value: &serde_json::Value) -> Option<u64> {
        value
            .as_u64()
            .or_else(|| value.as_str().and_then(|value| value.parse::<u64>().ok()))
    }

    fn validate_bucket_data_protection(
        req: &Request,
        payload: &serde_json::Value,
    ) -> Option<Response<Body>> {
        if req.query_param("enableObjectRetention").is_some()
            || payload.get("objectRetention").is_some()
        {
            return Some(Self::unimplemented_data_protection_response(
                "Per-object retention configuration is not supported by this emulator surface",
            ));
        }

        if payload.pointer("/retentionPolicy/isLocked").is_some()
            || payload.pointer("/retentionPolicy/effectiveTime").is_some()
        {
            return Some(Self::unimplemented_data_protection_response(
                "Bucket retention-policy lock and server-owned fields are not writable",
            ));
        }
        if let Some(policy) = payload.get("retentionPolicy") {
            if !policy.is_null() {
                let Some(period) = policy
                    .get("retentionPeriod")
                    .and_then(Self::json_unsigned_integer)
                else {
                    return Some(Self::invalid_data_protection_response(
                        "retentionPolicy.retentionPeriod must be an unsigned integer",
                    ));
                };
                if period == 0 || period >= GCS_MAX_RETENTION_SECONDS_EXCLUSIVE {
                    return Some(Self::invalid_data_protection_response(
                        "retentionPolicy.retentionPeriod must be greater than 0 and less than 3155760000 seconds",
                    ));
                }
            }
        }

        if payload.pointer("/softDeletePolicy/effectiveTime").is_some() {
            return Some(Self::unimplemented_data_protection_response(
                "softDeletePolicy.effectiveTime is server-owned and is not writable",
            ));
        }
        if let Some(policy) = payload.get("softDeletePolicy") {
            let Some(duration) = policy
                .get("retentionDurationSeconds")
                .and_then(Self::json_unsigned_integer)
            else {
                return Some(Self::invalid_data_protection_response(
                    "softDeletePolicy.retentionDurationSeconds must be an unsigned integer",
                ));
            };
            if duration == 0 {
                return Some(Self::unimplemented_data_protection_response(
                    "Disabling an enabled soft-delete policy is not supported by this emulator surface",
                ));
            }
            if !(GCS_MIN_SOFT_DELETE_SECONDS..GCS_MAX_SOFT_DELETE_SECONDS_EXCLUSIVE)
                .contains(&duration)
            {
                return Some(Self::invalid_data_protection_response(
                    "softDeletePolicy.retentionDurationSeconds must be at least 604800 and less than 7776000 seconds",
                ));
            }
        }

        None
    }

    fn json_bucket_response(
        status: StatusCode,
        namespace: &crate::blob::Namespace,
    ) -> Response<Body> {
        let soft_delete = namespace
            .metadata
            .get(GCS_SOFT_DELETE_SECONDS_KEY)
            .map(|seconds| serde_json::json!({"retentionDurationSeconds": seconds}));
        let retention = namespace
            .metadata
            .get(GCS_RETENTION_SECONDS_KEY)
            .map(|seconds| serde_json::json!({"retentionPeriod": seconds}));
        Self::json_response(
            status,
            &serde_json::json!({
                "kind": "storage#bucket",
                "name": namespace.name,
                "timeCreated": namespace.created_at.to_rfc3339(),
                "softDeletePolicy": soft_delete,
                "retentionPolicy": retention,
            })
            .to_string(),
        )
    }

    fn apply_bucket_data_protection(
        storage: &Arc<dyn Storage>,
        bucket: &str,
        payload: &serde_json::Value,
    ) -> Result<(), String> {
        let mut metadata = storage
            .get_bucket(bucket)
            .map_err(|err| err.to_string())?
            .metadata;
        if let Some(seconds) = payload
            .pointer("/softDeletePolicy/retentionDurationSeconds")
            .and_then(|value| {
                value
                    .as_str()
                    .map(str::to_string)
                    .or_else(|| value.as_u64().map(|n| n.to_string()))
            })
        {
            metadata.insert(GCS_SOFT_DELETE_SECONDS_KEY.to_string(), seconds);
            storage
                .enable_versioning(bucket)
                .map_err(|err| err.to_string())?;
        }
        if let Some(policy) = payload.get("retentionPolicy") {
            if policy.is_null() {
                metadata.remove(GCS_RETENTION_SECONDS_KEY);
            } else if let Some(seconds) = policy
                .get("retentionPeriod")
                .and_then(Self::json_unsigned_integer)
            {
                metadata.insert(GCS_RETENTION_SECONDS_KEY.to_string(), seconds.to_string());
            }
        }
        storage
            .update_bucket_metadata(bucket, metadata)
            .map_err(|err| err.to_string())?;
        Ok(())
    }

    fn object_is_retained(
        storage: &Arc<dyn Storage>,
        bucket: &str,
        blob: &crate::models::Object,
    ) -> bool {
        let Ok(bucket) = storage.get_bucket(bucket) else {
            return false;
        };
        let Some(seconds) = bucket
            .metadata
            .get(GCS_RETENTION_SECONDS_KEY)
            .and_then(|value| value.parse::<i64>().ok())
        else {
            return false;
        };
        chrono::Utc::now()
            .signed_duration_since(blob.last_modified)
            .num_seconds()
            < seconds
    }

    fn existing_object_is_retained(storage: &Arc<dyn Storage>, bucket: &str, key: &str) -> bool {
        storage
            .get_object(bucket, key)
            .ok()
            .is_some_and(|object| Self::object_is_retained(storage, bucket, &object))
    }

    fn retention_policy_not_met_response() -> Response<Body> {
        Self::json_error(
            StatusCode::FORBIDDEN,
            "retentionPolicyNotMet",
            "Object is subject to an active retention policy",
        )
    }

    fn handle_json_object_api(
        &self,
        storage: &Arc<dyn Storage>,
        auth_config: &Arc<AuthConfig>,
        req: &Request,
        parts: &[&str],
        bucket: &str,
    ) -> Result<Response<Body>, String> {
        if parts.len() == 5 {
            if ["softDeleted", "versions"].into_iter().any(|parameter| {
                req.query_param(parameter)
                    .is_some_and(|value| value.eq_ignore_ascii_case("true"))
            }) {
                return Ok(Self::unimplemented_data_protection_response(
                    "GCS JSON object history listings are not supported by this emulator surface",
                ));
            }
            if Self::foreign_data_protection_active(storage, bucket) {
                return Ok(Self::foreign_data_protection_json_response());
            }
            return Ok(Self::list_json_objects(storage, req, bucket));
        }

        let object = match Self::decode_object_path(&parts[5..].join("/")) {
            Ok(object) => object,
            Err(error) => {
                return Ok(Self::json_upload_error(
                    StatusCode::BAD_REQUEST,
                    "INVALID_ARGUMENT",
                    &error,
                ));
            }
        };
        if let Err(response) = Self::authorize(req, auth_config, bucket, Some(&object)) {
            return Ok(response);
        }
        if Self::foreign_data_protection_active(storage, bucket) {
            return Ok(Self::foreign_data_protection_json_response());
        }
        let alt_media = req.query_param("alt") == Some("media");
        self.handle_json_object_item(storage, req, bucket, &object, alt_media)
    }

    fn list_json_objects(
        storage: &Arc<dyn Storage>,
        req: &Request,
        bucket: &str,
    ) -> Response<Body> {
        let marker = match req.query_param("pageToken") {
            Some(token) => match Self::decode_page_token("objects", token) {
                Some(marker) => Some(marker),
                None => return Self::invalid_page_token_response(),
            },
            None => None,
        };
        let max_results = match Self::json_max_results(req) {
            Ok(value) => value,
            Err(response) => return response,
        };
        let result = match storage.list_objects(
            bucket,
            req.query_param("prefix"),
            req.query_param("delimiter"),
            marker.as_deref(),
            Some(max_results.max(1)),
        ) {
            Ok(result) => result,
            Err(crate::error::Error::BucketNotFound) => {
                return Self::json_bucket_not_found(bucket);
            }
            Err(error) => {
                return Self::json_upload_error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "INTERNAL",
                    &error.to_string(),
                );
            }
        };
        let zero_page_has_entries =
            max_results == 0 && (!result.common_prefixes.is_empty() || !result.objects.is_empty());
        let next_page_token = if zero_page_has_entries {
            Some(Self::encode_page_token("objects", ""))
        } else if result.is_truncated {
            result
                .next_marker
                .as_deref()
                .map(|marker| Self::encode_page_token("objects", marker))
        } else {
            None
        };
        let prefixes = if max_results == 0 {
            Vec::new()
        } else {
            result.common_prefixes
        };
        let objects = if max_results == 0 {
            Vec::new()
        } else {
            result.objects
        };
        let blobs = objects
            .iter()
            .map(|object| crate::blob::BlobRecord::from_object(bucket, object))
            .collect::<Vec<_>>();
        let mut body = serde_json::json!({
            "kind": "storage#objects",
            "items": blobs.into_iter().map(|blob| {
                Self::json_blob_record_metadata(bucket, &blob)
            }).collect::<Vec<_>>(),
            "prefixes": prefixes,
        });
        if let Some(next_page_token) = next_page_token {
            body["nextPageToken"] = serde_json::Value::String(next_page_token);
        }
        Self::json_response(StatusCode::OK, &body.to_string())
    }

    fn handle_json_object_item(
        &self,
        storage: &Arc<dyn Storage>,
        req: &Request,
        bucket: &str,
        object: &str,
        alt_media: bool,
    ) -> Result<Response<Body>, String> {
        let mutation_lock = matches!(*req.method(), Method::PATCH | Method::DELETE)
            .then(|| self.object_mutation_lock(bucket, object))
            .transpose()?;
        let _guard = mutation_lock
            .as_ref()
            .map(|lock| {
                lock.lock()
                    .map_err(|_| "Failed to lock GCS object mutation".to_string())
            })
            .transpose()?;
        if matches!(*req.method(), Method::PATCH | Method::DELETE) {
            if let Some(response) = Self::invalid_json_mutation_headers(req) {
                return Ok(response);
            }
        }
        match *req.method() {
            Method::GET => Self::get_json_object(storage, req, bucket, object, alt_media),
            Method::PATCH => Self::patch_json_object(storage, req, bucket, object),
            Method::DELETE => Self::delete_json_object(storage, req, bucket, object),
            _ => Ok(Self::json_error(
                StatusCode::METHOD_NOT_ALLOWED,
                "methodNotAllowed",
                "The HTTP verb in the request is not supported.",
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
        let blob = match Self::checked_json_blob(storage, req, bucket, object) {
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
        let blob = match Self::checked_json_blob(storage, req, bucket, object) {
            Ok(blob) => blob,
            Err(response) => return Ok(*response),
        };
        let payload: serde_json::Value = match serde_json::from_slice(&req.body) {
            Ok(payload) => payload,
            Err(error) => {
                return Ok(Self::json_upload_error(
                    StatusCode::BAD_REQUEST,
                    "INVALID_ARGUMENT",
                    &format!("Invalid object metadata JSON: {error}"),
                ));
            }
        };
        let Some(payload_object) = payload.as_object() else {
            return Ok(Self::json_upload_error(
                StatusCode::BAD_REQUEST,
                "INVALID_ARGUMENT",
                "Object metadata patch must be a JSON object",
            ));
        };
        if let Some(field) = payload_object
            .keys()
            .find(|field| !matches!(field.as_str(), "metadata" | "contentType"))
        {
            return Ok(Self::json_upload_error(
                StatusCode::NOT_IMPLEMENTED,
                "UNIMPLEMENTED",
                &format!(
                    "Object metadata patch field {field} is not supported by this emulator surface"
                ),
            ));
        }
        let observed_object = blob;
        let mut updated_object = observed_object.clone();
        updated_object.metadata =
            match Self::metadata_patch_with_gcs_state(&payload, &updated_object) {
                Ok(metadata) => metadata,
                Err(message) => {
                    return Ok(Self::json_upload_error(
                        StatusCode::BAD_REQUEST,
                        "INVALID_ARGUMENT",
                        &message,
                    ));
                }
            };
        if let Some(content_type) = payload.get("contentType") {
            updated_object.content_type = match content_type {
                serde_json::Value::String(value) if !value.is_empty() => value.clone(),
                serde_json::Value::Null => "application/octet-stream".to_string(),
                _ => {
                    return Ok(Self::json_upload_error(
                        StatusCode::BAD_REQUEST,
                        "INVALID_ARGUMENT",
                        "contentType must be a non-empty string or null",
                    ));
                }
            };
        }
        if !storage
            .replace_object_metadata_if_unchanged(bucket, object, &observed_object, &updated_object)
            .map_err(|err| err.to_string())?
        {
            return Ok(Self::current_precondition_failure(
                storage, req, bucket, object,
            ));
        }
        let updated_object = storage
            .get_object(bucket, object)
            .map_err(|err| err.to_string())?;
        let updated = crate::blob::BlobRecord::from_object(bucket, &updated_object);
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
        let blob = match Self::checked_json_blob(storage, req, bucket, object) {
            Ok(blob) => blob,
            Err(response) => return Ok(*response),
        };
        if Self::object_is_retained(storage, bucket, &blob) {
            return Ok(Self::json_error(
                StatusCode::FORBIDDEN,
                "retentionPolicyNotMet",
                "Object is subject to an active retention policy",
            ));
        }
        let condition = Self::json_mutation_condition(req, &blob);
        if !storage
            .delete_object_if(bucket, object, &condition)
            .map_err(|err| err.to_string())?
        {
            return Ok(Self::current_precondition_failure(
                storage, req, bucket, object,
            ));
        }
        Ok(Self::empty_response(StatusCode::NO_CONTENT))
    }

    fn json_mutation_condition(req: &Request, blob: &crate::models::Object) -> ObjectCondition {
        let mut conditions = Vec::new();
        for (match_name, not_match_name, metadata_key) in [
            (
                "ifGenerationMatch",
                "ifGenerationNotMatch",
                GCS_GENERATION_KEY,
            ),
            (
                "ifMetagenerationMatch",
                "ifMetagenerationNotMatch",
                GCS_METAGENERATION_KEY,
            ),
        ] {
            if let Some(value) = req.query_param(match_name) {
                conditions.push(ObjectCondition::Metadata {
                    key: metadata_key.to_string(),
                    value: value.to_string(),
                });
            }
            if let Some(value) = req.query_param(not_match_name) {
                conditions.push(ObjectCondition::MetadataNot {
                    key: metadata_key.to_string(),
                    value: value.to_string(),
                });
            }
        }
        if conditions.is_empty() {
            conditions.extend([
                ObjectCondition::Metadata {
                    key: GCS_GENERATION_KEY.to_string(),
                    value: Self::generation(blob),
                },
                ObjectCondition::Metadata {
                    key: GCS_METAGENERATION_KEY.to_string(),
                    value: Self::metageneration(blob),
                },
            ]);
        }
        ObjectCondition::All(conditions)
    }

    fn current_precondition_failure(
        storage: &Arc<dyn Storage>,
        req: &Request,
        bucket: &str,
        object: &str,
    ) -> Response<Body> {
        storage.get_object(bucket, object).map_or_else(
            |_| Self::json_not_found(object),
            |current| {
                Self::check_gcs_preconditions(req, &current)
                    .err()
                    .unwrap_or_else(Self::generation_precondition_failed)
            },
        )
    }

    fn checked_json_blob(
        storage: &Arc<dyn Storage>,
        req: &Request,
        bucket: &str,
        object: &str,
    ) -> Result<crate::models::Object, Box<Response<Body>>> {
        let blob = match storage.as_ref().get_blob(bucket, object) {
            Ok(blob) => blob,
            Err(crate::error::Error::KeyNotFound) => {
                return Err(Box::new(Self::json_not_found(object)));
            }
            Err(crate::error::Error::BucketNotFound) => {
                return Err(Box::new(Self::json_bucket_not_found(bucket)));
            }
            Err(err) => {
                return Err(Box::new(Self::json_upload_error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "INTERNAL",
                    &err.to_string(),
                )));
            }
        };
        Self::check_current_generation_selector(req, &blob)?;
        if let Err(response) = Self::check_gcs_preconditions(req, &blob) {
            return Err(Box::new(response));
        }
        Ok(blob)
    }

    fn check_current_generation_selector(
        req: &Request,
        blob: &crate::models::Object,
    ) -> Result<(), Box<Response<Body>>> {
        let Some(selected) = req.query_param("generation") else {
            return Ok(());
        };
        if selected.parse::<u64>().is_err() {
            return Err(Box::new(Self::json_error(
                StatusCode::BAD_REQUEST,
                "invalidParameter",
                "Generation selectors must be unsigned integers",
            )));
        }
        if selected != Self::generation(blob) {
            return Err(Box::new(Self::unimplemented_data_protection_response(
                "Historical GCS JSON object generations are not supported by this emulator surface",
            )));
        }
        Ok(())
    }

    fn metadata_patch_with_gcs_state(
        payload: &serde_json::Value,
        blob: &crate::models::Object,
    ) -> Result<HashMap<String, String>, String> {
        let mut metadata = Self::public_metadata(&blob.metadata);
        if let Some(patch) = payload.get("metadata") {
            match patch {
                serde_json::Value::Null => metadata.clear(),
                serde_json::Value::Object(values) => {
                    for (key, value) in values {
                        match value {
                            serde_json::Value::String(value) => {
                                metadata.insert(key.clone(), value.clone());
                            }
                            serde_json::Value::Null => {
                                metadata.remove(key);
                            }
                            _ => {
                                return Err(format!(
                                    "Object metadata value {key} must be a string or null"
                                ));
                            }
                        }
                    }
                }
                _ => return Err("Object metadata must be a JSON object or null".to_string()),
            }
        }
        let previous_updated = Self::gcs_updated(&blob.metadata, blob.last_modified);
        Ok(Self::metadata_with_gcs_state(
            metadata,
            Self::generation(blob),
            blob.metadata
                .get(GCS_METAGENERATION_KEY)
                .and_then(|value| value.parse::<u64>().ok())
                .unwrap_or(1)
                .saturating_add(1)
                .to_string(),
            Some(&previous_updated),
        ))
    }

    fn json_object_metadata(bucket: &str, blob: &crate::models::Object) -> serde_json::Value {
        serde_json::json!({
            "kind": "storage#object",
            "name": blob.key,
            "bucket": bucket,
            "size": blob.size.to_string(),
            "crc32c": blob.provider_metadata.get(GCS_CRC32C_KEY).cloned()
                .unwrap_or_else(|| Self::encoded_crc32c(&blob.data)),
            "etag": Self::json_etag(&blob.metadata, &blob.etag),
            "generation": Self::generation(blob),
            "metageneration": Self::metageneration(blob),
            "updated": Self::gcs_updated(&blob.metadata, blob.last_modified),
            "contentType": blob.content_type,
            "metadata": Self::public_metadata(&blob.metadata),
        })
    }

    fn json_blob_record_metadata(
        bucket: &str,
        blob: &crate::blob::BlobRecord,
    ) -> serde_json::Value {
        serde_json::json!({
            "kind": "storage#object",
            "name": blob.key,
            "bucket": bucket,
            "size": blob.size.to_string(),
            "crc32c": blob.provider_metadata.get(GCS_CRC32C_KEY),
            "etag": Self::json_etag(&blob.metadata, &blob.etag),
            "generation": Self::generation_from_metadata(&blob.metadata),
            "metageneration": Self::metageneration_from_metadata(&blob.metadata),
            "updated": Self::gcs_updated(&blob.metadata, blob.last_modified),
            "contentType": blob.content_type,
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
        let object = match Self::decode_object_path(&parts[6..].join("/")) {
            Ok(object) => object,
            Err(error) => {
                return Ok(Self::json_upload_error(
                    StatusCode::BAD_REQUEST,
                    "INVALID_ARGUMENT",
                    &error,
                ));
            }
        };
        if let Err(response) = Self::authorize(req, auth_config, bucket, Some(&object)) {
            return Ok(response);
        }
        if Self::foreign_data_protection_active(storage, bucket) {
            return Ok(Self::foreign_data_protection_json_response());
        }
        let blob = match storage.as_ref().get_blob(bucket, &object) {
            Ok(blob) => blob,
            Err(crate::error::Error::KeyNotFound) => {
                return Ok(Self::json_not_found(&object));
            }
            Err(crate::error::Error::BucketNotFound) => {
                return Ok(Self::json_bucket_not_found(bucket));
            }
            Err(err) => {
                return Ok(Self::json_upload_error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "INTERNAL",
                    &err.to_string(),
                ));
            }
        };
        if let Err(response) = Self::check_current_generation_selector(req, &blob) {
            return Ok(*response);
        }
        if let Err(response) = Self::check_gcs_preconditions(req, &blob) {
            return Ok(response);
        }
        Self::object_media_response_for_blob(storage, req, bucket, &object, blob)
    }
}
