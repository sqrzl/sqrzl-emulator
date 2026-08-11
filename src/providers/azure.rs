use super::{state, ProviderAdapter};
use crate::auth::{AuthConfig, HttpRequestLike};
use crate::blob::{BlobBackend, BlobRange, BlobRecord};
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
use chrono::{DateTime, Utc};
use hmac::{Hmac, KeyInit, Mac};
use http::{HeaderMap, HeaderValue, Method, StatusCode, Uri};
use hyper::Response;
use quick_xml::escape::unescape;
use quick_xml::events::Event;
use quick_xml::Reader;
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, Weak};

const AZURE_VERSION: &str = "2023-11-03";
const AZURE_BLOB_TYPE_KEY: &str = "azure_blob_type";
const AZURE_LEASE_ID_KEY: &str = "azure_lease_id";
const AZURE_LEASE_STATE_KEY: &str = "azure_lease_state";
const AZURE_LEASE_STATUS_KEY: &str = "azure_lease_status";
const AZURE_LEASE_DURATION_KEY: &str = "azure_lease_duration";
const AZURE_SNAPSHOT_TIME_KEY: &str = "azure_snapshot_time";
const AZURE_SNAPSHOT_SOURCE_KEY: &str = "azure_snapshot_source";
const AZURE_IMMUTABILITY_UNTIL_KEY: &str = "azure_immutability_until";
const AZURE_IMMUTABILITY_MODE_KEY: &str = "azure_immutability_mode";
const AZURE_LEGAL_HOLD_KEY: &str = "azure_legal_hold";
const AZURE_SNAPSHOT_PREFIX: &str = "__sqrzl_azure_snapshot__";
const AZURE_BLOCK_SESSION_STATE: &str = "azure-block-session";
const AZURE_COMMITTED_BLOCKS_STATE: &str = "azure-committed-blocks";
const AZURE_VERSIONING_KEY: &str = "azure_versioning_enabled";
const AZURE_SOFT_DELETE_DAYS_KEY: &str = "azure_soft_delete_days";
const AZURE_CONTAINER_DELETION_STATE: &str = "azure-container-deletion";
const DEFAULT_AZURE_CONTAINER_DELETE_DELAY_MS: i64 = 30_000;
const S3_VERSIONING_STATUS_KEY: &str = "s3_versioning_status";
const S3_OBJECT_LOCK_ENABLED_KEY: &str = "s3_object_lock_enabled";
const GCS_SOFT_DELETE_SECONDS_KEY: &str = "gcs_soft_delete_seconds";
const GCS_RETENTION_SECONDS_KEY: &str = "gcs_retention_seconds";
const AZURE_SHARED_KEY_MAX_CLOCK_SKEW_MINUTES: i64 = 15;

#[derive(Clone, Default, Serialize, Deserialize)]
struct AzureBlockSession {
    blocks: HashMap<String, Vec<u8>>,
    content_type: Option<String>,
    metadata: HashMap<String, String>,
}

#[derive(Clone, Serialize, Deserialize)]
struct AzureCommittedBlock {
    id: String,
    data: Vec<u8>,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum AzureBlockSelector {
    Latest,
    Committed,
    Uncommitted,
}

struct AzureBlockReference {
    id: String,
    selector: AzureBlockSelector,
}

#[derive(Clone, Copy)]
enum AzureBlockListError {
    InvalidXmlDocument,
    InvalidBlockList,
}

#[derive(Clone, Serialize, Deserialize)]
struct AzureContainerDeletion {
    purge_after: DateTime<Utc>,
}

#[derive(Clone)]
enum AzureListEntry {
    Blob(Box<BlobRecord>),
    Prefix(String),
}

impl AzureListEntry {
    fn name(&self) -> &str {
        match self {
            Self::Blob(blob) => &blob.key,
            Self::Prefix(prefix) => prefix,
        }
    }
}

#[derive(Debug, Clone)]
struct AzureResource {
    account: String,
    container: Option<String>,
    blob: Option<String>,
}

pub struct AzureBlobAdapter {
    block_sessions: Mutex<HashMap<String, AzureBlockSession>>,
    committed_blocks: Mutex<HashMap<String, Vec<AzureCommittedBlock>>>,
    blob_mutation_locks: Mutex<HashMap<String, Weak<Mutex<()>>>>,
    container_operations: Mutex<()>,
}

impl Default for AzureBlobAdapter {
    fn default() -> Self {
        Self::new()
    }
}

impl AzureBlobAdapter {
    #[must_use]
    pub fn new() -> Self {
        Self {
            block_sessions: Mutex::new(HashMap::new()),
            committed_blocks: Mutex::new(HashMap::new()),
            blob_mutation_locks: Mutex::new(HashMap::new()),
            container_operations: Mutex::new(()),
        }
    }

    fn blob_state_key(account: &str, container: &str, blob: &str) -> String {
        format!("{account}/{container}/{blob}")
    }

    fn blob_mutation_lock(&self, container: &str, blob: &str) -> Result<Arc<Mutex<()>>, String> {
        let key = format!("{container}/{blob}");
        let mut locks = self
            .blob_mutation_locks
            .lock()
            .map_err(|_| "Failed to lock Azure mutation registry".to_string())?;
        locks.retain(|_, lock| lock.strong_count() > 0);
        if let Some(lock) = locks.get(&key).and_then(Weak::upgrade) {
            return Ok(lock);
        }
        let lock = Arc::new(Mutex::new(()));
        locks.insert(key, Arc::downgrade(&lock));
        Ok(lock)
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

    fn parse_write_range_header(value: &str) -> Option<(usize, usize)> {
        let range = value.strip_prefix("bytes=")?;
        let (start, end) = range.split_once('-')?;
        let start = start.parse::<usize>().ok()?;
        let end = end.parse::<usize>().ok()?;
        if end < start {
            return None;
        }
        Some((start, end))
    }

    fn requested_range(req: &Request) -> Option<&str> {
        req.header("x-ms-range").or_else(|| req.header("range"))
    }

    fn decode_path_value(value: &str, resource: &str) -> Result<String, String> {
        crate::utils::request::decode_uri_path(value)
            .map_err(|error| format!("Azure {resource} path is invalid: {error}"))
    }

    fn parse_resource(req: &Request) -> Result<AzureResource, String> {
        let path = req.path().strip_prefix('/').unwrap_or(req.path());
        if path.is_empty() {
            return Err("Azure requests must include an account segment".to_string());
        }

        let (account, remainder) = path
            .split_once('/')
            .map_or((path, None), |(account, rest)| (account, Some(rest)));
        if account.is_empty() {
            return Err("Azure requests must include an account segment".to_string());
        }

        let (container, blob) = match remainder {
            None | Some("") => (None, None),
            Some(rest) => {
                let (raw_container, raw_blob) = rest
                    .split_once('/')
                    .map_or((rest, None), |(container, blob)| (container, Some(blob)));
                if raw_container.is_empty() {
                    return Err("Azure requests must include a container segment".to_string());
                }
                let container = Self::decode_path_value(raw_container, "container")?;
                let blob = raw_blob
                    .filter(|blob| !blob.is_empty())
                    .map(|blob| Self::decode_path_value(blob, "blob"))
                    .transpose()?;
                (Some(container), blob)
            }
        };

        Ok(AzureResource {
            account: account.to_string(),
            container,
            blob,
        })
    }

    fn put_operation_requires_content_length(req: &Request) -> bool {
        if req.method() != Method::PUT || req.query_param("restype").is_some() {
            return false;
        }
        let blob_request = Self::parse_resource(req)
            .ok()
            .is_some_and(|resource| resource.container.is_some() && resource.blob.is_some());
        if !blob_request || req.header("x-ms-copy-source").is_some() {
            return false;
        }
        matches!(
            req.query_param("comp"),
            None | Some(
                "block"
                    | "blocklist"
                    | "metadata"
                    | "appendblock"
                    | "page"
                    | "lease"
                    | "snapshot"
                    | "legalhold"
                    | "immutabilityPolicies"
            )
        )
    }

    fn request_id() -> String {
        uuid::Uuid::new_v4().to_string()
    }

    fn response(status: StatusCode) -> ResponseBuilder {
        ResponseBuilder::new(status)
            .header("x-ms-version", AZURE_VERSION)
            .header("x-ms-request-id", &Self::request_id())
            .header("date", &crate::utils::headers::format_last_modified())
    }

    fn with_client_request_id(
        client_request_id: Option<&str>,
        mut response: Response<Body>,
    ) -> Response<Body> {
        if let Some(value) = client_request_id.filter(|value| {
            value.len() <= 1_024 && value.bytes().all(|byte| (0x20..=0x7e).contains(&byte))
        }) {
            if let Ok(value) = HeaderValue::from_str(value) {
                response
                    .headers_mut()
                    .insert("x-ms-client-request-id", value);
            }
        }
        response
    }

    fn matches_head(uri: &Uri, headers: &HeaderMap) -> bool {
        let authorization = headers
            .get("authorization")
            .and_then(|value| value.to_str().ok())
            .unwrap_or("");
        let query = uri.query().unwrap_or("");

        headers.contains_key("x-ms-version")
            || headers.contains_key("x-ms-blob-type")
            || authorization.starts_with("SharedKey ")
            || query.contains("restype=")
            || query.contains("comp=")
    }

    fn payload_too_large_response(max_request_bytes: usize) -> Response<Body> {
        let message =
            format!("Request body exceeds SQRZL_MAX_REQUEST_BYTES ({max_request_bytes} bytes)");
        let mut body =
            "<?xml version=\"1.0\" encoding=\"utf-8\"?><Error><Code>RequestBodyTooLarge</Code><Message>"
                .to_string();
        push_escaped_xml(&mut body, &message);
        body.push_str("</Message></Error>");

        Self::response(StatusCode::PAYLOAD_TOO_LARGE)
            .content_type("application/xml")
            .header("x-ms-error-code", "RequestBodyTooLarge")
            .body(body.into_bytes())
            .build()
    }

    fn empty_response(status: StatusCode) -> Response<Body> {
        Self::response(status).empty()
    }

    fn xml_response(status: StatusCode, body: String) -> Response<Body> {
        Self::response(status)
            .content_type("application/xml")
            .body(body.into_bytes())
            .build()
    }

    fn error_response(status: StatusCode, code: &str, message: &str) -> Response<Body> {
        let body = format!(
            "<?xml version=\"1.0\" encoding=\"utf-8\"?><Error><Code>{}</Code><Message>{}</Message></Error>",
            escape_xml(code),
            escape_xml(message),
        );

        Self::response(status)
            .content_type("application/xml")
            .header("x-ms-error-code", code)
            .body(body.into_bytes())
            .build()
    }

    fn metadata_from_headers(req: &Request) -> HashMap<String, String> {
        req.headers()
            .into_iter()
            .filter_map(|(name, value)| {
                name.strip_prefix("x-ms-meta-")
                    .map(|key| (key.to_string(), value))
            })
            .collect()
    }

    fn content_type(req: &Request) -> String {
        req.header("x-ms-blob-content-type")
            .or_else(|| req.header("content-type"))
            .unwrap_or("application/octet-stream")
            .to_string()
    }

    fn namespace_etag(namespace: &crate::blob::Namespace) -> String {
        format!(
            "\"{}\"",
            crate::utils::headers::compute_etag(namespace.name.as_bytes())
        )
    }

    fn namespace_response(
        status: StatusCode,
        namespace: &crate::blob::Namespace,
    ) -> ResponseBuilder {
        Self::response(status)
            .header("etag", &Self::namespace_etag(namespace))
            .header(
                "last-modified",
                &crate::utils::headers::format_last_modified_at(&namespace.created_at),
            )
    }

    fn azure_versioning_configured(bucket: &crate::models::Bucket) -> bool {
        bucket
            .metadata
            .get(AZURE_VERSIONING_KEY)
            .is_some_and(|value| value == "true")
    }

    fn azure_versioning_enabled(bucket: &crate::models::Bucket) -> bool {
        Self::azure_versioning_configured(bucket) && bucket.versioning_enabled
    }

    fn azure_history_mode(bucket: &crate::models::Bucket) -> bool {
        Self::azure_versioning_configured(bucket)
            || bucket
                .metadata
                .get(AZURE_SOFT_DELETE_DAYS_KEY)
                .and_then(|value| value.parse::<u64>().ok())
                .is_some_and(|days| days > 0)
    }

    fn foreign_history_mode(bucket: &crate::models::Bucket) -> bool {
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
    }

    fn azure_history_visible(storage: &Arc<dyn Storage>, container: &str) -> bool {
        storage.get_bucket(container).ok().is_some_and(|bucket| {
            Self::azure_history_mode(&bucket) && !Self::foreign_history_mode(&bucket)
        })
    }

    fn azure_history_conflict(storage: &Arc<dyn Storage>, container: &str) -> bool {
        storage.get_bucket(container).ok().is_some_and(|bucket| {
            Self::foreign_history_mode(&bucket)
                || bucket.versioning_enabled && !Self::azure_history_mode(&bucket)
        })
    }

    fn history_mode_conflict() -> Response<Body> {
        Self::error_response(
            StatusCode::CONFLICT,
            "FeatureVersionMismatch",
            "The container data-protection mode is not compatible with this Azure Blob operation.",
        )
    }

    #[allow(clippy::result_large_err)]
    fn requested_local_retention_modes(
        req: &Request,
    ) -> Result<(bool, Option<u64>), Response<Body>> {
        let versioning = match req.header("x-sqrzl-azure-versioning-enabled") {
            None | Some("false") => false,
            Some("true") => true,
            Some(_) => {
                return Err(Self::error_response(
                    StatusCode::BAD_REQUEST,
                    "InvalidHeaderValue",
                    "The local Azure versioning selector must be true or false.",
                ))
            }
        };
        let soft_delete_days = match req.header("x-sqrzl-azure-soft-delete-days") {
            None => None,
            Some(value) => match value.parse::<u64>() {
                Ok(days) if (1..=365).contains(&days) => Some(days),
                _ => {
                    return Err(Self::error_response(
                        StatusCode::BAD_REQUEST,
                        "InvalidHeaderValue",
                        "The local Azure soft-delete period must be between 1 and 365 days.",
                    ))
                }
            },
        };
        Ok((versioning, soft_delete_days))
    }

    #[allow(clippy::result_large_err)]
    fn max_results(req: &Request) -> Result<usize, Response<Body>> {
        let Some(raw) = req.query_param("maxresults") else {
            return Ok(5_000);
        };
        let value = raw.parse::<usize>().map_err(|_| {
            Self::error_response(
                StatusCode::BAD_REQUEST,
                "InvalidQueryParameterValue",
                "The value for maxresults is invalid.",
            )
        })?;
        if value == 0 {
            return Err(Self::error_response(
                StatusCode::BAD_REQUEST,
                "InvalidQueryParameterValue",
                "The value for maxresults must be greater than zero.",
            ));
        }
        Ok(value.min(5_000))
    }

    fn list_containers_xml(
        req: &Request,
        account: &str,
        namespaces: &[crate::blob::Namespace],
        next_marker: Option<&str>,
    ) -> String {
        let origin = request_origin(req);
        let mut xml = String::with_capacity(160 + namespaces.len() * 192);
        xml.push_str(
            "<?xml version=\"1.0\" encoding=\"utf-8\"?><EnumerationResults ServiceEndpoint=\"",
        );
        push_escaped_xml(&mut xml, &origin);
        xml.push('/');
        push_escaped_xml(&mut xml, account);
        xml.push_str("\">");
        for (name, query) in [
            ("Prefix", "prefix"),
            ("Marker", "marker"),
            ("MaxResults", "maxresults"),
        ] {
            if let Some(value) = req.query_param(query) {
                write!(&mut xml, "<{name}>").unwrap();
                push_escaped_xml(&mut xml, value);
                write!(&mut xml, "</{name}>").unwrap();
            }
        }
        xml.push_str("<Containers>");

        for namespace in namespaces {
            xml.push_str("<Container><Name>");
            push_escaped_xml(&mut xml, &namespace.name);
            xml.push_str("</Name><Properties><Last-Modified>");
            xml.push_str(&crate::utils::headers::format_last_modified_at(
                &namespace.created_at,
            ));
            xml.push_str("</Last-Modified><Etag>\"");
            xml.push_str(&crate::utils::headers::compute_etag(
                namespace.name.as_bytes(),
            ));
            xml.push_str("\"</Etag></Properties></Container>");
        }

        xml.push_str("</Containers><NextMarker>");
        if let Some(next_marker) = next_marker {
            push_escaped_xml(&mut xml, next_marker);
        }
        xml.push_str("</NextMarker></EnumerationResults>");
        xml
    }

    fn list_blobs_xml(
        req: &Request,
        container: &str,
        entries: &[AzureListEntry],
        next_marker: Option<&str>,
    ) -> String {
        let mut xml = String::with_capacity(192 + entries.len() * 288);
        xml.push_str(
            "<?xml version=\"1.0\" encoding=\"utf-8\"?><EnumerationResults ContainerName=\"",
        );
        push_escaped_xml(&mut xml, container);
        xml.push_str("\">");
        for (name, query) in [
            ("Prefix", "prefix"),
            ("Marker", "marker"),
            ("MaxResults", "maxresults"),
            ("Delimiter", "delimiter"),
        ] {
            if let Some(value) = req.query_param(query) {
                write!(&mut xml, "<{name}>").unwrap();
                push_escaped_xml(&mut xml, value);
                write!(&mut xml, "</{name}>").unwrap();
            }
        }
        xml.push_str("<Blobs>");

        for entry in entries {
            let AzureListEntry::Blob(blob) = entry else {
                xml.push_str("<BlobPrefix><Name>");
                push_escaped_xml(&mut xml, entry.name());
                xml.push_str("</Name></BlobPrefix>");
                continue;
            };
            let blob_type = blob
                .provider_metadata
                .get(AZURE_BLOB_TYPE_KEY)
                .map_or("BlockBlob", std::string::String::as_str);
            xml.push_str("<Blob><Name>");
            push_escaped_xml(&mut xml, &blob.key);
            xml.push_str("</Name><Properties><Content-Length>");
            write!(&mut xml, "{}", blob.size).unwrap();
            xml.push_str("</Content-Length><Content-Type>");
            push_escaped_xml(&mut xml, &blob.content_type);
            xml.push_str("</Content-Type><Etag>\"");
            push_escaped_xml(&mut xml, &blob.etag);
            xml.push_str("\"</Etag><BlobType>");
            push_escaped_xml(&mut xml, blob_type);
            xml.push_str("</BlobType><Last-Modified>");
            xml.push_str(&crate::utils::headers::format_last_modified_at(
                &blob.last_modified,
            ));
            xml.push_str("</Last-Modified></Properties></Blob>");
        }

        xml.push_str("</Blobs><NextMarker>");
        if let Some(next_marker) = next_marker {
            push_escaped_xml(&mut xml, next_marker);
        }
        xml.push_str("</NextMarker></EnumerationResults>");
        xml
    }

    fn block_list_xml(
        committed: &[AzureCommittedBlock],
        uncommitted: &[(String, Vec<u8>)],
        include_committed: bool,
        include_uncommitted: bool,
    ) -> String {
        let mut xml = String::with_capacity(96 + (committed.len() + uncommitted.len()) * 48);
        xml.push_str("<?xml version=\"1.0\" encoding=\"utf-8\"?><BlockList>");
        if include_committed {
            xml.push_str("<CommittedBlocks>");
            for block in committed {
                xml.push_str("<Block><Name>");
                push_escaped_xml(&mut xml, &block.id);
                write!(&mut xml, "</Name><Size>{}</Size></Block>", block.data.len()).unwrap();
            }
            xml.push_str("</CommittedBlocks>");
        }
        if include_uncommitted {
            xml.push_str("<UncommittedBlocks>");
            for (id, data) in uncommitted {
                xml.push_str("<Block><Name>");
                push_escaped_xml(&mut xml, id);
                write!(&mut xml, "</Name><Size>{}</Size></Block>", data.len()).unwrap();
            }
            xml.push_str("</UncommittedBlocks>");
        }
        xml.push_str("</BlockList>");
        xml
    }

    fn blob_type(blob: &crate::models::Object) -> &str {
        blob.provider_metadata
            .get(AZURE_BLOB_TYPE_KEY)
            .map_or("BlockBlob", std::string::String::as_str)
    }

    fn snapshot_storage_key(blob_key: &str, snapshot: &str) -> String {
        format!(
            "{}/{}/{}",
            AZURE_SNAPSHOT_PREFIX,
            URL_SAFE_NO_PAD.encode(blob_key.as_bytes()),
            snapshot
        )
    }

    fn is_snapshot_storage_key(key: &str) -> bool {
        key.starts_with(AZURE_SNAPSHOT_PREFIX)
    }

    fn snapshot_query(req: &Request) -> Option<String> {
        req.query_param("snapshot")
            .map(std::string::ToString::to_string)
    }

    fn snapshot_timestamp() -> String {
        Utc::now().format("%Y-%m-%dT%H:%M:%S%.6fZ").to_string()
    }

    fn snapshot_keys(
        storage: &Arc<dyn Storage>,
        container: &str,
        blob_key: &str,
    ) -> Result<Vec<String>, String> {
        let prefix = format!(
            "{}/{}/",
            AZURE_SNAPSHOT_PREFIX,
            URL_SAFE_NO_PAD.encode(blob_key.as_bytes())
        );
        let mut marker = None;
        let mut keys = Vec::new();
        loop {
            let result = storage
                .list_objects(
                    container,
                    Some(&prefix),
                    None,
                    marker.as_deref(),
                    Some(1_000),
                )
                .map_err(|err| err.to_string())?;
            keys.extend(result.objects.into_iter().map(|object| object.key));
            let Some(next_marker) = result.next_marker else {
                break;
            };
            marker = Some(next_marker);
        }
        Ok(keys)
    }

    fn delete_snapshots(
        storage: &Arc<dyn Storage>,
        container: &str,
        keys: &[String],
    ) -> Result<(), String> {
        for key in keys {
            storage
                .delete_object(container, key)
                .map_err(|err| err.to_string())?;
        }
        Ok(())
    }

    fn conditions_match_blob(req: &Request, blob: &crate::models::Object) -> bool {
        if let Some(if_match) = req.header("if-match") {
            let Some(etag) = Self::strong_etag(if_match) else {
                return false;
            };
            if etag != "*" && etag != blob.etag {
                return false;
            }
        }
        if let Some(if_none_match) = req.header("if-none-match") {
            let etag = if_none_match
                .trim()
                .trim_start_matches("W/")
                .trim_matches('"');
            if etag == "*" || etag == blob.etag {
                return false;
            }
        }
        true
    }

    fn strong_etag(value: &str) -> Option<&str> {
        let value = value.trim();
        (!value.starts_with("W/")).then(|| value.trim_matches('"'))
    }

    fn namespace_conditions_match(req: &Request, namespace: &crate::blob::Namespace) -> bool {
        let etag = Self::namespace_etag(namespace);
        let etag = etag.trim_matches('"');
        if let Some(if_match) = req.header("if-match") {
            let Some(candidate) = Self::strong_etag(if_match) else {
                return false;
            };
            if candidate != "*" && candidate != etag {
                return false;
            }
        }
        if let Some(if_none_match) = req.header("if-none-match") {
            let candidate = if_none_match
                .trim()
                .trim_start_matches("W/")
                .trim_matches('"');
            if candidate == "*" || candidate == etag {
                return false;
            }
        }
        true
    }

    fn lease_id(blob: &crate::models::Object) -> Option<&str> {
        blob.provider_metadata
            .get(AZURE_LEASE_ID_KEY)
            .map(std::string::String::as_str)
    }

    fn lease_status(blob: &crate::models::Object) -> &str {
        blob.provider_metadata
            .get(AZURE_LEASE_STATUS_KEY)
            .map_or("unlocked", std::string::String::as_str)
    }

    fn lease_state(blob: &crate::models::Object) -> &str {
        blob.provider_metadata
            .get(AZURE_LEASE_STATE_KEY)
            .map_or("available", std::string::String::as_str)
    }

    fn lease_duration(blob: &crate::models::Object) -> Option<&str> {
        blob.provider_metadata
            .get(AZURE_LEASE_DURATION_KEY)
            .map(std::string::String::as_str)
    }

    fn has_active_lease(blob: &crate::models::Object) -> bool {
        Self::lease_status(blob) == "locked" && Self::lease_id(blob).is_some()
    }

    fn retention_until(blob: &crate::models::Object) -> Option<DateTime<Utc>> {
        blob.provider_metadata
            .get(AZURE_IMMUTABILITY_UNTIL_KEY)
            .and_then(|value| {
                DateTime::parse_from_rfc3339(value)
                    .or_else(|_| DateTime::parse_from_rfc2822(value))
                    .ok()
            })
            .map(|value| value.with_timezone(&Utc))
    }

    fn has_legal_hold(blob: &crate::models::Object) -> bool {
        match blob.provider_metadata.get(AZURE_LEGAL_HOLD_KEY) {
            None => false,
            Some(value) if value == "false" => false,
            // Durable WORM state is security-sensitive. Any value other than the
            // one well-formed inactive representation must remain protected.
            Some(_) => true,
        }
    }

    fn is_immutable(blob: &crate::models::Object) -> bool {
        if Self::has_legal_hold(blob) {
            return true;
        }

        let mode = blob.provider_metadata.get(AZURE_IMMUTABILITY_MODE_KEY);
        let until = blob.provider_metadata.get(AZURE_IMMUTABILITY_UNTIL_KEY);
        match (mode.map(String::as_str), until) {
            (None, None) => false,
            (Some("Unlocked" | "Locked"), Some(_)) => {
                Self::retention_until(blob).is_none_or(|value| value > Utc::now())
            }
            // A mode without an expiry, or an unknown mode, is corrupt durable
            // protection state. Treat it as active rather than making data mutable.
            _ => true,
        }
    }

    fn container_has_immutable_blob(
        storage: &Arc<dyn Storage>,
        container: &str,
    ) -> Result<bool, String> {
        let mut marker = None;
        let mut seen_markers = HashSet::new();
        loop {
            let page = storage
                .list_objects(container, None, None, marker.as_deref(), Some(1_000))
                .map_err(|error| error.to_string())?;
            if page.objects.iter().any(Self::is_immutable) {
                return Ok(true);
            }
            let Some(next_marker) = page.next_marker else {
                break;
            };
            if !seen_markers.insert(next_marker.clone()) {
                return Err("Azure WORM scan received a repeated object marker".to_string());
            }
            marker = Some(next_marker);
        }

        storage
            .list_object_versions(container, None)
            .map(|versions| versions.iter().any(Self::is_immutable))
            .map_err(|error| error.to_string())
    }

    #[allow(clippy::result_large_err)]
    fn ensure_lease_allows(
        req: &Request,
        blob: &crate::models::Object,
    ) -> Result<(), Response<Body>> {
        if !Self::has_active_lease(blob) {
            return Ok(());
        }

        let Some(expected) = Self::lease_id(blob) else {
            return Ok(());
        };

        match req.header("x-ms-lease-id") {
            Some(provided) if provided == expected => Ok(()),
            Some(_) => Err(Self::error_response(
                StatusCode::PRECONDITION_FAILED,
                "LeaseIdMismatchWithBlobOperation",
                "The lease ID specified did not match the lease ID for the blob.",
            )),
            None => Err(Self::error_response(
                StatusCode::PRECONDITION_FAILED,
                "LeaseIdMissing",
                "There is currently a lease on the blob and no lease ID was specified in the request.",
            )),
        }
    }

    #[allow(clippy::result_large_err)]
    fn ensure_mutation_allowed(
        req: &Request,
        blob: &crate::models::Object,
    ) -> Result<(), Response<Body>> {
        if Self::is_immutable(blob) {
            return Err(Self::error_response(
                StatusCode::CONFLICT,
                "BlobImmutableDueToPolicy",
                "The blob is immutable due to an active policy or legal hold.",
            ));
        }
        Self::ensure_lease_allows(req, blob)
    }

    #[allow(clippy::result_large_err)]
    fn ensure_version_creating_overwrite_allowed(
        req: &Request,
        blob: &crate::models::Object,
        azure_versioning_enabled: bool,
    ) -> Result<(), Response<Body>> {
        if Self::is_immutable(blob) && azure_versioning_enabled {
            // Azure permits Put Blob and Put Block List against an immutable
            // current version when versioning is enabled: the prior protected
            // bytes remain in history and the write creates a new current version.
            return Self::ensure_lease_allows(req, blob);
        }
        Self::ensure_mutation_allowed(req, blob)
    }

    #[allow(clippy::result_large_err)]
    fn write_condition(req: &Request) -> Result<Option<ObjectCondition>, Response<Body>> {
        let if_match = req.header("if-match");
        let if_none_match = req.header("if-none-match");
        if if_match.is_some() && if_none_match.is_some() {
            return Err(Self::error_response(
                StatusCode::BAD_REQUEST,
                "MultipleConditionHeadersNotSupported",
                "Multiple condition headers are not supported for this write operation.",
            ));
        }
        let parse_single = |value: &str| {
            (!value.contains(',')).then(|| value.trim().trim_matches('"').to_string())
        };
        if let Some(value) = if_match {
            if value.trim() == "*" {
                return Ok(Some(ObjectCondition::EtagNotIn(Vec::new())));
            }
            if value.trim().starts_with("W/") {
                return Ok(Some(ObjectCondition::Etag(
                    "__sqrzl_weak_etag_never_matches__".to_string(),
                )));
            }
            return parse_single(value)
                .map(ObjectCondition::Etag)
                .map(Some)
                .ok_or_else(|| {
                    Self::error_response(
                        StatusCode::BAD_REQUEST,
                        "InvalidHeaderValue",
                        "The If-Match header must contain exactly one ETag value.",
                    )
                });
        }
        if let Some(value) = if_none_match {
            if value.trim() == "*" {
                return Ok(Some(ObjectCondition::Missing));
            }
            return parse_single(value.trim().trim_start_matches("W/"))
                .map(|etag| ObjectCondition::MissingOrEtagNotIn(vec![etag]))
                .map(Some)
                .ok_or_else(|| {
                    Self::error_response(
                        StatusCode::BAD_REQUEST,
                        "InvalidHeaderValue",
                        "The If-None-Match header must contain exactly one ETag value.",
                    )
                });
        }
        Ok(None)
    }

    fn condition_failed() -> Response<Body> {
        Self::error_response(
            StatusCode::PRECONDITION_FAILED,
            "ConditionNotMet",
            "The condition specified using HTTP conditional header(s) is not met.",
        )
    }

    fn blob_not_found() -> Response<Body> {
        Self::error_response(
            StatusCode::NOT_FOUND,
            "BlobNotFound",
            "The specified blob does not exist.",
        )
    }

    fn replace_blob_metadata_if_unchanged(
        storage: &Arc<dyn Storage>,
        container: &str,
        blob_key: &str,
        observed: &crate::models::Object,
        updated: &crate::models::Object,
    ) -> Result<bool, String> {
        storage
            .replace_object_metadata_if_unchanged(container, blob_key, observed, updated)
            .map_err(|error| error.to_string())
    }

    fn unsupported_selected_mutation(req: &Request) -> Option<Response<Body>> {
        if req.query_param("versionid").is_none() && req.query_param("snapshot").is_none() {
            return None;
        }

        match *req.method() {
            Method::PUT if req.query_param("comp").is_none() => Some(Self::error_response(
                StatusCode::BAD_REQUEST,
                "InvalidQueryParameterValue",
                "Blob snapshots and previous versions are read-only.",
            )),
            Method::PUT | Method::DELETE if req.query_param("comp").is_some() => {
                Some(Self::error_response(
                    StatusCode::NOT_IMPLEMENTED,
                    "FeatureNotSupported",
                    "Version- and snapshot-scoped mutations are not implemented by this emulator.",
                ))
            }
            // Ordinary DELETE is the supported version/snapshot mutation.
            _ => None,
        }
    }

    fn set_lease_state(
        blob: &mut crate::models::Object,
        lease_id: Option<String>,
        state: &str,
        status: &str,
        duration: Option<String>,
    ) {
        match lease_id {
            Some(lease_id) => {
                blob.provider_metadata
                    .insert(AZURE_LEASE_ID_KEY.to_string(), lease_id);
            }
            None => {
                blob.provider_metadata.remove(AZURE_LEASE_ID_KEY);
            }
        }
        blob.provider_metadata
            .insert(AZURE_LEASE_STATE_KEY.to_string(), state.to_string());
        blob.provider_metadata
            .insert(AZURE_LEASE_STATUS_KEY.to_string(), status.to_string());
        match duration {
            Some(duration) => {
                blob.provider_metadata
                    .insert(AZURE_LEASE_DURATION_KEY.to_string(), duration);
            }
            None => {
                blob.provider_metadata.remove(AZURE_LEASE_DURATION_KEY);
            }
        }
    }

    fn preserve_active_lease(
        source: &crate::models::Object,
        destination: &mut crate::models::Object,
    ) {
        if !Self::has_active_lease(source) {
            return;
        }
        for key in [
            AZURE_LEASE_ID_KEY,
            AZURE_LEASE_STATE_KEY,
            AZURE_LEASE_STATUS_KEY,
            AZURE_LEASE_DURATION_KEY,
        ] {
            if let Some(value) = source.provider_metadata.get(key) {
                destination
                    .provider_metadata
                    .insert(key.to_string(), value.clone());
            }
        }
    }

    fn lookup_blob(
        storage: &Arc<dyn Storage>,
        container: &str,
        blob_key: &str,
        snapshot: Option<&str>,
        version_id: Option<&str>,
    ) -> crate::error::Result<crate::models::Object> {
        if let Some(version_id) = version_id {
            return storage.get_object_version(container, blob_key, version_id);
        }
        let key = snapshot.map_or_else(
            || blob_key.to_string(),
            |value| Self::snapshot_storage_key(blob_key, value),
        );
        storage.get_object(container, &key)
    }

    fn set_blob_type(blob: &mut crate::models::Object, blob_type: &str) {
        blob.provider_metadata
            .insert(AZURE_BLOB_TYPE_KEY.to_string(), blob_type.to_string());
    }

    fn blob_response(
        status: StatusCode,
        blob: &crate::models::Object,
        body_len: usize,
        content_range: Option<String>,
        is_current_version: Option<bool>,
        expose_version_id: bool,
    ) -> ResponseBuilder {
        let mut builder = Self::response(status)
            .header("accept-ranges", "bytes")
            .header("content-length", &body_len.to_string())
            .header("content-type", &blob.content_type)
            .header("etag", &format!("\"{}\"", blob.etag))
            .header(
                "last-modified",
                &crate::utils::headers::format_last_modified_at(&blob.last_modified),
            )
            .header("x-ms-blob-type", Self::blob_type(blob));
        if let Some(snapshot) = blob.provider_metadata.get(AZURE_SNAPSHOT_TIME_KEY) {
            builder = builder.header("x-ms-snapshot", snapshot);
        }
        if let Some(value) = blob.provider_metadata.get(AZURE_IMMUTABILITY_UNTIL_KEY) {
            builder = builder.header("x-ms-immutability-policy-until-date", value);
        }
        if let Some(value) = blob.provider_metadata.get(AZURE_IMMUTABILITY_MODE_KEY) {
            builder = builder.header("x-ms-immutability-policy-mode", value);
        }
        if let Some(value) = blob.provider_metadata.get(AZURE_LEGAL_HOLD_KEY) {
            builder = builder.header("x-ms-legal-hold", value);
        }
        builder = builder
            .header("x-ms-lease-status", Self::lease_status(blob))
            .header("x-ms-lease-state", Self::lease_state(blob));
        if expose_version_id && !blob.provider_metadata.contains_key(AZURE_SNAPSHOT_TIME_KEY) {
            if let Some(version_id) = blob.version_id.as_deref() {
                builder = builder.header("x-ms-version-id", version_id);
                builder = builder.header(
                    "x-ms-is-current-version",
                    if is_current_version.unwrap_or(false) {
                        "true"
                    } else {
                        "false"
                    },
                );
            }
        }
        if let Some(duration) = Self::lease_duration(blob) {
            builder = builder.header("x-ms-lease-duration", duration);
        }
        for (key, value) in &blob.metadata {
            builder = builder.header(&format!("x-ms-meta-{key}"), value);
        }
        if let Some(content_range) = content_range {
            builder = builder.header("content-range", &content_range);
        }
        builder
    }

    fn response_body_len(size: u64) -> Result<usize, String> {
        usize::try_from(size).map_err(|_| "Azure blob is too large for this platform".to_string())
    }

    fn valid_container_name(name: &str) -> bool {
        if name == "$root" {
            return true;
        }
        if !(3..=63).contains(&name.len()) {
            return false;
        }
        let bytes = name.as_bytes();
        bytes.first().is_some_and(u8::is_ascii_alphanumeric)
            && bytes.last().is_some_and(u8::is_ascii_alphanumeric)
            && bytes
                .iter()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || *byte == b'-')
            && !name.contains("--")
    }

