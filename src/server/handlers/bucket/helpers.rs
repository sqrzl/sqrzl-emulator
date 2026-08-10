use crate::server::handlers::cors;
use crate::server::http::{Request, ResponseBuilder};
use crate::services::bucket as bucket_service;
use crate::storage::BucketStore;
use crate::utils::xml as xml_utils;
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use quick_xml::escape::unescape;
use quick_xml::events::Event;
use quick_xml::Reader;
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};

pub(super) const S3_REQUEST_PAYMENT_KEY: &str = "s3_requester_pays";
pub(super) const S3_WEBSITE_XML_KEY: &str = "s3_website_xml";
pub(super) const S3_CORS_XML_KEY: &str = "s3_cors_xml";
pub(super) const S3_VERSIONING_STATUS_KEY: &str = "s3_versioning_status";
pub(super) const S3_OBJECT_LOCK_ENABLED_KEY: &str = "s3_object_lock_enabled";
pub(super) type DeleteObjectEntry = (String, Option<String>, Option<String>);

pub(super) fn escape_xml_str(input: &str) -> String {
    input
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

#[allow(
    clippy::too_many_lines,
    reason = "keeping the complete DeleteObjects XML schema state machine together makes its pre-mutation validation and nesting invariants auditable"
)]
pub(super) fn parse_delete_keys(xml: &str) -> Result<Vec<DeleteObjectEntry>, String> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(false);

    let mut buf = Vec::new();
    let mut root_closed = false;
    let mut stack: Vec<Vec<u8>> = Vec::new();
    let mut current_key: Option<String> = None;
    let mut current_version: Option<String> = None;
    let mut current_etag: Option<String> = None;
    let mut current_quiet: Option<String> = None;
    let mut quiet_seen = false;
    let mut objects = Vec::new();

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(_)) if root_closed => {
                return Err("Unexpected XML after the Delete document".to_string());
            }
            Ok(Event::Start(e)) => {
                let name = e.name().as_ref().to_vec();
                let attributes_are_valid = e.attributes().all(|attribute| {
                    attribute.is_ok_and(|attribute| {
                        stack.is_empty()
                            && name == b"Delete"
                            && attribute.key.as_ref().starts_with(b"xmlns")
                    })
                });
                if !attributes_are_valid {
                    return Err(
                        "Delete request elements contain unsupported attributes".to_string()
                    );
                }

                if stack.is_empty() {
                    if name != b"Delete" {
                        return Err("Delete must be the document root".to_string());
                    }
                } else if stack.len() == 1 && stack[0] == b"Delete" {
                    if name == b"Object" {
                        if objects.len() >= 1_000 {
                            return Err(
                                "Delete requests may contain at most 1000 objects".to_string()
                            );
                        }
                        current_key = None;
                        current_version = None;
                        current_etag = None;
                    } else if name == b"Quiet" && !quiet_seen {
                        quiet_seen = true;
                        current_quiet = Some(String::new());
                    } else {
                        return Err(
                            "Delete may contain only Object entries and one Quiet element"
                                .to_string(),
                        );
                    }
                } else if stack.len() == 2 && stack[0] == b"Delete" && stack[1] == b"Object" {
                    if name == b"Key" && current_key.is_none() {
                        current_key = Some(String::new());
                    } else if name == b"VersionId" && current_version.is_none() {
                        current_version = Some(String::new());
                    } else if name == b"ETag" && current_etag.is_none() {
                        current_etag = Some(String::new());
                    } else {
                        return Err(
                            "Object may contain one Key, VersionId, and ETag element".to_string()
                        );
                    }
                } else {
                    return Err("Delete request elements have invalid nesting".to_string());
                }
                stack.push(name);
            }
            Ok(Event::Empty(_)) => {
                return Err("Delete request elements must not be empty".to_string());
            }
            Ok(Event::Text(t)) => {
                let decoded = t.decode().map_err(|error| error.to_string())?;
                let value = unescape(&decoded).map_err(|error| error.to_string())?;
                match stack.last().map(Vec::as_slice) {
                    Some(b"Key") => current_key
                        .as_mut()
                        .ok_or_else(|| "Malformed Delete request Key".to_string())?
                        .push_str(&value),
                    Some(b"VersionId") => current_version
                        .as_mut()
                        .ok_or_else(|| "Malformed Delete request VersionId".to_string())?
                        .push_str(&value),
                    Some(b"ETag") => current_etag
                        .as_mut()
                        .ok_or_else(|| "Malformed Delete request ETag".to_string())?
                        .push_str(&value),
                    Some(b"Quiet") => current_quiet
                        .as_mut()
                        .ok_or_else(|| "Malformed Delete request Quiet".to_string())?
                        .push_str(&value),
                    _ if value.trim().is_empty() => {}
                    _ => return Err("Unexpected text in Delete request".to_string()),
                }
            }
            Ok(Event::CData(t)) => {
                let value = t.decode().map_err(|error| error.to_string())?;
                match stack.last().map(Vec::as_slice) {
                    Some(b"Key") => current_key
                        .as_mut()
                        .ok_or_else(|| "Malformed Delete request Key".to_string())?
                        .push_str(&value),
                    Some(b"VersionId") => current_version
                        .as_mut()
                        .ok_or_else(|| "Malformed Delete request VersionId".to_string())?
                        .push_str(&value),
                    Some(b"ETag") => current_etag
                        .as_mut()
                        .ok_or_else(|| "Malformed Delete request ETag".to_string())?
                        .push_str(&value),
                    Some(b"Quiet") => current_quiet
                        .as_mut()
                        .ok_or_else(|| "Malformed Delete request Quiet".to_string())?
                        .push_str(&value),
                    _ if value.trim().is_empty() => {}
                    _ => return Err("Unexpected CDATA in Delete request".to_string()),
                }
            }
            Ok(Event::End(e)) => {
                let name = e.name().as_ref().to_vec();
                if stack.last() != Some(&name) {
                    return Err("Delete request elements are not properly nested".to_string());
                }
                match name.as_slice() {
                    b"Key" => {
                        if current_key.as_deref().is_none_or(str::is_empty) {
                            return Err("Each Object must contain a non-empty Key".to_string());
                        }
                    }
                    b"VersionId" => {
                        if current_version.as_deref().is_none_or(str::is_empty) {
                            return Err("VersionId must not be empty".to_string());
                        }
                    }
                    b"ETag" => {
                        if current_etag.as_deref().is_none_or(str::is_empty) {
                            return Err("ETag must not be empty".to_string());
                        }
                    }
                    b"Quiet" => {
                        if !matches!(current_quiet.as_deref(), Some("true" | "false")) {
                            return Err("Quiet must be true or false".to_string());
                        }
                    }
                    b"Object" => {
                        let key = current_key
                            .take()
                            .filter(|key| !key.is_empty())
                            .ok_or_else(|| {
                                "Each Object must contain a non-empty Key".to_string()
                            })?;
                        objects.push((key, current_version.take(), current_etag.take()));
                    }
                    b"Delete" => {
                        if objects.is_empty() {
                            return Err("Delete must contain at least one Object".to_string());
                        }
                        root_closed = true;
                    }
                    _ => {}
                }
                stack.pop();
            }
            Ok(Event::DocType(_)) => {
                return Err("Delete requests must not contain a document type".to_string());
            }
            Ok(Event::Eof) => break,
            Err(error) => return Err(error.to_string()),
            _ => {}
        }
        buf.clear();
    }

    if !root_closed || !stack.is_empty() {
        return Err("Malformed Delete request XML".to_string());
    }
    Ok(objects)
}

