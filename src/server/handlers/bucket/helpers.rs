use crate::server::handlers::cors;
use crate::server::http::{Request, ResponseBuilder};
use crate::services::bucket as bucket_service;
use crate::storage::Storage;
use crate::utils::xml as xml_utils;
use quick_xml::escape::unescape;
use quick_xml::events::Event;
use quick_xml::Reader;
use std::collections::{HashMap, HashSet};

pub(super) const S3_REQUEST_PAYMENT_KEY: &str = "s3_requester_pays";
pub(super) const S3_WEBSITE_XML_KEY: &str = "s3_website_xml";
pub(super) const S3_CORS_XML_KEY: &str = "s3_cors_xml";

pub(super) fn escape_xml_str(input: &str) -> String {
    input
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace("'", "&apos;")
}

pub(super) fn parse_delete_keys(xml: &str) -> Vec<(String, Option<String>)> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);

    let mut buf = Vec::new();
    let mut in_key = false;
    let mut in_version = false;
    let mut current_key: Option<String> = None;
    let mut current_version: Option<String> = None;
    let mut objects = Vec::new();

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => match e.name().as_ref() {
                b"Key" => in_key = true,
                b"VersionId" => in_version = true,
                _ => {}
            },
            Ok(Event::Text(t)) => {
                if in_key {
                    let decoded = t.decode().unwrap_or_default();
                    current_key = Some(unescape(&decoded).unwrap_or_default().to_string());
                } else if in_version {
                    let decoded = t.decode().unwrap_or_default();
                    current_version = Some(unescape(&decoded).unwrap_or_default().to_string());
                }
            }
            Ok(Event::End(e)) => match e.name().as_ref() {
                b"Key" => in_key = false,
                b"VersionId" => in_version = false,
                b"Object" => {
                    if let Some(k) = current_key.take() {
                        objects.push((k, current_version.take()));
                    } else {
                        current_version = None;
                    }
                }
                _ => {}
            },
            Ok(Event::Eof) => break,
            Err(_) => break,
            _ => {}
        }
        buf.clear();
    }

    objects
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
            Ok(Event::Eof) => break,
            Err(_) => break,
            _ => {}
        }
        buf.clear();
    }

    None
}

pub(super) fn bucket_cors_snapshot(storage: &dyn Storage, bucket: &str) -> Option<String> {
    bucket_service::get_bucket(storage, bucket)
        .ok()
        .and_then(|bucket_record| bucket_record.metadata.get(S3_CORS_XML_KEY).cloned())
}

pub(super) fn apply_bucket_cors_headers(
    storage: &dyn Storage,
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
        .map(|part| part.trim())
        .find_map(|part| part.strip_prefix("boundary="))?;
    let boundary_marker = format!("--{}", boundary);
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
                for token in header.split(';').skip(1).map(|token| token.trim()) {
                    if let Some(name) = token.strip_prefix("name=\"") {
                        field_name = Some(name.trim_end_matches('"').to_string());
                    } else if let Some(name) = token.strip_prefix("filename=\"") {
                        filename = Some(name.trim_end_matches('"').to_string());
                    }
                }
            } else if lower.starts_with("content-type:") {
                file_content_type = header
                    .split_once(':')
                    .map(|(_, value)| value.trim().to_string())
                    .unwrap_or_else(|| "application/octet-stream".to_string());
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
    storage: &dyn Storage,
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
