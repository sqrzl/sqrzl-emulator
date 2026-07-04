use super::ProviderAdapter;
use crate::auth::{AuthConfig, HttpRequestLike};
use crate::blob::{BlobBackend, BlobRange, CreateUploadSessionRequest, UpdateBlobMetadataRequest};
use crate::body::Body;
use crate::server::{RequestExt as Request, ResponseBuilder};
use crate::storage::Storage;
use crate::utils::request_origin;
use crate::utils::xml::push_escaped_xml;
use base64::{
    engine::general_purpose::{STANDARD as BASE64, URL_SAFE_NO_PAD},
    Engine as _,
};
use chrono::{DateTime, Utc};
use hmac::{Hmac, KeyInit, Mac};
use http::{Method, StatusCode};
use hyper::Response;
use quick_xml::escape::unescape;
use quick_xml::events::Event;
use quick_xml::Reader;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use std::collections::HashMap;
use std::fmt::Write as _;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

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

#[derive(Clone, Default, Serialize, Deserialize)]
struct AzureBlockSession {
    blocks: HashMap<String, Vec<u8>>,
    content_type: Option<String>,
    metadata: HashMap<String, String>,
}

#[derive(Debug, Clone)]
struct AzureResource {
    account: String,
    container: Option<String>,
    blob: Option<String>,
}

pub struct AzureBlobAdapter {
    block_sessions: Mutex<HashMap<String, AzureBlockSession>>,
    committed_blocks: Mutex<HashMap<String, Vec<String>>>,
}

impl Default for AzureBlobAdapter {
    fn default() -> Self {
        Self::new()
    }
}

impl AzureBlobAdapter {
    pub fn new() -> Self {
        Self {
            block_sessions: Mutex::new(HashMap::new()),
            committed_blocks: Mutex::new(HashMap::new()),
        }
    }