pub(super) fn bucket_get_action(req: &Request) -> &'static str {
    if req.has_query_param("requestPayment") {
        "s3:GetBucketRequestPayment"
    } else if req.has_query_param("website") {
        "s3:GetBucketWebsite"
    } else if req.has_query_param("cors") {
        "s3:GetBucketCors"
    } else if req.has_query_param("lifecycle") {
        "s3:GetLifecycleConfiguration"
    } else if req.has_query_param("policy") {
        "s3:GetBucketPolicy"
    } else if req.has_query_param("acl") {
        "s3:GetBucketAcl"
    } else if req.has_query_param("versioning") {
        "s3:GetBucketVersioning"
    } else if req.has_query_param("object-lock") {
        "s3:GetBucketObjectLockConfiguration"
    } else if req.has_query_param("uploads") {
        "s3:ListBucketMultipartUploads"
    } else if req.has_query_param("versions") {
        "s3:ListBucketVersions"
    } else {
        "s3:ListBucket"
    }
}

pub(super) fn metadata_value(xml: &str, tag: &[u8]) -> Option<String> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);
    let mut buf = Vec::new();
    let mut in_tag = false;

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) if e.name().as_ref() == tag => in_tag = true,
            Ok(Event::End(e)) if e.name().as_ref() == tag => in_tag = false,
            Ok(Event::Text(t)) if in_tag => {
                let decoded = t.decode().unwrap_or_default();
                return Some(unescape(&decoded).unwrap_or_default().to_string());
            }
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
        buf.clear();
    }

    None
}

