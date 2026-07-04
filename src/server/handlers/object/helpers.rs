use crate::body::Body;
use crate::server::http::{Request, ResponseBuilder};
use crate::services::xml_error_response;
use crate::utils::headers as header_utils;
use crate::utils::xml as xml_utils;
use http::StatusCode;
use hyper::Response;
use std::collections::HashMap;
use urlencoding::decode;

const S3_SSE_MODE_KEY: &str = "s3_sse_mode";
const S3_SSE_KMS_KEY_ID: &str = "s3_sse_kms_key_id";
const S3_SSE_C_ALGORITHM_KEY: &str = "s3_sse_c_algorithm";
const S3_SSE_C_KEY_MD5_KEY: &str = "s3_sse_c_key_md5";
const S3_OBJECT_LOCK_MODE_KEY: &str = "s3_object_lock_mode";
const S3_OBJECT_LOCK_UNTIL_KEY: &str = "s3_object_lock_until";
const S3_OBJECT_LOCK_LEGAL_HOLD_KEY: &str = "s3_object_lock_legal_hold";

pub(super) fn upload_key_mismatch_response(req_id: &str) -> Response<Body> {
    xml_error_response(
        StatusCode::BAD_REQUEST,
        "InvalidRequest",
        "The upload ID is not valid for the specified object",
        req_id,
    )
}

pub(super) fn parse_range(range_header: &str) -> Option<(u64, Option<u64>)> {
    // Expect formats like "bytes=start-end" or "bytes=start-"
    let range = range_header.strip_prefix("bytes=")?;
    let (start_str, end_str_opt) = range.split_once('-')?;
    let start = start_str.parse::<u64>().ok()?;
    let end = if let Some(end_str) = end_str_opt.strip_prefix(' ') {
        end_str.parse::<u64>().ok()
    } else {
        end_str_opt.parse::<u64>().ok()
    };
    Some((start, end))
}

pub(super) fn parse_tagging_header(tag_header: &str) -> Result<HashMap<String, String>, String> {
    let mut tags = HashMap::new();
    for pair in tag_header.split('&') {
        if pair.is_empty() {
            continue;
        }
        let (k, v) = pair
            .split_once('=')
            .ok_or_else(|| "Invalid tagging format".to_string())?;
        let key = decode(k).map_err(|e| e.to_string())?.into_owned();
        let value = decode(v).map_err(|e| e.to_string())?.into_owned();
        tags.insert(key, value);
    }
    Ok(tags)
}

pub(super) fn copy_source_range_data(
    source_obj: &crate::models::Object,
    range_header: &str,
) -> Result<Vec<u8>, String> {
    let (start, end_opt) =
        parse_range(range_header).ok_or_else(|| "Invalid copy source range".to_string())?;

    let source_len = source_obj.data.len() as u64;
    if source_len == 0 || start >= source_len {
        return Err("Invalid copy source range".to_string());
    }

    let end = end_opt.unwrap_or(source_len - 1).min(source_len - 1);
    if end < start {
        return Err("Invalid copy source range".to_string());
    }

    let start_idx = usize::try_from(start).map_err(|_| "Invalid copy source range".to_string())?;
    let end_idx = usize::try_from(end).map_err(|_| "Invalid copy source range".to_string())?;
    Ok(source_obj.data[start_idx..=end_idx].to_vec())
}

pub(super) fn add_version_header(
    builder: ResponseBuilder,
    version_id: Option<&str>,
) -> ResponseBuilder {
    if let Some(version_id) = version_id {
        builder.header("x-amz-version-id", version_id)
    } else {
        builder
    }
}

fn normalize_etag(value: &str) -> &str {
    let value = value.trim();
    let value = value.strip_prefix("W/").unwrap_or(value);
    value.trim_matches('"')
}

fn etag_list_matches(header_value: &str, etag: &str) -> bool {
    let normalized_etag = normalize_etag(etag);

    header_value
        .split(',')
        .map(normalize_etag)
        .any(|candidate| candidate == "*" || candidate == normalized_etag)
}