    fn blob_state_key(account: &str, container: &str, blob: &str) -> String {
        format!("{}/{}/{}", account, container, blob)
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

    fn parse_resource(req: &Request) -> Result<AzureResource, String> {
        let parts: Vec<&str> = req
            .path()
            .trim_start_matches('/')
            .split('/')
            .filter(|segment| !segment.is_empty())
            .collect();

        if parts.is_empty() {
            return Err("Azure requests must include an account segment".to_string());
        }

        Ok(AzureResource {
            account: parts[0].to_string(),
            container: parts.get(1).map(|segment| (*segment).to_string()),
            blob: if parts.len() > 2 {
                Some(parts[2..].join("/"))
            } else {
                None
            },
        })
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

    fn list_containers_xml(
        req: &Request,
        account: &str,
        namespaces: &[crate::blob::Namespace],
    ) -> String {
        let origin = request_origin(req);
        let mut xml = String::with_capacity(160 + namespaces.len() * 192);
        xml.push_str(
            "<?xml version=\"1.0\" encoding=\"utf-8\"?><EnumerationResults ServiceEndpoint=\"",
        );
        push_escaped_xml(&mut xml, &origin);
        xml.push('/');
        push_escaped_xml(&mut xml, account);
        xml.push_str("\"><Containers>");

        for namespace in namespaces {
            xml.push_str("<Container><Name>");
            push_escaped_xml(&mut xml, &namespace.name);
            xml.push_str("</Name><Properties><Last-Modified>");
            xml.push_str(&namespace.created_at.to_rfc2822());
            xml.push_str("</Last-Modified><Etag>\"");
            xml.push_str(&crate::utils::headers::compute_etag(
                namespace.name.as_bytes(),
            ));
            xml.push_str("\"</Etag></Properties></Container>");
        }

        xml.push_str("</Containers><NextMarker /></EnumerationResults>");
        xml
    }

    fn list_blobs_xml(container: &str, blobs: &[crate::blob::BlobRecord]) -> String {
        let mut xml = String::with_capacity(192 + blobs.len() * 288);
        xml.push_str(
            "<?xml version=\"1.0\" encoding=\"utf-8\"?><EnumerationResults ContainerName=\"",
        );
        push_escaped_xml(&mut xml, container);
        xml.push_str("\"><Blobs>");

        for blob in blobs
            .iter()
            .filter(|blob| !Self::is_snapshot_storage_key(&blob.key))
        {
            let blob_type = blob
                .provider_metadata
                .get(AZURE_BLOB_TYPE_KEY)
                .map(|value| value.as_str())
                .unwrap_or("BlockBlob");
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
            xml.push_str(&blob.last_modified.to_rfc2822());
            xml.push_str("</Last-Modified></Properties></Blob>");
        }

        xml.push_str("</Blobs><NextMarker /></EnumerationResults>");
        xml
    }

    fn block_list_xml(block_ids: &[String]) -> String {
        let mut xml = String::with_capacity(64 + block_ids.len() * 32);
        xml.push_str("<?xml version=\"1.0\" encoding=\"utf-8\"?><BlockList><CommittedBlocks>");
        for id in block_ids {
            xml.push_str("<Block><Name>");
            push_escaped_xml(&mut xml, id);
            xml.push_str("</Name><Size>0</Size></Block>");
        }
        xml.push_str("</CommittedBlocks><UncommittedBlocks /></BlockList>");
        xml
    }

    fn blob_type(blob: &crate::models::Object) -> &str {
        blob.provider_metadata
            .get(AZURE_BLOB_TYPE_KEY)
            .map(|value| value.as_str())
            .unwrap_or("BlockBlob")
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
        req.query_param("snapshot").map(|value| value.to_string())
    }

    fn snapshot_timestamp() -> String {
        Utc::now().format("%Y-%m-%dT%H:%M:%S%.6fZ").to_string()
    }

    fn lease_id(blob: &crate::models::Object) -> Option<&str> {
        blob.provider_metadata
            .get(AZURE_LEASE_ID_KEY)
            .map(|value| value.as_str())
    }

    fn lease_status(blob: &crate::models::Object) -> &str {
        blob.provider_metadata
            .get(AZURE_LEASE_STATUS_KEY)
            .map(|value| value.as_str())
            .unwrap_or("unlocked")
    }

    fn lease_state(blob: &crate::models::Object) -> &str {
        blob.provider_metadata
            .get(AZURE_LEASE_STATE_KEY)
            .map(|value| value.as_str())
            .unwrap_or("available")
    }

    fn lease_duration(blob: &crate::models::Object) -> Option<&str> {
        blob.provider_metadata
            .get(AZURE_LEASE_DURATION_KEY)
            .map(|value| value.as_str())
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
        blob.provider_metadata
            .get(AZURE_LEGAL_HOLD_KEY)
            .map(|value| value == "true")
            .unwrap_or(false)
    }

    fn is_immutable(blob: &crate::models::Object) -> bool {
        Self::has_legal_hold(blob)
            || Self::retention_until(blob)
                .map(|value| value > Utc::now())
                .unwrap_or(false)
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

    fn lookup_blob(
        storage: &Arc<dyn Storage>,
        container: &str,
        blob_key: &str,
        snapshot: Option<&str>,
    ) -> Result<crate::models::Object, String> {
        let key = snapshot
            .map(|value| Self::snapshot_storage_key(blob_key, value))
            .unwrap_or_else(|| blob_key.to_string());
        storage
            .get_object(container, &key)
            .map_err(|err| err.to_string())
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
    ) -> ResponseBuilder {
        let mut builder = Self::response(status)
            .header("accept-ranges", "bytes")
            .header("content-length", &body_len.to_string())
            .header("content-type", &blob.content_type)
            .header("etag", &format!("\"{}\"", blob.etag))
            .header("last-modified", &blob.last_modified.to_rfc2822())
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
        if let Some(duration) = Self::lease_duration(blob) {
            builder = builder.header("x-ms-lease-duration", duration);
        }
        for (key, value) in &blob.metadata {
            builder = builder.header(&format!("x-ms-meta-{}", key), value);
        }
        if let Some(content_range) = content_range {
            builder = builder.header("content-range", &content_range);
        }
        builder
    }

    fn canonicalized_headers(req: &Request) -> String {
        let mut headers: Vec<(String, String)> = req
            .headers()
            .into_iter()
            .filter(|(name, _)| name.starts_with("x-ms-"))
            .map(|(name, value)| {
                (
                    name.to_lowercase(),
                    value.split_whitespace().collect::<Vec<_>>().join(" "),
                )
            })
            .collect();
        headers.sort_by(|left, right| left.0.cmp(&right.0));

        headers
            .into_iter()
            .map(|(name, value)| format!("{}:{}\n", name, value))
            .collect::<String>()
    }

    fn canonicalized_resource(req: &Request, account: &str) -> String {
        let mut resource = format!("/{}{}", account, req.path());
        let mut query_map: HashMap<String, Vec<String>> = HashMap::new();
        for (key, value) in &req.query_params {
            query_map
                .entry(key.to_lowercase())
                .or_default()
                .push(value.to_string());
        }

        let mut keys: Vec<_> = query_map.keys().cloned().collect();
        keys.sort();
        for key in keys {
            let mut values = query_map.remove(&key).unwrap_or_default();
            values.sort();
            resource.push_str(&format!("\n{}:{}", key, values.join(",")));
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

        [
            req.method().as_str().to_string(),
            req.header("content-encoding").unwrap_or("").to_string(),
            req.header("content-language").unwrap_or("").to_string(),
            content_length,
            req.header("content-md5").unwrap_or("").to_string(),
            req.header("content-type").unwrap_or("").to_string(),
            String::new(),
            req.header("if-modified-since").unwrap_or("").to_string(),
            req.header("if-match").unwrap_or("").to_string(),
            req.header("if-none-match").unwrap_or("").to_string(),
            req.header("if-unmodified-since").unwrap_or("").to_string(),
            req.header("range").unwrap_or("").to_string(),
            Self::canonicalized_headers(req),
            Self::canonicalized_resource(req, account),
        ]
        .join("\n")
    }

    fn validate_shared_key(
        req: &Request,
        config: &AuthConfig,
        account: &str,
    ) -> Result<(), String> {
        let authorization = req
            .header("authorization")
            .ok_or_else(|| "Missing Authorization header".to_string())?;
        let prefix = format!("SharedKey {}:", account);
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

    fn parse_block_list(xml: &str) -> Result<Vec<String>, String> {
        let mut reader = Reader::from_str(xml);
        reader.config_mut().trim_text(true);
        let mut buf = Vec::new();
        let mut in_name = false;
        let mut block_ids = Vec::new();

        loop {
            match reader.read_event_into(&mut buf) {
                Ok(Event::Start(event)) => {
                    if matches!(
                        event.name().as_ref(),
                        b"Latest" | b"Committed" | b"Uncommitted"
                    ) {
                        in_name = true;
                    }
                }
                Ok(Event::End(event)) => {
                    if matches!(
                        event.name().as_ref(),
                        b"Latest" | b"Committed" | b"Uncommitted"
                    ) {
                        in_name = false;
                    }
                }
                Ok(Event::Text(text)) if in_name => {
                    let decoded = text.decode().map_err(|err| err.to_string())?;
                    let value = unescape(&decoded)
                        .map_err(|err| err.to_string())?
                        .to_string();
                    block_ids.push(value);
                }
                Ok(Event::Eof) => break,
                Err(err) => return Err(err.to_string()),
                _ => {}
            }
            buf.clear();
        }

        if block_ids.is_empty() {
            return Err("Block list cannot be empty".to_string());
        }

        Ok(block_ids)
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
                .map(|value| value.starts_with("SharedKey "))
                .unwrap_or(false)
            || req.header("x-ms-blob-type").is_some()
            || req.query_param("restype").is_some()
            || req.query_param("comp").is_some()
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

impl AzureBlobAdapter {
    async fn handle_request(
        &self,
        storage: Arc<dyn Storage>,
        auth_config: Arc<AuthConfig>,
        req: Request,
    ) -> Result<Response<Body>, String> {
        let resource = match Self::parse_resource(&req) {
            Ok(resource) => resource,
            Err(msg) => {
                return Ok(Self::error_response(
                    StatusCode::BAD_REQUEST,
                    "InvalidUri",
                    &msg,
                ))
            }
        };

        if let Err(response) = Self::authorize(&req, &auth_config, &resource) {
            return Ok(response);
        }

        if resource.container.is_none() {
            if req.method() == Method::GET && req.query_param("comp") == Some("list") {
                let namespaces = storage
                    .as_ref()
                    .list_namespaces()
                    .map_err(|err| err.to_string())?;
                return Ok(Self::xml_response(
                    StatusCode::OK,
                    Self::list_containers_xml(&req, &resource.account, &namespaces),
                ));
            }

            return Ok(Self::error_response(
                StatusCode::BAD_REQUEST,
                "InvalidUri",
                "Azure account requests must use comp=list",
            ));
        }

        let container = resource.container.clone().unwrap_or_default();
        if req.query_param("restype") == Some("container") {
            return match *req.method() {
                Method::PUT => {
                    storage
                        .as_ref()
                        .create_namespace(container)
                        .map_err(|err| err.to_string())?;
                    Ok(Self::empty_response(StatusCode::CREATED))
                }
                Method::DELETE => {
                    storage
                        .as_ref()
                        .delete_namespace(&container)
                        .map_err(|err| err.to_string())?;
                    Ok(Self::empty_response(StatusCode::ACCEPTED))
                }
                Method::GET => {
                    if req.query_param("comp") == Some("list") {
                        let blobs = storage
                            .as_ref()
                            .list_blobs(
                                &container,
                                req.query_param("prefix"),
                                req.query_param("delimiter"),
                                None,
                                None,
                            )
                            .map_err(|err| err.to_string())?;
                        Ok(Self::xml_response(
                            StatusCode::OK,
                            Self::list_blobs_xml(&container, &blobs),
                        ))
                    } else {
                        storage
                            .as_ref()
                            .get_namespace(&container)
                            .map_err(|err| err.to_string())?;
                        Ok(Self::empty_response(StatusCode::OK))
                    }
                }
                _ => Ok(Self::error_response(
                    StatusCode::METHOD_NOT_ALLOWED,
                    "UnsupportedHttpVerb",
                    "Unsupported Azure container operation",
                )),
            };
        }

        let Some(blob_key) = resource.blob.clone() else {
            return Ok(Self::error_response(
                StatusCode::BAD_REQUEST,
                "InvalidUri",
                "Blob requests must include a blob name",
            ));
        };
        let snapshot = Self::snapshot_query(&req);

        if req.method() == Method::PUT && req.query_param("comp") == Some("lease") {
            let action = req.header("x-ms-lease-action").unwrap_or("");
            let mut blob = storage
                .as_ref()
                .get_blob(&container, &blob_key)
                .map_err(|err| err.to_string())?;
            match action {
                "acquire" => {
                    if Self::has_active_lease(&blob) {
                        return Ok(Self::error_response(
                            StatusCode::CONFLICT,
                            "LeaseAlreadyPresent",
                            "The blob already has an active lease.",
                        ));
                    }
                    let lease_id = req
                        .header("x-ms-proposed-lease-id")
                        .map(|value| value.to_string())
                        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
                    let duration = req
                        .header("x-ms-lease-duration")
                        .unwrap_or("-1")
                        .to_string();
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
                    storage
                        .put_object(&container, blob_key.clone(), blob)
                        .map_err(|err| err.to_string())?;
                    return Ok(Self::response(StatusCode::CREATED)
                        .header("x-ms-lease-id", &lease_id)
                        .empty());
                }
                "renew" => {
                    if let Err(response) = Self::ensure_lease_allows(&req, &blob) {
                        return Ok(response);
                    }
                    storage
                        .put_object(&container, blob_key.clone(), blob.clone())
                        .map_err(|err| err.to_string())?;
                    return Ok(Self::response(StatusCode::OK)
                        .header("x-ms-lease-id", Self::lease_id(&blob).unwrap_or(""))
                        .empty());
                }
                "release" => {
                    if let Err(response) = Self::ensure_lease_allows(&req, &blob) {
                        return Ok(response);
                    }
                    Self::set_lease_state(&mut blob, None, "available", "unlocked", None);
                    storage
                        .put_object(&container, blob_key.clone(), blob)
                        .map_err(|err| err.to_string())?;
                    return Ok(Self::empty_response(StatusCode::OK));
                }
                "break" => {
                    Self::set_lease_state(&mut blob, None, "broken", "unlocked", None);
                    storage
                        .put_object(&container, blob_key.clone(), blob)
                        .map_err(|err| err.to_string())?;
                    return Ok(Self::response(StatusCode::ACCEPTED)
                        .header("x-ms-lease-time", "0")
                        .empty());
                }
                _ => {
                    return Ok(Self::error_response(
                        StatusCode::BAD_REQUEST,
                        "InvalidHeaderValue",
                        "Unsupported Azure lease action.",
                    ));
                }
            }
        }

        if req.method() == Method::PUT && req.query_param("comp") == Some("snapshot") {
            let blob = storage
                .as_ref()
                .get_blob(&container, &blob_key)
                .map_err(|err| err.to_string())?;
            let snapshot_time = Self::snapshot_timestamp();
            let snapshot_key = Self::snapshot_storage_key(&blob_key, &snapshot_time);
            let mut snapshot_blob = blob.clone();
            snapshot_blob.key = snapshot_key.clone();
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
                .insert(AZURE_SNAPSHOT_SOURCE_KEY.to_string(), blob_key.clone());
            storage
                .put_object(&container, snapshot_key, snapshot_blob)
                .map_err(|err| err.to_string())?;
            return Ok(Self::response(StatusCode::CREATED)
                .header("x-ms-snapshot", &snapshot_time)
                .empty());
        }

        if req.method() == Method::PUT
            && matches!(
                req.query_param("comp"),
                Some("immutabilityPolicies") | Some("immutabilitypolicy")
            )
        {
            let until = req
                .header("x-ms-immutability-policy-until-date")
                .ok_or_else(|| "Missing immutability policy until date".to_string())?;
            let mode = req
                .header("x-ms-immutability-policy-mode")
                .unwrap_or("Unlocked");
            let mut blob = storage
                .as_ref()
                .get_blob(&container, &blob_key)
                .map_err(|err| err.to_string())?;
            blob.provider_metadata
                .insert(AZURE_IMMUTABILITY_UNTIL_KEY.to_string(), until.to_string());
            blob.provider_metadata
                .insert(AZURE_IMMUTABILITY_MODE_KEY.to_string(), mode.to_string());
            storage
                .put_object(&container, blob_key.clone(), blob)
                .map_err(|err| err.to_string())?;
            return Ok(Self::response(StatusCode::OK)
                .header("x-ms-immutability-policy-until-date", until)
                .header("x-ms-immutability-policy-mode", mode)
                .empty());
        }

        if req.method() == Method::DELETE
            && matches!(
                req.query_param("comp"),
                Some("immutabilityPolicies") | Some("immutabilitypolicy")
            )
        {
            let mut blob = storage
                .as_ref()
                .get_blob(&container, &blob_key)
                .map_err(|err| err.to_string())?;
            blob.provider_metadata.remove(AZURE_IMMUTABILITY_UNTIL_KEY);
            blob.provider_metadata.remove(AZURE_IMMUTABILITY_MODE_KEY);
            storage
                .put_object(&container, blob_key.clone(), blob)
                .map_err(|err| err.to_string())?;
            return Ok(Self::empty_response(StatusCode::ACCEPTED));
        }

        if req.method() == Method::PUT && req.query_param("comp") == Some("legalhold") {
            let enabled = req
                .header("x-ms-legal-hold")
                .map(|value| value.eq_ignore_ascii_case("true"))
                .or_else(|| {
                    String::from_utf8(req.body.to_vec())
                        .ok()
                        .map(|body| body.to_ascii_lowercase().contains("true"))
                })
                .unwrap_or(false);
            let mut blob = storage
                .as_ref()
                .get_blob(&container, &blob_key)
                .map_err(|err| err.to_string())?;
            blob.provider_metadata
                .insert(AZURE_LEGAL_HOLD_KEY.to_string(), enabled.to_string());
            storage
                .put_object(&container, blob_key.clone(), blob)
                .map_err(|err| err.to_string())?;
            return Ok(Self::response(StatusCode::OK)
                .header("x-ms-legal-hold", &enabled.to_string())
                .empty());
        }

        if req.method() == Method::PUT && req.query_param("comp") == Some("block") {
            let block_id = req
                .query_param("blockid")
                .ok_or_else(|| "Missing blockid query parameter".to_string())?;
            let session_key = Self::blob_state_key(&resource.account, &container, &blob_key);
            let mut session = self
                .block_sessions
                .lock()
                .map_err(|_| "Failed to lock Azure block session state".to_string())?
                .get(&session_key)
                .cloned()
                .or(Self::load_provider_state(
                    storage.as_ref(),
                    AZURE_BLOCK_SESSION_STATE,
                    &session_key,
                )?)
                .unwrap_or_default();

            session.content_type = Some(Self::content_type(&req));
            session.metadata = Self::metadata_from_headers(&req);
            session
                .blocks
                .insert(block_id.to_string(), req.body.to_vec());
            Self::save_provider_state(
                storage.as_ref(),
                AZURE_BLOCK_SESSION_STATE,
                &session_key,
                &session,
            )?;
            self.block_sessions
                .lock()
                .map_err(|_| "Failed to lock Azure block session state".to_string())?
                .insert(session_key, session);

            return Ok(Self::response(StatusCode::CREATED).empty());
        }

        if req.method() == Method::PUT && req.query_param("comp") == Some("blocklist") {
            let block_ids = Self::parse_block_list(
                &String::from_utf8(req.body.to_vec()).map_err(|err| err.to_string())?,
            )?;
            let session_key = Self::blob_state_key(&resource.account, &container, &blob_key);
            let session = {
                let mut sessions = self
                    .block_sessions
                    .lock()
                    .map_err(|_| "Failed to lock Azure block session state".to_string())?;
                sessions.remove(&session_key)
            }
            .or(Self::load_provider_state(
                storage.as_ref(),
                AZURE_BLOCK_SESSION_STATE,
                &session_key,
            )?)
            .ok_or_else(|| "No staged Azure blocks were found".to_string())?;
            let upload = storage
                .as_ref()
                .create_upload_session(CreateUploadSessionRequest {
                    namespace: container.clone(),
                    key: blob_key.clone(),
                    content_type: session.content_type.clone(),
                    metadata: session.metadata.clone(),
                    provider_metadata: HashMap::new(),
                })
                .map_err(|err| err.to_string())?;

            for (index, block_id) in block_ids.iter().enumerate() {
                let block = session
                    .blocks
                    .get(block_id)
                    .ok_or_else(|| format!("Unknown block id {}", block_id))?;
                storage
                    .as_ref()
                    .upload_session_part(
                        &container,
                        &upload.upload_id,
                        index as u32 + 1,
                        block.clone(),
                    )
                    .map_err(|err| err.to_string())?;
            }

            storage
                .as_ref()
                .complete_upload_session(&container, &upload.upload_id)
                .map_err(|err| err.to_string())?;

            self.committed_blocks
                .lock()
                .map_err(|_| "Failed to lock Azure committed block state".to_string())?
                .insert(session_key.clone(), block_ids.clone());
            Self::save_provider_state(
                storage.as_ref(),
                AZURE_COMMITTED_BLOCKS_STATE,
                &session_key,
                &block_ids,
            )?;
            storage
                .delete_provider_state(AZURE_BLOCK_SESSION_STATE, &session_key)
                .map_err(|err| err.to_string())?;

            return Ok(Self::empty_response(StatusCode::CREATED));
        }

        if req.query_param("comp") == Some("metadata") && req.method() == Method::PUT {
            let existing = storage
                .as_ref()
                .get_blob(&container, &blob_key)
                .map_err(|err| err.to_string())?;
            if let Err(response) = Self::ensure_mutation_allowed(&req, &existing) {
                return Ok(response);
            }
            storage
                .as_ref()
                .update_blob_metadata(UpdateBlobMetadataRequest {
                    namespace: container.clone(),
                    key: blob_key.clone(),
                    metadata: Self::metadata_from_headers(&req),
                })
                .map_err(|err| err.to_string())?;
            return Ok(Self::empty_response(StatusCode::OK));
        }

        if req.query_param("comp") == Some("blocklist") && req.method() == Method::GET {
            let session_key = Self::blob_state_key(&resource.account, &container, &blob_key);
            let block_ids = self
                .committed_blocks
                .lock()
                .map_err(|_| "Failed to lock Azure committed block state".to_string())?
                .get(&session_key)
                .cloned()
                .or(Self::load_provider_state(
                    storage.as_ref(),
                    AZURE_COMMITTED_BLOCKS_STATE,
                    &session_key,
                )?)
                .unwrap_or_default();
            return Ok(Self::xml_response(
                StatusCode::OK,
                Self::block_list_xml(&block_ids),
            ));
        }

        if req.method() == Method::PUT && req.query_param("comp") == Some("appendblock") {
            let mut blob = storage
                .as_ref()
                .get_blob(&container, &blob_key)
                .map_err(|err| err.to_string())?;
            if let Err(response) = Self::ensure_mutation_allowed(&req, &blob) {
                return Ok(response);
            }
            if Self::blob_type(&blob) != "AppendBlob" {
                return Ok(Self::error_response(
                    StatusCode::CONFLICT,
                    "InvalidBlobType",
                    "The blob type is invalid for this operation.",
                ));
            }

            blob.data.extend_from_slice(&req.body);
            blob.size = blob.data.len() as u64;
            blob.etag = crate::models::object::compute_etag(&blob.data);
            blob.last_modified = Utc::now();
            storage
                .put_object(&container, blob_key.clone(), blob)
                .map_err(|err| err.to_string())?;

            let stored = storage
                .get_object(&container, &blob_key)
                .map_err(|err| err.to_string())?;
            return Ok(Self::response(StatusCode::CREATED)
                .header("etag", &format!("\"{}\"", stored.etag))
                .header(
                    "x-ms-blob-append-offset",
                    &(stored.size - req.body.len() as u64).to_string(),
                )
                .header("x-ms-blob-committed-block-count", "1")
                .empty());
        }

        if req.method() == Method::PUT && req.query_param("comp") == Some("page") {
            let Some(range_header) = Self::requested_range(&req) else {
                return Ok(Self::error_response(
                    StatusCode::BAD_REQUEST,
                    "InvalidHeaderValue",
                    "Page writes require a valid x-ms-range header.",
                ));
            };
            let Some((start, end)) = Self::parse_write_range_header(range_header) else {
                return Ok(Self::error_response(
                    StatusCode::BAD_REQUEST,
                    "InvalidHeaderValue",
                    "Page writes require a valid x-ms-range header.",
                ));
            };
            if start % 512 != 0 || (end + 1) % 512 != 0 {
                return Ok(Self::error_response(
                    StatusCode::BAD_REQUEST,
                    "InvalidPageRange",
                    "Page blob ranges must align to 512-byte boundaries.",
                ));
            }

            let mut blob = storage
                .as_ref()
                .get_blob(&container, &blob_key)
                .map_err(|err| err.to_string())?;
            if let Err(response) = Self::ensure_mutation_allowed(&req, &blob) {
                return Ok(response);
            }
            if Self::blob_type(&blob) != "PageBlob" {
                return Ok(Self::error_response(
                    StatusCode::CONFLICT,
                    "InvalidBlobType",
                    "The blob type is invalid for this operation.",
                ));
            }

            let expected_len = end - start + 1;
            if req.body.len() != expected_len {
                return Ok(Self::error_response(
                    StatusCode::BAD_REQUEST,
                    "InvalidPageRange",
                    "Page payload length must match the requested range.",
                ));
            }
            if end >= blob.data.len() {
                return Ok(Self::error_response(
                    StatusCode::BAD_REQUEST,
                    "InvalidPageRange",
                    "Page write exceeds the blob length.",
                ));
            }

            blob.data[start..=end].copy_from_slice(&req.body);
            blob.etag = crate::models::object::compute_etag(&blob.data);
            blob.last_modified = Utc::now();
            storage
                .put_object(&container, blob_key.clone(), blob)
                .map_err(|err| err.to_string())?;

            return Ok(Self::response(StatusCode::CREATED).empty());
        }

        match *req.method() {
            Method::PUT => {
                if snapshot.is_some() {
                    return Ok(Self::error_response(
                        StatusCode::BAD_REQUEST,
                        "InvalidQueryParameterValue",
                        "Snapshots are read-only.",
                    ));
                }
                if let Ok(existing) = storage.as_ref().get_blob(&container, &blob_key) {
                    if let Err(response) = Self::ensure_mutation_allowed(&req, &existing) {
                        return Ok(response);
                    }
                }
                let blob_type = req.header("x-ms-blob-type").unwrap_or("BlockBlob");
                let stored = if blob_type == "AppendBlob" {
                    let mut object = crate::models::Object::new_with_metadata(
                        blob_key.clone(),
                        req.body.to_vec(),
                        Self::content_type(&req),
                        Self::metadata_from_headers(&req),
                    );
                    Self::set_blob_type(&mut object, "AppendBlob");
                    storage
                        .put_object(&container, blob_key.clone(), object.clone())
                        .map_err(|err| err.to_string())?;
                    object
                } else if blob_type == "PageBlob" {
                    let declared_len = req
                        .header("x-ms-blob-content-length")
                        .or_else(|| req.header("content-length"))
                        .and_then(|value| value.parse::<usize>().ok())
                        .unwrap_or(req.body.len());
                    if !declared_len.is_multiple_of(512) {
                        return Ok(Self::error_response(
                            StatusCode::BAD_REQUEST,
                            "InvalidHeaderValue",
                            "Page blob length must be a multiple of 512 bytes.",
                        ));
                    }
                    let mut object = crate::models::Object::new_with_metadata(
                        blob_key.clone(),
                        vec![0u8; declared_len],
                        Self::content_type(&req),
                        Self::metadata_from_headers(&req),
                    );
                    Self::set_blob_type(&mut object, "PageBlob");
                    storage
                        .put_object(&container, blob_key.clone(), object.clone())
                        .map_err(|err| err.to_string())?;
                    object
                } else {
                    let mut object = crate::models::Object::new_with_metadata(
                        blob_key.clone(),
                        req.body.to_vec(),
                        Self::content_type(&req),
                        Self::metadata_from_headers(&req),
                    );
                    Self::set_blob_type(&mut object, "BlockBlob");
                    storage
                        .put_object(&container, blob_key.clone(), object.clone())
                        .map_err(|err| err.to_string())?;
                    object
                };
                Ok(Self::response(StatusCode::CREATED)
                    .header("etag", &format!("\"{}\"", stored.etag))
                    .header("last-modified", &stored.last_modified.to_rfc2822())
                    .header("x-ms-blob-type", Self::blob_type(&stored))
                    .empty())
            }
            Method::GET => {
                let blob =
                    match Self::lookup_blob(&storage, &container, &blob_key, snapshot.as_deref()) {
                        Ok(blob) => blob,
                        Err(err) => return Err(err),
                    };
                if let Some(range_header) = Self::requested_range(&req) {
                    if let Some((start, end)) = Self::parse_range_header(range_header, blob.size) {
                        let payload = if snapshot.is_some() {
                            let data = blob.data[start..=end].to_vec();
                            crate::blob::BlobPayload {
                                blob: blob.clone(),
                                data,
                            }
                        } else {
                            storage
                                .as_ref()
                                .get_blob_range(
                                    &container,
                                    &blob_key,
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
                            Some(format!("bytes {}-{}/{}", start, end, blob.size)),
                        )
                        .body(payload.data)
                        .build());
                    }
                    return Ok(Self::error_response(
                        StatusCode::RANGE_NOT_SATISFIABLE,
                        "InvalidRange",
                        "The requested range is not satisfiable.",
                    ));
                }
                Ok(
                    Self::blob_response(StatusCode::OK, &blob, blob.size as usize, None)
                        .body(blob.data)
                        .build(),
                )
            }
            Method::HEAD => {
                let blob = Self::lookup_blob(&storage, &container, &blob_key, snapshot.as_deref())?;
                Ok(Self::blob_response(StatusCode::OK, &blob, blob.size as usize, None).empty())
            }
            Method::DELETE => {
                if let Some(snapshot) = snapshot.as_deref() {
                    let snapshot_key = Self::snapshot_storage_key(&blob_key, snapshot);
                    storage
                        .as_ref()
                        .delete_blob(&container, &snapshot_key)
                        .map_err(|err| err.to_string())?;
                    return Ok(Self::empty_response(StatusCode::ACCEPTED));
                }
                let blob = storage
                    .as_ref()
                    .get_blob(&container, &blob_key)
                    .map_err(|err| err.to_string())?;
                if let Err(response) = Self::ensure_mutation_allowed(&req, &blob) {
                    return Ok(response);
                }
                storage
                    .as_ref()
                    .delete_blob(&container, &blob_key)
                    .map_err(|err| err.to_string())?;
                Ok(Self::empty_response(StatusCode::ACCEPTED))
            }
            _ => Ok(Self::error_response(
                StatusCode::METHOD_NOT_ALLOWED,
                "UnsupportedHttpVerb",
                "Unsupported Azure blob operation",
            )),
        }
    }
}

fn sign_hmac_base64(key: &[u8], payload: &str) -> Result<String, String> {
    type HmacSha256 = Hmac<Sha256>;
    let mut mac = HmacSha256::new_from_slice(key)
        .map_err(|err| format!("Invalid Azure signing key: {}", err))?;
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
            lifecycle_interval: std::time::Duration::from_secs(3600),
            api_port: 9000,
            ui_port: 9001,
            max_request_bytes: crate::config::DEFAULT_SQRZL_MAX_REQUEST_BYTES,
        })
    }

    fn azure_auth() -> Arc<AuthConfig> {
        Arc::new(Config {
            access_key_id: Some("devstoreaccount1".to_string()),
            secret_access_key: Some(BASE64.encode("topsecretkey")),
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

    fn signed_headers(req: &Request, config: &AuthConfig, account: &str) -> String {
        let string_to_sign = AzureBlobAdapter::shared_key_string_to_sign(req, account);
        let key = AzureBlobAdapter::shared_key_secret(config).expect("key should exist");
        format!(
            "SharedKey {}:{}",
            account,
            sign_hmac_base64(&key, &string_to_sign).expect("signature should build")
        )
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
                storage.clone(),
                auth_disabled(),
                parsed_request(
                    "PUT",
                    "http://localhost/devstoreaccount1/photos?restype=container",
                    &[("x-ms-version", AZURE_VERSION)],
                    b"",
                )
                .await,
            )
            .await
            .expect("container create should succeed");
        assert_eq!(response.status(), StatusCode::CREATED);

        let response = adapter
            .handle_request(
                storage.clone(),
                auth_disabled(),
                parsed_request(
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
            .await
            .expect("put blob should succeed");
        assert_eq!(response.status(), StatusCode::CREATED);

        let response = adapter
            .handle_request(
                storage.clone(),
                auth_disabled(),
                parsed_request(
                    "GET",
                    "http://localhost/devstoreaccount1/photos?restype=container&comp=list",
                    &[("x-ms-version", AZURE_VERSION)],
                    b"",
                )
                .await,
            )
            .await
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
                storage,
                auth_disabled(),
                parsed_request(
                    "GET",
                    "http://localhost/devstoreaccount1/photos/kitten.txt",
                    &[("x-ms-version", AZURE_VERSION)],
                    b"",
                )
                .await,
            )
            .await
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
                storage.clone(),
                auth_disabled(),
                parsed_request(
                    "PUT",
                    "http://localhost/devstoreaccount1/archive?restype=container",
                    &[("x-ms-version", AZURE_VERSION)],
                    b"",
                )
                .await,
            )
            .await
            .expect("container create should succeed");

        let block_one = BASE64.encode("block-001");
        let block_two = BASE64.encode("block-002");
        for (block_id, payload) in [
            (&block_one, b"abc".as_slice()),
            (&block_two, b"def".as_slice()),
        ] {
            let response = adapter
                .handle_request(
                    storage.clone(),
                    auth_disabled(),
                    parsed_request(
                        "PUT",
                        &format!(
                            "http://localhost/devstoreaccount1/archive/report.txt?comp=block&blockid={}",
                            urlencoding::encode(block_id)
                        ),
                        &[("x-ms-version", AZURE_VERSION)],
                        payload,
                    )
                    .await,
                )
                .await
                .expect("put block should succeed");
            assert_eq!(response.status(), StatusCode::CREATED);
        }

        let block_list = format!(
            "<?xml version=\"1.0\" encoding=\"utf-8\"?><BlockList><Latest>{}</Latest><Latest>{}</Latest></BlockList>",
            block_one, block_two
        );
        adapter
            .handle_request(
                storage.clone(),
                auth_disabled(),
                parsed_request(
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
            .await
            .expect("put block list should succeed");

        let response = adapter
            .handle_request(
                storage,
                auth_disabled(),
                parsed_request(
                    "GET",
                    "http://localhost/devstoreaccount1/archive/report.txt",
                    &[("x-ms-version", AZURE_VERSION)],
                    b"",
                )
                .await,
            )
            .await
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
    async fn should_commit_and_list_blocks_after_adapter_restart() {
        let adapter = AzureBlobAdapter::new();
        let storage = temp_storage();

        adapter
            .handle_request(
                storage.clone(),
                auth_disabled(),
                parsed_request(
                    "PUT",
                    "http://localhost/devstoreaccount1/restart?restype=container",
                    &[("x-ms-version", AZURE_VERSION)],
                    b"",
                )
                .await,
            )
            .await
            .expect("container create should succeed");

        let block_id = BASE64.encode("restart-block");
        adapter
            .handle_request(
                storage.clone(),
                auth_disabled(),
                parsed_request(
                    "PUT",
                    &format!(
                        "http://localhost/devstoreaccount1/restart/report.txt?comp=block&blockid={}",
                        urlencoding::encode(&block_id)
                    ),
                    &[("x-ms-version", AZURE_VERSION)],
                    b"restart-safe",
                )
                .await,
            )
            .await
            .expect("put block should succeed");

        let restarted = AzureBlobAdapter::new();
        let block_list = format!(
            "<?xml version=\"1.0\" encoding=\"utf-8\"?><BlockList><Latest>{}</Latest></BlockList>",
            block_id
        );
        let commit = restarted
            .handle_request(
                storage.clone(),
                auth_disabled(),
                parsed_request(
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
            .await
            .expect("put block list should succeed after restart");
        assert_eq!(commit.status(), StatusCode::CREATED);

        let block_list_response = AzureBlobAdapter::new()
            .handle_request(
                storage.clone(),
                auth_disabled(),
                parsed_request(
                    "GET",
                    "http://localhost/devstoreaccount1/restart/report.txt?comp=blocklist",
                    &[("x-ms-version", AZURE_VERSION)],
                    b"",
                )
                .await,
            )
            .await
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
                ("x-ms-date", "Sat, 01 Jan 2024 00:00:00 +0000"),
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
            .handle_request(storage.clone(), azure_auth(), shared_key_request)
            .await
            .expect("shared key request should complete");
        assert_eq!(response.status(), StatusCode::OK);

        let expiry = "2035-01-01T00:00:00Z";
        let canonical_resource = "/blob/devstoreaccount1/secure/blob.txt";
        let sig = sas_signature(canonical_resource, &azure_auth(), "r", expiry);
        let response = adapter
            .handle_request(
                storage,
                azure_auth(),
                parsed_request(
                    "GET",
                    &format!(
                        "http://localhost/devstoreaccount1/secure/blob.txt?sp=r&se={}&sv=2023-11-03&sr=b&sig={}",
                        urlencoding::encode(expiry),
                        urlencoding::encode(&sig)
                    ),
                    &[("x-ms-version", AZURE_VERSION)],
                    b"",
                )
                .await,
            )
            .await
            .expect("sas request should complete");
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn should_update_metadata_return_block_list_and_support_ranges() {
        let adapter = AzureBlobAdapter::new();
        let storage = temp_storage();

        adapter
            .handle_request(
                storage.clone(),
                auth_disabled(),
                parsed_request(
                    "PUT",
                    "http://localhost/devstoreaccount1/media?restype=container",
                    &[("x-ms-version", AZURE_VERSION)],
                    b"",
                )
                .await,
            )
            .await
            .expect("container create should succeed");

        let block_one = BASE64.encode("block-a");
        let block_two = BASE64.encode("block-b");
        for (block_id, payload) in [
            (&block_one, b"hello ".as_slice()),
            (&block_two, b"azure".as_slice()),
        ] {
            adapter
                .handle_request(
                    storage.clone(),
                    auth_disabled(),
                    parsed_request(
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
                .await
                .expect("put block should succeed");
        }

        let block_list = format!(
            "<?xml version=\"1.0\" encoding=\"utf-8\"?><BlockList><Latest>{}</Latest><Latest>{}</Latest></BlockList>",
            block_one, block_two
        );
        adapter
            .handle_request(
                storage.clone(),
                auth_disabled(),
                parsed_request(
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
            .await
            .expect("block list commit should succeed");

        let response = adapter
            .handle_request(
                storage.clone(),
                auth_disabled(),
                parsed_request(
                    "GET",
                    "http://localhost/devstoreaccount1/media/greeting.txt?comp=blocklist",
                    &[("x-ms-version", AZURE_VERSION)],
                    b"",
                )
                .await,
            )
            .await
            .expect("block list fetch should succeed");
        let body = response
            .into_body()
            .collect()
            .await
            .expect("body should read")
            .to_bytes();
        let xml = String::from_utf8(body.to_vec()).expect("xml");
        assert!(xml.contains(&block_one));
        assert!(xml.contains(&block_two));

        let response = adapter
            .handle_request(
                storage.clone(),
                auth_disabled(),
                parsed_request(
                    "PUT",
                    "http://localhost/devstoreaccount1/media/greeting.txt?comp=metadata",
                    &[("x-ms-version", AZURE_VERSION), ("x-ms-meta-owner", "bob")],
                    b"",
                )
                .await,
            )
            .await
            .expect("metadata update should succeed");
        assert_eq!(response.status(), StatusCode::OK);

        let response = adapter
            .handle_request(
                storage.clone(),
                auth_disabled(),
                parsed_request(
                    "HEAD",
                    "http://localhost/devstoreaccount1/media/greeting.txt",
                    &[("x-ms-version", AZURE_VERSION)],
                    b"",
                )
                .await,
            )
            .await
            .expect("head should succeed");
        assert_eq!(
            response
                .headers()
                .get("x-ms-meta-owner")
                .and_then(|value| value.to_str().ok()),
            Some("bob")
        );

        let response = adapter
            .handle_request(
                storage,
                auth_disabled(),
                parsed_request(
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
            .await
            .expect("range get should succeed");
        assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(
            response
                .headers()
                .get("content-range")
                .and_then(|value| value.to_str().ok()),
            Some("bytes 6-10/11")
        );
        assert_eq!(
            response
                .headers()
                .get("x-ms-meta-owner")
                .and_then(|value| value.to_str().ok()),
            Some("bob")
        );
        let body = response
            .into_body()
            .collect()
            .await
            .expect("body should read")
            .to_bytes();
        assert_eq!(body.as_ref(), b"azure");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn should_support_append_and_page_blob_writes() {
        let adapter = AzureBlobAdapter::new();
        let storage = temp_storage();

        adapter
            .handle_request(
                storage.clone(),
                auth_disabled(),
                parsed_request(
                    "PUT",
                    "http://localhost/devstoreaccount1/state?restype=container",
                    &[("x-ms-version", AZURE_VERSION)],
                    b"",
                )
                .await,
            )
            .await
            .expect("container create should succeed");

        let response = adapter
            .handle_request(
                storage.clone(),
                auth_disabled(),
                parsed_request(
                    "PUT",
                    "http://localhost/devstoreaccount1/state/events.log",
                    &[
                        ("x-ms-version", AZURE_VERSION),
                        ("x-ms-blob-type", "AppendBlob"),
                    ],
                    b"hello",
                )
                .await,
            )
            .await
            .expect("append blob create should succeed");
        assert_eq!(response.status(), StatusCode::CREATED);
        assert_eq!(
            response
                .headers()
                .get("x-ms-blob-type")
                .and_then(|v| v.to_str().ok()),
            Some("AppendBlob")
        );

        adapter
            .handle_request(
                storage.clone(),
                auth_disabled(),
                parsed_request(
                    "PUT",
                    "http://localhost/devstoreaccount1/state/events.log?comp=appendblock",
                    &[("x-ms-version", AZURE_VERSION)],
                    b" world",
                )
                .await,
            )
            .await
            .expect("append block should succeed");

        let response = adapter
            .handle_request(
                storage.clone(),
                auth_disabled(),
                parsed_request(
                    "GET",
                    "http://localhost/devstoreaccount1/state/events.log",
                    &[("x-ms-version", AZURE_VERSION)],
                    b"",
                )
                .await,
            )
            .await
            .expect("append blob get should succeed");
        let body = response
            .into_body()
            .collect()
            .await
            .expect("body should read")
            .to_bytes();
        assert_eq!(body.as_ref(), b"hello world");

        let response = adapter
            .handle_request(
                storage.clone(),
                auth_disabled(),
                parsed_request(
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
            .await
            .expect("page blob create should succeed");
        assert_eq!(response.status(), StatusCode::CREATED);
        assert_eq!(
            response
                .headers()
                .get("x-ms-blob-type")
                .and_then(|v| v.to_str().ok()),
            Some("PageBlob")
        );

        adapter
            .handle_request(
                storage.clone(),
                auth_disabled(),
                parsed_request(
                    "PUT",
                    "http://localhost/devstoreaccount1/state/page.bin?comp=page",
                    &[
                        ("x-ms-version", AZURE_VERSION),
                        ("x-ms-range", "bytes=0-511"),
                    ],
                    &vec![b'a'; 512],
                )
                .await,
            )
            .await
            .expect("page write should succeed");

        let response = adapter
            .handle_request(
                storage,
                auth_disabled(),
                parsed_request(
                    "GET",
                    "http://localhost/devstoreaccount1/state/page.bin",
                    &[("x-ms-version", AZURE_VERSION), ("x-ms-range", "bytes=0-7")],
                    b"",
                )
                .await,
            )
            .await
            .expect("page blob range get should succeed");
        let body = response
            .into_body()
            .collect()
            .await
            .expect("body should read")
            .to_bytes();
        assert_eq!(body.as_ref(), b"aaaaaaaa");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn should_manage_leases_snapshots_and_immutability() {
        let adapter = AzureBlobAdapter::new();
        let storage = temp_storage();

        adapter
            .handle_request(
                storage.clone(),
                auth_disabled(),
                parsed_request(
                    "PUT",
                    "http://localhost/devstoreaccount1/state?restype=container",
                    &[("x-ms-version", AZURE_VERSION)],
                    b"",
                )
                .await,
            )
            .await
            .expect("container create should succeed");

        adapter
            .handle_request(
                storage.clone(),
                auth_disabled(),
                parsed_request(
                    "PUT",
                    "http://localhost/devstoreaccount1/state/lease.txt",
                    &[("x-ms-version", AZURE_VERSION)],
                    b"initial",
                )
                .await,
            )
            .await
            .expect("blob create should succeed");

        let lease_response = adapter
            .handle_request(
                storage.clone(),
                auth_disabled(),
                parsed_request(
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
            .await
            .expect("lease acquire should succeed");
        assert_eq!(lease_response.status(), StatusCode::CREATED);
        let lease_id = lease_response
            .headers()
            .get("x-ms-lease-id")
            .and_then(|value| value.to_str().ok())
            .expect("lease id")
            .to_string();

        let denied = adapter
            .handle_request(
                storage.clone(),
                auth_disabled(),
                parsed_request(
                    "DELETE",
                    "http://localhost/devstoreaccount1/state/lease.txt",
                    &[("x-ms-version", AZURE_VERSION)],
                    b"",
                )
                .await,
            )
            .await
            .expect("delete should return a response");
        assert_eq!(denied.status(), StatusCode::PRECONDITION_FAILED);

        let release = adapter
            .handle_request(
                storage.clone(),
                auth_disabled(),
                parsed_request(
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
            .await
            .expect("lease release should succeed");
        assert_eq!(release.status(), StatusCode::OK);

        let snapshot = adapter
            .handle_request(
                storage.clone(),
                auth_disabled(),
                parsed_request(
                    "PUT",
                    "http://localhost/devstoreaccount1/state/lease.txt?comp=snapshot",
                    &[("x-ms-version", AZURE_VERSION)],
                    b"",
                )
                .await,
            )
            .await
            .expect("snapshot create should succeed");
        assert_eq!(snapshot.status(), StatusCode::CREATED);
        let snapshot_id = snapshot
            .headers()
            .get("x-ms-snapshot")
            .and_then(|value| value.to_str().ok())
            .expect("snapshot id")
            .to_string();

        adapter
            .handle_request(
                storage.clone(),
                auth_disabled(),
                parsed_request(
                    "PUT",
                    "http://localhost/devstoreaccount1/state/lease.txt",
                    &[("x-ms-version", AZURE_VERSION)],
                    b"updated",
                )
                .await,
            )
            .await
            .expect("overwrite should succeed");

        let snapshot_get = adapter
            .handle_request(
                storage.clone(),
                auth_disabled(),
                parsed_request(
                    "GET",
                    &format!(
                        "http://localhost/devstoreaccount1/state/lease.txt?snapshot={}",
                        snapshot_id
                    ),
                    &[("x-ms-version", AZURE_VERSION)],
                    b"",
                )
                .await,
            )
            .await
            .expect("snapshot get should succeed");
        let snapshot_body = snapshot_get
            .into_body()
            .collect()
            .await
            .expect("snapshot body should read")
            .to_bytes();
        assert_eq!(snapshot_body.as_ref(), b"initial");

        let retention_response = adapter
            .handle_request(
                storage.clone(),
                auth_disabled(),
                parsed_request(
                    "PUT",
                    "http://localhost/devstoreaccount1/state/lease.txt?comp=immutabilitypolicy",
                    &[
                        ("x-ms-version", AZURE_VERSION),
                        (
                            "x-ms-immutability-policy-until-date",
                            "2099-01-01T00:00:00Z",
                        ),
                        ("x-ms-immutability-policy-mode", "Unlocked"),
                    ],
                    b"",
                )
                .await,
            )
            .await
            .expect("immutability policy should succeed");
        assert_eq!(retention_response.status(), StatusCode::OK);

        let legal_hold = adapter
            .handle_request(
                storage.clone(),
                auth_disabled(),
                parsed_request(
                    "PUT",
                    "http://localhost/devstoreaccount1/state/lease.txt?comp=legalhold",
                    &[("x-ms-version", AZURE_VERSION)],
                    b"<LegalHold>true</LegalHold>",
                )
                .await,
            )
            .await
            .expect("legal hold should succeed");
        assert_eq!(legal_hold.status(), StatusCode::OK);

        let immutable_delete = adapter
            .handle_request(
                storage.clone(),
                auth_disabled(),
                parsed_request(
                    "DELETE",
                    "http://localhost/devstoreaccount1/state/lease.txt",
                    &[("x-ms-version", AZURE_VERSION)],
                    b"",
                )
                .await,
            )
            .await
            .expect("immutable delete should return a response");
        assert_eq!(immutable_delete.status(), StatusCode::CONFLICT);

        let head = adapter
            .handle_request(
                storage,
                auth_disabled(),
                parsed_request(
                    "HEAD",
                    "http://localhost/devstoreaccount1/state/lease.txt",
                    &[("x-ms-version", AZURE_VERSION)],
                    b"",
                )
                .await,
            )
            .await
            .expect("head should succeed");
        assert_eq!(
            head.headers()
                .get("x-ms-immutability-policy-until-date")
                .and_then(|value| value.to_str().ok()),
            Some("2099-01-01T00:00:00Z")
        );
        assert_eq!(
            head.headers()
                .get("x-ms-legal-hold")
                .and_then(|value| value.to_str().ok()),
            Some("true")
        );
    }
}