pub(super) fn bucket_cors_snapshot(
    storage: &(impl BucketStore + ?Sized),
    bucket: &str,
) -> Option<String> {
    bucket_service::get_bucket(storage, bucket)
        .ok()
        .and_then(|bucket_record| bucket_record.metadata.get(S3_CORS_XML_KEY).cloned())
}

pub(super) fn apply_bucket_cors_headers(
    storage: &(impl BucketStore + ?Sized),
    bucket: &str,
    req: &Request,
    builder: ResponseBuilder,
    cors_xml_snapshot: Option<&str>,
) -> ResponseBuilder {
    if let Some(cors_xml) = cors_xml_snapshot {
        cors::apply_actual_request_headers_from_xml(req, builder, cors_xml)
    } else {
        cors::apply_actual_request_headers(storage, bucket, req, builder)
    }
}

pub(super) fn parse_multipart_form_upload(
    content_type: &str,
    body: &[u8],
) -> Option<(String, Vec<u8>, String)> {
    let boundary = content_type
        .split(';')
        .map(str::trim)
        .find_map(|part| part.strip_prefix("boundary="))?;
    let boundary_marker = format!("--{boundary}");
    let boundary_bytes = boundary_marker.as_bytes();

    let mut key: Option<String> = None;
    let mut file: Option<Vec<u8>> = None;
    let mut file_content_type = "application/octet-stream".to_string();

    for raw_part in split_bytes(body, boundary_bytes) {
        let part = raw_part.strip_prefix(b"\r\n").unwrap_or(raw_part);
        if part.is_empty() || part == b"--\r\n" || part == b"--" {
            continue;
        }
        let part = part
            .strip_suffix(b"--\r\n")
            .or_else(|| part.strip_suffix(b"--"))
            .unwrap_or(part);
        let Some((raw_headers, raw_body)) = split_once_bytes(part, b"\r\n\r\n") else {
            continue;
        };
        let field_body = raw_body.strip_suffix(b"\r\n").unwrap_or(raw_body);
        let raw_headers = std::str::from_utf8(raw_headers).ok()?;

        let mut field_name: Option<String> = None;
        let mut filename: Option<String> = None;
        for header in raw_headers.split("\r\n") {
            let lower = header.to_ascii_lowercase();
            if lower.starts_with("content-disposition:") {
                for token in header.split(';').skip(1).map(str::trim) {
                    if let Some(name) = token.strip_prefix("name=\"") {
                        field_name = Some(name.trim_end_matches('"').to_string());
                    } else if let Some(name) = token.strip_prefix("filename=\"") {
                        filename = Some(name.trim_end_matches('"').to_string());
                    }
                }
            } else if lower.starts_with("content-type:") {
                file_content_type = header.split_once(':').map_or_else(
                    || "application/octet-stream".to_string(),
                    |(_, value)| value.trim().to_string(),
                );
            }
        }

        match field_name.as_deref() {
            Some("key") => key = Some(String::from_utf8(field_body.to_vec()).ok()?),
            Some("file") if filename.is_some() => file = Some(field_body.to_vec()),
            _ => {}
        }
    }

    Some((key?, file?, file_content_type))
}