pub(super) fn object_response_headers(
    mut builder: ResponseBuilder,
    obj: &crate::models::Object,
    req_id: &str,
) -> ResponseBuilder {
    let last_modified = header_utils::format_last_modified_at(&obj.last_modified);

    builder = builder
        .header("ETag", &obj.etag)
        .header("Last-Modified", &last_modified)
        .header("x-amz-request-id", req_id)
        .header("x-amz-id-2", &header_utils::generate_request_id())
        .header("x-amz-storage-class", &obj.storage_class)
        .header("Accept-Ranges", "bytes");

    builder = add_version_header(builder, obj.version_id.as_deref());

    for (k, v) in &obj.metadata {
        builder = builder.header(&format!("x-amz-meta-{k}"), v);
    }

    if let Some(value) = obj.provider_metadata.get(S3_SSE_MODE_KEY) {
        builder = builder.header("x-amz-server-side-encryption", value);
    }
    if let Some(value) = obj.provider_metadata.get(S3_SSE_KMS_KEY_ID) {
        builder = builder.header("x-amz-server-side-encryption-aws-kms-key-id", value);
    }
    if let Some(value) = obj.provider_metadata.get(S3_SSE_C_ALGORITHM_KEY) {
        builder = builder.header("x-amz-server-side-encryption-customer-algorithm", value);
    }
    if let Some(value) = obj.provider_metadata.get(S3_SSE_C_KEY_MD5_KEY) {
        builder = builder.header("x-amz-server-side-encryption-customer-key-MD5", value);
    }
    if let Some(value) = obj.provider_metadata.get(S3_OBJECT_LOCK_MODE_KEY) {
        builder = builder.header("x-amz-object-lock-mode", value);
    }
    if let Some(value) = obj.provider_metadata.get(S3_OBJECT_LOCK_UNTIL_KEY) {
        builder = builder.header("x-amz-object-lock-retain-until-date", value);
    }
    if let Some(value) = obj.provider_metadata.get(S3_OBJECT_LOCK_LEGAL_HOLD_KEY) {
        builder = builder.header("x-amz-object-lock-legal-hold", value);
    }

    builder
}

fn parse_lock_timestamp(value: &str) -> Option<chrono::DateTime<chrono::Utc>> {
    chrono::DateTime::parse_from_rfc3339(value)
        .or_else(|_| chrono::DateTime::parse_from_rfc2822(value))
        .ok()
        .map(|value| value.with_timezone(&chrono::Utc))
}

pub(super) fn object_is_locked(obj: &crate::models::Object) -> bool {
    obj.provider_metadata
        .get(S3_OBJECT_LOCK_LEGAL_HOLD_KEY)
        .is_some_and(|value| value.eq_ignore_ascii_case("ON"))
        || obj
            .provider_metadata
            .get(S3_OBJECT_LOCK_UNTIL_KEY)
            .and_then(|value| parse_lock_timestamp(value))
            .is_some_and(|value| value > chrono::Utc::now())
}

pub(super) fn locked_object_response(req_id: &str) -> Response<Body> {
    xml_error_response(
        StatusCode::FORBIDDEN,
        "AccessDenied",
        "Object is protected by an active retention policy or legal hold",
        req_id,
    )
}

pub(super) fn validate_get_sse_headers(
    req: &Request,
    obj: &crate::models::Object,
    req_id: &str,
) -> Option<Response<Body>> {
    if let Some(expected_algorithm) = obj.provider_metadata.get(S3_SSE_C_ALGORITHM_KEY) {
        let provided_algorithm = req.header("x-amz-server-side-encryption-customer-algorithm");
        let provided_md5 = req.header("x-amz-server-side-encryption-customer-key-MD5");
        let Some(provided_algorithm) = provided_algorithm else {
            return Some(xml_error_response(
                StatusCode::BAD_REQUEST,
                "InvalidRequest",
                "Missing SSE-C algorithm for customer encrypted object",
                req_id,
            ));
        };
        let Some(provided_md5) = provided_md5 else {
            return Some(xml_error_response(
                StatusCode::BAD_REQUEST,
                "InvalidRequest",
                "Missing SSE-C key MD5 for customer encrypted object",
                req_id,
            ));
        };
        if provided_algorithm != expected_algorithm
            || obj
                .provider_metadata
                .get(S3_SSE_C_KEY_MD5_KEY)
                .is_none_or(|value| value != provided_md5)
        {
            return Some(xml_error_response(
                StatusCode::FORBIDDEN,
                "AccessDenied",
                "The provided SSE-C headers do not match the stored object",
                req_id,
            ));
        }
    }
    None
}