    fn rollback_created_container(
        storage: &Arc<dyn Storage>,
        container: &str,
        error: String,
    ) -> String {
        match storage.delete_namespace(container) {
            Ok(()) => error,
            Err(rollback_error) => {
                format!("{error}; failed to roll back Azure container creation: {rollback_error}")
            }
        }
    }

    fn canonicalized_headers(req: &Request) -> String {
        let mut headers: HashMap<String, Vec<String>> = HashMap::new();
        for (name, value) in req
            .headers()
            .into_iter()
            .filter(|(name, _)| name.starts_with("x-ms-"))
        {
            headers
                .entry(name.to_lowercase())
                .or_default()
                .push(value.split_whitespace().collect::<Vec<_>>().join(" "));
        }
        let mut names = headers.keys().cloned().collect::<Vec<_>>();
        names.sort_unstable();

        let mut canonical = String::new();
        for name in names {
            let values = headers.remove(&name).unwrap_or_default();
            let _ = writeln!(canonical, "{name}:{}", values.join(","));
        }
        canonical
    }

    fn canonicalized_resource(req: &Request, account: &str) -> String {
        let mut resource = format!("/{}{}", account, req.path());
        let mut query_map: HashMap<String, Vec<String>> = HashMap::new();
        for parameter in req.uri.query().unwrap_or("").split('&') {
            if parameter.is_empty() {
                continue;
            }
            let (key, value) = parameter.split_once('=').unwrap_or((parameter, ""));
            let key = urlencoding::decode(key)
                .map_or_else(|_| key.to_string(), std::borrow::Cow::into_owned)
                .to_lowercase();
            let value = urlencoding::decode(value)
                .map_or_else(|_| value.to_string(), std::borrow::Cow::into_owned);
            query_map.entry(key).or_default().push(value);
        }

        let mut keys: Vec<_> = query_map.keys().cloned().collect();
        keys.sort();
        for key in keys {
            let mut values = query_map.remove(&key).unwrap_or_default();
            values.sort();
            let _ = write!(resource, "\n{}:{}", key, values.join(","));
        }

        resource
    }

    fn shared_key_secret(config: &AuthConfig) -> Option<Vec<u8>> {
        let secret = config.secret_key()?;
        BASE64
            .decode(secret)
            .ok()
            .or_else(|| Some(secret.as_bytes().to_vec()))
    }

    fn shared_key_string_to_sign(req: &Request, account: &str) -> String {
        let content_length = match req.method() {
            &Method::GET | &Method::HEAD => String::new(),
            _ => req
                .header("content-length")
                .filter(|value| *value != "0")
                .unwrap_or("")
                .to_string(),
        };
        let date = if req.header("x-ms-date").is_some() {
            String::new()
        } else {
            req.header("date").unwrap_or("").to_string()
        };

        let standard_headers = [
            req.method().as_str().to_string(),
            req.header("content-encoding").unwrap_or("").to_string(),
            req.header("content-language").unwrap_or("").to_string(),
            content_length,
            req.header("content-md5").unwrap_or("").to_string(),
            req.header("content-type").unwrap_or("").to_string(),
            date,
            req.header("if-modified-since").unwrap_or("").to_string(),
            req.header("if-match").unwrap_or("").to_string(),
            req.header("if-none-match").unwrap_or("").to_string(),
            req.header("if-unmodified-since").unwrap_or("").to_string(),
            req.header("range").unwrap_or("").to_string(),
        ]
        .join("\n");

        // CanonicalizedHeaders already terminates every entry with a newline.
        // Azure requires direct concatenation with CanonicalizedResource; adding
        // another separator here changes the signature by inserting a blank line.
        format!(
            "{standard_headers}\n{}{}",
            Self::canonicalized_headers(req),
            Self::canonicalized_resource(req, account)
        )
    }

    fn validate_shared_key_date(req: &Request) -> Result<(), String> {
        let raw_date = req
            .header("x-ms-date")
            .or_else(|| req.header("date"))
            .ok_or_else(|| "Missing required Azure request date".to_string())?;
        let request_date = DateTime::parse_from_rfc2822(raw_date)
            .map_err(|_| "Invalid Azure request date".to_string())?
            .with_timezone(&Utc);
        let skew = Utc::now().signed_duration_since(request_date).abs();
        if skew > chrono::Duration::minutes(AZURE_SHARED_KEY_MAX_CLOCK_SKEW_MINUTES) {
            return Err("Azure request date is outside the permitted 15 minute window".to_string());
        }
        Ok(())
    }

    fn validate_shared_key(
        req: &Request,
        config: &AuthConfig,
        account: &str,
    ) -> Result<(), String> {
        Self::validate_shared_key_date(req)?;
        let authorization = req
            .header("authorization")
            .ok_or_else(|| "Missing Authorization header".to_string())?;
        let prefix = format!("SharedKey {account}:");
        let provided = authorization
            .strip_prefix(&prefix)
            .ok_or_else(|| "Unsupported Azure authorization scheme".to_string())?;
        let key = Self::shared_key_secret(config)
            .ok_or_else(|| "Missing Azure shared key".to_string())?;
        let expected = sign_hmac_base64(&key, &Self::shared_key_string_to_sign(req, account))?;

        if provided == expected {
            Ok(())
        } else {
            Err("Azure shared key signature mismatch".to_string())
        }
    }

    fn sas_string_to_sign(
        resource: &str,
        permissions: &str,
        starts_on: &str,
        expires_on: &str,
        version: &str,
        resource_type: &str,
    ) -> String {
        [
            permissions,
            starts_on,
            expires_on,
            resource,
            "",
            "",
            "",
            version,
            resource_type,
            "",
            "",
            "",
            "",
            "",
            "",
        ]
        .join("\n")
    }

    fn validate_sas(
        req: &Request,
        config: &AuthConfig,
        resource: &AzureResource,
    ) -> Result<(), String> {
        let signature = req
            .query_param("sig")
            .ok_or_else(|| "Missing SAS signature".to_string())?;
        let expires_on = req
            .query_param("se")
            .ok_or_else(|| "Missing SAS expiry".to_string())?;
        let permissions = req.query_param("sp").unwrap_or("");
        let starts_on = req.query_param("st").unwrap_or("");
        let version = req.query_param("sv").unwrap_or("");
        let resource_type = req.query_param("sr").unwrap_or("");

        let expiry = DateTime::parse_from_rfc3339(expires_on)
            .or_else(|_| DateTime::parse_from_str(expires_on, "%Y-%m-%dT%H:%M:%SZ"))
            .map_err(|_| "Invalid SAS expiry".to_string())?
            .with_timezone(&Utc);

        if Utc::now() > expiry {
            return Err("SAS token has expired".to_string());
        }
        if !starts_on.is_empty() {
            let start = DateTime::parse_from_rfc3339(starts_on)
                .or_else(|_| DateTime::parse_from_str(starts_on, "%Y-%m-%dT%H:%M:%SZ"))
                .map_err(|_| "Invalid SAS start time".to_string())?
                .with_timezone(&Utc);
            if Utc::now() < start {
                return Err("SAS token is not valid yet".to_string());
            }
        }
        if req.query_param("spr") == Some("https")
            && req.uri.scheme_str().is_some_and(|scheme| scheme != "https")
        {
            return Err("SAS token requires HTTPS".to_string());
        }

        let required_permission = match *req.method() {
            Method::GET | Method::HEAD => 'r',
            Method::DELETE => 'd',
            Method::PUT | Method::POST => 'w',
            _ => return Err("SAS token does not permit this method".to_string()),
        };
        if !permissions.contains(required_permission) {
            return Err(format!(
                "SAS token lacks required '{required_permission}' permission"
            ));
        }
        let expected_resource_type = if resource.blob.is_some() { "b" } else { "c" };
        if resource_type != expected_resource_type {
            return Err("SAS resource scope does not match the request".to_string());
        }

        let canonical_resource = if let Some(container) = &resource.container {
            if let Some(blob) = &resource.blob {
                format!("/blob/{}/{}/{}", resource.account, container, blob)
            } else {
                format!("/blob/{}/{}", resource.account, container)
            }
        } else {
            format!("/blob/{}", resource.account)
        };

        let key = Self::shared_key_secret(config)
            .ok_or_else(|| "Missing Azure shared key".to_string())?;
        let expected = sign_hmac_base64(
            &key,
            &Self::sas_string_to_sign(
                &canonical_resource,
                permissions,
                starts_on,
                expires_on,
                version,
                resource_type,
            ),
        )?;

        if expected == signature {
            Ok(())
        } else {
            Err("Azure SAS signature mismatch".to_string())
        }
    }

    #[allow(clippy::result_large_err)]
    fn authorize(
        req: &Request,
        config: &AuthConfig,
        resource: &AzureResource,
    ) -> Result<(), Response<Body>> {
        if !config.enforce_auth {
            return Ok(());
        }

        if req.query_param("sig").is_some() {
            return Self::validate_sas(req, config, resource).map_err(|msg| {
                Self::error_response(StatusCode::FORBIDDEN, "AuthenticationFailed", &msg)
            });
        }

        Self::validate_shared_key(req, config, &resource.account).map_err(|msg| {
            Self::error_response(StatusCode::FORBIDDEN, "AuthenticationFailed", &msg)
        })
    }

    fn parse_block_list(xml: &str) -> Result<Vec<AzureBlockReference>, AzureBlockListError> {
        let mut reader = Reader::from_str(xml);
        reader.config_mut().trim_text(true);
        let mut buf = Vec::new();
        let mut root_seen = false;
        let mut root_closed = false;
        let mut current_element: Option<Vec<u8>> = None;
        let mut current_block_id = String::new();
        let mut block_references = Vec::new();

        loop {
            match reader.read_event_into(&mut buf) {
                Ok(Event::Start(event)) => {
                    let name = event.name().as_ref().to_vec();
                    if !root_seen && name == b"BlockList" {
                        root_seen = true;
                    } else if root_seen
                        && !root_closed
                        && current_element.is_none()
                        && matches!(name.as_slice(), b"Latest" | b"Committed" | b"Uncommitted")
                    {
                        current_element = Some(name);
                        current_block_id.clear();
                    } else {
                        return Err(AzureBlockListError::InvalidBlockList);
                    }
                }
                Ok(Event::End(event)) => {
                    let name = event.name().as_ref().to_vec();
                    if current_element.as_deref() == Some(name.as_slice()) {
                        if current_block_id.is_empty() {
                            return Err(AzureBlockListError::InvalidBlockList);
                        }
                        let selector = match name.as_slice() {
                            b"Latest" => AzureBlockSelector::Latest,
                            b"Committed" => AzureBlockSelector::Committed,
                            b"Uncommitted" => AzureBlockSelector::Uncommitted,
                            _ => return Err(AzureBlockListError::InvalidBlockList),
                        };
                        block_references.push(AzureBlockReference {
                            id: std::mem::take(&mut current_block_id),
                            selector,
                        });
                        current_element = None;
                    } else if root_seen
                        && !root_closed
                        && current_element.is_none()
                        && name == b"BlockList"
                    {
                        root_closed = true;
                    } else {
                        return Err(AzureBlockListError::InvalidXmlDocument);
                    }
                }
                Ok(Event::Text(text)) if current_element.is_some() => {
                    let decoded = text
                        .decode()
                        .map_err(|_| AzureBlockListError::InvalidXmlDocument)?;
                    let value = unescape(&decoded)
                        .map_err(|_| AzureBlockListError::InvalidXmlDocument)?
                        .to_string();
                    current_block_id.push_str(&value);
                }
                Ok(Event::CData(text)) if current_element.is_some() => {
                    let decoded = text
                        .decode()
                        .map_err(|_| AzureBlockListError::InvalidXmlDocument)?;
                    current_block_id.push_str(&decoded);
                }
                Ok(Event::Text(text)) => {
                    let decoded = text
                        .decode()
                        .map_err(|_| AzureBlockListError::InvalidXmlDocument)?;
                    if !decoded.trim().is_empty() {
                        return Err(AzureBlockListError::InvalidBlockList);
                    }
                }
                Ok(Event::Eof) => break,
                Err(_) => return Err(AzureBlockListError::InvalidXmlDocument),
                Ok(Event::Decl(_) | Event::Comment(_) | Event::PI(_)) => {}
                _ => return Err(AzureBlockListError::InvalidBlockList),
            }
            buf.clear();
        }

        if current_element.is_some() || (root_seen && !root_closed) {
            return Err(AzureBlockListError::InvalidXmlDocument);
        }
        if !root_seen || block_references.is_empty() {
            return Err(AzureBlockListError::InvalidBlockList);
        }

        Ok(block_references)
    }

    fn invalid_block_list_response(error: AzureBlockListError) -> Response<Body> {
        let (code, message) = match error {
            AzureBlockListError::InvalidXmlDocument => (
                "InvalidXmlDocument",
                "The specified XML is not syntactically valid.",
            ),
            AzureBlockListError::InvalidBlockList => {
                ("InvalidBlockList", "The specified block list is invalid.")
            }
        };
        Self::error_response(StatusCode::BAD_REQUEST, code, message)
    }
}

impl ProviderAdapter for AzureBlobAdapter {
    fn name(&self) -> &'static str {
        "azure-blob"
    }