fn split_bytes<'a>(haystack: &'a [u8], needle: &[u8]) -> Vec<&'a [u8]> {
    let mut parts = Vec::new();
    let mut start = 0;

    while let Some(offset) = find_subslice(&haystack[start..], needle) {
        let end = start + offset;
        parts.push(&haystack[start..end]);
        start = end + needle.len();
    }

    parts.push(&haystack[start..]);
    parts
}

fn split_once_bytes<'a>(haystack: &'a [u8], needle: &[u8]) -> Option<(&'a [u8], &'a [u8])> {
    let index = find_subslice(haystack, needle)?;
    Some((&haystack[..index], &haystack[index + needle.len()..]))
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() {
        return Some(0);
    }

    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

pub(super) fn with_bucket_metadata<F>(
    storage: &(impl BucketStore + ?Sized),
    bucket: &str,
    update: F,
) -> crate::error::Result<crate::models::Bucket>
where
    F: FnOnce(&mut HashMap<String, String>),
{
    let mut bucket_record = bucket_service::get_bucket(storage, bucket)?;
    update(&mut bucket_record.metadata);
    bucket_service::update_bucket_metadata(storage, bucket, bucket_record.metadata)
}

pub(super) fn build_list_objects_v2_entries(
    objects: Vec<crate::models::Object>,
    prefix: &str,
    delimiter: Option<&str>,
) -> Vec<xml_utils::ListObjectsV2Entry> {
    let mut entries = Vec::new();
    let mut seen_common_prefixes = HashSet::new();
    let delimiter = delimiter.filter(|value| !value.is_empty());

    for object in objects {
        if let Some(delimiter) = delimiter {
            if let Some(stripped_key) = object.key.strip_prefix(prefix) {
                if let Some(index) = stripped_key.find(delimiter) {
                    let common_prefix =
                        format!("{}{}", prefix, &stripped_key[..index + delimiter.len()]);
                    if seen_common_prefixes.insert(common_prefix.clone()) {
                        entries.push(xml_utils::ListObjectsV2Entry::CommonPrefix(common_prefix));
                    }
                    continue;
                }
            }
        }

        entries.push(xml_utils::ListObjectsV2Entry::Object(object));
    }

    entries
}

pub(super) fn list_objects_v2_start_index(
    entries: &[xml_utils::ListObjectsV2Entry],
    continuation_token: Option<&str>,
    start_after: Option<&str>,
) -> usize {
    if let Some(token) = continuation_token {
        if let Some(position) = entries.iter().position(|entry| entry.token() == token) {
            return position + 1;
        }

        if let Some(position) = entries.iter().position(|entry| entry.token() > token) {
            return position;
        }

        return entries.len();
    }

    if let Some(start_after) = start_after {
        return entries
            .iter()
            .position(|entry| entry.token() > start_after)
            .unwrap_or(entries.len());
    }

    0
}

pub(super) fn encode_list_objects_v2_token(marker: &str) -> String {
    let digest = Sha256::digest(format!("sqrzl-list-v2:{marker}").as_bytes());
    URL_SAFE_NO_PAD.encode(format!("{marker}\0{}", hex::encode(digest)))
}

pub(super) fn decode_list_objects_v2_token(token: &str) -> Option<String> {
    let decoded = URL_SAFE_NO_PAD.decode(token).ok()?;
    let decoded = String::from_utf8(decoded).ok()?;
    let (marker, supplied_digest) = decoded.rsplit_once('\0')?;
    if marker.is_empty() {
        return None;
    }
    let expected_digest = Sha256::digest(format!("sqrzl-list-v2:{marker}").as_bytes());
    (supplied_digest == hex::encode(expected_digest)).then(|| marker.to_string())
}