#[allow(clippy::result_large_err)]
pub(super) fn apply_s3_request_contracts(
    req: &Request,
    obj: &mut crate::models::Object,
    req_id: &str,
) -> Result<(), Response<Body>> {
    if let Some(mode) = req.header("x-amz-server-side-encryption") {
        if mode != "AES256" && mode != "aws:kms" {
            return Err(xml_error_response(
                StatusCode::BAD_REQUEST,
                "InvalidArgument",
                "Unsupported server-side encryption mode",
                req_id,
            ));
        }
        obj.provider_metadata
            .insert(S3_SSE_MODE_KEY.to_string(), mode.to_string());
        if mode == "aws:kms" {
            if let Some(key_id) = req.header("x-amz-server-side-encryption-aws-kms-key-id") {
                obj.provider_metadata
                    .insert(S3_SSE_KMS_KEY_ID.to_string(), key_id.to_string());
            }
        } else {
            obj.provider_metadata.remove(S3_SSE_KMS_KEY_ID);
        }
    }

    let sse_c_algorithm = req.header("x-amz-server-side-encryption-customer-algorithm");
    let sse_c_key = req.header("x-amz-server-side-encryption-customer-key");
    let sse_c_md5 = req.header("x-amz-server-side-encryption-customer-key-MD5");
    if sse_c_algorithm.is_some() || sse_c_key.is_some() || sse_c_md5.is_some() {
        if sse_c_algorithm.is_none() || sse_c_key.is_none() || sse_c_md5.is_none() {
            return Err(xml_error_response(
                StatusCode::BAD_REQUEST,
                "InvalidRequest",
                "SSE-C requests must include algorithm, key, and key MD5 headers",
                req_id,
            ));
        }
        obj.provider_metadata.insert(
            S3_SSE_C_ALGORITHM_KEY.to_string(),
            sse_c_algorithm.unwrap_or_default().to_string(),
        );
        obj.provider_metadata.insert(
            S3_SSE_C_KEY_MD5_KEY.to_string(),
            sse_c_md5.unwrap_or_default().to_string(),
        );
        obj.provider_metadata.remove(S3_SSE_MODE_KEY);
        obj.provider_metadata.remove(S3_SSE_KMS_KEY_ID);
    }

    if let Some(mode) = req.header("x-amz-object-lock-mode") {
        if mode != "GOVERNANCE" && mode != "COMPLIANCE" {
            return Err(xml_error_response(
                StatusCode::BAD_REQUEST,
                "InvalidRequest",
                "Unsupported object lock mode",
                req_id,
            ));
        }
        obj.provider_metadata
            .insert(S3_OBJECT_LOCK_MODE_KEY.to_string(), mode.to_string());
    }
    if let Some(until) = req.header("x-amz-object-lock-retain-until-date") {
        if parse_lock_timestamp(until).is_none() {
            return Err(xml_error_response(
                StatusCode::BAD_REQUEST,
                "InvalidRequest",
                "Invalid object lock retain-until date",
                req_id,
            ));
        }
        obj.provider_metadata
            .insert(S3_OBJECT_LOCK_UNTIL_KEY.to_string(), until.to_string());
    }
    if let Some(legal_hold) = req.header("x-amz-object-lock-legal-hold") {
        if !legal_hold.eq_ignore_ascii_case("ON") && !legal_hold.eq_ignore_ascii_case("OFF") {
            return Err(xml_error_response(
                StatusCode::BAD_REQUEST,
                "InvalidRequest",
                "Invalid object lock legal hold value",
                req_id,
            ));
        }
        obj.provider_metadata.insert(
            S3_OBJECT_LOCK_LEGAL_HOLD_KEY.to_string(),
            legal_hold.to_ascii_uppercase(),
        );
    }

    Ok(())
}

fn precondition_failed_response(req_id: &str) -> Response<Body> {
    let xml = xml_utils::error_xml(
        "PreconditionFailed",
        "At least one of the pre-conditions you specified did not hold",
        req_id,
    );

    ResponseBuilder::new(StatusCode::PRECONDITION_FAILED)
        .content_type("application/xml; charset=utf-8")
        .header("x-amz-request-id", req_id)
        .body(xml.into_bytes())
        .build()
}

fn not_modified_response(obj: &crate::models::Object, req_id: &str) -> Response<Body> {
    object_response_headers(ResponseBuilder::new(StatusCode::NOT_MODIFIED), obj, req_id).empty()
}