    fn matches(&self, req: &Request) -> bool {
        req.header("x-ms-version").is_some()
            || req
                .header("authorization")
                .is_some_and(|value| value.starts_with("SharedKey "))
            || req.header("x-ms-blob-type").is_some()
            || req.query_param("restype").is_some()
            || req.query_param("comp").is_some()
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
                .get("x-ms-client-request-id")
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
                .get("x-ms-client-request-id")
                .and_then(|value| value.to_str().ok()),
            Self::error_response(
                StatusCode::BAD_REQUEST,
                "InvalidHeaderValue",
                "The request body ended before the declared Content-Length was received.",
            ),
        )
    }

    fn validate_request_framing(&self, req: &Request) -> Option<Response<Body>> {
        if Self::put_operation_requires_content_length(req)
            && req.header("content-length").is_none()
        {
            return Some(Self::with_client_request_id(
                req.header("x-ms-client-request-id"),
                Self::error_response(
                    StatusCode::LENGTH_REQUIRED,
                    "MissingContentLengthHeader",
                    "The Content-Length header was not specified.",
                ),
            ));
        }
        if req
            .header("content-length")
            .is_some_and(|value| value.parse::<usize>().is_err())
        {
            return Some(Self::with_client_request_id(
                req.header("x-ms-client-request-id"),
                Self::error_response(
                    StatusCode::BAD_REQUEST,
                    "InvalidHeaderValue",
                    "The value for Content-Length is invalid.",
                ),
            ));
        }
        super::content_length_mismatch(req).then(|| {
            Self::with_client_request_id(
                req.header("x-ms-client-request-id"),
                Self::error_response(
                    StatusCode::BAD_REQUEST,
                    "InvalidHeaderValue",
                    "The value for Content-Length does not match the request body.",
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
        let client_request_id = req.header("x-ms-client-request-id").map(str::to_string);
        let result = self
            .handle_request(&storage, &auth_config, &req)
            .map(|response| Self::with_client_request_id(client_request_id.as_deref(), response));
        Box::pin(std::future::ready(result))
    }
}

impl AzureBlobAdapter {
    fn handle_request(
        &self,
        storage: &Arc<dyn Storage>,
        auth_config: &Arc<AuthConfig>,
        req: &Request,
    ) -> Result<Response<Body>, String> {
        let resource = match Self::parse_resource(req) {
            Ok(resource) => resource,
            Err(msg) => {
                return Ok(Self::error_response(
                    StatusCode::BAD_REQUEST,
                    "InvalidUri",
                    &msg,
                ))
            }
        };

        if let Err(response) = Self::authorize(req, auth_config, &resource) {
            return Ok(response);
        }

        if resource.container.is_none() {
            return self.handle_account_request(storage, req, &resource);
        }

        let container = resource.container.clone().unwrap_or_default();
        // This process-wide per-container lock is shared across adapter instances
        // and provider front doors. Hold it across pending-deletion resolution,
        // mutation admission, and retention activation so a protection policy
        // cannot appear between the last WORM check and physical reclamation.
        let data_protection_lock = super::data_protection_activation_lock(&container)?;
        let _data_protection_guard = data_protection_lock
            .lock()
            .map_err(|_| "Failed to lock Azure container data protection".to_string())?;
        let _container_guard = self
            .container_operations
            .lock()
            .map_err(|_| "Failed to lock Azure container operations".to_string())?;
        if let Some(response) = Self::resolve_container_deletion(storage, req, &container)? {
            return Ok(response);
        }
        if req.query_param("restype") == Some("container") {
            let mut claims_data_protection = false;
            if req.method() == Method::PUT {
                let (versioning, soft_delete_days) =
                    match Self::requested_local_retention_modes(req) {
                        Ok(modes) => modes,
                        Err(response) => return Ok(response),
                    };
                claims_data_protection = versioning || soft_delete_days.is_some();
            }
            if req.method() == Method::PUT
                && claims_data_protection
                && Self::azure_history_conflict(storage, &container)
            {
                return Ok(Self::history_mode_conflict());
            }
            if req.method() == Method::DELETE && Self::azure_history_conflict(storage, &container) {
                return Ok(Self::history_mode_conflict());
            }
            return Self::handle_container_request(storage, req, &container);
        }

        let Some(blob_key) = resource.blob.clone() else {
            return Ok(Self::error_response(
                StatusCode::BAD_REQUEST,
                "InvalidUri",
                "Blob requests must include a blob name",
            ));
        };
        match storage.get_bucket(&container) {
            Ok(_) => {}
            Err(crate::error::Error::BucketNotFound) => {
                return Ok(Self::container_not_found_for_blob_request(req));
            }
            Err(error) => {
                return Ok(Self::error_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "InternalError",
                    &error.to_string(),
                ));
            }
        }
        if Self::azure_history_conflict(storage, &container)
            && (matches!(*req.method(), Method::PUT | Method::DELETE)
                || req.query_param("versionid").is_some())
        {
            return Ok(Self::history_mode_conflict());
        }
        self.handle_blob_request(storage, req, &resource, &container, &blob_key)
    }

    fn handle_account_request(
        &self,
        storage: &Arc<dyn Storage>,
        req: &Request,
        resource: &AzureResource,
    ) -> Result<Response<Body>, String> {
        if req.method() == Method::GET && req.query_param("comp") == Some("list") {
            let max_results = match Self::max_results(req) {
                Ok(value) => value,
                Err(response) => return Ok(response),
            };
            let namespaces = {
                let _container_guard = self
                    .container_operations
                    .lock()
                    .map_err(|_| "Failed to lock Azure container operations".to_string())?;
                storage
                    .as_ref()
                    .list_namespaces()
                    .map_err(|err| err.to_string())?
            };
            let mut visible = Vec::with_capacity(namespaces.len());
            for namespace in namespaces {
                let data_protection_lock = super::data_protection_activation_lock(&namespace.name)?;
                let _data_protection_guard = data_protection_lock
                    .lock()
                    .map_err(|_| "Failed to lock Azure container data protection".to_string())?;
                if Self::finish_expired_container_deletion(storage, &namespace.name)? {
                    continue;
                }
                if state::load_json::<AzureContainerDeletion>(
                    storage.as_ref(),
                    AZURE_CONTAINER_DELETION_STATE,
                    &namespace.name,
                )?
                .is_none()
                {
                    visible.push(namespace);
                }
            }
            let mut namespaces = visible;
            namespaces.sort_unstable_by(|left, right| left.name.cmp(&right.name));
            if let Some(prefix) = req.query_param("prefix") {
                namespaces.retain(|namespace| namespace.name.starts_with(prefix));
            }
            if let Some(marker) = req.query_param("marker") {
                namespaces.retain(|namespace| namespace.name.as_str() >= marker);
            }
            let next_marker = namespaces
                .get(max_results)
                .map(|namespace| namespace.name.clone());
            namespaces.truncate(max_results);
            return Ok(Self::xml_response(
                StatusCode::OK,
                Self::list_containers_xml(
                    req,
                    &resource.account,
                    &namespaces,
                    next_marker.as_deref(),
                ),
            ));
        }

        Ok(Self::error_response(
            StatusCode::BAD_REQUEST,
            "InvalidUri",
            "Azure account requests must use comp=list",
        ))
    }

    fn create_container(
        storage: &Arc<dyn Storage>,
        req: &Request,
        container: &str,
    ) -> Result<Response<Body>, String> {
        if !Self::valid_container_name(container) {
            return Ok(Self::error_response(
                StatusCode::BAD_REQUEST,
                "InvalidResourceName",
                "The specified container name is invalid.",
            ));
        }
        let (versioning, soft_delete_days) = match Self::requested_local_retention_modes(req) {
            Ok(modes) => modes,
            Err(response) => return Ok(response),
        };
        let namespace = match storage.as_ref().create_namespace(container.to_string()) {
            Ok(namespace) => namespace,
            Err(crate::error::Error::BucketAlreadyExists) => {
                return Ok(Self::error_response(
                    StatusCode::CONFLICT,
                    "ContainerAlreadyExists",
                    "The specified container already exists.",
                ));
            }
            Err(error) => return Err(error.to_string()),
        };
        if versioning || soft_delete_days.is_some() {
            if let Err(error) = storage.enable_versioning(container) {
                return Err(Self::rollback_created_container(
                    storage,
                    container,
                    error.to_string(),
                ));
            }
            let mut metadata = match storage.get_bucket(container) {
                Ok(bucket) => bucket.metadata,
                Err(error) => {
                    return Err(Self::rollback_created_container(
                        storage,
                        container,
                        error.to_string(),
                    ));
                }
            };
            if versioning {
                metadata.insert(AZURE_VERSIONING_KEY.to_string(), "true".to_string());
            }
            if let Some(days) = soft_delete_days {
                metadata.insert(AZURE_SOFT_DELETE_DAYS_KEY.to_string(), days.to_string());
            }
            if let Err(error) = storage.update_bucket_metadata(container, metadata) {
                return Err(Self::rollback_created_container(
                    storage,
                    container,
                    error.to_string(),
                ));
            }
        }
        Ok(Self::namespace_response(StatusCode::CREATED, &namespace).empty())
    }

    fn handle_container_request(
        storage: &Arc<dyn Storage>,
        req: &Request,
        container: &str,
    ) -> Result<Response<Body>, String> {
        if req
            .query_param("comp")
            .is_some_and(|comp| !(comp == "list" && *req.method() == Method::GET))
        {
            return Ok(Self::error_response(
                StatusCode::NOT_IMPLEMENTED,
                "FeatureNotSupported",
                "This Azure container subresource is not implemented by the emulator.",
            ));
        }
        match *req.method() {
            Method::PUT => Self::create_container(storage, req, container),
            Method::DELETE => {
                let namespace = match storage.as_ref().get_namespace(container) {
                    Ok(namespace) => namespace,
                    Err(crate::error::Error::BucketNotFound) => {
                        return Ok(Self::container_not_found())
                    }
                    Err(error) => return Err(error.to_string()),
                };
                if !Self::namespace_conditions_match(req, &namespace) {
                    return Ok(Self::condition_failed());
                }
                if Self::container_has_immutable_blob(storage, container)? {
                    return Ok(Self::error_response(
                        StatusCode::CONFLICT,
                        "BlobImmutableDueToPolicy",
                        "The container contains blob data protected by an active policy or legal hold.",
                    ));
                }
                let delay_ms = match req.header("x-sqrzl-azure-delete-delay-ms") {
                    Some(value) => match value.parse::<i64>() {
                        Ok(value) if value >= 0 => value,
                        _ => {
                            return Ok(Self::error_response(
                                StatusCode::BAD_REQUEST,
                                "InvalidHeaderValue",
                                "The local Azure container deletion delay must be a nonnegative integer.",
                            ))
                        }
                    },
                    None => DEFAULT_AZURE_CONTAINER_DELETE_DELAY_MS,
                };
                let deletion = AzureContainerDeletion {
                    purge_after: Utc::now() + chrono::Duration::milliseconds(delay_ms),
                };
                state::save_json(
                    storage.as_ref(),
                    AZURE_CONTAINER_DELETION_STATE,
                    container,
                    &deletion,
                )?;
                Ok(Self::empty_response(StatusCode::ACCEPTED))
            }
            Method::GET => Self::get_container(storage, req, container),
            _ => Ok(Self::error_response(
                StatusCode::METHOD_NOT_ALLOWED,
                "UnsupportedHttpVerb",
                "Unsupported Azure container operation",
            )),
        }
    }

    fn get_container(
        storage: &Arc<dyn Storage>,
        req: &Request,
        container: &str,
    ) -> Result<Response<Body>, String> {
        if req.query_param("comp") == Some("list") {
            let max_results = match Self::max_results(req) {
                Ok(value) => value,
                Err(response) => return Ok(response),
            };
            let mut entries = match Self::azure_list_entries(storage, req, container) {
                Ok(entries) => entries,
                Err(crate::error::Error::BucketNotFound) => return Ok(Self::container_not_found()),
                Err(error) => return Err(error.to_string()),
            };
            let next_marker = entries
                .get(max_results)
                .map(|entry| entry.name().to_string());
            entries.truncate(max_results);
            return Ok(Self::xml_response(
                StatusCode::OK,
                Self::list_blobs_xml(req, container, &entries, next_marker.as_deref()),
            ));
        }

        let namespace = match storage.as_ref().get_namespace(container) {
            Ok(namespace) => namespace,
            Err(crate::error::Error::BucketNotFound) => return Ok(Self::container_not_found()),
            Err(error) => return Err(error.to_string()),
        };
        Ok(Self::namespace_response(StatusCode::OK, &namespace).empty())
    }

    fn azure_list_entries(
        storage: &Arc<dyn Storage>,
        req: &Request,
        container: &str,
    ) -> crate::error::Result<Vec<AzureListEntry>> {
        let mut marker = None;
        let mut objects = Vec::new();
        loop {
            let page = storage.list_objects(
                container,
                req.query_param("prefix"),
                None,
                marker.as_deref(),
                Some(1_000),
            )?;
            objects.extend(
                page.objects
                    .into_iter()
                    .filter(|object| !Self::is_snapshot_storage_key(&object.key)),
            );
            let Some(next_marker) = page.next_marker else {
                break;
            };
            if marker.as_deref() == Some(next_marker.as_str()) {
                break;
            }
            marker = Some(next_marker);
        }

        let prefix = req.query_param("prefix").unwrap_or("");
        let request_marker = req.query_param("marker");
        let delimiter = req
            .query_param("delimiter")
            .filter(|value| !value.is_empty());
        let mut entries = Vec::new();
        let mut prefixes = std::collections::BTreeSet::new();
        for object in objects {
            if let Some(delimiter) = delimiter {
                let remainder = object.key.strip_prefix(prefix).unwrap_or(&object.key);
                if let Some(index) = remainder.find(delimiter) {
                    let end = prefix.len() + index + delimiter.len();
                    prefixes.insert(object.key[..end].to_string());
                    continue;
                }
            }
            entries.push(AzureListEntry::Blob(Box::new(BlobRecord::from_object(
                container, &object,
            ))));
        }
        entries.extend(prefixes.into_iter().map(AzureListEntry::Prefix));
        entries.sort_unstable_by(|left, right| left.name().cmp(right.name()));
        if let Some(marker) = request_marker {
            entries.retain(|entry| entry.name() >= marker);
        }
        Ok(entries)
    }

    fn container_not_found() -> Response<Body> {
        Self::error_response(
            StatusCode::NOT_FOUND,
            "ContainerNotFound",
            "The specified container does not exist.",
        )
    }

    fn container_not_found_for_blob_request(req: &Request) -> Response<Body> {
        if *req.method() == Method::HEAD {
            Self::response(StatusCode::NOT_FOUND)
                .header("x-ms-error-code", "ContainerNotFound")
                .empty()
        } else {
            Self::container_not_found()
        }
    }

    fn resolve_container_deletion(
        storage: &Arc<dyn Storage>,
        req: &Request,
        container: &str,
    ) -> Result<Option<Response<Body>>, String> {
        if Self::finish_expired_container_deletion(storage, container)? {
            return Ok(None);
        }
        let Some(_) = state::load_json::<AzureContainerDeletion>(
            storage.as_ref(),
            AZURE_CONTAINER_DELETION_STATE,
            container,
        )?
        else {
            return Ok(None);
        };
        if req.method() == Method::PUT && req.query_param("restype") == Some("container") {
            return Ok(Some(Self::error_response(
                StatusCode::CONFLICT,
                "ContainerBeingDeleted",
                "The specified container is being deleted.",
            )));
        }
        Ok(Some(Self::container_not_found()))
    }

    fn finish_expired_container_deletion(
        storage: &Arc<dyn Storage>,
        container: &str,
    ) -> Result<bool, String> {
        let Some(deletion) = state::load_json::<AzureContainerDeletion>(
            storage.as_ref(),
            AZURE_CONTAINER_DELETION_STATE,
            container,
        )?
        else {
            return Ok(false);
        };
        if deletion.purge_after > Utc::now() {
            return Ok(false);
        }
        if Self::container_has_immutable_blob(storage, container)? {
            // Protection may have become active after deletion was scheduled.
            // Abandon the pending deletion instead of reclaiming protected bytes.
            storage
                .delete_provider_state(AZURE_CONTAINER_DELETION_STATE, container)
                .map_err(|error| error.to_string())?;
            return Ok(false);
        }
        match Self::purge_container(storage, container) {
            Ok(()) | Err(crate::error::Error::BucketNotFound) => {}
            Err(error) => return Err(error.to_string()),
        }
        storage
            .delete_provider_state(AZURE_CONTAINER_DELETION_STATE, container)
            .map_err(|error| error.to_string())?;
        Ok(true)
    }

    fn purge_container(storage: &Arc<dyn Storage>, container: &str) -> crate::error::Result<()> {
        storage.suspend_versioning(container)?;
        loop {
            let objects = storage
                .list_objects(container, None, None, None, Some(1_000))?
                .objects;
            if objects.is_empty() {
                break;
            }
            for object in objects {
                storage.delete_object(container, &object.key)?;
            }
        }
        for version in storage.list_object_versions(container, None)? {
            if let Some(version_id) = version.version_id.as_deref() {
                storage.delete_object_version(container, &version.key, version_id)?;
            }
        }
        storage.delete_namespace(container)
    }

    fn handle_blob_request(
        &self,
        storage: &Arc<dyn Storage>,
        req: &Request,
        resource: &AzureResource,
        container: &str,
        blob_key: &str,
    ) -> Result<Response<Body>, String> {
        if let Some(response) = Self::unsupported_selected_mutation(req) {
            return Ok(response);
        }
        if req.method() == Method::PUT && req.header("x-ms-copy-source").is_some() {
            return Ok(Self::error_response(
                StatusCode::NOT_IMPLEMENTED,
                "FeatureNotSupported",
                "Azure copy operations are not implemented by this emulator.",
            ));
        }
        let mutation_lock = matches!(*req.method(), Method::PUT | Method::DELETE)
            .then(|| self.blob_mutation_lock(container, blob_key))
            .transpose()?;
        let _mutation_guard = mutation_lock
            .as_ref()
            .map(|lock| {
                lock.lock()
                    .map_err(|_| "Failed to lock Azure blob mutation".to_string())
            })
            .transpose()?;
        let comp = req.query_param("comp");
        if req.method() == Method::PUT && comp == Some("lease") {
            return Self::handle_lease(storage, req, container, blob_key);
        }
        if req.method() == Method::PUT && comp == Some("snapshot") {
            return Self::create_snapshot(storage, req, container, blob_key);
        }
        if req.method() == Method::PUT && Self::is_immutability_comp(comp) {
            return Self::put_immutability_policy(storage, req, container, blob_key);
        }
        if req.method() == Method::DELETE && Self::is_immutability_comp(comp) {
            return Self::delete_immutability_policy(storage, req, container, blob_key);
        }
        if req.method() == Method::PUT && comp == Some("legalhold") {
            return Self::put_legal_hold(storage, req, container, blob_key);
        }
        self.handle_blob_comp_request(storage, req, resource, container, blob_key)
    }

    fn handle_blob_comp_request(
        &self,
        storage: &Arc<dyn Storage>,
        req: &Request,
        resource: &AzureResource,
        container: &str,
        blob_key: &str,
    ) -> Result<Response<Body>, String> {
        match (req.method(), req.query_param("comp")) {
            (&Method::PUT, Some("block")) => {
                self.put_block(storage, req, &resource.account, container, blob_key)
            }
            (&Method::PUT, Some("blocklist")) => {
                self.put_block_list(storage, req, &resource.account, container, blob_key)
            }
            (&Method::PUT, Some("metadata")) => {
                Self::update_metadata(storage, req, container, blob_key)
            }
            (&Method::GET, Some("blocklist")) => {
                self.get_block_list(storage, req, &resource.account, container, blob_key)
            }
            (&Method::PUT, Some("appendblock")) => {
                Self::append_block(storage, req, container, blob_key)
            }
            (&Method::PUT, Some("page")) => Self::put_page(storage, req, container, blob_key),
            (_, Some(_)) => Ok(Self::error_response(
                StatusCode::NOT_IMPLEMENTED,
                "FeatureNotSupported",
                "This Azure blob subresource is not implemented by the emulator.",
            )),
            (_, None) => Self::handle_blob_crud(storage, req, container, blob_key),
        }
    }

    fn is_immutability_comp(comp: Option<&str>) -> bool {
        comp == Some("immutabilityPolicies")
    }

    fn handle_lease(
        storage: &Arc<dyn Storage>,
        req: &Request,
        container: &str,
        blob_key: &str,
    ) -> Result<Response<Body>, String> {
        let action = req.header("x-ms-lease-action").unwrap_or("");
        let mut blob = match storage.as_ref().get_blob(container, blob_key) {
            Ok(blob) => blob,
            Err(crate::error::Error::KeyNotFound | crate::error::Error::BucketNotFound) => {
                return Ok(Self::blob_not_found())
            }
            Err(error) => return Err(error.to_string()),
        };
        if !Self::conditions_match_blob(req, &blob) {
            return Ok(Self::condition_failed());
        }
        match action {
            "acquire" => Self::acquire_lease(storage, req, container, blob_key, blob),
            "renew" => {
                if let Err(response) = Self::ensure_lease_allows(req, &blob) {
                    return Ok(response);
                }
                if !Self::replace_blob_metadata_if_unchanged(
                    storage, container, blob_key, &blob, &blob,
                )? {
                    return Ok(Self::condition_failed());
                }
                Ok(Self::response(StatusCode::OK)
                    .header("x-ms-lease-id", Self::lease_id(&blob).unwrap_or(""))
                    .empty())
            }
            "release" => {
                if let Err(response) = Self::ensure_lease_allows(req, &blob) {
                    return Ok(response);
                }
                let observed = blob.clone();
                Self::set_lease_state(&mut blob, None, "available", "unlocked", None);
                if !Self::replace_blob_metadata_if_unchanged(
                    storage, container, blob_key, &observed, &blob,
                )? {
                    return Ok(Self::condition_failed());
                }
                Ok(Self::empty_response(StatusCode::OK))
            }
            "break" => {
                if !Self::has_active_lease(&blob) {
                    return Ok(Self::error_response(
                        StatusCode::CONFLICT,
                        "LeaseNotPresentWithBlobOperation",
                        "There is currently no lease on the blob.",
                    ));
                }
                let observed = blob.clone();
                Self::set_lease_state(&mut blob, None, "broken", "unlocked", None);
                if !Self::replace_blob_metadata_if_unchanged(
                    storage, container, blob_key, &observed, &blob,
                )? {
                    return Ok(Self::condition_failed());
                }
                Ok(Self::response(StatusCode::ACCEPTED)
                    .header("x-ms-lease-time", "0")
                    .empty())
            }
            _ => Ok(Self::error_response(
                StatusCode::BAD_REQUEST,
                "InvalidHeaderValue",
                "Unsupported Azure lease action.",
            )),
        }
    }

    fn acquire_lease(
        storage: &Arc<dyn Storage>,
        req: &Request,
        container: &str,
        blob_key: &str,
        mut blob: crate::models::Object,
    ) -> Result<Response<Body>, String> {
        if Self::has_active_lease(&blob) {
            return Ok(Self::error_response(
                StatusCode::CONFLICT,
                "LeaseAlreadyPresent",
                "The blob already has an active lease.",
            ));
        }
        let observed = blob.clone();
        let lease_id = req.header("x-ms-proposed-lease-id").map_or_else(
            || uuid::Uuid::new_v4().to_string(),
            std::string::ToString::to_string,
        );
        let duration = req.header("x-ms-lease-duration").unwrap_or("-1");
        Self::set_lease_state(
            &mut blob,
            Some(lease_id.clone()),
            "leased",
            "locked",
            Some(if duration == "-1" {
                "infinite".to_string()
            } else {
                "fixed".to_string()
            }),
        );
        if !Self::replace_blob_metadata_if_unchanged(
            storage, container, blob_key, &observed, &blob,
        )? {
            return Ok(Self::condition_failed());
        }
        Ok(Self::response(StatusCode::CREATED)
            .header("x-ms-lease-id", &lease_id)
            .empty())
    }

    fn create_snapshot(
        storage: &Arc<dyn Storage>,
        req: &Request,
        container: &str,
        blob_key: &str,
    ) -> Result<Response<Body>, String> {
        let blob = match storage.as_ref().get_blob(container, blob_key) {
            Ok(blob) => blob,
            Err(crate::error::Error::KeyNotFound | crate::error::Error::BucketNotFound) => {
                return Ok(Self::error_response(
                    StatusCode::NOT_FOUND,
                    "BlobNotFound",
                    "The specified blob does not exist.",
                ))
            }
            Err(error) => return Err(error.to_string()),
        };
        if let Err(response) = Self::ensure_lease_allows(req, &blob) {
            return Ok(response);
        }
        if !Self::conditions_match_blob(req, &blob) {
            return Ok(Self::condition_failed());
        }
        let snapshot_time = Self::snapshot_timestamp();
        let snapshot_key = Self::snapshot_storage_key(blob_key, &snapshot_time);
        let mut snapshot_blob = blob.clone();
        snapshot_blob.key.clone_from(&snapshot_key);
        snapshot_blob.provider_metadata.remove(AZURE_LEASE_ID_KEY);
        snapshot_blob
            .provider_metadata
            .remove(AZURE_LEASE_STATE_KEY);
        snapshot_blob
            .provider_metadata
            .remove(AZURE_LEASE_STATUS_KEY);
        snapshot_blob
            .provider_metadata
            .remove(AZURE_LEASE_DURATION_KEY);
        snapshot_blob
            .provider_metadata
            .insert(AZURE_SNAPSHOT_TIME_KEY.to_string(), snapshot_time.clone());
        snapshot_blob
            .provider_metadata
            .insert(AZURE_SNAPSHOT_SOURCE_KEY.to_string(), blob_key.to_string());
        let metadata = Self::metadata_from_headers(req);
        if !metadata.is_empty() {
            snapshot_blob.metadata = metadata;
            snapshot_blob.last_modified = Utc::now();
            snapshot_blob.etag = uuid::Uuid::new_v4().simple().to_string();
        }
        let snapshot_etag = snapshot_blob.etag.clone();
        let snapshot_last_modified = snapshot_blob.last_modified;
        storage
            .put_object(container, snapshot_key, snapshot_blob)
            .map_err(|err| err.to_string())?;
        Ok(Self::response(StatusCode::CREATED)
            .header("x-ms-snapshot", &snapshot_time)
            .header("etag", &format!("\"{snapshot_etag}\""))
            .header(
                "last-modified",
                &crate::utils::headers::format_last_modified_at(&snapshot_last_modified),
            )
            .empty())
    }

    fn put_immutability_policy(
        storage: &Arc<dyn Storage>,
        req: &Request,
        container: &str,
        blob_key: &str,
    ) -> Result<Response<Body>, String> {
        let Some(until) = req.header("x-ms-immutability-policy-until-date") else {
            return Ok(Self::error_response(
                StatusCode::BAD_REQUEST,
                "MissingRequiredHeader",
                "The x-ms-immutability-policy-until-date header is required.",
            ));
        };
        let requested_until = match DateTime::parse_from_rfc2822(until) {
            Ok(value) if value.with_timezone(&Utc) > Utc::now() => value.with_timezone(&Utc),
            _ => return Ok(Self::error_response(
                StatusCode::BAD_REQUEST,
                "InvalidHeaderValue",
                "The x-ms-immutability-policy-until-date header must be a future RFC 1123 date.",
            )),
        };
        let mode = req
            .header("x-ms-immutability-policy-mode")
            .unwrap_or("Unlocked");
        if !matches!(mode, "Unlocked" | "Locked") {
            return Ok(Self::error_response(
                StatusCode::BAD_REQUEST,
                "InvalidHeaderValue",
                "The x-ms-immutability-policy-mode header must be Unlocked or Locked.",
            ));
        }
        let mut blob = match storage.as_ref().get_blob(container, blob_key) {
            Ok(blob) => blob,
            Err(crate::error::Error::KeyNotFound | crate::error::Error::BucketNotFound) => {
                return Ok(Self::blob_not_found())
            }
            Err(error) => return Err(error.to_string()),
        };
        if !Self::conditions_match_blob(req, &blob) {
            return Ok(Self::condition_failed());
        }
        if blob
            .provider_metadata
            .get(AZURE_IMMUTABILITY_MODE_KEY)
            .is_some_and(|existing_mode| existing_mode == "Locked")
            && (mode != "Locked"
                || Self::retention_until(&blob)
                    .is_none_or(|existing_until| requested_until < existing_until))
        {
            return Ok(Self::error_response(
                StatusCode::CONFLICT,
                "BlobImmutableDueToPolicy",
                "A locked immutability policy cannot be unlocked or shortened.",
            ));
        }
        let observed = blob.clone();
        blob.provider_metadata
            .insert(AZURE_IMMUTABILITY_UNTIL_KEY.to_string(), until.to_string());
        blob.provider_metadata
            .insert(AZURE_IMMUTABILITY_MODE_KEY.to_string(), mode.to_string());
        if !Self::replace_blob_metadata_if_unchanged(
            storage, container, blob_key, &observed, &blob,
        )? {
            return Ok(Self::condition_failed());
        }
        Ok(Self::response(StatusCode::OK)
            .header("x-ms-immutability-policy-until-date", until)
            .header("x-ms-immutability-policy-mode", mode)
            .empty())
    }

    fn delete_immutability_policy(
        storage: &Arc<dyn Storage>,
        req: &Request,
        container: &str,
        blob_key: &str,
    ) -> Result<Response<Body>, String> {
        let mut blob = match storage.as_ref().get_blob(container, blob_key) {
            Ok(blob) => blob,
            Err(crate::error::Error::KeyNotFound | crate::error::Error::BucketNotFound) => {
                return Ok(Self::blob_not_found())
            }
            Err(error) => return Err(error.to_string()),
        };
        if !Self::conditions_match_blob(req, &blob) {
            return Ok(Self::condition_failed());
        }
        if blob
            .provider_metadata
            .get(AZURE_IMMUTABILITY_MODE_KEY)
            .is_some_and(|mode| mode == "Locked")
        {
            return Ok(Self::error_response(
                StatusCode::CONFLICT,
                "BlobImmutableDueToPolicy",
                "A locked immutability policy cannot be deleted.",
            ));
        }
        let observed = blob.clone();
        blob.provider_metadata.remove(AZURE_IMMUTABILITY_UNTIL_KEY);
        blob.provider_metadata.remove(AZURE_IMMUTABILITY_MODE_KEY);
        if !Self::replace_blob_metadata_if_unchanged(
            storage, container, blob_key, &observed, &blob,
        )? {
            return Ok(Self::condition_failed());
        }
        Ok(Self::empty_response(StatusCode::OK))
    }

    fn put_legal_hold(
        storage: &Arc<dyn Storage>,
        req: &Request,
        container: &str,
        blob_key: &str,
    ) -> Result<Response<Body>, String> {
        let Some(legal_hold) = req.header("x-ms-legal-hold") else {
            return Ok(Self::error_response(
                StatusCode::BAD_REQUEST,
                "MissingRequiredHeader",
                "The x-ms-legal-hold header is required.",
            ));
        };
        let enabled = match legal_hold {
            "true" => true,
            "false" => false,
            _ => {
                return Ok(Self::error_response(
                    StatusCode::BAD_REQUEST,
                    "InvalidHeaderValue",
                    "The x-ms-legal-hold header must be true or false.",
                ))
            }
        };
        let mut blob = match storage.as_ref().get_blob(container, blob_key) {
            Ok(blob) => blob,
            Err(crate::error::Error::KeyNotFound | crate::error::Error::BucketNotFound) => {
                return Ok(Self::blob_not_found())
            }
            Err(error) => return Err(error.to_string()),
        };
        if !Self::conditions_match_blob(req, &blob) {
            return Ok(Self::condition_failed());
        }
        let observed = blob.clone();
        blob.provider_metadata
            .insert(AZURE_LEGAL_HOLD_KEY.to_string(), enabled.to_string());
        if !Self::replace_blob_metadata_if_unchanged(
            storage, container, blob_key, &observed, &blob,
        )? {
            return Ok(Self::condition_failed());
        }
        Ok(Self::response(StatusCode::OK)
            .header("x-ms-legal-hold", &enabled.to_string())
            .empty())
    }

    fn put_block(
        &self,
        storage: &Arc<dyn Storage>,
        req: &Request,
        account: &str,
        container: &str,
        blob_key: &str,
    ) -> Result<Response<Body>, String> {
        let Some(block_id) = req.query_param("blockid") else {
            return Ok(Self::error_response(
                StatusCode::BAD_REQUEST,
                "InvalidBlockId",
                "The specified block ID is invalid. The block ID must be Base64-encoded.",
            ));
        };
        let decoded_block_id = match BASE64.decode(block_id) {
            Ok(decoded) if !decoded.is_empty() && decoded.len() <= 64 => decoded,
            _ => {
                return Ok(Self::error_response(
                    StatusCode::BAD_REQUEST,
                    "InvalidBlockId",
                    "The specified block ID is invalid. The block ID must be Base64-encoded.",
                ))
            }
        };
        match storage.as_ref().get_blob(container, blob_key) {
            Ok(existing) => {
                if let Err(response) = Self::ensure_lease_allows(req, &existing) {
                    return Ok(response);
                }
                if !Self::conditions_match_blob(req, &existing) {
                    return Ok(Self::condition_failed());
                }
            }
            Err(crate::error::Error::KeyNotFound) => {
                if req.header("if-match").is_some() {
                    return Ok(Self::condition_failed());
                }
            }
            Err(crate::error::Error::BucketNotFound) => return Ok(Self::container_not_found()),
            Err(error) => return Err(error.to_string()),
        }
        let session_key = Self::blob_state_key(account, container, blob_key);
        let mut session = self
            .block_sessions
            .lock()
            .map_err(|_| "Failed to lock Azure block session state".to_string())?
            .get(&session_key)
            .cloned()
            .or(state::load_json(
                storage.as_ref(),
                AZURE_BLOCK_SESSION_STATE,
                &session_key,
            )?)
            .unwrap_or_default();
        if session.blocks.keys().any(|existing| {
            BASE64
                .decode(existing)
                .map_or(true, |decoded| decoded.len() != decoded_block_id.len())
        }) {
            return Ok(Self::error_response(
                StatusCode::BAD_REQUEST,
                "InvalidBlockId",
                "All block IDs for a blob must have the same decoded length.",
            ));
        }

        session
            .blocks
            .insert(block_id.to_string(), req.body.to_vec());
        state::save_json(
            storage.as_ref(),
            AZURE_BLOCK_SESSION_STATE,
            &session_key,
            &session,
        )?;
        self.block_sessions
            .lock()
            .map_err(|_| "Failed to lock Azure block session state".to_string())?
            .insert(session_key, session);

        Ok(Self::response(StatusCode::CREATED).empty())
    }

    // Resolution, conditional commit, and durable staged/committed state must remain one
    // auditable flow so no failed selector can partially consume the uncommitted list.
    #[allow(clippy::too_many_lines)]
    fn put_block_list(
        &self,
        storage: &Arc<dyn Storage>,
        req: &Request,
        account: &str,
        container: &str,
        blob_key: &str,
    ) -> Result<Response<Body>, String> {
        let Ok(xml) = std::str::from_utf8(&req.body) else {
            return Ok(Self::invalid_block_list_response(
                AzureBlockListError::InvalidXmlDocument,
            ));
        };
        let block_references = match Self::parse_block_list(xml) {
            Ok(block_references) => block_references,
            Err(error) => return Ok(Self::invalid_block_list_response(error)),
        };
        let mut expected_block_id_length = None;
        let mut duplicate_selectors = HashMap::new();
        for block in &block_references {
            let Ok(decoded) = BASE64.decode(&block.id) else {
                return Ok(Self::invalid_block_list_response(
                    AzureBlockListError::InvalidBlockList,
                ));
            };
            if decoded.is_empty()
                || expected_block_id_length.is_some_and(|length| length != decoded.len())
                || duplicate_selectors
                    .insert(block.id.as_str(), block.selector)
                    .is_some_and(|selector| selector != block.selector)
            {
                return Ok(Self::invalid_block_list_response(
                    AzureBlockListError::InvalidBlockList,
                ));
            }
            expected_block_id_length.get_or_insert(decoded.len());
        }
        let request_condition = match Self::write_condition(req) {
            Ok(condition) => condition,
            Err(response) => return Ok(response),
        };
        let (observed_condition, existing_blob) =
            match storage.as_ref().get_blob(container, blob_key) {
                Ok(existing) => {
                    let bucket = storage
                        .get_bucket(container)
                        .map_err(|error| error.to_string())?;
                    if let Err(response) = Self::ensure_version_creating_overwrite_allowed(
                        req,
                        &existing,
                        Self::azure_versioning_enabled(&bucket),
                    ) {
                        return Ok(response);
                    }
                    if !Self::conditions_match_blob(req, &existing) {
                        return Ok(Self::condition_failed());
                    }
                    (ObjectCondition::Etag(existing.etag.clone()), Some(existing))
                }
                Err(crate::error::Error::KeyNotFound) => {
                    if req.header("if-match").is_some() {
                        return Ok(Self::condition_failed());
                    }
                    (ObjectCondition::Missing, None)
                }
                Err(crate::error::Error::BucketNotFound) => return Ok(Self::container_not_found()),
                Err(error) => return Err(error.to_string()),
            };
        let session_key = Self::blob_state_key(account, container, blob_key);
        let mut session = self
            .load_block_session(storage, &session_key)?
            .unwrap_or_default();
        let committed = self.load_committed_blocks(storage, &session_key)?;
        let committed_by_id = committed
            .iter()
            .map(|block| (block.id.as_str(), block))
            .collect::<HashMap<_, _>>();
        let mut used_uncommitted = HashSet::new();
        let mut resolved_blocks = Vec::with_capacity(block_references.len());
        for reference in &block_references {
            let staged = session.blocks.get(&reference.id);
            let previously_committed = committed_by_id.get(reference.id.as_str()).copied();
            let (data, staged_was_used) = match reference.selector {
                AzureBlockSelector::Uncommitted => (staged, true),
                AzureBlockSelector::Committed => {
                    (previously_committed.map(|block| &block.data), false)
                }
                AzureBlockSelector::Latest => staged.map_or_else(
                    || (previously_committed.map(|block| &block.data), false),
                    |data| (Some(data), true),
                ),
            };
            let Some(data) = data else {
                return Ok(Self::error_response(
                    StatusCode::BAD_REQUEST,
                    "InvalidBlockList",
                    &format!("The block list contains unknown block ID {}.", reference.id),
                ));
            };
            if staged_was_used {
                used_uncommitted.insert(reference.id.clone());
            }
            resolved_blocks.push(AzureCommittedBlock {
                id: reference.id.clone(),
                data: data.clone(),
            });
        }
        let mut data = Vec::new();
        for block in &resolved_blocks {
            data.extend_from_slice(&block.data);
        }
        let mut object = crate::models::Object::new_with_metadata(
            blob_key.to_string(),
            data,
            Self::content_type(req),
            Self::metadata_from_headers(req),
        );
        Self::set_blob_type(&mut object, "BlockBlob");
        if let Some(existing) = existing_blob.as_ref() {
            Self::preserve_active_lease(existing, &mut object);
        }
        let condition = request_condition.unwrap_or(observed_condition);
        if !storage
            .put_object_if(container, blob_key.to_string(), object, &condition)
            .map_err(|error| error.to_string())?
        {
            return Ok(Self::condition_failed());
        }
        for block_id in used_uncommitted {
            session.blocks.remove(&block_id);
        }
        self.record_committed_block_session(storage, &session_key, &resolved_blocks, &session)?;
        let stored = storage
            .get_object(container, blob_key)
            .map_err(|error| error.to_string())?;
        let mut response = Self::response(StatusCode::CREATED)
            .header("etag", &format!("\"{}\"", stored.etag))
            .header(
                "last-modified",
                &crate::utils::headers::format_last_modified_at(&stored.last_modified),
            );
        if let Some(version_id) = stored.version_id.as_deref() {
            response = response.header("x-ms-version-id", version_id);
        }
        Ok(response.empty())
    }

    fn load_block_session(
        &self,
        storage: &Arc<dyn Storage>,
        session_key: &str,
    ) -> Result<Option<AzureBlockSession>, String> {
        Ok(self
            .block_sessions
            .lock()
            .map_err(|_| "Failed to lock Azure block session state".to_string())?
            .get(session_key)
            .cloned()
            .or(state::load_json(
                storage.as_ref(),
                AZURE_BLOCK_SESSION_STATE,
                session_key,
            )?))
    }

    fn load_committed_blocks(
        &self,
        storage: &Arc<dyn Storage>,
        session_key: &str,
    ) -> Result<Vec<AzureCommittedBlock>, String> {
        if let Some(blocks) = self
            .committed_blocks
            .lock()
            .map_err(|_| "Failed to lock Azure committed block state".to_string())?
            .get(session_key)
            .cloned()
        {
            return Ok(blocks);
        }
        let blocks: Vec<AzureCommittedBlock> =
            state::load_json(storage.as_ref(), AZURE_COMMITTED_BLOCKS_STATE, session_key)?
                .unwrap_or_default();
        self.committed_blocks
            .lock()
            .map_err(|_| "Failed to lock Azure committed block state".to_string())?
            .insert(session_key.to_string(), blocks.clone());
        Ok(blocks)
    }

    fn remove_block_session(&self, session_key: &str) -> Result<(), String> {
        self.block_sessions
            .lock()
            .map_err(|_| "Failed to lock Azure block session state".to_string())?
            .remove(session_key);
        Ok(())
    }

    fn record_committed_block_session(
        &self,
        storage: &Arc<dyn Storage>,
        session_key: &str,
        blocks: &[AzureCommittedBlock],
        remaining_session: &AzureBlockSession,
    ) -> Result<(), String> {
        self.committed_blocks
            .lock()
            .map_err(|_| "Failed to lock Azure committed block state".to_string())?
            .insert(session_key.to_string(), blocks.to_vec());
        state::save_json(
            storage.as_ref(),
            AZURE_COMMITTED_BLOCKS_STATE,
            session_key,
            &blocks,
        )?;
        if remaining_session.blocks.is_empty() {
            self.remove_block_session(session_key)?;
            storage
                .delete_provider_state(AZURE_BLOCK_SESSION_STATE, session_key)
                .map_err(|err| err.to_string())
        } else {
            state::save_json(
                storage.as_ref(),
                AZURE_BLOCK_SESSION_STATE,
                session_key,
                remaining_session,
            )?;
            self.block_sessions
                .lock()
                .map_err(|_| "Failed to lock Azure block session state".to_string())?
                .insert(session_key.to_string(), remaining_session.clone());
            Ok(())
        }
    }

    fn update_metadata(
        storage: &Arc<dyn Storage>,
        req: &Request,
        container: &str,
        blob_key: &str,
    ) -> Result<Response<Body>, String> {
        let existing = match storage.as_ref().get_blob(container, blob_key) {
            Ok(existing) => existing,
            Err(crate::error::Error::KeyNotFound | crate::error::Error::BucketNotFound) => {
                return Ok(Self::error_response(
                    StatusCode::NOT_FOUND,
                    "BlobNotFound",
                    "The specified blob does not exist.",
                ))
            }
            Err(error) => return Err(error.to_string()),
        };
        if let Err(response) = Self::ensure_mutation_allowed(req, &existing) {
            return Ok(response);
        }
        if !Self::conditions_match_blob(req, &existing) {
            return Ok(Self::condition_failed());
        }
        let observed_etag = existing.etag.clone();
        let mut updated = existing;
        updated.metadata = Self::metadata_from_headers(req);
        updated.etag = uuid::Uuid::new_v4().simple().to_string();
        updated.last_modified = Utc::now();
        if !storage
            .put_object_if(
                container,
                blob_key.to_string(),
                updated,
                &ObjectCondition::Etag(observed_etag),
            )
            .map_err(|error| error.to_string())?
        {
            return Ok(Self::condition_failed());
        }
        let stored = storage
            .get_object(container, blob_key)
            .map_err(|error| error.to_string())?;
        Ok(Self::response(StatusCode::OK)
            .header("etag", &format!("\"{}\"", stored.etag))
            .header(
                "last-modified",
                &crate::utils::headers::format_last_modified_at(&stored.last_modified),
            )
            .empty())
    }

    fn get_block_list(
        &self,
        storage: &Arc<dyn Storage>,
        req: &Request,
        account: &str,
        container: &str,
        blob_key: &str,
    ) -> Result<Response<Body>, String> {
        if req.query_param("snapshot").is_some() || req.query_param("versionid").is_some() {
            return Ok(Self::error_response(
                StatusCode::NOT_IMPLEMENTED,
                "FeatureNotSupported",
                "Snapshot and version-scoped block lists are not implemented by the emulator.",
            ));
        }
        let selector = req.query_param("blocklisttype").unwrap_or("committed");
        if !matches!(selector, "committed" | "uncommitted" | "all") {
            return Ok(Self::error_response(
                StatusCode::BAD_REQUEST,
                "InvalidQueryParameterValue",
                "The value for blocklisttype is invalid.",
            ));
        }
        let session_key = Self::blob_state_key(account, container, blob_key);
        let committed = self.load_committed_blocks(storage, &session_key)?;
        let session = self.load_block_session(storage, &session_key)?;
        let existing = match storage.as_ref().get_blob(container, blob_key) {
            Ok(existing) => Some(existing),
            Err(crate::error::Error::KeyNotFound) => None,
            Err(crate::error::Error::BucketNotFound) => return Ok(Self::container_not_found()),
            Err(error) => return Err(error.to_string()),
        };
        if let Some(existing) = &existing {
            if Self::blob_type(existing) != "BlockBlob" {
                return Ok(Self::error_response(
                    StatusCode::BAD_REQUEST,
                    "InvalidBlobType",
                    "The operation is not supported for this blob type.",
                ));
            }
            if !Self::conditions_match_blob(req, existing) {
                return Ok(Self::condition_failed());
            }
            if let Err(response) = Self::ensure_lease_allows(req, existing) {
                return Ok(response);
            }
        } else if selector == "committed" || session.is_none() {
            return Ok(Self::blob_not_found());
        }
        let include_committed = matches!(selector, "committed" | "all");
        let include_uncommitted = matches!(selector, "uncommitted" | "all");
        let has_committed = !committed.is_empty();
        let committed = if include_committed {
            committed
        } else {
            Vec::new()
        };
        let mut uncommitted = if include_uncommitted {
            session
                .map(|session| session.blocks.into_iter().collect::<Vec<_>>())
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        uncommitted.sort_by(|left, right| left.0.cmp(&right.0));
        let body = Self::block_list_xml(
            &committed,
            &uncommitted,
            include_committed,
            include_uncommitted,
        );
        let response = Self::response(StatusCode::OK)
            .content_type("application/xml")
            .header(
                "x-ms-blob-content-length",
                &existing.as_ref().map_or(0, |blob| blob.size).to_string(),
            );
        let response = if has_committed {
            if let Some(blob) = &existing {
                response
                    .header("etag", &format!("\"{}\"", blob.etag))
                    .header(
                        "last-modified",
                        &crate::utils::headers::format_last_modified_at(&blob.last_modified),
                    )
            } else {
                response
            }
        } else {
            response
        };
        Ok(response.body(body.into_bytes()).build())
    }

    fn append_block(
        storage: &Arc<dyn Storage>,
        req: &Request,
        container: &str,
        blob_key: &str,
    ) -> Result<Response<Body>, String> {
        let mut blob = match storage.as_ref().get_blob(container, blob_key) {
            Ok(blob) => blob,
            Err(crate::error::Error::KeyNotFound | crate::error::Error::BucketNotFound) => {
                return Ok(Self::error_response(
                    StatusCode::NOT_FOUND,
                    "BlobNotFound",
                    "The specified blob does not exist.",
                ))
            }
            Err(error) => return Err(error.to_string()),
        };
        if let Err(response) = Self::ensure_mutation_allowed(req, &blob) {
            return Ok(response);
        }
        if !Self::conditions_match_blob(req, &blob) {
            return Ok(Self::condition_failed());
        }
        if Self::blob_type(&blob) != "AppendBlob" {
            return Ok(Self::error_response(
                StatusCode::CONFLICT,
                "InvalidBlobType",
                "The blob type is invalid for this operation.",
            ));
        }

        let observed_etag = blob.etag.clone();
        blob.data.extend_from_slice(&req.body);
        blob.size = blob.data.len() as u64;
        blob.etag = crate::models::object::compute_etag(&blob.data);
        blob.last_modified = Utc::now();
        if !storage
            .put_object_if(
                container,
                blob_key.to_string(),
                blob,
                &ObjectCondition::Etag(observed_etag),
            )
            .map_err(|error| error.to_string())?
        {
            return Ok(Self::condition_failed());
        }

        let stored = storage
            .get_object(container, blob_key)
            .map_err(|err| err.to_string())?;
        Ok(Self::response(StatusCode::CREATED)
            .header("etag", &format!("\"{}\"", stored.etag))
            .header(
                "x-ms-blob-append-offset",
                &(stored.size - req.body.len() as u64).to_string(),
            )
            .header("x-ms-blob-committed-block-count", "1")
            .empty())
    }

    fn put_page(
        storage: &Arc<dyn Storage>,
        req: &Request,
        container: &str,
        blob_key: &str,
    ) -> Result<Response<Body>, String> {
        let page_write = match req.header("x-ms-page-write") {
            Some("update") => "update",
            Some("clear") => "clear",
            Some(_) => {
                return Ok(Self::error_response(
                    StatusCode::BAD_REQUEST,
                    "InvalidHeaderValue",
                    "The x-ms-page-write header must be update or clear.",
                ))
            }
            None => {
                return Ok(Self::error_response(
                    StatusCode::BAD_REQUEST,
                    "MissingRequiredHeader",
                    "The x-ms-page-write header is required.",
                ))
            }
        };
        let Some(range_header) = Self::requested_range(req) else {
            return Ok(Self::page_range_error());
        };
        let Some((start, end)) = Self::parse_write_range_header(range_header) else {
            return Ok(Self::page_range_error());
        };
        if start % 512 != 0 || (end + 1) % 512 != 0 {
            return Ok(Self::error_response(
                StatusCode::BAD_REQUEST,
                "InvalidPageRange",
                "Page blob ranges must align to 512-byte boundaries.",
            ));
        }

        let mut blob = match storage.as_ref().get_blob(container, blob_key) {
            Ok(blob) => blob,
            Err(crate::error::Error::KeyNotFound | crate::error::Error::BucketNotFound) => {
                return Ok(Self::error_response(
                    StatusCode::NOT_FOUND,
                    "BlobNotFound",
                    "The specified blob does not exist.",
                ))
            }
            Err(error) => return Err(error.to_string()),
        };
        if let Err(response) = Self::ensure_mutation_allowed(req, &blob) {
            return Ok(response);
        }
        if !Self::conditions_match_blob(req, &blob) {
            return Ok(Self::condition_failed());
        }
        if Self::blob_type(&blob) != "PageBlob" {
            return Ok(Self::error_response(
                StatusCode::CONFLICT,
                "InvalidBlobType",
                "The blob type is invalid for this operation.",
            ));
        }

        if let Some(response) = Self::validate_page_write(req, &blob, start, end, page_write) {
            return Ok(response);
        }

        let observed_etag = blob.etag.clone();
        if page_write == "clear" {
            blob.data[start..=end].fill(0);
        } else {
            blob.data[start..=end].copy_from_slice(&req.body);
        }
        blob.etag = crate::models::object::compute_etag(&blob.data);
        blob.last_modified = Utc::now();
        if !storage
            .put_object_if(
                container,
                blob_key.to_string(),
                blob,
                &ObjectCondition::Etag(observed_etag),
            )
            .map_err(|error| error.to_string())?
        {
            return Ok(Self::condition_failed());
        }
        let stored = storage
            .get_object(container, blob_key)
            .map_err(|err| err.to_string())?;
        Ok(Self::response(StatusCode::CREATED)
            .header("etag", &format!("\"{}\"", stored.etag))
            .header(
                "last-modified",
                &crate::utils::headers::format_last_modified_at(&stored.last_modified),
            )
            .header("x-ms-blob-sequence-number", "0")
            .empty())
    }

    fn page_range_error() -> Response<Body> {
        Self::error_response(
            StatusCode::BAD_REQUEST,
            "InvalidHeaderValue",
            "Page writes require a valid x-ms-range header.",
        )
    }

    fn validate_page_write(
        req: &Request,
        blob: &crate::models::Object,
        start: usize,
        end: usize,
        page_write: &str,
    ) -> Option<Response<Body>> {
        let expected_len = end - start + 1;
        if (page_write == "update" && req.body.len() != expected_len)
            || (page_write == "clear" && !req.body.is_empty())
        {
            return Some(Self::error_response(
                StatusCode::BAD_REQUEST,
                "InvalidPageRange",
                "Page payload length must match the requested range.",
            ));
        }
        if end >= blob.data.len() {
            return Some(Self::error_response(
                StatusCode::BAD_REQUEST,
                "InvalidPageRange",
                "Page write exceeds the blob length.",
            ));
        }
        None
    }

    fn handle_blob_crud(
        storage: &Arc<dyn Storage>,
        req: &Request,
        container: &str,
        blob_key: &str,
    ) -> Result<Response<Body>, String> {
        let snapshot = Self::snapshot_query(req);
        if snapshot.is_some() && req.query_param("versionid").is_some() {
            return Ok(Self::error_response(
                StatusCode::BAD_REQUEST,
                "InvalidQueryParameterValue",
                "The snapshot and versionid parameters cannot be specified together.",
            ));
        }
        match *req.method() {
            Method::PUT => Self::put_blob(storage, req, container, blob_key, snapshot.as_deref()),
            Method::GET => Self::get_blob(storage, req, container, blob_key, snapshot.as_deref()),
            Method::HEAD => Self::head_blob(storage, req, container, blob_key, snapshot.as_deref()),
            Method::DELETE => {
                Self::delete_blob(storage, req, container, blob_key, snapshot.as_deref())
            }
            _ => Ok(Self::error_response(
                StatusCode::METHOD_NOT_ALLOWED,
                "UnsupportedHttpVerb",
                "Unsupported Azure blob operation",
            )),
        }
    }

    fn put_blob(
        storage: &Arc<dyn Storage>,
        req: &Request,
        container: &str,
        blob_key: &str,
        snapshot: Option<&str>,
    ) -> Result<Response<Body>, String> {
        if snapshot.is_some() {
            return Ok(Self::error_response(
                StatusCode::BAD_REQUEST,
                "InvalidQueryParameterValue",
                "Snapshots are read-only.",
            ));
        }
        let existing_blob = match storage.as_ref().get_blob(container, blob_key) {
            Ok(existing) => {
                let bucket = storage
                    .get_bucket(container)
                    .map_err(|error| error.to_string())?;
                if let Err(response) = Self::ensure_version_creating_overwrite_allowed(
                    req,
                    &existing,
                    Self::azure_versioning_enabled(&bucket),
                ) {
                    return Ok(response);
                }
                Some(existing)
            }
            Err(crate::error::Error::KeyNotFound) => {
                if req.header("if-match").is_some() {
                    return Ok(Self::condition_failed());
                }
                None
            }
            Err(crate::error::Error::BucketNotFound) => return Ok(Self::container_not_found()),
            Err(error) => return Err(error.to_string()),
        };
        if let Some(response) = Self::validate_blob_create_request(req) {
            return Ok(response);
        }
        let mut object = Self::blob_for_type(req, blob_key);
        if let Some(existing) = existing_blob.as_ref() {
            Self::preserve_active_lease(existing, &mut object);
        }
        let condition = match Self::write_condition(req) {
            Ok(condition) => condition,
            Err(response) => return Ok(response),
        };
        let written = if let Some(condition) = condition {
            storage
                .put_object_if(container, blob_key.to_string(), object, &condition)
                .map_err(|err| err.to_string())?
        } else {
            storage
                .put_object(container, blob_key.to_string(), object)
                .map_err(|err| err.to_string())?;
            true
        };
        if !written {
            return Ok(Self::condition_failed());
        }
        let stored = storage
            .get_object(container, blob_key)
            .map_err(|err| err.to_string())?;
        let mut response = Self::response(StatusCode::CREATED)
            .header("etag", &format!("\"{}\"", stored.etag))
            .header(
                "last-modified",
                &crate::utils::headers::format_last_modified_at(&stored.last_modified),
            )
            .header("x-ms-blob-type", Self::blob_type(&stored));
        if let Some(version_id) = stored.version_id.as_deref() {
            response = response.header("x-ms-version-id", version_id);
        }
        Ok(response.empty())
    }

    fn blob_for_type(req: &Request, blob_key: &str) -> crate::models::Object {
        let blob_type = req
            .header("x-ms-blob-type")
            .expect("blob type is validated before object construction");
        let mut object = crate::models::Object::new_with_metadata(
            blob_key.to_string(),
            if blob_type == "PageBlob" {
                vec![0_u8; Self::page_blob_declared_len(req)]
            } else {
                req.body.to_vec()
            },
            Self::content_type(req),
            Self::metadata_from_headers(req),
        );
        Self::set_blob_type(&mut object, blob_type);
        object
    }

    fn validate_blob_create_request(req: &Request) -> Option<Response<Body>> {
        let Some(blob_type) = req.header("x-ms-blob-type") else {
            return Some(Self::error_response(
                StatusCode::BAD_REQUEST,
                "MissingRequiredHeader",
                "The x-ms-blob-type header is required.",
            ));
        };
        if !matches!(blob_type, "BlockBlob" | "PageBlob" | "AppendBlob") {
            return Some(Self::error_response(
                StatusCode::BAD_REQUEST,
                "InvalidHeaderValue",
                "The x-ms-blob-type header value is invalid.",
            ));
        }
        if matches!(blob_type, "PageBlob" | "AppendBlob") && !req.body.is_empty() {
            return Some(Self::error_response(
                StatusCode::BAD_REQUEST,
                "InvalidHeaderValue",
                "Page and append blob creation requests must have an empty body.",
            ));
        }
        if blob_type != "PageBlob" {
            if req.header("x-ms-blob-content-length").is_some() {
                return Some(Self::error_response(
                    StatusCode::BAD_REQUEST,
                    "InvalidHeaderValue",
                    "The x-ms-blob-content-length header is valid only for page blobs.",
                ));
            }
            return None;
        }
        let Some(raw_length) = req.header("x-ms-blob-content-length") else {
            return Some(Self::error_response(
                StatusCode::BAD_REQUEST,
                "MissingRequiredHeader",
                "The x-ms-blob-content-length header is required for page blobs.",
            ));
        };
        let Ok(length) = raw_length.parse::<usize>() else {
            return Some(Self::error_response(
                StatusCode::BAD_REQUEST,
                "InvalidHeaderValue",
                "The x-ms-blob-content-length header value is invalid.",
            ));
        };
        (!length.is_multiple_of(512)).then(|| {
            Self::error_response(
                StatusCode::BAD_REQUEST,
                "InvalidHeaderValue",
                "Page blob length must be a multiple of 512 bytes.",
            )
        })
    }

    fn page_blob_declared_len(req: &Request) -> usize {
        req.header("x-ms-blob-content-length")
            .and_then(|value| value.parse::<usize>().ok())
            .unwrap_or_default()
    }

    fn get_blob(
        storage: &Arc<dyn Storage>,
        req: &Request,
        container: &str,
        blob_key: &str,
        snapshot: Option<&str>,
    ) -> Result<Response<Body>, String> {
        let blob = match Self::lookup_blob(
            storage,
            container,
            blob_key,
            snapshot,
            req.query_param("versionid"),
        ) {
            Ok(blob) => blob,
            Err(
                crate::error::Error::KeyNotFound
                | crate::error::Error::NoSuchVersion
                | crate::error::Error::BucketNotFound,
            ) => {
                return Ok(Self::error_response(
                    StatusCode::NOT_FOUND,
                    "BlobNotFound",
                    "The specified blob does not exist.",
                ));
            }
            Err(err) => return Err(err.to_string()),
        };
        if let Some(range_header) = Self::requested_range(req) {
            return Self::get_blob_range(
                storage,
                container,
                blob_key,
                snapshot,
                req.query_param("versionid"),
                &blob,
                range_header,
            );
        }
        let body_len = Self::response_body_len(blob.size)?;
        let expose_version_id = Self::azure_history_visible(storage, container);
        let is_current_version = expose_version_id
            .then(|| Self::is_current_version(storage, container, blob_key, &blob))
            .flatten();
        Ok(Self::blob_response(
            StatusCode::OK,
            &blob,
            body_len,
            None,
            is_current_version,
            expose_version_id,
        )
        .body(blob.data)
        .build())
    }

    fn get_blob_range(
        storage: &Arc<dyn Storage>,
        container: &str,
        blob_key: &str,
        snapshot: Option<&str>,
        version_id: Option<&str>,
        blob: &crate::models::Object,
        range_header: &str,
    ) -> Result<Response<Body>, String> {
        if let Some((start, end)) = Self::parse_range_header(range_header, blob.size) {
            let payload = if snapshot.is_some() || version_id.is_some() {
                let data = blob.data[start..=end].to_vec();
                crate::blob::BlobPayload {
                    blob: blob.clone(),
                    data,
                }
            } else {
                storage
                    .as_ref()
                    .get_blob_range(
                        container,
                        blob_key,
                        BlobRange {
                            start: start as u64,
                            end: end as u64,
                        },
                    )
                    .map_err(|err| err.to_string())?
            };
            return Ok(Self::blob_response(
                StatusCode::PARTIAL_CONTENT,
                &payload.blob,
                payload.data.len(),
                Some(format!("bytes {start}-{end}/{}", blob.size)),
                Self::azure_history_visible(storage, container)
                    .then(|| Self::is_current_version(storage, container, blob_key, blob))
                    .flatten(),
                Self::azure_history_visible(storage, container),
            )
            .body(payload.data)
            .build());
        }
        Ok(Self::error_response(
            StatusCode::RANGE_NOT_SATISFIABLE,
            "InvalidRange",
            "The requested range is not satisfiable.",
        ))
    }

    fn head_blob(
        storage: &Arc<dyn Storage>,
        req: &Request,
        container: &str,
        blob_key: &str,
        snapshot: Option<&str>,
    ) -> Result<Response<Body>, String> {
        let blob = match Self::lookup_blob(
            storage,
            container,
            blob_key,
            snapshot,
            req.query_param("versionid"),
        ) {
            Ok(blob) => blob,
            Err(
                crate::error::Error::KeyNotFound
                | crate::error::Error::NoSuchVersion
                | crate::error::Error::BucketNotFound,
            ) => {
                return Ok(Self::response(StatusCode::NOT_FOUND)
                    .header("x-ms-error-code", "BlobNotFound")
                    .empty());
            }
            Err(err) => return Err(err.to_string()),
        };
        let body_len = Self::response_body_len(blob.size)?;
        let expose_version_id = Self::azure_history_visible(storage, container);
        let is_current_version = expose_version_id
            .then(|| Self::is_current_version(storage, container, blob_key, &blob))
            .flatten();
        Ok(Self::blob_response(
            StatusCode::OK,
            &blob,
            body_len,
            None,
            is_current_version,
            expose_version_id,
        )
        .empty())
    }

    fn is_current_version(
        storage: &Arc<dyn Storage>,
        container: &str,
        blob_key: &str,
        selected: &crate::models::Object,
    ) -> Option<bool> {
        let selected_version = selected.version_id.as_deref()?;
        Some(
            storage
                .get_object(container, blob_key)
                .ok()
                .and_then(|current| current.version_id)
                .is_some_and(|current| current == selected_version),
        )
    }

    // The provider contract has three distinct deletion resources (version,
    // snapshot, and current blob); keeping their ordered validation visible
    // here makes the no-mutation failure boundary auditable.
    #[allow(clippy::too_many_lines)]
    fn delete_blob(
        storage: &Arc<dyn Storage>,
        req: &Request,
        container: &str,
        blob_key: &str,
        snapshot: Option<&str>,
    ) -> Result<Response<Body>, String> {
        if let Some(version_id) = req.query_param("versionid") {
            let version = match storage.get_object_version(container, blob_key, version_id) {
                Ok(version) => version,
                Err(
                    crate::error::Error::NoSuchVersion
                    | crate::error::Error::KeyNotFound
                    | crate::error::Error::BucketNotFound,
                ) => {
                    return Ok(Self::error_response(
                        StatusCode::NOT_FOUND,
                        "BlobNotFound",
                        "The specified blob version does not exist.",
                    ))
                }
                Err(error) => return Err(error.to_string()),
            };
            if let Err(response) = Self::ensure_mutation_allowed(req, &version) {
                return Ok(response);
            }
            if !Self::conditions_match_blob(req, &version) {
                return Ok(Self::condition_failed());
            }
            if let Err(error) = storage.delete_object_version(container, blob_key, version_id) {
                if matches!(error, crate::error::Error::NoSuchVersion) {
                    return Ok(Self::error_response(
                        StatusCode::NOT_FOUND,
                        "BlobNotFound",
                        "The specified blob version does not exist.",
                    ));
                }
                return Err(error.to_string());
            }
            return Ok(Self::empty_response(StatusCode::ACCEPTED));
        }
        if let Some(snapshot) = snapshot {
            let snapshot_key = Self::snapshot_storage_key(blob_key, snapshot);
            let selected = match storage.as_ref().get_blob(container, &snapshot_key) {
                Ok(selected) => selected,
                Err(crate::error::Error::KeyNotFound | crate::error::Error::BucketNotFound) => {
                    return Ok(Self::error_response(
                        StatusCode::NOT_FOUND,
                        "BlobNotFound",
                        "The specified blob snapshot does not exist.",
                    ))
                }
                Err(error) => return Err(error.to_string()),
            };
            if let Err(response) = Self::ensure_mutation_allowed(req, &selected) {
                return Ok(response);
            }
            if !Self::conditions_match_blob(req, &selected) {
                return Ok(Self::condition_failed());
            }
            if !storage
                .delete_object_if(
                    container,
                    &snapshot_key,
                    &ObjectCondition::Etag(selected.etag),
                )
                .map_err(|error| error.to_string())?
            {
                return Ok(Self::condition_failed());
            }
            return Ok(Self::empty_response(StatusCode::ACCEPTED));
        }
        let blob = match storage.as_ref().get_blob(container, blob_key) {
            Ok(blob) => blob,
            Err(crate::error::Error::KeyNotFound | crate::error::Error::BucketNotFound) => {
                return Ok(Self::error_response(
                    StatusCode::NOT_FOUND,
                    "BlobNotFound",
                    "The specified blob does not exist.",
                ));
            }
            Err(err) => return Err(err.to_string()),
        };
        if let Err(response) = Self::ensure_mutation_allowed(req, &blob) {
            return Ok(response);
        }
        let snapshots = Self::snapshot_keys(storage, container, blob_key)?;
        let snapshot_mode = req.header("x-ms-delete-snapshots");
        if !snapshots.is_empty() && snapshot_mode.is_none() {
            return Ok(Self::error_response(
                StatusCode::CONFLICT,
                "SnapshotsPresent",
                "This operation is not permitted because the blob has snapshots.",
            ));
        }
        if snapshot_mode.is_some_and(|mode| !matches!(mode, "include" | "only")) {
            return Ok(Self::error_response(
                StatusCode::BAD_REQUEST,
                "InvalidHeaderValue",
                "The x-ms-delete-snapshots header must be include or only.",
            ));
        }
        if snapshot_mode == Some("only") {
            if !Self::conditions_match_blob(req, &blob) {
                return Ok(Self::condition_failed());
            }
            Self::delete_snapshots(storage, container, &snapshots)?;
            return Ok(Self::empty_response(StatusCode::ACCEPTED));
        }
        let condition = match Self::write_condition(req) {
            Ok(condition) => condition,
            Err(response) => return Ok(response),
        };
        if let Some(condition) = condition {
            if !storage
                .delete_object_if(container, blob_key, &condition)
                .map_err(|err| err.to_string())?
            {
                return Ok(Self::condition_failed());
            }
        } else {
            storage
                .as_ref()
                .delete_blob(container, blob_key)
                .map_err(|err| err.to_string())?;
        }
        Self::delete_snapshots(storage, container, &snapshots)?;
        Ok(Self::empty_response(StatusCode::ACCEPTED))
    }
}

fn sign_hmac_base64(key: &[u8], payload: &str) -> Result<String, String> {
    type HmacSha256 = Hmac<Sha256>;
    let mut mac = HmacSha256::new_from_slice(key)
        .map_err(|err| format!("Invalid Azure signing key: {err}"))?;
    mac.update(payload.as_bytes());
    Ok(BASE64.encode(mac.finalize().into_bytes()))
}

fn escape_xml(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
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
        let dir = std::env::temp_dir().join(format!("sqrzl-azure-test-{}", uuid::Uuid::new_v4()));
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

    fn azure_auth() -> Arc<AuthConfig> {
        Arc::new(Config {
            access_key_id: Some("devstoreaccount1".to_string()),
            secret_access_key: Some(BASE64.encode("topsecretkey")),
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

    fn signed_headers(req: &Request, config: &AuthConfig, account: &str) -> String {
        let string_to_sign = AzureBlobAdapter::shared_key_string_to_sign(req, account);
        let key = AzureBlobAdapter::shared_key_secret(config).expect("key should exist");
        format!(
            "SharedKey {}:{}",
            account,
            sign_hmac_base64(&key, &string_to_sign).expect("signature should build")
        )
    }

    #[tokio::test]
    async fn should_concatenate_azure_canonical_headers_and_resource_without_blank_line() {
        let request = parsed_request(
            "GET",
            "http://localhost/devstoreaccount1/container/blob?comp=metadata&foo=b&foo=a",
            &[
                ("x-ms-date", "Tue, 11 Aug 2026 12:00:00 +0000"),
                ("x-ms-meta-test", "one"),
                ("x-ms-meta-test", "two"),
                ("x-ms-version", AZURE_VERSION),
            ],
            b"",
        )
        .await;

        let actual = AzureBlobAdapter::shared_key_string_to_sign(&request, "devstoreaccount1");
        let expected = concat!(
            "GET\n\n\n\n\n\n\n\n\n\n\n\n",
            "x-ms-date:Tue, 11 Aug 2026 12:00:00 +0000\n",
            "x-ms-meta-test:one,two\n",
            "x-ms-version:2023-11-03\n",
            "/devstoreaccount1/devstoreaccount1/container/blob\n",
            "comp:metadata\n",
            "foo:a,b"
        );

        assert_eq!(actual, expected);
        assert!(!actual.contains("2023-11-03\n\n/devstoreaccount1"));

        let date_only_request = parsed_request(
            "GET",
            "http://localhost/devstoreaccount1/container/blob",
            &[("date", "Tue, 11 Aug 2026 12:00:00 +0000")],
            b"",
        )
        .await;
        let date_only =
            AzureBlobAdapter::shared_key_string_to_sign(&date_only_request, "devstoreaccount1");
        assert!(date_only.contains("\nTue, 11 Aug 2026 12:00:00 +0000\n"));
    }

    #[test]
    fn should_apply_azure_container_naming_rules() {
        // Arrange
        let valid_names = ["abc", "container-01", "$root"];
        let invalid_names = ["aa", "Upper", "bad--name", "-leading", "trailing-"];

        // Act
        let valid_results = valid_names.map(AzureBlobAdapter::valid_container_name);
        let invalid_results = invalid_names.map(AzureBlobAdapter::valid_container_name);

        // Assert
        assert_eq!(valid_results, [true; 3]);
        assert_eq!(invalid_results, [false; 5]);
    }

    #[tokio::test]
    async fn should_reject_invalid_azure_container_name_without_mutation() {
        let adapter = AzureBlobAdapter::new();
        let storage = temp_storage();
        let request = parsed_request(
            "PUT",
            "http://localhost/devstoreaccount1/AA?restype=container",
            &[("x-ms-version", AZURE_VERSION)],
            b"",
        )
        .await;

        let response = adapter
            .handle_request(&storage, &auth_disabled(), &request)
            .expect("invalid container request should complete");

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert!(storage.get_namespace("AA").is_err());
    }

    fn sas_signature(
        resource: &str,
        config: &AuthConfig,
        permissions: &str,
        expires: &str,
    ) -> String {
        let key = AzureBlobAdapter::shared_key_secret(config).expect("key should exist");
        let payload = AzureBlobAdapter::sas_string_to_sign(
            resource,
            permissions,
            "",
            expires,
            "2023-11-03",
            "b",
        );
        sign_hmac_base64(&key, &payload).expect("signature should build")
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn should_create_list_and_fetch_azure_blobs() {
        let adapter = AzureBlobAdapter::new();
        let storage = temp_storage();

        let response = adapter
            .handle_request(
                &storage.clone(),
                &auth_disabled(),
                &parsed_request(
                    "PUT",
                    "http://localhost/devstoreaccount1/photos?restype=container",
                    &[("x-ms-version", AZURE_VERSION)],
                    b"",
                )
                .await,
            )
            .expect("container create should succeed");
        assert_eq!(response.status(), StatusCode::CREATED);

        let response = adapter
            .handle_request(
                &storage.clone(),
                &auth_disabled(),
                &parsed_request(
                    "PUT",
                    "http://localhost/devstoreaccount1/photos/kitten.txt",
                    &[
                        ("x-ms-version", AZURE_VERSION),
                        ("x-ms-blob-type", "BlockBlob"),
                        ("x-ms-meta-owner", "alice"),
                        ("content-type", "text/plain"),
                    ],
                    b"hello azure",
                )
                .await,
            )
            .expect("put blob should succeed");
        assert_eq!(response.status(), StatusCode::CREATED);

        let response = adapter
            .handle_request(
                &storage.clone(),
                &auth_disabled(),
                &parsed_request(
                    "GET",
                    "http://localhost/devstoreaccount1/photos?restype=container&comp=list",
                    &[("x-ms-version", AZURE_VERSION)],
                    b"",
                )
                .await,
            )
            .expect("list blobs should succeed");
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
                    "http://localhost/devstoreaccount1/photos/kitten.txt",
                    &[("x-ms-version", AZURE_VERSION)],
                    b"",
                )
                .await,
            )
            .expect("get blob should succeed");
        let body = response
            .into_body()
            .collect()
            .await
            .expect("body should read")
            .to_bytes();
        assert_eq!(body.as_ref(), b"hello azure");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn should_commit_block_blob_from_put_block_list() {
        let adapter = AzureBlobAdapter::new();
        let storage = temp_storage();

        adapter
            .handle_request(
                &storage.clone(),
                &auth_disabled(),
                &parsed_request(
                    "PUT",
                    "http://localhost/devstoreaccount1/archive?restype=container",
                    &[("x-ms-version", AZURE_VERSION)],
                    b"",
                )
                .await,
            )
            .expect("container create should succeed");

        let block_one = BASE64.encode("block-001");
        let block_two = BASE64.encode("block-002");
        for (block_id, payload) in [
            (&block_one, b"abc".as_slice()),
            (&block_two, b"def".as_slice()),
        ] {
            let response = adapter
                .handle_request(
                    &storage.clone(),
                    &auth_disabled(),
                    &parsed_request(
                        "PUT",
                        &format!(
                            "http://localhost/devstoreaccount1/archive/report.txt?comp=block&blockid={}",
                            urlencoding::encode(block_id)
                        ),
                        &[("x-ms-version", AZURE_VERSION)],
                        payload,
                    )
                    .await,)
                .expect("put block should succeed");
            assert_eq!(response.status(), StatusCode::CREATED);
        }

        let block_list = format!(
            "<?xml version=\"1.0\" encoding=\"utf-8\"?><BlockList><Latest>{block_one}</Latest><Latest>{block_two}</Latest></BlockList>"
        );
        adapter
            .handle_request(
                &storage.clone(),
                &auth_disabled(),
                &parsed_request(
                    "PUT",
                    "http://localhost/devstoreaccount1/archive/report.txt?comp=blocklist",
                    &[
                        ("x-ms-version", AZURE_VERSION),
                        ("content-type", "application/xml"),
                    ],
                    block_list.as_bytes(),
                )
                .await,
            )
            .expect("put block list should succeed");

        let response = adapter
            .handle_request(
                &storage,
                &auth_disabled(),
                &parsed_request(
                    "GET",
                    "http://localhost/devstoreaccount1/archive/report.txt",
                    &[("x-ms-version", AZURE_VERSION)],
                    b"",
                )
                .await,
            )
            .expect("get blob should succeed");
        let body = response
            .into_body()
            .collect()
            .await
            .expect("body should read")
            .to_bytes();
        assert_eq!(body.as_ref(), b"abcdef");
    }

    #[tokio::test(flavor = "multi_thread")]
    #[allow(clippy::too_many_lines)]
    async fn should_preserve_azure_committed_and_uncommitted_block_selectors() {
        // Arrange
        let adapter = AzureBlobAdapter::new();
        let storage = temp_storage();
        let container_uri = "http://localhost/devstoreaccount1/selectors?restype=container";
        let blob_uri = "http://localhost/devstoreaccount1/selectors/report.bin";
        adapter
            .handle_request(
                &storage.clone(),
                &auth_disabled(),
                &parsed_request(
                    "PUT",
                    container_uri,
                    &[("x-ms-version", AZURE_VERSION)],
                    b"",
                )
                .await,
            )
            .expect("container create should succeed");
        let block_id = BASE64.encode("block-001");
        let encoded_block_id = urlencoding::encode(&block_id);
        let stage_uri = format!("{blob_uri}?comp=block&blockid={encoded_block_id}");
        let commit_uri = format!("{blob_uri}?comp=blocklist");
        let list_uri = format!("{blob_uri}?comp=blocklist&blocklisttype=all");
        let commit =
            |selector: &str| format!("<BlockList><{selector}>{block_id}</{selector}></BlockList>");
        adapter
            .handle_request(
                &storage.clone(),
                &auth_disabled(),
                &parsed_request(
                    "PUT",
                    &stage_uri,
                    &[("x-ms-version", AZURE_VERSION)],
                    b"old",
                )
                .await,
            )
            .expect("old block should stage");
        adapter
            .handle_request(
                &storage.clone(),
                &auth_disabled(),
                &parsed_request(
                    "PUT",
                    &commit_uri,
                    &[("x-ms-version", AZURE_VERSION)],
                    commit("Latest").as_bytes(),
                )
                .await,
            )
            .expect("old block should commit");
        adapter
            .handle_request(
                &storage.clone(),
                &auth_disabled(),
                &parsed_request(
                    "PUT",
                    &stage_uri,
                    &[("x-ms-version", AZURE_VERSION)],
                    b"newest",
                )
                .await,
            )
            .expect("replacement block should stage");

        // Act
        let all_blocks = adapter
            .handle_request(
                &storage.clone(),
                &auth_disabled(),
                &parsed_request("GET", &list_uri, &[("x-ms-version", AZURE_VERSION)], b"").await,
            )
            .expect("all block lists should load");
        let all_blocks = all_blocks
            .into_body()
            .collect()
            .await
            .expect("block list body should read")
            .to_bytes();
        let all_blocks = String::from_utf8(all_blocks.to_vec()).expect("block list should be XML");
        let uncommitted_blocks = adapter
            .handle_request(
                &storage.clone(),
                &auth_disabled(),
                &parsed_request(
                    "GET",
                    &format!("{blob_uri}?comp=blocklist&blocklisttype=uncommitted"),
                    &[("x-ms-version", AZURE_VERSION)],
                    b"",
                )
                .await,
            )
            .expect("uncommitted block list should load")
            .into_body()
            .collect()
            .await
            .expect("uncommitted block list body should read")
            .to_bytes();
        let uncommitted_blocks =
            String::from_utf8(uncommitted_blocks.to_vec()).expect("block list should be XML");
        let committed_response = adapter
            .handle_request(
                &storage.clone(),
                &auth_disabled(),
                &parsed_request(
                    "PUT",
                    &commit_uri,
                    &[("x-ms-version", AZURE_VERSION)],
                    commit("Committed").as_bytes(),
                )
                .await,
            )
            .expect("committed selector should succeed");
        let committed_data = storage
            .get_object("selectors", "report.bin")
            .expect("committed blob should exist")
            .data;
        let latest_response = adapter
            .handle_request(
                &storage.clone(),
                &auth_disabled(),
                &parsed_request(
                    "PUT",
                    &commit_uri,
                    &[("x-ms-version", AZURE_VERSION)],
                    commit("Latest").as_bytes(),
                )
                .await,
            )
            .expect("latest selector should succeed");
        let latest_data = storage
            .get_object("selectors", "report.bin")
            .expect("updated blob should exist")
            .data;
        let recommit_response = adapter
            .handle_request(
                &storage.clone(),
                &auth_disabled(),
                &parsed_request(
                    "PUT",
                    &commit_uri,
                    &[("x-ms-version", AZURE_VERSION)],
                    commit("Committed").as_bytes(),
                )
                .await,
            )
            .expect("pure committed recommit should succeed");
        let invalid_selector = adapter
            .handle_request(
                &storage,
                &auth_disabled(),
                &parsed_request(
                    "GET",
                    &format!("{blob_uri}?comp=blocklist&blocklisttype=bogus"),
                    &[("x-ms-version", AZURE_VERSION)],
                    b"",
                )
                .await,
            )
            .expect("invalid selector should return a provider response");

        // Assert
        assert!(all_blocks.contains("<CommittedBlocks>"));
        assert!(all_blocks.contains("<Size>3</Size>"));
        assert!(all_blocks.contains("<UncommittedBlocks>"));
        assert!(all_blocks.contains("<Size>6</Size>"));
        assert!(!uncommitted_blocks.contains("<CommittedBlocks>"));
        assert!(uncommitted_blocks.contains("<UncommittedBlocks>"));
        assert!(uncommitted_blocks.contains("<Size>6</Size>"));
        assert_eq!(committed_response.status(), StatusCode::CREATED);
        assert_eq!(committed_data, b"old");
        assert_eq!(latest_response.status(), StatusCode::CREATED);
        assert_eq!(latest_data, b"newest");
        assert_eq!(recommit_response.status(), StatusCode::CREATED);
        assert_eq!(invalid_selector.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            invalid_selector
                .headers()
                .get("x-ms-error-code")
                .and_then(|value| value.to_str().ok()),
            Some("InvalidQueryParameterValue")
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn should_reject_invalid_azure_block_ids_without_staging_them() {
        let adapter = AzureBlobAdapter::new();
        let storage = temp_storage();
        create_azure_container(&adapter, &storage, "invalid-blocks").await;

        for uri in [
            "http://localhost/devstoreaccount1/invalid-blocks/report.txt?comp=block",
            "http://localhost/devstoreaccount1/invalid-blocks/report.txt?comp=block&blockid=not_base64",
        ] {
            let response = adapter
                .handle_request(
                    &storage.clone(),
                    &auth_disabled(),
                    &parsed_request(
                        "PUT",
                        uri,
                        &[("x-ms-version", AZURE_VERSION)],
                        b"must-not-stage",
                    )
                    .await,
                )
                .expect("invalid block ID should return an Azure response");
            assert_eq!(response.status(), StatusCode::BAD_REQUEST);
            assert_eq!(
                response
                    .headers()
                    .get("x-ms-error-code")
                    .and_then(|value| value.to_str().ok()),
                Some("InvalidBlockId")
            );
        }
        assert!(adapter
            .block_sessions
            .lock()
            .expect("block sessions should lock")
            .is_empty());

        let first_id = BASE64.encode("one");
        let response = adapter
            .handle_request(
                &storage.clone(),
                &auth_disabled(),
                &parsed_request(
                    "PUT",
                    &format!(
                        "http://localhost/devstoreaccount1/invalid-blocks/report.txt?comp=block&blockid={}",
                        urlencoding::encode(&first_id)
                    ),
                    &[("x-ms-version", AZURE_VERSION)],
                    b"kept",
                )
                .await,
            )
            .expect("first block should stage");
        assert_eq!(response.status(), StatusCode::CREATED);

        let different_length_id = BASE64.encode("longer");
        let response = adapter
            .handle_request(
                &storage.clone(),
                &auth_disabled(),
                &parsed_request(
                    "PUT",
                    &format!(
                        "http://localhost/devstoreaccount1/invalid-blocks/report.txt?comp=block&blockid={}",
                        urlencoding::encode(&different_length_id)
                    ),
                    &[("x-ms-version", AZURE_VERSION)],
                    b"must-not-stage",
                )
                .await,
            )
            .expect("different-length block ID should return an Azure response");
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            response
                .headers()
                .get("x-ms-error-code")
                .and_then(|value| value.to_str().ok()),
            Some("InvalidBlockId")
        );

        let session_key =
            AzureBlobAdapter::blob_state_key("devstoreaccount1", "invalid-blocks", "report.txt");
        let session = adapter
            .load_block_session(&storage, &session_key)
            .expect("block session should load")
            .expect("valid block should remain staged");
        assert_eq!(session.blocks.len(), 1);
        assert_eq!(session.blocks.get(&first_id), Some(&b"kept".to_vec()));
        assert!(matches!(
            storage.get_object("invalid-blocks", "report.txt"),
            Err(crate::error::Error::KeyNotFound)
        ));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn should_reject_malformed_azure_block_lists_without_committing() {
        let adapter = AzureBlobAdapter::new();
        let storage = temp_storage();
        create_azure_container(&adapter, &storage, "invalid-block-list").await;

        let block_id = BASE64.encode("block-001");
        let response = adapter
            .handle_request(
                &storage.clone(),
                &auth_disabled(),
                &parsed_request(
                    "PUT",
                    &format!(
                        "http://localhost/devstoreaccount1/invalid-block-list/report.txt?comp=block&blockid={}",
                        urlencoding::encode(&block_id)
                    ),
                    &[("x-ms-version", AZURE_VERSION)],
                    b"staged",
                )
                .await,
            )
            .expect("valid block should stage");
        assert_eq!(response.status(), StatusCode::CREATED);

        for (body, error_code) in [
            (
                b"<BlockList><Latest>unterminated".as_slice(),
                "InvalidXmlDocument",
            ),
            (
                b"<NotBlockList><Latest>YmxvY2s=</Latest></NotBlockList>".as_slice(),
                "InvalidBlockList",
            ),
            (
                b"<BlockList><Latest>\xff</Latest></BlockList>".as_slice(),
                "InvalidXmlDocument",
            ),
        ] {
            let response = adapter
                .handle_request(
                    &storage.clone(),
                    &auth_disabled(),
                    &parsed_request(
                        "PUT",
                        "http://localhost/devstoreaccount1/invalid-block-list/report.txt?comp=blocklist",
                        &[
                            ("x-ms-version", AZURE_VERSION),
                            ("content-type", "application/xml"),
                        ],
                        body,
                    )
                    .await,
                )
                .expect("invalid block list should return an Azure response");
            assert_eq!(response.status(), StatusCode::BAD_REQUEST);
            assert_eq!(
                response
                    .headers()
                    .get("x-ms-error-code")
                    .and_then(|value| value.to_str().ok()),
                Some(error_code)
            );
            assert!(matches!(
                storage.get_object("invalid-block-list", "report.txt"),
                Err(crate::error::Error::KeyNotFound)
            ));
        }

        let session_key = AzureBlobAdapter::blob_state_key(
            "devstoreaccount1",
            "invalid-block-list",
            "report.txt",
        );
        let session = adapter
            .load_block_session(&storage, &session_key)
            .expect("block session should load")
            .expect("failed commits must preserve staged blocks");
        assert_eq!(session.blocks.get(&block_id), Some(&b"staged".to_vec()));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn should_reject_unsupported_azure_copy_variants_without_mutating() {
        let adapter = AzureBlobAdapter::new();
        let storage = temp_storage();
        create_azure_container(&adapter, &storage, "copy-targets").await;
        storage
            .put_object(
                "copy-targets",
                "existing.txt".to_string(),
                crate::models::Object::new(
                    "existing.txt".to_string(),
                    b"preserve".to_vec(),
                    "text/plain".to_string(),
                ),
            )
            .expect("destination fixture should be written");

        for uri in [
            "http://localhost/devstoreaccount1/copy-targets/existing.txt",
            "http://localhost/devstoreaccount1/copy-targets/from-url.txt?comp=block&blockid=YmxvY2stMDAx",
        ] {
            let response = adapter
                .handle_request(
                    &storage.clone(),
                    &auth_disabled(),
                    &parsed_request(
                        "PUT",
                        uri,
                        &[
                            ("x-ms-version", AZURE_VERSION),
                            (
                                "x-ms-copy-source",
                                "/devstoreaccount1/copy-targets/source.txt",
                            ),
                        ],
                        b"",
                    )
                    .await,
                )
                .expect("unsupported copy should return an Azure response");
            assert_eq!(response.status(), StatusCode::NOT_IMPLEMENTED);
            assert_eq!(
                response
                    .headers()
                    .get("x-ms-error-code")
                    .and_then(|value| value.to_str().ok()),
                Some("FeatureNotSupported")
            );
        }

        assert_eq!(
            storage
                .get_object("copy-targets", "existing.txt")
                .expect("existing destination should remain")
                .data,
            b"preserve"
        );
        assert!(matches!(
            storage.get_object("copy-targets", "from-url.txt"),
            Err(crate::error::Error::KeyNotFound)
        ));
        assert!(adapter
            .block_sessions
            .lock()
            .expect("block sessions should lock")
            .is_empty());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn should_commit_and_list_blocks_after_adapter_restart() {
        let adapter = AzureBlobAdapter::new();
        let storage = temp_storage();

        adapter
            .handle_request(
                &storage.clone(),
                &auth_disabled(),
                &parsed_request(
                    "PUT",
                    "http://localhost/devstoreaccount1/restart?restype=container",
                    &[("x-ms-version", AZURE_VERSION)],
                    b"",
                )
                .await,
            )
            .expect("container create should succeed");

        let block_id = BASE64.encode("restart-block");
        adapter
            .handle_request(
                &storage.clone(),
                &auth_disabled(),
                &parsed_request(
                    "PUT",
                    &format!(
                        "http://localhost/devstoreaccount1/restart/report.txt?comp=block&blockid={}",
                        urlencoding::encode(&block_id)
                    ),
                    &[("x-ms-version", AZURE_VERSION)],
                    b"restart-safe",
                )
                .await,)
            .expect("put block should succeed");

        let restarted = AzureBlobAdapter::new();
        let block_list = format!(
            "<?xml version=\"1.0\" encoding=\"utf-8\"?><BlockList><Latest>{block_id}</Latest></BlockList>"
        );
        let commit = restarted
            .handle_request(
                &storage.clone(),
                &auth_disabled(),
                &parsed_request(
                    "PUT",
                    "http://localhost/devstoreaccount1/restart/report.txt?comp=blocklist",
                    &[
                        ("x-ms-version", AZURE_VERSION),
                        ("content-type", "application/xml"),
                    ],
                    block_list.as_bytes(),
                )
                .await,
            )
            .expect("put block list should succeed after restart");
        assert_eq!(commit.status(), StatusCode::CREATED);

        let block_list_response = AzureBlobAdapter::new()
            .handle_request(
                &storage.clone(),
                &auth_disabled(),
                &parsed_request(
                    "GET",
                    "http://localhost/devstoreaccount1/restart/report.txt?comp=blocklist",
                    &[("x-ms-version", AZURE_VERSION)],
                    b"",
                )
                .await,
            )
            .expect("get block list should succeed after restart");
        let block_list_body = block_list_response
            .into_body()
            .collect()
            .await
            .expect("body should read")
            .to_bytes();
        assert!(String::from_utf8(block_list_body.to_vec())
            .expect("xml")
            .contains(&block_id));

        let object = storage.get_object("restart", "report.txt").unwrap();
        assert_eq!(object.data, b"restart-safe".to_vec());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn should_validate_azure_shared_key_and_sas_authorization() {
        let adapter = AzureBlobAdapter::new();
        let storage = temp_storage();
        storage.create_bucket("secure".to_string()).unwrap();
        storage
            .put_object(
                "secure",
                "blob.txt".to_string(),
                crate::models::Object::new(
                    "blob.txt".to_string(),
                    b"secret".to_vec(),
                    "text/plain".to_string(),
                ),
            )
            .unwrap();

        let mut shared_key_request = parsed_request(
            "GET",
            "http://localhost/devstoreaccount1?comp=list",
            &[
                ("x-ms-version", AZURE_VERSION),
                ("x-ms-date", &Utc::now().to_rfc2822()),
                ("host", "localhost:9000"),
            ],
            b"",
        )
        .await;
        let auth = signed_headers(&shared_key_request, &azure_auth(), "devstoreaccount1");
        shared_key_request
            .headers
            .insert("authorization", auth.parse().expect("header should parse"));

        let response = adapter
            .handle_request(&storage.clone(), &azure_auth(), &shared_key_request)
            .expect("shared key request should complete");
        assert_eq!(response.status(), StatusCode::OK);

        let mut stale_request = parsed_request(
            "GET",
            "http://localhost/devstoreaccount1?comp=list",
            &[
                ("x-ms-version", AZURE_VERSION),
                ("x-ms-date", "Sat, 01 Jan 2024 00:00:00 +0000"),
                ("host", "localhost:9000"),
            ],
            b"",
        )
        .await;
        let stale_auth = signed_headers(&stale_request, &azure_auth(), "devstoreaccount1");
        stale_request.headers.insert(
            "authorization",
            stale_auth.parse().expect("header should parse"),
        );
        let stale_response = adapter
            .handle_request(&storage.clone(), &azure_auth(), &stale_request)
            .expect("stale shared key request should complete");
        assert_eq!(stale_response.status(), StatusCode::FORBIDDEN);

        let expiry = "2035-01-01T00:00:00Z";
        let canonical_resource = "/blob/devstoreaccount1/secure/blob.txt";
        let sig = sas_signature(canonical_resource, &azure_auth(), "r", expiry);
        let response = adapter
            .handle_request(
                &storage,
                &azure_auth(),
                &parsed_request(
                    "GET",
                    &format!(
                        "http://localhost/devstoreaccount1/secure/blob.txt?sp=r&se={}&sv=2023-11-03&sr=b&sig={}",
                        urlencoding::encode(expiry),
                        urlencoding::encode(&sig)
                    ),
                    &[("x-ms-version", AZURE_VERSION)],
                    b"",
                )
                .await,)
            .expect("sas request should complete");
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn should_update_metadata_return_block_list_and_support_ranges() {
        let adapter = AzureBlobAdapter::new();
        let storage = temp_storage();

        create_azure_container(&adapter, &storage, "media").await;
        let (block_one, block_two) = commit_azure_block_blob(&adapter, &storage).await;
        verify_azure_block_list(&adapter, &storage, &block_one, &block_two).await;
        update_and_verify_azure_metadata(&adapter, &storage).await;
        verify_azure_range_read(&adapter, &storage).await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn should_render_native_azure_incomplete_body_error() {
        let adapter = AzureBlobAdapter::new();
        let mut headers = HeaderMap::new();
        headers.insert("x-ms-version", HeaderValue::from_static(AZURE_VERSION));
        headers.insert(
            "x-ms-client-request-id",
            HeaderValue::from_static("azure-short-body"),
        );

        let response = adapter.render_incomplete_body(
            &Method::PUT,
            &Uri::from_static("http://localhost/devstoreaccount1/container/blob"),
            &headers,
        );

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            response
                .headers()
                .get("x-ms-client-request-id")
                .and_then(|value| value.to_str().ok()),
            Some("azure-short-body")
        );
        assert!(String::from_utf8(read_test_body(response).await)
            .expect("error body should be utf8")
            .contains("<Code>InvalidHeaderValue</Code>"));
    }

    #[tokio::test(flavor = "multi_thread")]
    #[allow(clippy::too_many_lines)] // Activation, read isolation, and no-mutation checks share one foreign bucket.
    async fn should_isolate_foreign_version_history_and_validate_local_mode_before_create() {
        let adapter = AzureBlobAdapter::new();
        let storage = temp_storage();
        storage.create_bucket("foreign".to_string()).unwrap();
        storage.enable_versioning("foreign").unwrap();
        storage
            .update_bucket_metadata(
                "foreign",
                HashMap::from([(S3_VERSIONING_STATUS_KEY.to_string(), "Enabled".to_string())]),
            )
            .unwrap();
        storage
            .put_object(
                "foreign",
                "blob.txt".to_string(),
                crate::models::Object::new(
                    "blob.txt".to_string(),
                    b"original".to_vec(),
                    "text/plain".to_string(),
                ),
            )
            .unwrap();
        let stored = storage.get_object("foreign", "blob.txt").unwrap();
        let foreign_version_id = stored
            .version_id
            .clone()
            .expect("foreign mode should assign a version ID");

        let current = adapter
            .handle_request(
                &storage.clone(),
                &auth_disabled(),
                &parsed_request(
                    "GET",
                    "http://localhost/devstoreaccount1/foreign/blob.txt",
                    &[("x-ms-version", AZURE_VERSION)],
                    b"",
                )
                .await,
            )
            .expect("current Azure read should respond");
        assert_eq!(current.status(), StatusCode::OK);
        assert!(current.headers().get("x-ms-version-id").is_none());

        let activation = adapter
            .handle_request(
                &storage.clone(),
                &auth_disabled(),
                &parsed_request(
                    "PUT",
                    "http://localhost/devstoreaccount1/foreign?restype=container",
                    &[
                        ("x-ms-version", AZURE_VERSION),
                        ("x-sqrzl-azure-versioning-enabled", "true"),
                    ],
                    b"",
                )
                .await,
            )
            .expect("foreign mode activation conflict should respond");
        assert_eq!(activation.status(), StatusCode::CONFLICT);
        assert!(String::from_utf8(read_test_body(activation).await)
            .expect("error body should be utf8")
            .contains("<Code>FeatureVersionMismatch</Code>"));
        let bucket = storage.get_bucket("foreign").unwrap();
        assert_eq!(
            bucket.metadata.get(S3_VERSIONING_STATUS_KEY),
            Some(&"Enabled".to_string())
        );
        assert!(!bucket.metadata.contains_key(AZURE_VERSIONING_KEY));

        for request in [
            parsed_request(
                "PUT",
                "http://localhost/devstoreaccount1/foreign/blob.txt",
                &[
                    ("x-ms-version", AZURE_VERSION),
                    ("x-ms-blob-type", "BlockBlob"),
                ],
                b"replacement",
            )
            .await,
            parsed_request(
                "GET",
                &format!(
                    "http://localhost/devstoreaccount1/foreign/blob.txt?versionid={foreign_version_id}"
                ),
                &[("x-ms-version", AZURE_VERSION)],
                b"",
            )
            .await,
        ] {
            let response = adapter
                .handle_request(&storage.clone(), &auth_disabled(), &request)
                .expect("foreign history conflict should respond");
            assert_eq!(response.status(), StatusCode::CONFLICT);
            assert!(String::from_utf8(read_test_body(response).await)
                .expect("error body should be utf8")
                .contains("<Code>FeatureVersionMismatch</Code>"));
        }
        assert_eq!(
            storage.get_object("foreign", "blob.txt").unwrap().data,
            b"original"
        );

        let invalid_create = adapter
            .handle_request(
                &storage.clone(),
                &auth_disabled(),
                &parsed_request(
                    "PUT",
                    "http://localhost/devstoreaccount1/invalid-mode?restype=container",
                    &[
                        ("x-ms-version", AZURE_VERSION),
                        ("x-sqrzl-azure-soft-delete-days", "0"),
                    ],
                    b"",
                )
                .await,
            )
            .expect("invalid mode should respond");
        assert_eq!(invalid_create.status(), StatusCode::BAD_REQUEST);
        assert!(matches!(
            storage.get_bucket("invalid-mode"),
            Err(crate::error::Error::BucketNotFound)
        ));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn should_support_append_and_page_blob_writes() {
        let adapter = AzureBlobAdapter::new();
        let storage = temp_storage();

        create_azure_container(&adapter, &storage, "state").await;
        verify_append_blob_writes(&adapter, &storage).await;
        verify_page_blob_writes(&adapter, &storage).await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn should_manage_leases_snapshots_and_immutability() {
        let adapter = AzureBlobAdapter::new();
        let storage = temp_storage();

        create_azure_container(&adapter, &storage, "state").await;
        create_azure_lease_blob(&adapter, &storage).await;
        acquire_deny_and_release_azure_lease(&adapter, &storage).await;
        create_overwrite_and_read_azure_snapshot(&adapter, &storage).await;
        verify_azure_immutability_and_legal_hold(&adapter, &storage).await;
    }

    #[tokio::test(flavor = "multi_thread")]
    #[allow(clippy::too_many_lines)] // Locked-policy and legal-hold fail-closed checks share one fixture.
    async fn should_preserve_azure_worm_state_on_invalid_mutations() {
        // Arrange
        let adapter = AzureBlobAdapter::new();
        let storage = temp_storage();
        create_azure_container(&adapter, &storage, "worm").await;
        for (key, data) in [
            ("locked.txt", b"locked".as_slice()),
            ("held.txt", b"held".as_slice()),
        ] {
            storage
                .put_object(
                    "worm",
                    key.to_string(),
                    crate::models::Object::new(
                        key.to_string(),
                        data.to_vec(),
                        "text/plain".to_string(),
                    ),
                )
                .unwrap();
        }
        let locked_until = (Utc::now() + chrono::Duration::days(3_650)).to_rfc2822();
        let shorter_until = (Utc::now() + chrono::Duration::days(365)).to_rfc2822();
        let extended_until = (Utc::now() + chrono::Duration::days(7_300)).to_rfc2822();
        let locked_uri =
            "http://localhost/devstoreaccount1/worm/locked.txt?comp=immutabilityPolicies";
        let set_locked = adapter
            .handle_request(
                &storage.clone(),
                &auth_disabled(),
                &parsed_request(
                    "PUT",
                    locked_uri,
                    &[
                        ("x-ms-version", AZURE_VERSION),
                        ("x-ms-immutability-policy-until-date", &locked_until),
                        ("x-ms-immutability-policy-mode", "Locked"),
                    ],
                    b"",
                )
                .await,
            )
            .expect("locked policy should be created");
        assert_eq!(set_locked.status(), StatusCode::OK);

        // Act and assert: Locked can neither become Unlocked nor be shortened or deleted.
        for (mode, until) in [
            ("Unlocked", locked_until.as_str()),
            ("Locked", shorter_until.as_str()),
        ] {
            let response = adapter
                .handle_request(
                    &storage.clone(),
                    &auth_disabled(),
                    &parsed_request(
                        "PUT",
                        locked_uri,
                        &[
                            ("x-ms-version", AZURE_VERSION),
                            ("x-ms-immutability-policy-until-date", until),
                            ("x-ms-immutability-policy-mode", mode),
                        ],
                        b"",
                    )
                    .await,
                )
                .expect("invalid locked-policy mutation should respond");
            assert_eq!(response.status(), StatusCode::CONFLICT);
            assert_eq!(
                header_value(&response, "x-ms-error-code"),
                Some("BlobImmutableDueToPolicy")
            );
        }
        let delete_policy = adapter
            .handle_request(
                &storage.clone(),
                &auth_disabled(),
                &parsed_request(
                    "DELETE",
                    locked_uri,
                    &[("x-ms-version", AZURE_VERSION)],
                    b"",
                )
                .await,
            )
            .expect("locked policy delete should respond");
        assert_eq!(delete_policy.status(), StatusCode::CONFLICT);

        // A legitimate extension remains supported.
        let extension = adapter
            .handle_request(
                &storage.clone(),
                &auth_disabled(),
                &parsed_request(
                    "PUT",
                    locked_uri,
                    &[
                        ("x-ms-version", AZURE_VERSION),
                        ("x-ms-immutability-policy-until-date", &extended_until),
                        ("x-ms-immutability-policy-mode", "Locked"),
                    ],
                    b"",
                )
                .await,
            )
            .expect("locked policy extension should succeed");
        assert_eq!(extension.status(), StatusCode::OK);

        let held_uri = "http://localhost/devstoreaccount1/worm/held.txt?comp=legalhold";
        let set_hold = adapter
            .handle_request(
                &storage.clone(),
                &auth_disabled(),
                &parsed_request(
                    "PUT",
                    held_uri,
                    &[("x-ms-version", AZURE_VERSION), ("x-ms-legal-hold", "true")],
                    b"",
                )
                .await,
            )
            .expect("legal hold should be set");
        assert_eq!(set_hold.status(), StatusCode::OK);
        for request in [
            parsed_request(
                "PUT",
                held_uri,
                &[("x-ms-version", AZURE_VERSION)],
                b"<LegalHold>false</LegalHold>",
            )
            .await,
            parsed_request(
                "PUT",
                held_uri,
                &[("x-ms-version", AZURE_VERSION), ("x-ms-legal-hold", "typo")],
                b"",
            )
            .await,
        ] {
            let response = adapter
                .handle_request(&storage.clone(), &auth_disabled(), &request)
                .expect("invalid legal-hold mutation should respond");
            assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        }

        let locked = storage.get_object("worm", "locked.txt").unwrap();
        assert_eq!(locked.data, b"locked");
        assert_eq!(
            locked.provider_metadata.get(AZURE_IMMUTABILITY_MODE_KEY),
            Some(&"Locked".to_string())
        );
        assert_eq!(
            locked.provider_metadata.get(AZURE_IMMUTABILITY_UNTIL_KEY),
            Some(&extended_until)
        );
        let held = storage.get_object("worm", "held.txt").unwrap();
        assert_eq!(held.data, b"held");
        assert_eq!(
            held.provider_metadata.get(AZURE_LEGAL_HOLD_KEY),
            Some(&"true".to_string())
        );
        for key in ["locked.txt", "held.txt"] {
            let response = adapter
                .handle_request(
                    &storage.clone(),
                    &auth_disabled(),
                    &parsed_request(
                        "DELETE",
                        &format!("http://localhost/devstoreaccount1/worm/{key}"),
                        &[("x-ms-version", AZURE_VERSION)],
                        b"",
                    )
                    .await,
                )
                .expect("WORM-protected delete should respond");
            assert_eq!(response.status(), StatusCode::CONFLICT);
        }
    }

    #[allow(
        clippy::too_many_lines,
        reason = "the integrated regression proves both version-creating overwrite surfaces and their unversioned fail-closed counterparts"
    )]
    #[tokio::test(flavor = "multi_thread")]
    async fn should_create_new_versions_when_overwriting_azure_worm_blobs() {
        // Arrange a versioned blob whose first version is protected by legal hold.
        let adapter = AzureBlobAdapter::new();
        let storage = temp_storage();
        let versioned_container = "versioned-worm-overwrite";
        let key = "doc.txt";
        let versioned_uri =
            format!("http://localhost/devstoreaccount1/{versioned_container}/{key}");
        storage
            .create_bucket(versioned_container.to_string())
            .unwrap();
        storage.enable_versioning(versioned_container).unwrap();
        storage
            .update_bucket_metadata(
                versioned_container,
                HashMap::from([(AZURE_VERSIONING_KEY.to_string(), "true".to_string())]),
            )
            .unwrap();
        let mut held = crate::models::Object::new(
            key.to_string(),
            b"held-version".to_vec(),
            "text/plain".to_string(),
        );
        AzureBlobAdapter::set_blob_type(&mut held, "BlockBlob");
        held.provider_metadata
            .insert(AZURE_LEGAL_HOLD_KEY.to_string(), "true".to_string());
        storage
            .put_object(versioned_container, key.to_string(), held)
            .unwrap();
        let held_version_id = storage
            .get_object(versioned_container, key)
            .unwrap()
            .version_id
            .expect("held current blob should have a version ID");
        let initial_version_count = storage
            .list_object_versions_for_key(versioned_container, key)
            .unwrap()
            .len();

        // Put Blob is a documented WORM exception when it creates a new version.
        let put_blob = adapter
            .handle_request(
                &storage.clone(),
                &auth_disabled(),
                &parsed_request(
                    "PUT",
                    &versioned_uri,
                    &[
                        ("x-ms-version", AZURE_VERSION),
                        ("x-ms-blob-type", "BlockBlob"),
                    ],
                    b"replacement",
                )
                .await,
            )
            .expect("versioned Put Blob should respond");
        assert_eq!(put_blob.status(), StatusCode::CREATED);
        let replacement_version_id = header_value(&put_blob, "x-ms-version-id")
            .expect("versioned Put Blob should identify the new version")
            .to_string();
        assert_ne!(replacement_version_id, held_version_id);
        assert_eq!(
            storage.get_object(versioned_container, key).unwrap().data,
            b"replacement"
        );
        let archived_held = storage
            .get_object_version(versioned_container, key, &held_version_id)
            .unwrap();
        assert_eq!(archived_held.data, b"held-version");
        assert_eq!(
            archived_held
                .provider_metadata
                .get(AZURE_LEGAL_HOLD_KEY)
                .map(String::as_str),
            Some("true")
        );
        assert_eq!(
            storage
                .list_object_versions_for_key(versioned_container, key)
                .unwrap()
                .len(),
            initial_version_count + 1
        );

        // Protect the new current version, then prove Put Block List also creates
        // a distinct current version while retaining the locked predecessor.
        let observed = storage.get_object(versioned_container, key).unwrap();
        let mut locked = observed.clone();
        let locked_until = (Utc::now() + chrono::Duration::days(365)).to_rfc2822();
        locked.provider_metadata.insert(
            AZURE_IMMUTABILITY_MODE_KEY.to_string(),
            "Locked".to_string(),
        );
        locked.provider_metadata.insert(
            AZURE_IMMUTABILITY_UNTIL_KEY.to_string(),
            locked_until.clone(),
        );
        assert!(storage
            .replace_object_metadata_if_unchanged(versioned_container, key, &observed, &locked)
            .unwrap());
        let locked_version_id = observed
            .version_id
            .expect("locked current blob should have a version ID");
        let block_id = BASE64.encode("block-0001");
        let put_block = adapter
            .handle_request(
                &storage.clone(),
                &auth_disabled(),
                &parsed_request(
                    "PUT",
                    &format!(
                        "{versioned_uri}?comp=block&blockid={}",
                        urlencoding::encode(&block_id)
                    ),
                    &[("x-ms-version", AZURE_VERSION)],
                    b"committed-block",
                )
                .await,
            )
            .expect("Put Block should respond");
        assert_eq!(put_block.status(), StatusCode::CREATED);
        let block_list = format!("<BlockList><Latest>{block_id}</Latest></BlockList>");
        let put_block_list = adapter
            .handle_request(
                &storage.clone(),
                &auth_disabled(),
                &parsed_request(
                    "PUT",
                    &format!("{versioned_uri}?comp=blocklist"),
                    &[
                        ("x-ms-version", AZURE_VERSION),
                        ("content-type", "application/xml"),
                    ],
                    block_list.as_bytes(),
                )
                .await,
            )
            .expect("versioned Put Block List should respond");
        assert_eq!(put_block_list.status(), StatusCode::CREATED);
        let block_version_id = header_value(&put_block_list, "x-ms-version-id")
            .expect("versioned Put Block List should identify the new version")
            .to_string();
        assert_ne!(block_version_id, locked_version_id);
        assert_eq!(
            storage.get_object(versioned_container, key).unwrap().data,
            b"committed-block"
        );
        let archived_locked = storage
            .get_object_version(versioned_container, key, &locked_version_id)
            .unwrap();
        assert_eq!(archived_locked.data, b"replacement");
        assert_eq!(
            archived_locked
                .provider_metadata
                .get(AZURE_IMMUTABILITY_MODE_KEY)
                .map(String::as_str),
            Some("Locked")
        );
        assert_eq!(
            archived_locked
                .provider_metadata
                .get(AZURE_IMMUTABILITY_UNTIL_KEY),
            Some(&locked_until)
        );
        assert_eq!(
            storage
                .list_object_versions_for_key(versioned_container, key)
                .unwrap()
                .len(),
            initial_version_count + 2
        );

        // The same protected overwrites remain conflicts when Azure versioning
        // is disabled, and a failed block-list commit retains its staged block.
        let unversioned_container = "unversioned-worm-overwrite";
        let unversioned_uri =
            format!("http://localhost/devstoreaccount1/{unversioned_container}/{key}");
        storage
            .create_bucket(unversioned_container.to_string())
            .unwrap();
        let mut protected = crate::models::Object::new(
            key.to_string(),
            b"must-remain".to_vec(),
            "text/plain".to_string(),
        );
        AzureBlobAdapter::set_blob_type(&mut protected, "BlockBlob");
        protected
            .provider_metadata
            .insert(AZURE_LEGAL_HOLD_KEY.to_string(), "true".to_string());
        protected.provider_metadata.insert(
            AZURE_IMMUTABILITY_MODE_KEY.to_string(),
            "Locked".to_string(),
        );
        protected
            .provider_metadata
            .insert(AZURE_IMMUTABILITY_UNTIL_KEY.to_string(), locked_until);
        storage
            .put_object(unversioned_container, key.to_string(), protected)
            .unwrap();
        let rejected_put = adapter
            .handle_request(
                &storage.clone(),
                &auth_disabled(),
                &parsed_request(
                    "PUT",
                    &unversioned_uri,
                    &[
                        ("x-ms-version", AZURE_VERSION),
                        ("x-ms-blob-type", "BlockBlob"),
                    ],
                    b"forbidden",
                )
                .await,
            )
            .expect("unversioned protected Put Blob should respond");
        assert_eq!(rejected_put.status(), StatusCode::CONFLICT);
        assert_eq!(
            header_value(&rejected_put, "x-ms-error-code"),
            Some("BlobImmutableDueToPolicy")
        );
        let stage = adapter
            .handle_request(
                &storage.clone(),
                &auth_disabled(),
                &parsed_request(
                    "PUT",
                    &format!(
                        "{unversioned_uri}?comp=block&blockid={}",
                        urlencoding::encode(&block_id)
                    ),
                    &[("x-ms-version", AZURE_VERSION)],
                    b"forbidden-block",
                )
                .await,
            )
            .expect("unversioned Put Block should stage without changing the blob");
        assert_eq!(stage.status(), StatusCode::CREATED);
        let rejected_commit = adapter
            .handle_request(
                &storage.clone(),
                &auth_disabled(),
                &parsed_request(
                    "PUT",
                    &format!("{unversioned_uri}?comp=blocklist"),
                    &[
                        ("x-ms-version", AZURE_VERSION),
                        ("content-type", "application/xml"),
                    ],
                    block_list.as_bytes(),
                )
                .await,
            )
            .expect("unversioned protected Put Block List should respond");
        assert_eq!(rejected_commit.status(), StatusCode::CONFLICT);
        assert_eq!(
            storage.get_object(unversioned_container, key).unwrap().data,
            b"must-remain"
        );
        let session_key =
            AzureBlobAdapter::blob_state_key("devstoreaccount1", unversioned_container, key);
        assert!(adapter
            .load_block_session(&storage, &session_key)
            .unwrap()
            .is_some_and(|session| session.blocks.contains_key(&block_id)));
    }

    #[allow(
        clippy::too_many_lines,
        reason = "Put Blob and Put Block List share one versioned lease fixture and post-overwrite denial proof"
    )]
    #[tokio::test(flavor = "multi_thread")]
    async fn should_preserve_active_lease_across_azure_blob_overwrites() {
        // Arrange two versioned block blobs with independent active leases.
        let adapter = AzureBlobAdapter::new();
        let storage = temp_storage();
        let container = "leased-overwrites";
        storage.create_bucket(container.to_string()).unwrap();
        storage.enable_versioning(container).unwrap();
        storage
            .update_bucket_metadata(
                container,
                HashMap::from([(AZURE_VERSIONING_KEY.to_string(), "true".to_string())]),
            )
            .unwrap();
        for key in ["put.txt", "blocks.txt"] {
            let mut object = crate::models::Object::new(
                key.to_string(),
                b"initial".to_vec(),
                "text/plain".to_string(),
            );
            AzureBlobAdapter::set_blob_type(&mut object, "BlockBlob");
            storage
                .put_object(container, key.to_string(), object)
                .unwrap();
        }
        let put_uri = format!("http://localhost/devstoreaccount1/{container}/put.txt");
        let block_uri = format!("http://localhost/devstoreaccount1/{container}/blocks.txt");
        let put_lease_id = "20e2a601-58ea-4312-b20c-bde31fc24124";
        let block_lease_id = "9f889ecb-a264-4a3e-8304-ac23fa12f7cc";
        acquire_azure_lease_for(&adapter, &storage, &put_uri, put_lease_id).await;
        acquire_azure_lease_for(&adapter, &storage, &block_uri, block_lease_id).await;

        // Put Blob accepts the matching lease and carries it onto the new version.
        let put = adapter
            .handle_request(
                &storage.clone(),
                &auth_disabled(),
                &parsed_request(
                    "PUT",
                    &put_uri,
                    &[
                        ("x-ms-version", AZURE_VERSION),
                        ("x-ms-blob-type", "BlockBlob"),
                        ("x-ms-lease-id", put_lease_id),
                    ],
                    b"put replacement",
                )
                .await,
            )
            .expect("leased Put Blob should respond");
        assert_eq!(put.status(), StatusCode::CREATED);
        assert!(header_value(&put, "x-ms-version-id").is_some());
        let put_current = storage.get_object(container, "put.txt").unwrap();
        assert_eq!(put_current.data, b"put replacement");
        assert_eq!(AzureBlobAdapter::lease_id(&put_current), Some(put_lease_id));
        assert!(AzureBlobAdapter::has_active_lease(&put_current));
        assert_eq!(
            storage
                .list_object_versions_for_key(container, "put.txt")
                .unwrap()
                .len(),
            2
        );
        let denied_put = adapter
            .handle_request(
                &storage.clone(),
                &auth_disabled(),
                &parsed_request(
                    "PUT",
                    &put_uri,
                    &[
                        ("x-ms-version", AZURE_VERSION),
                        ("x-ms-blob-type", "BlockBlob"),
                    ],
                    b"must not replace",
                )
                .await,
            )
            .expect("Put Blob without the retained lease ID should respond");
        assert_eq!(denied_put.status(), StatusCode::PRECONDITION_FAILED);
        assert_eq!(
            header_value(&denied_put, "x-ms-error-code"),
            Some("LeaseIdMissing")
        );
        assert_eq!(
            storage.get_object(container, "put.txt").unwrap().data,
            b"put replacement"
        );

        // Put Block List likewise preserves the lease after committing a new version.
        let block_id = BASE64.encode("leased-block-001");
        let stage = adapter
            .handle_request(
                &storage.clone(),
                &auth_disabled(),
                &parsed_request(
                    "PUT",
                    &format!(
                        "{block_uri}?comp=block&blockid={}",
                        urlencoding::encode(&block_id)
                    ),
                    &[
                        ("x-ms-version", AZURE_VERSION),
                        ("x-ms-lease-id", block_lease_id),
                    ],
                    b"block replacement",
                )
                .await,
            )
            .expect("leased Put Block should respond");
        assert_eq!(stage.status(), StatusCode::CREATED);
        let block_list = format!("<BlockList><Latest>{block_id}</Latest></BlockList>");
        let commit = adapter
            .handle_request(
                &storage.clone(),
                &auth_disabled(),
                &parsed_request(
                    "PUT",
                    &format!("{block_uri}?comp=blocklist"),
                    &[
                        ("x-ms-version", AZURE_VERSION),
                        ("x-ms-lease-id", block_lease_id),
                        ("content-type", "application/xml"),
                    ],
                    block_list.as_bytes(),
                )
                .await,
            )
            .expect("leased Put Block List should respond");
        assert_eq!(commit.status(), StatusCode::CREATED);
        assert!(header_value(&commit, "x-ms-version-id").is_some());
        let block_current = storage.get_object(container, "blocks.txt").unwrap();
        assert_eq!(block_current.data, b"block replacement");
        assert_eq!(
            AzureBlobAdapter::lease_id(&block_current),
            Some(block_lease_id)
        );
        assert!(AzureBlobAdapter::has_active_lease(&block_current));
        assert_eq!(
            storage
                .list_object_versions_for_key(container, "blocks.txt")
                .unwrap()
                .len(),
            2
        );
        let denied_commit = adapter
            .handle_request(
                &storage.clone(),
                &auth_disabled(),
                &parsed_request(
                    "PUT",
                    &format!("{block_uri}?comp=blocklist"),
                    &[
                        ("x-ms-version", AZURE_VERSION),
                        ("content-type", "application/xml"),
                    ],
                    block_list.as_bytes(),
                )
                .await,
            )
            .expect("Put Block List without the retained lease ID should respond");
        assert_eq!(denied_commit.status(), StatusCode::PRECONDITION_FAILED);
        assert_eq!(
            header_value(&denied_commit, "x-ms-error-code"),
            Some("LeaseIdMissing")
        );
        assert_eq!(
            storage.get_object(container, "blocks.txt").unwrap().data,
            b"block replacement"
        );
    }

    #[allow(
        clippy::too_many_lines,
        reason = "the integrated fail-closed regression keeps every corrupt WORM representation and its no-mutation assertion in one auditable flow"
    )]
    #[tokio::test(flavor = "multi_thread")]
    async fn should_fail_closed_for_malformed_azure_worm_metadata() {
        // Arrange
        let adapter = AzureBlobAdapter::new();
        let storage = temp_storage();
        storage.create_bucket("malformed-worm".to_string()).unwrap();
        for (key, provider_metadata) in [
            (
                "locked-without-until.txt",
                HashMap::from([(
                    AZURE_IMMUTABILITY_MODE_KEY.to_string(),
                    "Locked".to_string(),
                )]),
            ),
            (
                "locked-with-invalid-until.txt",
                HashMap::from([
                    (
                        AZURE_IMMUTABILITY_MODE_KEY.to_string(),
                        "Locked".to_string(),
                    ),
                    (
                        AZURE_IMMUTABILITY_UNTIL_KEY.to_string(),
                        "not-a-date".to_string(),
                    ),
                ]),
            ),
            (
                "invalid-legal-hold.txt",
                HashMap::from([(AZURE_LEGAL_HOLD_KEY.to_string(), "TRUE".to_string())]),
            ),
            (
                "orphan-until.txt",
                HashMap::from([(
                    AZURE_IMMUTABILITY_UNTIL_KEY.to_string(),
                    (Utc::now() - chrono::Duration::days(1)).to_rfc2822(),
                )]),
            ),
        ] {
            let mut object = crate::models::Object::new(
                key.to_string(),
                b"protected".to_vec(),
                "text/plain".to_string(),
            );
            object.provider_metadata = provider_metadata;
            storage
                .put_object("malformed-worm", key.to_string(), object)
                .unwrap();
        }

        // Act and assert: corrupt durable protection state never becomes mutable.
        for key in [
            "locked-without-until.txt",
            "locked-with-invalid-until.txt",
            "invalid-legal-hold.txt",
            "orphan-until.txt",
        ] {
            for request in [
                parsed_request(
                    "PUT",
                    &format!("http://localhost/devstoreaccount1/malformed-worm/{key}"),
                    &[
                        ("x-ms-version", AZURE_VERSION),
                        ("x-ms-blob-type", "BlockBlob"),
                    ],
                    b"replacement",
                )
                .await,
                parsed_request(
                    "DELETE",
                    &format!("http://localhost/devstoreaccount1/malformed-worm/{key}"),
                    &[("x-ms-version", AZURE_VERSION)],
                    b"",
                )
                .await,
            ] {
                let response = adapter
                    .handle_request(&storage.clone(), &auth_disabled(), &request)
                    .expect("malformed WORM mutation should respond");
                assert_eq!(response.status(), StatusCode::CONFLICT);
                assert_eq!(
                    header_value(&response, "x-ms-error-code"),
                    Some("BlobImmutableDueToPolicy")
                );
            }
            assert_eq!(
                storage.get_object("malformed-worm", key).unwrap().data,
                b"protected"
            );
        }

        // A well-formed inactive legal-hold marker remains a legitimate mutable state.
        let mut mutable = crate::models::Object::new(
            "released.txt".to_string(),
            b"old".to_vec(),
            "text/plain".to_string(),
        );
        mutable
            .provider_metadata
            .insert(AZURE_LEGAL_HOLD_KEY.to_string(), "false".to_string());
        storage
            .put_object("malformed-worm", "released.txt".to_string(), mutable)
            .unwrap();
        let response = adapter
            .handle_request(
                &storage.clone(),
                &auth_disabled(),
                &parsed_request(
                    "PUT",
                    "http://localhost/devstoreaccount1/malformed-worm/released.txt",
                    &[
                        ("x-ms-version", AZURE_VERSION),
                        ("x-ms-blob-type", "BlockBlob"),
                    ],
                    b"new",
                )
                .await,
            )
            .expect("released legal hold should remain mutable");
        assert_eq!(response.status(), StatusCode::CREATED);
        assert_eq!(
            storage
                .get_object("malformed-worm", "released.txt")
                .unwrap()
                .data,
            b"new"
        );
    }

    #[allow(
        clippy::too_many_lines,
        reason = "the integrated purge regression verifies current, version, snapshot, direct, and account-list paths preserve all protected data"
    )]
    #[tokio::test(flavor = "multi_thread")]
    async fn should_not_schedule_or_purge_container_with_azure_worm_data() {
        // Arrange: leave the current object mutable while a protected historical
        // version and a protected snapshot exercise both hidden retention paths.
        let adapter = AzureBlobAdapter::new();
        let storage = temp_storage();
        let container = "worm-version-container";
        let current_container = "worm-current-container";
        let snapshot_container = "worm-snapshot-container";
        storage.create_bucket(container.to_string()).unwrap();
        storage.enable_versioning(container).unwrap();
        storage
            .update_bucket_metadata(
                container,
                HashMap::from([(AZURE_VERSIONING_KEY.to_string(), "true".to_string())]),
            )
            .unwrap();
        let future = (Utc::now() + chrono::Duration::days(365)).to_rfc2822();
        let mut protected = crate::models::Object::new(
            "doc.txt".to_string(),
            b"protected-version".to_vec(),
            "text/plain".to_string(),
        );
        protected.provider_metadata.insert(
            AZURE_IMMUTABILITY_MODE_KEY.to_string(),
            "Locked".to_string(),
        );
        protected
            .provider_metadata
            .insert(AZURE_IMMUTABILITY_UNTIL_KEY.to_string(), future.clone());
        storage
            .create_bucket(current_container.to_string())
            .unwrap();
        storage
            .put_object(current_container, "doc.txt".to_string(), protected.clone())
            .unwrap();
        storage
            .put_object(container, "doc.txt".to_string(), protected)
            .unwrap();
        storage
            .put_object(
                container,
                "doc.txt".to_string(),
                crate::models::Object::new(
                    "doc.txt".to_string(),
                    b"current".to_vec(),
                    "text/plain".to_string(),
                ),
            )
            .unwrap();
        storage
            .create_bucket(snapshot_container.to_string())
            .unwrap();
        storage
            .put_object(
                snapshot_container,
                "other.txt".to_string(),
                crate::models::Object::new(
                    "other.txt".to_string(),
                    b"snapshot-current".to_vec(),
                    "text/plain".to_string(),
                ),
            )
            .unwrap();
        let snapshot_key =
            AzureBlobAdapter::snapshot_storage_key("other.txt", "2026-08-10T12:00:00.0000000Z");
        let mut snapshot = crate::models::Object::new(
            snapshot_key.clone(),
            b"protected-snapshot".to_vec(),
            "text/plain".to_string(),
        );
        snapshot
            .provider_metadata
            .insert(AZURE_LEGAL_HOLD_KEY.to_string(), "true".to_string());
        storage
            .put_object(snapshot_container, snapshot_key.clone(), snapshot)
            .unwrap();
        let versions_before = storage.list_object_versions(container, None).unwrap();

        // Act and assert: current, historical-version, and snapshot protection
        // each independently prevent a deletion marker from being scheduled.
        for protected_container in [current_container, container, snapshot_container] {
            let delete = adapter
                .handle_request(
                    &storage.clone(),
                    &auth_disabled(),
                    &parsed_request(
                        "DELETE",
                        &format!(
                            "http://localhost/devstoreaccount1/{protected_container}?restype=container"
                        ),
                        &[
                            ("x-ms-version", AZURE_VERSION),
                            ("x-sqrzl-azure-delete-delay-ms", "0"),
                        ],
                        b"",
                    )
                    .await,
                )
                .expect("protected container delete should respond");
            assert_eq!(delete.status(), StatusCode::CONFLICT);
            assert_eq!(
                header_value(&delete, "x-ms-error-code"),
                Some("BlobImmutableDueToPolicy")
            );
            assert!(state::load_json::<AzureContainerDeletion>(
                storage.as_ref(),
                AZURE_CONTAINER_DELETION_STATE,
                protected_container,
            )
            .unwrap()
            .is_none());
        }

        // A deletion that became unsafe after scheduling must also abandon purge.
        state::save_json(
            storage.as_ref(),
            AZURE_CONTAINER_DELETION_STATE,
            container,
            &AzureContainerDeletion {
                purge_after: Utc::now() - chrono::Duration::seconds(1),
            },
        )
        .unwrap();
        let list = adapter
            .handle_request(
                &storage.clone(),
                &auth_disabled(),
                &parsed_request(
                    "GET",
                    "http://localhost/devstoreaccount1?comp=list",
                    &[("x-ms-version", AZURE_VERSION)],
                    b"",
                )
                .await,
            )
            .expect("account listing should abandon a newly protected purge");
        assert_eq!(list.status(), StatusCode::OK);
        assert!(String::from_utf8(read_test_body(list).await)
            .expect("account listing should be utf8")
            .contains("<Name>worm-version-container</Name>"));
        let read = adapter
            .handle_request(
                &storage.clone(),
                &auth_disabled(),
                &parsed_request(
                    "GET",
                    "http://localhost/devstoreaccount1/worm-version-container/doc.txt",
                    &[("x-ms-version", AZURE_VERSION)],
                    b"",
                )
                .await,
            )
            .expect("abandoned protected purge should expose the intact container");
        assert_eq!(read.status(), StatusCode::OK);
        assert_eq!(read_test_body(read).await, b"current");
        assert_eq!(
            storage.list_object_versions(container, None).unwrap().len(),
            versions_before.len()
        );
        assert_eq!(
            storage
                .get_object(snapshot_container, &snapshot_key)
                .unwrap()
                .data,
            b"protected-snapshot"
        );
        assert!(state::load_json::<AzureContainerDeletion>(
            storage.as_ref(),
            AZURE_CONTAINER_DELETION_STATE,
            container,
        )
        .unwrap()
        .is_none());
    }

    #[tokio::test(flavor = "multi_thread")]
    #[allow(clippy::too_many_lines)] // Every rejected selector is checked against one shared version-history fixture.
    async fn should_reject_unsafe_and_unsupported_azure_version_selectors_without_mutating() {
        // Arrange
        let adapter = AzureBlobAdapter::new();
        let storage = temp_storage();
        storage.create_bucket("versions".to_string()).unwrap();
        storage.enable_versioning("versions").unwrap();
        storage
            .update_bucket_metadata(
                "versions",
                HashMap::from([(AZURE_VERSIONING_KEY.to_string(), "true".to_string())]),
            )
            .unwrap();
        storage
            .put_object(
                "versions",
                "doc.txt".to_string(),
                crate::models::Object::new(
                    "doc.txt".to_string(),
                    b"first".to_vec(),
                    "text/plain".to_string(),
                ),
            )
            .unwrap();
        let first_version_id = storage
            .get_object("versions", "doc.txt")
            .unwrap()
            .version_id
            .expect("first version id should exist");
        storage
            .put_object(
                "versions",
                "doc.txt".to_string(),
                crate::models::Object::new(
                    "doc.txt".to_string(),
                    b"current".to_vec(),
                    "text/plain".to_string(),
                ),
            )
            .unwrap();
        let version_count = storage
            .list_object_versions_for_key("versions", "doc.txt")
            .unwrap()
            .len();

        // Act and assert: an encoded parent component cannot alias the current object.
        for method in ["GET", "HEAD", "DELETE"] {
            let response = adapter
                .handle_request(
                    &storage.clone(),
                    &auth_disabled(),
                    &parsed_request(
                        method,
                        "http://localhost/devstoreaccount1/versions/doc.txt?versionid=%2E%2E",
                        &[("x-ms-version", AZURE_VERSION)],
                        b"",
                    )
                    .await,
                )
                .expect("unsafe Azure version selector should respond");
            assert_eq!(response.status(), StatusCode::NOT_FOUND);
            assert_eq!(
                header_value(&response, "x-ms-error-code"),
                Some("BlobNotFound")
            );
        }

        let future = (Utc::now() + chrono::Duration::days(365)).to_rfc2822();
        for request in [
            parsed_request(
                "PUT",
                &format!(
                    "http://localhost/devstoreaccount1/versions/doc.txt?comp=immutabilityPolicies&versionid={first_version_id}"
                ),
                &[
                    ("x-ms-version", AZURE_VERSION),
                    ("x-ms-immutability-policy-until-date", &future),
                    ("x-ms-immutability-policy-mode", "Locked"),
                ],
                b"",
            )
            .await,
            parsed_request(
                "PUT",
                "http://localhost/devstoreaccount1/versions/doc.txt?comp=legalhold&snapshot=2026-01-01T00%3A00%3A00Z",
                &[
                    ("x-ms-version", AZURE_VERSION),
                    ("x-ms-legal-hold", "true"),
                ],
                b"",
            )
            .await,
        ] {
            let response = adapter
                .handle_request(&storage.clone(), &auth_disabled(), &request)
                .expect("unsupported scoped retention should respond");
            assert_eq!(response.status(), StatusCode::NOT_IMPLEMENTED);
            assert_eq!(
                header_value(&response, "x-ms-error-code"),
                Some("FeatureNotSupported")
            );
        }

        let current = storage.get_object("versions", "doc.txt").unwrap();
        assert_eq!(current.data, b"current");
        assert!(!current
            .provider_metadata
            .contains_key(AZURE_IMMUTABILITY_MODE_KEY));
        assert!(!current.provider_metadata.contains_key(AZURE_LEGAL_HOLD_KEY));
        assert_eq!(
            storage
                .get_object_version("versions", "doc.txt", &first_version_id)
                .unwrap()
                .data,
            b"first"
        );
        assert_eq!(
            storage
                .list_object_versions_for_key("versions", "doc.txt")
                .unwrap()
                .len(),
            version_count
        );
    }

    #[allow(
        clippy::too_many_lines,
        reason = "the table-driven selector regression keeps every accepted Azure mutation surface and its shared no-side-effect assertions together"
    )]
    #[tokio::test(flavor = "multi_thread")]
    async fn should_reject_azure_selectors_on_unsupported_mutations_without_side_effects() {
        // Arrange
        let adapter = AzureBlobAdapter::new();
        let storage = temp_storage();
        let container = "selector-mutations";
        let key = "doc.txt";
        storage.create_bucket(container.to_string()).unwrap();
        storage.enable_versioning(container).unwrap();
        storage
            .update_bucket_metadata(
                container,
                HashMap::from([(AZURE_VERSIONING_KEY.to_string(), "true".to_string())]),
            )
            .unwrap();
        let mut selected_version = None;
        for data in [b"first".as_slice(), b"current".as_slice()] {
            let mut object = crate::models::Object::new(
                key.to_string(),
                data.to_vec(),
                "text/plain".to_string(),
            );
            AzureBlobAdapter::set_blob_type(&mut object, "BlockBlob");
            storage
                .put_object(container, key.to_string(), object)
                .unwrap();
            selected_version.get_or_insert_with(|| {
                storage
                    .get_object(container, key)
                    .unwrap()
                    .version_id
                    .expect("first version ID should exist")
            });
        }
        let selected_version = selected_version.expect("historical version ID should exist");
        let observed = storage.get_object(container, key).unwrap();
        let versions_before = storage
            .list_object_versions_for_key(container, key)
            .unwrap()
            .len();
        let snapshot = "2026-08-10T12%3A00%3A00.0000000Z";
        let block_id = BASE64.encode("block-0001");
        let base = format!("http://localhost/devstoreaccount1/{container}/{key}");

        // Act and assert: every unsupported selected-resource mutation is rejected
        // before parsing, staging, leasing, or changing the current object.
        let requests = [
            (
                parsed_request(
                    "PUT",
                    &format!("{base}?versionid={selected_version}"),
                    &[
                        ("x-ms-version", AZURE_VERSION),
                        ("x-ms-blob-type", "BlockBlob"),
                    ],
                    b"replacement",
                )
                .await,
                StatusCode::BAD_REQUEST,
                "InvalidQueryParameterValue",
            ),
            (
                parsed_request(
                    "PUT",
                    &format!("{base}?snapshot={snapshot}"),
                    &[
                        ("x-ms-version", AZURE_VERSION),
                        ("x-ms-blob-type", "BlockBlob"),
                    ],
                    b"replacement",
                )
                .await,
                StatusCode::BAD_REQUEST,
                "InvalidQueryParameterValue",
            ),
            (
                parsed_request(
                    "PUT",
                    &format!("{base}?comp=lease&versionid={selected_version}"),
                    &[
                        ("x-ms-version", AZURE_VERSION),
                        ("x-ms-lease-action", "acquire"),
                        ("x-ms-lease-duration", "-1"),
                    ],
                    b"",
                )
                .await,
                StatusCode::NOT_IMPLEMENTED,
                "FeatureNotSupported",
            ),
            (
                parsed_request(
                    "PUT",
                    &format!("{base}?comp=metadata&snapshot={snapshot}"),
                    &[
                        ("x-ms-version", AZURE_VERSION),
                        ("x-ms-meta-owner", "attacker"),
                    ],
                    b"",
                )
                .await,
                StatusCode::NOT_IMPLEMENTED,
                "FeatureNotSupported",
            ),
            (
                parsed_request(
                    "PUT",
                    &format!("{base}?comp=block&blockid={block_id}&versionid={selected_version}"),
                    &[("x-ms-version", AZURE_VERSION)],
                    b"staged",
                )
                .await,
                StatusCode::NOT_IMPLEMENTED,
                "FeatureNotSupported",
            ),
            (
                parsed_request(
                    "PUT",
                    &format!("{base}?comp=blocklist&snapshot={snapshot}"),
                    &[("x-ms-version", AZURE_VERSION)],
                    b"<BlockList><Latest>not-base64</Latest></BlockList>",
                )
                .await,
                StatusCode::NOT_IMPLEMENTED,
                "FeatureNotSupported",
            ),
            (
                parsed_request(
                    "PUT",
                    &format!("{base}?comp=appendblock&versionid={selected_version}"),
                    &[("x-ms-version", AZURE_VERSION)],
                    b"append",
                )
                .await,
                StatusCode::NOT_IMPLEMENTED,
                "FeatureNotSupported",
            ),
            (
                parsed_request(
                    "PUT",
                    &format!("{base}?comp=page&snapshot={snapshot}"),
                    &[
                        ("x-ms-version", AZURE_VERSION),
                        ("x-ms-page-write", "clear"),
                        ("x-ms-range", "bytes=0-511"),
                    ],
                    b"",
                )
                .await,
                StatusCode::NOT_IMPLEMENTED,
                "FeatureNotSupported",
            ),
            (
                parsed_request(
                    "PUT",
                    &format!("{base}?comp=snapshot&versionid={selected_version}"),
                    &[("x-ms-version", AZURE_VERSION)],
                    b"",
                )
                .await,
                StatusCode::NOT_IMPLEMENTED,
                "FeatureNotSupported",
            ),
            (
                parsed_request(
                    "PUT",
                    &format!("{base}?versionid={selected_version}"),
                    &[
                        ("x-ms-version", AZURE_VERSION),
                        ("x-ms-copy-source", "/source/blob.txt"),
                    ],
                    b"",
                )
                .await,
                StatusCode::BAD_REQUEST,
                "InvalidQueryParameterValue",
            ),
            (
                parsed_request(
                    "DELETE",
                    &format!("{base}?comp=lease&versionid={selected_version}"),
                    &[("x-ms-version", AZURE_VERSION)],
                    b"",
                )
                .await,
                StatusCode::NOT_IMPLEMENTED,
                "FeatureNotSupported",
            ),
        ];
        for (request, expected_status, expected_error_code) in requests {
            let response = adapter
                .handle_request(&storage.clone(), &auth_disabled(), &request)
                .expect("unsupported selected mutation should respond");
            assert_eq!(response.status(), expected_status);
            assert_eq!(
                header_value(&response, "x-ms-error-code"),
                Some(expected_error_code)
            );
        }

        let current = storage.get_object(container, key).unwrap();
        assert_eq!(current.data, observed.data);
        assert_eq!(current.etag, observed.etag);
        assert_eq!(current.last_modified, observed.last_modified);
        assert_eq!(current.version_id, observed.version_id);
        assert_eq!(current.metadata, observed.metadata);
        assert_eq!(current.provider_metadata, observed.provider_metadata);
        assert_eq!(
            storage
                .list_object_versions_for_key(container, key)
                .unwrap()
                .len(),
            versions_before
        );
        let session_key = AzureBlobAdapter::blob_state_key("devstoreaccount1", container, key);
        assert!(adapter
            .load_block_session(&storage, &session_key)
            .unwrap()
            .is_none());
    }

    #[tokio::test(flavor = "multi_thread")]
    #[allow(clippy::too_many_lines)] // Every metadata-only Azure control operation must preserve one shared identity.
    async fn should_preserve_version_identity_for_azure_lease_and_worm_metadata() {
        // Arrange
        let adapter = AzureBlobAdapter::new();
        let storage = temp_storage();
        storage.create_bucket("identity".to_string()).unwrap();
        storage.enable_versioning("identity").unwrap();
        storage
            .update_bucket_metadata(
                "identity",
                HashMap::from([(AZURE_VERSIONING_KEY.to_string(), "true".to_string())]),
            )
            .unwrap();
        storage
            .put_object(
                "identity",
                "doc.txt".to_string(),
                crate::models::Object::new(
                    "doc.txt".to_string(),
                    b"preserve".to_vec(),
                    "text/plain".to_string(),
                ),
            )
            .unwrap();
        let observed = storage.get_object("identity", "doc.txt").unwrap();
        let version_count = storage
            .list_object_versions_for_key("identity", "doc.txt")
            .unwrap()
            .len();
        let lease_id = "5f8dc9d6-7a92-4ee0-8c1a-a169952b6218";
        let blob_uri = "http://localhost/devstoreaccount1/identity/doc.txt";

        // Act: lease operations change only lease metadata.
        for (action, expected_status) in [
            ("acquire", StatusCode::CREATED),
            ("renew", StatusCode::OK),
            ("release", StatusCode::OK),
        ] {
            let mut headers = vec![
                ("x-ms-version", AZURE_VERSION),
                ("x-ms-lease-action", action),
            ];
            if action == "acquire" {
                headers.push(("x-ms-proposed-lease-id", lease_id));
                headers.push(("x-ms-lease-duration", "-1"));
            } else {
                headers.push(("x-ms-lease-id", lease_id));
            }
            let response = adapter
                .handle_request(
                    &storage.clone(),
                    &auth_disabled(),
                    &parsed_request("PUT", &format!("{blob_uri}?comp=lease"), &headers, b"").await,
                )
                .expect("lease operation should respond");
            assert_eq!(response.status(), expected_status);
            assert!(response.headers().get("x-ms-version-id").is_none());
        }

        // Immutability and legal-hold changes target the same current version.
        let future = crate::utils::headers::format_last_modified_at(
            &(Utc::now() + chrono::Duration::days(365)),
        );
        let policy_uri = format!("{blob_uri}?comp=immutabilityPolicies");
        for request in [
            parsed_request(
                "PUT",
                &policy_uri,
                &[
                    ("x-ms-version", AZURE_VERSION),
                    ("x-ms-immutability-policy-until-date", &future),
                    ("x-ms-immutability-policy-mode", "Unlocked"),
                ],
                b"",
            )
            .await,
            parsed_request(
                "PUT",
                &format!("{blob_uri}?comp=legalhold"),
                &[("x-ms-version", AZURE_VERSION), ("x-ms-legal-hold", "true")],
                b"",
            )
            .await,
            parsed_request(
                "PUT",
                &format!("{blob_uri}?comp=legalhold"),
                &[
                    ("x-ms-version", AZURE_VERSION),
                    ("x-ms-legal-hold", "false"),
                ],
                b"",
            )
            .await,
            parsed_request(
                "DELETE",
                &policy_uri,
                &[("x-ms-version", AZURE_VERSION)],
                b"",
            )
            .await,
        ] {
            let response = adapter
                .handle_request(&storage.clone(), &auth_disabled(), &request)
                .expect("WORM metadata operation should respond");
            assert_eq!(response.status(), StatusCode::OK);
            assert!(response.headers().get("x-ms-version-id").is_none());
        }

        // Assert
        let current = storage.get_object("identity", "doc.txt").unwrap();
        assert_eq!(current.data, observed.data);
        assert_eq!(current.etag, observed.etag);
        assert_eq!(current.last_modified, observed.last_modified);
        assert_eq!(current.version_id, observed.version_id);
        assert_eq!(
            storage
                .list_object_versions_for_key("identity", "doc.txt")
                .unwrap()
                .len(),
            version_count
        );
    }

    async fn create_azure_container(
        adapter: &AzureBlobAdapter,
        storage: &Arc<dyn Storage>,
        container: &str,
    ) {
        adapter
            .handle_request(
                &storage.clone(),
                &auth_disabled(),
                &parsed_request(
                    "PUT",
                    &format!("http://localhost/devstoreaccount1/{container}?restype=container"),
                    &[("x-ms-version", AZURE_VERSION)],
                    b"",
                )
                .await,
            )
            .expect("container create should succeed");
    }

    async fn acquire_azure_lease_for(
        adapter: &AzureBlobAdapter,
        storage: &Arc<dyn Storage>,
        blob_uri: &str,
        lease_id: &str,
    ) {
        let response = adapter
            .handle_request(
                &storage.clone(),
                &auth_disabled(),
                &parsed_request(
                    "PUT",
                    &format!("{blob_uri}?comp=lease"),
                    &[
                        ("x-ms-version", AZURE_VERSION),
                        ("x-ms-lease-action", "acquire"),
                        ("x-ms-lease-duration", "-1"),
                        ("x-ms-proposed-lease-id", lease_id),
                    ],
                    b"",
                )
                .await,
            )
            .expect("lease acquire should respond");
        assert_eq!(response.status(), StatusCode::CREATED);
        assert_eq!(header_value(&response, "x-ms-lease-id"), Some(lease_id));
    }

    async fn commit_azure_block_blob(
        adapter: &AzureBlobAdapter,
        storage: &Arc<dyn Storage>,
    ) -> (String, String) {
        let block_one = BASE64.encode("block-a");
        let block_two = BASE64.encode("block-b");
        for (block_id, payload) in [
            (&block_one, b"hello ".as_slice()),
            (&block_two, b"azure".as_slice()),
        ] {
            adapter
                .handle_request(
                    &storage.clone(),
                    &auth_disabled(),
                    &parsed_request(
                        "PUT",
                        &format!(
                            "http://localhost/devstoreaccount1/media/greeting.txt?comp=block&blockid={}",
                            urlencoding::encode(block_id)
                        ),
                        &[("x-ms-version", AZURE_VERSION)],
                        payload,
                    )
                    .await,
                )
                .expect("put block should succeed");
        }

        let block_list = format!(
            "<?xml version=\"1.0\" encoding=\"utf-8\"?><BlockList><Latest>{block_one}</Latest><Latest>{block_two}</Latest></BlockList>"
        );
        adapter
            .handle_request(
                &storage.clone(),
                &auth_disabled(),
                &parsed_request(
                    "PUT",
                    "http://localhost/devstoreaccount1/media/greeting.txt?comp=blocklist",
                    &[
                        ("x-ms-version", AZURE_VERSION),
                        ("content-type", "application/xml"),
                    ],
                    block_list.as_bytes(),
                )
                .await,
            )
            .expect("block list commit should succeed");
        (block_one, block_two)
    }

    async fn verify_azure_block_list(
        adapter: &AzureBlobAdapter,
        storage: &Arc<dyn Storage>,
        block_one: &str,
        block_two: &str,
    ) {
        let response = adapter
            .handle_request(
                &storage.clone(),
                &auth_disabled(),
                &parsed_request(
                    "GET",
                    "http://localhost/devstoreaccount1/media/greeting.txt?comp=blocklist",
                    &[("x-ms-version", AZURE_VERSION)],
                    b"",
                )
                .await,
            )
            .expect("block list fetch should succeed");
        let stored = storage
            .get_object("media", "greeting.txt")
            .expect("committed block blob should exist");
        let expected_etag = format!("\"{}\"", stored.etag);
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            header_value(&response, "etag"),
            Some(expected_etag.as_str())
        );
        assert!(header_value(&response, "last-modified").is_some());
        assert_eq!(
            header_value(&response, "x-ms-blob-content-length"),
            Some("11")
        );
        let xml = String::from_utf8(read_test_body(response).await).expect("xml");
        assert!(xml.contains(block_one));
        assert!(xml.contains(block_two));
        assert!(xml.contains("<Size>6</Size>"));
        assert!(xml.contains("<Size>5</Size>"));
        assert!(xml.contains("<CommittedBlocks>"));
        assert!(!xml.contains("<UncommittedBlocks>"));
    }

    async fn update_and_verify_azure_metadata(
        adapter: &AzureBlobAdapter,
        storage: &Arc<dyn Storage>,
    ) {
        let response = adapter
            .handle_request(
                &storage.clone(),
                &auth_disabled(),
                &parsed_request(
                    "PUT",
                    "http://localhost/devstoreaccount1/media/greeting.txt?comp=metadata",
                    &[("x-ms-version", AZURE_VERSION), ("x-ms-meta-owner", "bob")],
                    b"",
                )
                .await,
            )
            .expect("metadata update should succeed");
        assert_eq!(response.status(), StatusCode::OK);

        let response = adapter
            .handle_request(
                &storage.clone(),
                &auth_disabled(),
                &parsed_request(
                    "HEAD",
                    "http://localhost/devstoreaccount1/media/greeting.txt",
                    &[("x-ms-version", AZURE_VERSION)],
                    b"",
                )
                .await,
            )
            .expect("head should succeed");
        assert_eq!(header_value(&response, "x-ms-meta-owner"), Some("bob"));
    }

    async fn verify_azure_range_read(adapter: &AzureBlobAdapter, storage: &Arc<dyn Storage>) {
        let response = adapter
            .handle_request(
                &storage.clone(),
                &auth_disabled(),
                &parsed_request(
                    "GET",
                    "http://localhost/devstoreaccount1/media/greeting.txt",
                    &[
                        ("x-ms-version", AZURE_VERSION),
                        ("x-ms-range", "bytes=6-10"),
                    ],
                    b"",
                )
                .await,
            )
            .expect("range get should succeed");
        assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(
            header_value(&response, "content-range"),
            Some("bytes 6-10/11")
        );
        assert_eq!(header_value(&response, "x-ms-meta-owner"), Some("bob"));
        assert_eq!(read_test_body(response).await.as_slice(), b"azure");
    }

    async fn verify_append_blob_writes(adapter: &AzureBlobAdapter, storage: &Arc<dyn Storage>) {
        let response = adapter
            .handle_request(
                &storage.clone(),
                &auth_disabled(),
                &parsed_request(
                    "PUT",
                    "http://localhost/devstoreaccount1/state/events.log",
                    &[
                        ("x-ms-version", AZURE_VERSION),
                        ("x-ms-blob-type", "AppendBlob"),
                    ],
                    b"",
                )
                .await,
            )
            .expect("append blob create should succeed");
        assert_eq!(response.status(), StatusCode::CREATED);
        assert_eq!(
            header_value(&response, "x-ms-blob-type"),
            Some("AppendBlob")
        );

        adapter
            .handle_request(
                &storage.clone(),
                &auth_disabled(),
                &parsed_request(
                    "PUT",
                    "http://localhost/devstoreaccount1/state/events.log?comp=appendblock",
                    &[("x-ms-version", AZURE_VERSION)],
                    b"hello",
                )
                .await,
            )
            .expect("first append block should succeed");

        adapter
            .handle_request(
                &storage.clone(),
                &auth_disabled(),
                &parsed_request(
                    "PUT",
                    "http://localhost/devstoreaccount1/state/events.log?comp=appendblock",
                    &[("x-ms-version", AZURE_VERSION)],
                    b" world",
                )
                .await,
            )
            .expect("append block should succeed");

        let response = adapter
            .handle_request(
                &storage.clone(),
                &auth_disabled(),
                &parsed_request(
                    "GET",
                    "http://localhost/devstoreaccount1/state/events.log",
                    &[("x-ms-version", AZURE_VERSION)],
                    b"",
                )
                .await,
            )
            .expect("append blob get should succeed");
        assert_eq!(read_test_body(response).await.as_slice(), b"hello world");
    }

    async fn verify_page_blob_writes(adapter: &AzureBlobAdapter, storage: &Arc<dyn Storage>) {
        let response = adapter
            .handle_request(
                &storage.clone(),
                &auth_disabled(),
                &parsed_request(
                    "PUT",
                    "http://localhost/devstoreaccount1/state/page.bin",
                    &[
                        ("x-ms-version", AZURE_VERSION),
                        ("x-ms-blob-type", "PageBlob"),
                        ("x-ms-blob-content-length", "512"),
                    ],
                    b"",
                )
                .await,
            )
            .expect("page blob create should succeed");
        assert_eq!(response.status(), StatusCode::CREATED);
        assert_eq!(header_value(&response, "x-ms-blob-type"), Some("PageBlob"));

        adapter
            .handle_request(
                &storage.clone(),
                &auth_disabled(),
                &parsed_request(
                    "PUT",
                    "http://localhost/devstoreaccount1/state/page.bin?comp=page",
                    &[
                        ("x-ms-version", AZURE_VERSION),
                        ("x-ms-range", "bytes=0-511"),
                        ("x-ms-page-write", "update"),
                    ],
                    &vec![b'a'; 512],
                )
                .await,
            )
            .expect("page write should succeed");

        let response = adapter
            .handle_request(
                &storage.clone(),
                &auth_disabled(),
                &parsed_request(
                    "GET",
                    "http://localhost/devstoreaccount1/state/page.bin",
                    &[("x-ms-version", AZURE_VERSION), ("x-ms-range", "bytes=0-7")],
                    b"",
                )
                .await,
            )
            .expect("page blob range get should succeed");
        assert_eq!(read_test_body(response).await.as_slice(), b"aaaaaaaa");
    }

    async fn create_azure_lease_blob(adapter: &AzureBlobAdapter, storage: &Arc<dyn Storage>) {
        adapter
            .handle_request(
                &storage.clone(),
                &auth_disabled(),
                &parsed_request(
                    "PUT",
                    "http://localhost/devstoreaccount1/state/lease.txt",
                    &[
                        ("x-ms-version", AZURE_VERSION),
                        ("x-ms-blob-type", "BlockBlob"),
                    ],
                    b"initial",
                )
                .await,
            )
            .expect("blob create should succeed");
    }

    async fn acquire_deny_and_release_azure_lease(
        adapter: &AzureBlobAdapter,
        storage: &Arc<dyn Storage>,
    ) {
        let lease_response = adapter
            .handle_request(
                &storage.clone(),
                &auth_disabled(),
                &parsed_request(
                    "PUT",
                    "http://localhost/devstoreaccount1/state/lease.txt?comp=lease",
                    &[
                        ("x-ms-version", AZURE_VERSION),
                        ("x-ms-lease-action", "acquire"),
                        ("x-ms-lease-duration", "-1"),
                    ],
                    b"",
                )
                .await,
            )
            .expect("lease acquire should succeed");
        assert_eq!(lease_response.status(), StatusCode::CREATED);
        let lease_id = header_value(&lease_response, "x-ms-lease-id")
            .expect("lease id")
            .to_string();

        let denied = adapter
            .handle_request(
                &storage.clone(),
                &auth_disabled(),
                &parsed_request(
                    "DELETE",
                    "http://localhost/devstoreaccount1/state/lease.txt",
                    &[("x-ms-version", AZURE_VERSION)],
                    b"",
                )
                .await,
            )
            .expect("delete should return a response");
        assert_eq!(denied.status(), StatusCode::PRECONDITION_FAILED);

        let release = adapter
            .handle_request(
                &storage.clone(),
                &auth_disabled(),
                &parsed_request(
                    "PUT",
                    "http://localhost/devstoreaccount1/state/lease.txt?comp=lease",
                    &[
                        ("x-ms-version", AZURE_VERSION),
                        ("x-ms-lease-action", "release"),
                        ("x-ms-lease-id", &lease_id),
                    ],
                    b"",
                )
                .await,
            )
            .expect("lease release should succeed");
        assert_eq!(release.status(), StatusCode::OK);
    }

    async fn create_overwrite_and_read_azure_snapshot(
        adapter: &AzureBlobAdapter,
        storage: &Arc<dyn Storage>,
    ) {
        let snapshot = adapter
            .handle_request(
                &storage.clone(),
                &auth_disabled(),
                &parsed_request(
                    "PUT",
                    "http://localhost/devstoreaccount1/state/lease.txt?comp=snapshot",
                    &[("x-ms-version", AZURE_VERSION)],
                    b"",
                )
                .await,
            )
            .expect("snapshot create should succeed");
        assert_eq!(snapshot.status(), StatusCode::CREATED);
        let snapshot_id = header_value(&snapshot, "x-ms-snapshot")
            .expect("snapshot id")
            .to_string();

        adapter
            .handle_request(
                &storage.clone(),
                &auth_disabled(),
                &parsed_request(
                    "PUT",
                    "http://localhost/devstoreaccount1/state/lease.txt",
                    &[
                        ("x-ms-version", AZURE_VERSION),
                        ("x-ms-blob-type", "BlockBlob"),
                    ],
                    b"updated",
                )
                .await,
            )
            .expect("overwrite should succeed");

        let snapshot_get = adapter
            .handle_request(
                &storage.clone(),
                &auth_disabled(),
                &parsed_request(
                    "GET",
                    &format!(
                        "http://localhost/devstoreaccount1/state/lease.txt?snapshot={snapshot_id}"
                    ),
                    &[("x-ms-version", AZURE_VERSION)],
                    b"",
                )
                .await,
            )
            .expect("snapshot get should succeed");
        assert_eq!(read_test_body(snapshot_get).await.as_slice(), b"initial");
    }

    async fn verify_azure_immutability_and_legal_hold(
        adapter: &AzureBlobAdapter,
        storage: &Arc<dyn Storage>,
    ) {
        let retention_response = adapter
            .handle_request(
                &storage.clone(),
                &auth_disabled(),
                &parsed_request(
                    "PUT",
                    "http://localhost/devstoreaccount1/state/lease.txt?comp=immutabilityPolicies",
                    &[
                        ("x-ms-version", AZURE_VERSION),
                        (
                            "x-ms-immutability-policy-until-date",
                            "Thu, 01 Jan 2099 00:00:00 GMT",
                        ),
                        ("x-ms-immutability-policy-mode", "Unlocked"),
                    ],
                    b"",
                )
                .await,
            )
            .expect("immutability policy should succeed");
        assert_eq!(retention_response.status(), StatusCode::OK);

        let legal_hold = adapter
            .handle_request(
                &storage.clone(),
                &auth_disabled(),
                &parsed_request(
                    "PUT",
                    "http://localhost/devstoreaccount1/state/lease.txt?comp=legalhold",
                    &[("x-ms-version", AZURE_VERSION), ("x-ms-legal-hold", "true")],
                    b"",
                )
                .await,
            )
            .expect("legal hold should succeed");
        assert_eq!(legal_hold.status(), StatusCode::OK);

        let immutable_delete = adapter
            .handle_request(
                &storage.clone(),
                &auth_disabled(),
                &parsed_request(
                    "DELETE",
                    "http://localhost/devstoreaccount1/state/lease.txt",
                    &[("x-ms-version", AZURE_VERSION)],
                    b"",
                )
                .await,
            )
            .expect("immutable delete should return a response");
        assert_eq!(immutable_delete.status(), StatusCode::CONFLICT);

        let head = adapter
            .handle_request(
                &storage.clone(),
                &auth_disabled(),
                &parsed_request(
                    "HEAD",
                    "http://localhost/devstoreaccount1/state/lease.txt",
                    &[("x-ms-version", AZURE_VERSION)],
                    b"",
                )
                .await,
            )
            .expect("head should succeed");
        assert_eq!(
            header_value(&head, "x-ms-immutability-policy-until-date"),
            Some("Thu, 01 Jan 2099 00:00:00 GMT")
        );
        assert_eq!(header_value(&head, "x-ms-legal-hold"), Some("true"));
    }

    // One integrated flow proves that decoding is identical across every Azure
    // blob verb and that no path component is normalized between operations.
    #[allow(clippy::too_many_lines)]
    #[tokio::test(flavor = "multi_thread")]
    async fn should_decode_azure_blob_paths_once_and_preserve_empty_segments() {
        // Arrange
        let adapter = AzureBlobAdapter::new();
        let storage = temp_storage();
        storage.create_bucket("encoded".to_string()).unwrap();
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
                        &format!("http://localhost/devstoreaccount1/encoded/{encoded}"),
                        &[
                            ("x-ms-version", AZURE_VERSION),
                            ("x-ms-blob-type", "BlockBlob"),
                            ("content-length", "7"),
                        ],
                        b"payload",
                    )
                    .await,
                )
                .expect("encoded blob PUT should respond");
            assert_eq!(response.status(), StatusCode::CREATED);
            assert_eq!(
                storage.get_object("encoded", decoded).unwrap().data,
                b"payload"
            );
        }
        let get = adapter
            .handle_request(
                &storage,
                &auth_disabled(),
                &parsed_request(
                    "GET",
                    "http://localhost/devstoreaccount1/encoded/a%20b",
                    &[("x-ms-version", AZURE_VERSION)],
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
                    "http://localhost/devstoreaccount1/encoded/a%20b",
                    &[("x-ms-version", AZURE_VERSION)],
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
                    "http://localhost/devstoreaccount1/encoded?restype=container&comp=list",
                    &[("x-ms-version", AZURE_VERSION)],
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
                    "http://localhost/devstoreaccount1/encoded/a%20b",
                    &[("x-ms-version", AZURE_VERSION)],
                    b"",
                )
                .await,
            )
            .unwrap();
        let malformed = adapter
            .handle_request(
                &storage,
                &auth_disabled(),
                &parsed_request(
                    "PUT",
                    "http://localhost/devstoreaccount1/encoded/bad%ZZ",
                    &[
                        ("x-ms-version", AZURE_VERSION),
                        ("x-ms-blob-type", "BlockBlob"),
                        ("content-length", "7"),
                    ],
                    b"payload",
                )
                .await,
            )
            .unwrap();

        // Assert
        assert_eq!(get.status(), StatusCode::OK);
        assert_eq!(read_test_body(get).await, b"payload");
        assert_eq!(head.status(), StatusCode::OK);
        assert_eq!(delete.status(), StatusCode::ACCEPTED);
        assert!(storage.get_object("encoded", "a b").is_err());
        assert!(String::from_utf8(read_test_body(list).await)
            .unwrap()
            .contains("<Name>a b</Name>"));
        assert_eq!(malformed.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            header_value(&malformed, "x-ms-error-code"),
            Some("InvalidUri")
        );
        assert!(storage.get_object("encoded", "bad%ZZ").is_err());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn should_return_container_not_found_for_every_missing_container_blob_verb() {
        // Arrange
        let adapter = AzureBlobAdapter::new();
        let storage = temp_storage();

        // Act
        let mut responses = Vec::new();
        for method in ["GET", "HEAD", "PUT", "DELETE"] {
            let response = adapter
                .handle_request(
                    &storage,
                    &auth_disabled(),
                    &parsed_request(
                        method,
                        "http://localhost/devstoreaccount1/absent/blob",
                        &[
                            ("x-ms-version", AZURE_VERSION),
                            ("x-ms-blob-type", "BlockBlob"),
                            ("content-length", "0"),
                        ],
                        b"",
                    )
                    .await,
                )
                .unwrap();
            responses.push((method, response));
        }

        // Assert
        for (method, response) in responses {
            assert_eq!(response.status(), StatusCode::NOT_FOUND);
            assert_eq!(
                header_value(&response, "x-ms-error-code"),
                Some("ContainerNotFound")
            );
            let body = read_test_body(response).await;
            if method == "HEAD" {
                assert!(body.is_empty());
            } else {
                assert!(String::from_utf8(body)
                    .unwrap()
                    .contains("<Code>ContainerNotFound</Code>"));
            }
        }
        assert!(storage.get_bucket("absent").is_err());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn should_reject_unsupported_azure_subresources_before_mutation() {
        // Arrange
        let adapter = AzureBlobAdapter::new();
        let storage = temp_storage();
        storage.create_bucket("subresources".to_string()).unwrap();
        storage
            .put_object(
                "subresources",
                "blob".to_string(),
                crate::models::Object::new(
                    "blob".to_string(),
                    b"original".to_vec(),
                    "application/octet-stream".to_string(),
                ),
            )
            .unwrap();

        // Act
        let mut blobs = Vec::new();
        for (comp, body) in [
            ("tags", b"<Tags>new</Tags>".as_slice()),
            ("immutabilitypolicy", b"".as_slice()),
        ] {
            blobs.push(
                adapter
                    .handle_request(
                        &storage,
                        &auth_disabled(),
                        &parsed_request(
                            "PUT",
                            &format!(
                                "http://localhost/devstoreaccount1/subresources/blob?comp={comp}"
                            ),
                            &[("x-ms-version", AZURE_VERSION)],
                            body,
                        )
                        .await,
                    )
                    .unwrap(),
            );
        }
        let mut containers = Vec::new();
        for (method, comp) in [("PUT", "metadata"), ("DELETE", "lease"), ("GET", "acl")] {
            containers.push(
                adapter
                    .handle_request(
                        &storage,
                        &auth_disabled(),
                        &parsed_request(
                            method,
                            &format!(
                                "http://localhost/devstoreaccount1/subresources?restype=container&comp={comp}"
                            ),
                            &[("x-ms-version", AZURE_VERSION)],
                            b"",
                        )
                        .await,
                    )
                    .unwrap(),
            );
        }

        // Assert
        for response in blobs.into_iter().chain(containers) {
            assert_eq!(response.status(), StatusCode::NOT_IMPLEMENTED);
            assert_eq!(
                header_value(&response, "x-ms-error-code"),
                Some("FeatureNotSupported")
            );
        }
        assert_eq!(
            storage.get_object("subresources", "blob").unwrap().data,
            b"original"
        );
        assert!(storage.get_bucket("subresources").is_ok());
        assert!(state::load_json::<AzureContainerDeletion>(
            storage.as_ref(),
            AZURE_CONTAINER_DELETION_STATE,
            "subresources",
        )
        .unwrap()
        .is_none());
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
}