pub(super) fn check_object_conditionals(
    req: &Request,
    obj: &crate::models::Object,
    req_id: &str,
) -> Option<Response<Body>> {
    if let Some(if_match) = req.header("if-match") {
        if !etag_list_matches(if_match, &obj.etag) {
            return Some(precondition_failed_response(req_id));
        }
    }

    if let Some(if_unmodified_since) = req.header("if-unmodified-since") {
        if let Ok(since_dt) = chrono::DateTime::parse_from_rfc2822(if_unmodified_since) {
            if obj.last_modified > since_dt.with_timezone(&chrono::Utc) {
                return Some(precondition_failed_response(req_id));
            }
        }
    }

    if let Some(if_none_match) = req.header("if-none-match") {
        if etag_list_matches(if_none_match, &obj.etag) {
            return Some(not_modified_response(obj, req_id));
        }
    }

    if let Some(if_modified_since) = req.header("if-modified-since") {
        if let Ok(since_dt) = chrono::DateTime::parse_from_rfc2822(if_modified_since) {
            if obj.last_modified <= since_dt.with_timezone(&chrono::Utc) {
                return Some(not_modified_response(obj, req_id));
            }
        }
    }

    None
}

pub(super) fn check_put_conditionals(
    req: &Request,
    existing_obj: Option<&crate::models::Object>,
    req_id: &str,
) -> Option<Response<Body>> {
    if let Some(existing_obj) = existing_obj {
        if let Some(if_match) = req.header("if-match") {
            if !etag_list_matches(if_match, &existing_obj.etag) {
                return Some(precondition_failed_response(req_id));
            }
        }

        if let Some(if_unmodified_since) = req.header("if-unmodified-since") {
            if let Ok(since_dt) = chrono::DateTime::parse_from_rfc2822(if_unmodified_since) {
                if existing_obj.last_modified > since_dt.with_timezone(&chrono::Utc) {
                    return Some(precondition_failed_response(req_id));
                }
            }
        }

        if let Some(if_none_match) = req.header("if-none-match") {
            if etag_list_matches(if_none_match, &existing_obj.etag) {
                return Some(precondition_failed_response(req_id));
            }
        }
    } else if req.header("if-match").is_some() {
        return Some(precondition_failed_response(req_id));
    }

    None
}

pub(super) fn check_copy_conditionals(
    req: &Request,
    source_obj: &crate::models::Object,
    req_id: &str,
) -> Option<Response<Body>> {
    // x-amz-copy-source-if-match: copy if source ETag matches
    if let Some(match_etag) = req.header("x-amz-copy-source-if-match") {
        if source_obj.etag != match_etag {
            let xml = xml_utils::error_xml(
                "PreconditionFailed",
                "At least one of the pre-conditions you specified did not hold",
                req_id,
            );
            return Some(
                ResponseBuilder::new(StatusCode::PRECONDITION_FAILED)
                    .content_type("application/xml; charset=utf-8")
                    .header("x-amz-request-id", req_id)
                    .body(xml.into_bytes())
                    .build(),
            );
        }
    }

    // x-amz-copy-source-if-none-match: copy if source ETag does NOT match
    if let Some(none_match) = req.header("x-amz-copy-source-if-none-match") {
        if source_obj.etag == none_match {
            let xml = xml_utils::error_xml(
                "PreconditionFailed",
                "At least one of the pre-conditions you specified did not hold",
                req_id,
            );
            return Some(
                ResponseBuilder::new(StatusCode::PRECONDITION_FAILED)
                    .content_type("application/xml; charset=utf-8")
                    .header("x-amz-request-id", req_id)
                    .body(xml.into_bytes())
                    .build(),
            );
        }
    }

    // x-amz-copy-source-if-modified-since: copy if modified after date
    if let Some(modified_since) = req.header("x-amz-copy-source-if-modified-since") {
        if let Ok(since_dt) = chrono::DateTime::parse_from_rfc2822(modified_since) {
            if source_obj.last_modified <= since_dt.with_timezone(&chrono::Utc) {
                return Some(
                    ResponseBuilder::new(StatusCode::NOT_MODIFIED)
                        .header("x-amz-request-id", req_id)
                        .empty(),
                );
            }
        }
    }

    // x-amz-copy-source-if-unmodified-since: copy if NOT modified after date
    if let Some(unmodified_since) = req.header("x-amz-copy-source-if-unmodified-since") {
        if let Ok(since_dt) = chrono::DateTime::parse_from_rfc2822(unmodified_since) {
            if source_obj.last_modified > since_dt.with_timezone(&chrono::Utc) {
                let xml = xml_utils::error_xml(
                    "PreconditionFailed",
                    "At least one of the pre-conditions you specified did not hold",
                    req_id,
                );
                return Some(
                    ResponseBuilder::new(StatusCode::PRECONDITION_FAILED)
                        .content_type("application/xml; charset=utf-8")
                        .header("x-amz-request-id", req_id)
                        .body(xml.into_bytes())
                        .build(),
                );
            }
        }
    }

    None
}
