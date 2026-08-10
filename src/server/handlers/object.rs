use super::acl;
use super::auth::{check_authorization, verify_presigned_url};
use super::cors;
use super::{s3_foreign_history_conflict, s3_foreign_history_conflict_response, ResponseBuilder};
use crate::auth::AuthConfig;
use crate::body::Body;
use crate::services::{
    object as object_service, storage_error_response, xml_error_response, xml_success_response,
};
use crate::storage::Storage;
use crate::utils::{headers as header_utils, validation, xml as xml_utils};
use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use http::StatusCode;
use hyper::Response;
use std::collections::HashMap;
use std::sync::Arc;

mod helpers;

use self::helpers::{
    add_version_header, apply_s3_request_contracts, check_copy_conditionals,
    check_object_conditionals, check_put_conditionals, clear_object_lock_metadata,
    locked_object_response, mutation_condition, mutation_if_match_condition, object_is_locked,
    object_response_headers, parse_range, parse_tagging_header, quoted_etag,
    upload_key_mismatch_response, validate_get_sse_headers,
};

fn missing_object_bucket_response(req_id: &str, head: bool) -> Response<Body> {
    if head {
        return ResponseBuilder::new(StatusCode::NOT_FOUND)
            .header("x-amz-request-id", req_id)
            .header("x-amz-id-2", &header_utils::generate_request_id())
            .empty();
    }
    xml_error_response(
        StatusCode::NOT_FOUND,
        "NoSuchBucket",
        "The specified bucket does not exist.",
        req_id,
    )
}

fn object_bucket_error_response(
    storage: &dyn Storage,
    bucket: &str,
    req_id: &str,
    head: bool,
) -> Option<Response<Body>> {
    match storage.get_bucket(bucket) {
        Ok(_) => None,
        Err(crate::error::Error::BucketNotFound) => {
            Some(missing_object_bucket_response(req_id, head))
        }
        Err(error) => Some(if head {
            ResponseBuilder::new(StatusCode::INTERNAL_SERVER_ERROR)
                .header("x-amz-request-id", req_id)
                .header("x-amz-id-2", &header_utils::generate_request_id())
                .empty()
        } else {
            storage_error_response(&error, req_id)
        }),
    }
}

fn unsupported_object_lock_subresource_response(
    req: &crate::server::http::Request,
    req_id: &str,
) -> Option<Response<Body>> {
    ["retention", "legal-hold"]
        .into_iter()
        .find(|parameter| req.has_query_param(parameter))
        .map(|parameter| {
            xml_error_response(
                StatusCode::NOT_IMPLEMENTED,
                "NotImplemented",
                &format!(
                    "The S3 object {parameter} subresource is not supported by this emulator."
                ),
                req_id,
            )
        })
}

pub async fn object_get(
    storage: Arc<dyn Storage>,
    auth_config: Arc<AuthConfig>,
    bucket: &str,
    key: &str,
    req: &crate::server::http::Request,
    req_id: String,
) -> Result<Response<Body>, String> {
    if cors::is_preflight(req) {
        return Ok(cors::preflight_response(
            storage.as_ref(),
            bucket,
            req,
            &req_id,
        ));
    }

    if let Err(response) = check_authorization(
        req,
        &auth_config,
        &storage,
        bucket,
        Some(key),
        "s3:GetObject",
    ) {
        return Ok(response);
    }

    // Verify presigned URL if present
    if let Err(response) = verify_presigned_url(req, bucket, key, &auth_config) {
        return Ok(response);
    }

    if let Some(response) = object_bucket_error_response(storage.as_ref(), bucket, &req_id, false) {
        return Ok(response);
    }

    if let Some(response) = unsupported_object_lock_subresource_response(req, &req_id) {
        return Ok(response);
    }

    if req.has_query_param("tagging") && req.has_query_param("versionId") {
        return Ok(unsupported_version_tagging_response(&req_id));
    }

    if s3_foreign_history_conflict(storage.as_ref(), bucket) {
        return Ok(s3_foreign_history_conflict_response(&req_id));
    }

    if object_expired(&storage, bucket, key) {
        return Ok(xml_error_response(
            StatusCode::NOT_FOUND,
            "NoSuchKey",
            "Key not found",
            &req_id,
        ));
    }

    if req.has_query_param("tagging") {
        return Ok(object_tagging_response(&storage, bucket, key, &req_id));
    }

    if req.has_query_param("acl") {
        return Ok(object_acl_response(&storage, bucket, key, &req_id));
    }

    if let Some(version_id) = req.query_param("versionId") {
        return Ok(object_version_response(
            &storage, bucket, key, version_id, req, &req_id,
        ));
    }

    if req.has_query_param("uploadId") {
        let upload_id = req.query_param("uploadId").unwrap_or("");
        return Ok(object_parts_response(
            &storage, bucket, key, upload_id, &req_id,
        ));
    }

    if let Some(range) = req.header("range") {
        return Ok(object_range_response(
            &storage, bucket, key, req, &req_id, range,
        ));
    }

    Ok(object_full_response(&storage, bucket, key, req, &req_id))
}

fn object_expired(storage: &Arc<dyn Storage>, bucket: &str, key: &str) -> bool {
    tokio::task::block_in_place(|| crate::lifecycle::check_object_expiration(storage, bucket, key))
        .unwrap_or(false)
}

fn object_tagging_response(
    storage: &Arc<dyn Storage>,
    bucket: &str,
    key: &str,
    req_id: &str,
) -> Response<Body> {
    match tokio::task::block_in_place(|| {
        object_service::get_object_tags(storage.as_ref(), bucket, key)
    }) {
        Ok(tags) => xml_success_response(StatusCode::OK, xml_utils::tagging_xml(&tags), req_id),
        Err(e) => storage_error_response(&e, req_id),
    }
}

fn object_acl_response(
    storage: &Arc<dyn Storage>,
    bucket: &str,
    key: &str,
    req_id: &str,
) -> Response<Body> {
    match tokio::task::block_in_place(|| {
        object_service::get_object_acl(storage.as_ref(), bucket, key)
    }) {
        Ok(acl) => {
            let owner = crate::models::policy::Owner {
                id: "sqrzl-emulator".to_string(),
                display_name: "S3 Emulator".to_string(),
            };
            xml_success_response(StatusCode::OK, xml_utils::acl_xml(&owner, &acl), req_id)
        }
        Err(e) => storage_error_response(&e, req_id),
    }
}

fn object_version_response(
    storage: &Arc<dyn Storage>,
    bucket: &str,
    key: &str,
    version_id: &str,
    req: &crate::server::http::Request,
    req_id: &str,
) -> Response<Body> {
    match tokio::task::block_in_place(|| {
        object_service::get_object_version(storage.as_ref(), bucket, key, version_id)
    }) {
        Ok(obj) if is_s3_delete_marker(&obj) => {
            delete_marker_response(storage, bucket, req, req_id, &obj, true, false)
        }
        Ok(obj) => object_payload_response(storage, bucket, req, req_id, obj, StatusCode::OK, None),
        Err(e) => storage_error_response(&e, req_id),
    }
}

fn is_s3_delete_marker(object: &crate::models::Object) -> bool {
    object
        .provider_metadata
        .get("s3_delete_marker")
        .is_some_and(|value| value == "true")
}

fn current_delete_marker(
    storage: &Arc<dyn Storage>,
    bucket: &str,
    key: &str,
) -> Option<crate::models::Object> {
    tokio::task::block_in_place(|| {
        object_service::list_object_versions_for_key(storage.as_ref(), bucket, key)
    })
    .ok()?
    .into_iter()
    .max_by(|left, right| {
        left.last_modified
            .cmp(&right.last_modified)
            .then_with(|| left.version_id.cmp(&right.version_id))
    })
    .filter(is_s3_delete_marker)
}

fn delete_marker_response(
    storage: &Arc<dyn Storage>,
    bucket: &str,
    req: &crate::server::http::Request,
    req_id: &str,
    marker: &crate::models::Object,
    version_specific: bool,
    head: bool,
) -> Response<Body> {
    let status = if version_specific {
        StatusCode::METHOD_NOT_ALLOWED
    } else {
        StatusCode::NOT_FOUND
    };
    let host_id = header_utils::generate_request_id();
    let mut builder = ResponseBuilder::new(status)
        .header("x-amz-request-id", req_id)
        .header("x-amz-id-2", &host_id)
        .header("x-amz-delete-marker", "true");
    if let Some(version_id) = marker.version_id.as_deref() {
        builder = builder.header("x-amz-version-id", version_id);
    }
    if version_specific {
        builder = builder.header(
            "Last-Modified",
            &header_utils::format_last_modified_at(&marker.last_modified),
        );
    }
    builder = cors::apply_actual_request_headers(storage.as_ref(), bucket, req, builder);
    if head {
        return builder.empty();
    }

    let (code, message) = if version_specific {
        (
            "MethodNotAllowed",
            "The specified method is not allowed against this resource.",
        )
    } else {
        ("NoSuchKey", "The specified key does not exist.")
    };
    builder
        .content_type("application/xml; charset=utf-8")
        .body(xml_utils::error_xml_with_host_id(code, message, req_id, &host_id).into_bytes())
        .build()
}

fn object_parts_response(
    storage: &Arc<dyn Storage>,
    bucket: &str,
    key: &str,
    upload_id: &str,
    req_id: &str,
) -> Response<Body> {
    match tokio::task::block_in_place(|| {
        object_service::list_parts(storage.as_ref(), bucket, upload_id)
    }) {
        Ok(parts) => {
            let xml = xml_utils::list_parts_xml(bucket, key, upload_id, &parts);
            ResponseBuilder::new(StatusCode::OK)
                .content_type("application/xml; charset=utf-8")
                .header("x-amz-request-id", req_id)
                .body(xml.into_bytes())
                .build()
        }
        Err(e) => xml_error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "InternalError",
            &e.to_string(),
            req_id,
        ),
    }
}

fn object_range_response(
    storage: &Arc<dyn Storage>,
    bucket: &str,
    key: &str,
    req: &crate::server::http::Request,
    req_id: &str,
    range_header: &str,
) -> Response<Body> {
    let Some((start, end)) = parse_range(range_header) else {
        return xml_error_response(
            StatusCode::RANGE_NOT_SATISFIABLE,
            "InvalidRange",
            "Invalid Range header",
            req_id,
        );
    };

    match tokio::task::block_in_place(|| {
        object_service::get_object_range(storage.as_ref(), bucket, key, start, end)
    }) {
        Ok((obj, data)) => {
            let len = data.len() as u64;
            let end_idx = start + len.saturating_sub(1);
            let content_range = format!("bytes {}-{}/{}", start, end_idx, obj.size);
            object_payload_response(
                storage,
                bucket,
                req,
                req_id,
                obj,
                StatusCode::PARTIAL_CONTENT,
                Some((data, len, content_range)),
            )
        }
        Err(e) => current_delete_marker(storage, bucket, key).map_or_else(
            || match e {
                crate::error::Error::KeyNotFound | crate::error::Error::NoSuchVersion => {
                    xml_error_response(
                        StatusCode::NOT_FOUND,
                        "NoSuchKey",
                        "The specified key does not exist.",
                        req_id,
                    )
                }
                _ => xml_error_response(
                    StatusCode::RANGE_NOT_SATISFIABLE,
                    "InvalidRange",
                    &e.to_string(),
                    req_id,
                ),
            },
            |marker| delete_marker_response(storage, bucket, req, req_id, &marker, false, false),
        ),
    }
}

fn object_full_response(
    storage: &Arc<dyn Storage>,
    bucket: &str,
    key: &str,
    req: &crate::server::http::Request,
    req_id: &str,
) -> Response<Body> {
    match tokio::task::block_in_place(|| object_service::get_object(storage.as_ref(), bucket, key))
    {
        Ok(obj) => object_payload_response(storage, bucket, req, req_id, obj, StatusCode::OK, None),
        Err(e) => current_delete_marker(storage, bucket, key).map_or_else(
            || xml_error_response(StatusCode::NOT_FOUND, "NoSuchKey", &e.to_string(), req_id),
            |marker| delete_marker_response(storage, bucket, req, req_id, &marker, false, false),
        ),
    }
}

fn object_payload_response(
    storage: &Arc<dyn Storage>,
    bucket: &str,
    req: &crate::server::http::Request,
    req_id: &str,
    mut obj: crate::models::Object,
    status: StatusCode,
    range: Option<(Vec<u8>, u64, String)>,
) -> Response<Body> {
    if let Some(response) = validate_get_sse_headers(req, &obj, req_id) {
        return response;
    }
    if let Some(response) = check_object_conditionals(req, &obj, req_id) {
        return response;
    }

    let (data, content_length, content_range) = match range {
        Some((data, len, content_range)) => (data, len, Some(content_range)),
        None => (std::mem::take(&mut obj.data), obj.size, None),
    };
    let mut builder = ResponseBuilder::new(status)
        .content_type(&obj.content_type)
        .header("Content-Length", &content_length.to_string());

    if let Some(content_range) = content_range {
        builder = builder.header("Content-Range", &content_range);
    }

    let builder = object_response_headers(builder, &obj, req_id);
    cors::apply_actual_request_headers(storage.as_ref(), bucket, req, builder)
        .body(data)
        .build()
}

pub async fn object_put(
    storage: Arc<dyn Storage>,
    auth_config: Arc<AuthConfig>,
    bucket: &str,
    key: &str,
    req: &crate::server::http::Request,
    req_id: String,
) -> Result<Response<Body>, String> {
    if let Err(response) = check_authorization(
        req,
        &auth_config,
        &storage,
        bucket,
        Some(key),
        "s3:PutObject",
    ) {
        return Ok(response);
    }

    // Verify presigned URL if present
    if let Err(response) = verify_presigned_url(req, bucket, key, &auth_config) {
        return Ok(response);
    }

    if let Some(response) = object_bucket_error_response(storage.as_ref(), bucket, &req_id, false) {
        return Ok(response);
    }

    if let Some(response) = unsupported_object_lock_subresource_response(req, &req_id) {
        return Ok(response);
    }

    if req.has_query_param("tagging") && req.has_query_param("versionId") {
        return Ok(unsupported_version_tagging_response(&req_id));
    }

    if s3_foreign_history_conflict(storage.as_ref(), bucket) {
        return Ok(s3_foreign_history_conflict_response(&req_id));
    }

    if let Some(response) = validate_object_lock_put_request(&storage, bucket, req, &req_id) {
        return Ok(response);
    }

    let existing =
        tokio::task::block_in_place(|| object_service::get_object(storage.as_ref(), bucket, key))
            .ok();
    if let Some(existing) = existing.as_ref() {
        if object_is_locked(existing)
            && !s3_mutation_preserves_current_version(&storage, bucket, existing)
        {
            return Ok(locked_object_response(&req_id));
        }
    }
    if let Some(response) = check_put_conditionals(req, existing.as_ref(), &req_id) {
        return Ok(response);
    }

    if req.has_query_param("tagging") {
        return put_object_tagging(&storage, bucket, key, req, &req_id);
    }

    if req.has_query_param("acl") {
        return Ok(put_object_acl(&storage, bucket, key, req, &req_id));
    }

    if req.has_query_param("uploadId") && req.query_param("partNumber").is_some() {
        if req.header("x-amz-copy-source").is_some() {
            return Ok(xml_error_response(
                StatusCode::NOT_IMPLEMENTED,
                "NotImplemented",
                "UploadPartCopy is not implemented by this emulator.",
                &req_id,
            ));
        }
        return Ok(upload_multipart_part(&storage, bucket, req, &req_id));
    }

    if let Some(copy_source) = req.header("x-amz-copy-source") {
        return Ok(copy_object(
            &storage,
            bucket,
            key,
            copy_source,
            req,
            &req_id,
        ));
    }

    Ok(put_object_body(
        &storage,
        bucket,
        key,
        req,
        &req_id,
        existing.as_ref(),
    ))
}

fn s3_mutation_preserves_current_version(
    storage: &Arc<dyn Storage>,
    bucket: &str,
    current: &crate::models::Object,
) -> bool {
    let Ok(bucket) = storage.get_bucket(bucket) else {
        return false;
    };
    if bucket.versioning_enabled {
        return true;
    }

    bucket
        .metadata
        .get("s3_versioning_status")
        .is_some_and(|status| status == "Suspended")
        && current.version_id.as_deref() != Some("null")
}

fn validate_object_lock_put_request(
    storage: &Arc<dyn Storage>,
    bucket: &str,
    req: &crate::server::http::Request,
    req_id: &str,
) -> Option<Response<Body>> {
    if !object_lock_headers_requested(req) {
        return None;
    }

    let lock_enabled = storage
        .get_bucket(bucket)
        .ok()
        .and_then(|bucket| bucket.metadata.get("s3_object_lock_enabled").cloned())
        .is_some_and(|value| value == "true");
    if !lock_enabled {
        return Some(xml_error_response(
            StatusCode::BAD_REQUEST,
            "InvalidRequest",
            "Bucket is missing an Object Lock configuration.",
            req_id,
        ));
    }

    let mode = req.header("x-amz-object-lock-mode");
    let retain_until = req.header("x-amz-object-lock-retain-until-date");
    if mode.is_some() != retain_until.is_some() {
        return Some(xml_error_response(
            StatusCode::BAD_REQUEST,
            "InvalidRequest",
            "Object Lock mode and retain-until date must be supplied together.",
            req_id,
        ));
    }
    if let Some(mode) = mode {
        if !matches!(mode, "GOVERNANCE" | "COMPLIANCE") {
            return Some(xml_error_response(
                StatusCode::BAD_REQUEST,
                "InvalidRequest",
                "Unsupported object lock mode",
                req_id,
            ));
        }
        let valid_future_date = retain_until
            .and_then(helpers::parse_lock_timestamp)
            .is_some_and(|until| until > chrono::Utc::now());
        if !valid_future_date {
            return Some(xml_error_response(
                StatusCode::BAD_REQUEST,
                "InvalidRequest",
                "Object Lock retain-until date must be a valid future timestamp.",
                req_id,
            ));
        }
    }

    if req.header("x-amz-copy-source").is_none() && req.header("content-md5").is_none() {
        let message = if req.header("x-amz-sdk-checksum-algorithm").is_some() {
            "SDK checksum algorithms for Object Lock PUT are not supported by this emulator subset."
        } else {
            "Content-MD5 is required when Object Lock parameters are supplied."
        };
        return Some(xml_error_response(
            if req.header("x-amz-sdk-checksum-algorithm").is_some() {
                StatusCode::NOT_IMPLEMENTED
            } else {
                StatusCode::BAD_REQUEST
            },
            if req.header("x-amz-sdk-checksum-algorithm").is_some() {
                "NotImplemented"
            } else {
                "InvalidRequest"
            },
            message,
            req_id,
        ));
    }

    None
}

fn object_lock_headers_requested(req: &crate::server::http::Request) -> bool {
    [
        "x-amz-object-lock-mode",
        "x-amz-object-lock-retain-until-date",
        "x-amz-object-lock-legal-hold",
    ]
    .into_iter()
    .any(|header| req.header(header).is_some())
}

fn validate_content_md5(
    req: &crate::server::http::Request,
    req_id: &str,
) -> Option<Response<Body>> {
    let supplied = req.header("content-md5")?;
    let Ok(decoded) = BASE64.decode(supplied) else {
        return Some(xml_error_response(
            StatusCode::BAD_REQUEST,
            "InvalidDigest",
            "The Content-MD5 value is not valid base64.",
            req_id,
        ));
    };
    if decoded.len() != 16 {
        return Some(xml_error_response(
            StatusCode::BAD_REQUEST,
            "InvalidDigest",
            "The Content-MD5 value is not a valid MD5 digest.",
            req_id,
        ));
    }
    if decoded.as_slice() != md5::compute(&req.body).0 {
        return Some(xml_error_response(
            StatusCode::BAD_REQUEST,
            "BadDigest",
            "The Content-MD5 value does not match the request body.",
            req_id,
        ));
    }
    None
}

fn put_object_tagging(
    storage: &Arc<dyn Storage>,
    bucket: &str,
    key: &str,
    req: &crate::server::http::Request,
    req_id: &str,
) -> Result<Response<Body>, String> {
    if tokio::task::block_in_place(|| object_service::get_object(storage.as_ref(), bucket, key))
        .as_ref()
        .is_ok_and(object_is_locked)
    {
        return Ok(locked_object_response(req_id));
    }

    let body =
        String::from_utf8(req.body.to_vec()).map_err(|e| format!("Invalid UTF-8 body: {e}"))?;
    let tags = match xml_utils::parse_tagging_xml(&body) {
        Ok(tags) => tags,
        Err(message) => {
            return Ok(xml_error_response(
                StatusCode::BAD_REQUEST,
                "MalformedXML",
                &message,
                req_id,
            ));
        }
    };

    match tokio::task::block_in_place(|| {
        object_service::put_object_tags(storage.as_ref(), bucket, key, tags)
    }) {
        Ok(()) => Ok(ok_empty_object_response(
            storage.as_ref(),
            bucket,
            req,
            req_id,
        )),
        Err(crate::error::Error::KeyNotFound) => Ok(xml_error_response(
            StatusCode::NOT_FOUND,
            "NoSuchKey",
            "Key not found",
            req_id,
        )),
        Err(err) => Ok(internal_error_response(&err, req_id)),
    }
}

fn put_object_acl(
    storage: &Arc<dyn Storage>,
    bucket: &str,
    key: &str,
    req: &crate::server::http::Request,
    req_id: &str,
) -> Response<Body> {
    let acl = match if req.body.is_empty() {
        acl::acl_from_headers(req).map_err(|message| ("InvalidArgument", message))
    } else {
        acl::acl_from_xml_body(&req.body).map_err(|message| ("MalformedXML", message))
    } {
        Ok(acl) => acl,
        Err((code, message)) => {
            return xml_error_response(StatusCode::BAD_REQUEST, code, &message, req_id);
        }
    };

    match tokio::task::block_in_place(|| {
        object_service::put_object_acl(storage.as_ref(), bucket, key, acl)
    }) {
        Ok(()) => ok_empty_object_response(storage.as_ref(), bucket, req, req_id),
        Err(crate::error::Error::KeyNotFound) => {
            xml_error_response(StatusCode::NOT_FOUND, "NoSuchKey", "Key not found", req_id)
        }
        Err(err) => internal_error_response(&err, req_id),
    }
}

fn upload_multipart_part(
    storage: &Arc<dyn Storage>,
    bucket: &str,
    req: &crate::server::http::Request,
    req_id: &str,
) -> Response<Body> {
    let upload_id = req.query_param("uploadId").unwrap_or("");
    let part_number = req
        .query_param("partNumber")
        .and_then(|part| part.parse::<u32>().ok())
        .unwrap_or(0);

    match tokio::task::block_in_place(|| {
        object_service::upload_part(
            storage.as_ref(),
            bucket,
            upload_id,
            part_number,
            req.body.to_vec(),
        )
    }) {
        Ok(etag) => {
            let builder = ResponseBuilder::new(StatusCode::OK)
                .header("ETag", &quoted_etag(&etag))
                .header("x-amz-request-id", req_id)
                .header("x-amz-id-2", &header_utils::generate_request_id());
            cors::apply_actual_request_headers(storage.as_ref(), bucket, req, builder).empty()
        }
        Err(err) => storage_error_response(&err, req_id),
    }
}

fn copy_object(
    storage: &Arc<dyn Storage>,
    bucket: &str,
    key: &str,
    copy_source: &str,
    req: &crate::server::http::Request,
    req_id: &str,
) -> Response<Body> {
    let copy_source = copy_source.trim_start_matches('/');
    let Some((source_bucket, source_key_and_query)) = copy_source.split_once('/') else {
        return xml_error_response(
            StatusCode::BAD_REQUEST,
            "InvalidArgument",
            "Invalid copy source format",
            req_id,
        );
    };
    let (source_key, source_version_id) = source_key_and_query.split_once('?').map_or(
        (source_key_and_query, None),
        |(key, query)| {
            let version_id = query.split('&').find_map(|parameter| {
                let (name, value) = parameter.split_once('=')?;
                (name == "versionId").then_some(value)
            });
            (key, version_id)
        },
    );
    let decode = |value: &str| crate::utils::request::decode_uri_path(value).map_err(|_| ());
    let Ok(source_bucket) = decode(source_bucket) else {
        return invalid_copy_source_encoding_response(req_id);
    };
    let Ok(source_key) = decode(source_key) else {
        return invalid_copy_source_encoding_response(req_id);
    };
    let Ok(source_version_id) = source_version_id.map(decode).transpose() else {
        return invalid_copy_source_encoding_response(req_id);
    };

    if s3_foreign_history_conflict(storage.as_ref(), &source_bucket) {
        return s3_foreign_history_conflict_response(req_id);
    }

    match tokio::task::block_in_place(|| match source_version_id.as_deref() {
        Some(version_id) => object_service::get_object_version(
            storage.as_ref(),
            &source_bucket,
            &source_key,
            version_id,
        ),
        None => object_service::get_object(storage.as_ref(), &source_bucket, &source_key),
    }) {
        Ok(src_obj) => copy_loaded_object(storage, bucket, key, req, req_id, &src_obj),
        Err(crate::error::Error::KeyNotFound | crate::error::Error::NoSuchVersion) => {
            xml_error_response(
                StatusCode::NOT_FOUND,
                "NoSuchKey",
                "Copy source not found",
                req_id,
            )
        }
        Err(err) => internal_error_response(&err, req_id),
    }
}

fn invalid_copy_source_encoding_response(req_id: &str) -> Response<Body> {
    xml_error_response(
        StatusCode::BAD_REQUEST,
        "InvalidArgument",
        "The x-amz-copy-source header contains invalid percent encoding.",
        req_id,
    )
}

fn copy_loaded_object(
    storage: &Arc<dyn Storage>,
    bucket: &str,
    key: &str,
    req: &crate::server::http::Request,
    req_id: &str,
    src_obj: &crate::models::Object,
) -> Response<Body> {
    if is_s3_delete_marker(src_obj) {
        return xml_error_response(
            StatusCode::BAD_REQUEST,
            "InvalidRequest",
            "The source of a copy request may not refer to a delete marker.",
            req_id,
        );
    }
    if let Some(response) = check_copy_conditionals(req, src_obj, req_id) {
        return response;
    }
    if let Some(response) = validate_copy_request_contract(req, req_id) {
        return response;
    }

    let tags = match copy_object_tags(req, src_obj) {
        Ok(tags) => tags,
        Err(message) => {
            return xml_error_response(StatusCode::BAD_REQUEST, "InvalidTag", &message, req_id);
        }
    };
    let metadata = copy_object_metadata(req, src_obj);
    let content_type = if copy_object_replaces_metadata(req) {
        req.header("content-type")
            .unwrap_or("application/octet-stream")
            .to_string()
    } else {
        src_obj.content_type.clone()
    };
    let mut dest_obj = crate::models::Object::new_with_metadata(
        key.to_string(),
        src_obj.data.clone(),
        content_type,
        metadata,
    );
    dest_obj
        .provider_metadata
        .clone_from(&src_obj.provider_metadata);
    clear_object_lock_metadata(&mut dest_obj);
    if let Err(response) = apply_s3_request_contracts(req, &mut dest_obj, req_id) {
        return response;
    }
    if let Some(tags) = tags {
        dest_obj.tags = tags;
    } else {
        dest_obj.tags.clone_from(&src_obj.tags);
    }

    store_copied_object(storage, bucket, key, req, req_id, dest_obj)
}

fn validate_copy_request_contract(
    req: &crate::server::http::Request,
    req_id: &str,
) -> Option<Response<Body>> {
    if req
        .header("if-none-match")
        .is_some_and(|value| value.trim() != "*")
    {
        return Some(xml_error_response(
            StatusCode::BAD_REQUEST,
            "InvalidArgument",
            "If-None-Match for CopyObject only supports the '*' value.",
            req_id,
        ));
    }

    if req.header("x-amz-copy-source-range").is_some() {
        return Some(xml_error_response(
            StatusCode::BAD_REQUEST,
            "InvalidRequest",
            "x-amz-copy-source-range is valid only for UploadPartCopy.",
            req_id,
        ));
    }

    for header in ["x-amz-metadata-directive", "x-amz-tagging-directive"] {
        if let Some(value) = req.header(header) {
            if !value.eq_ignore_ascii_case("COPY") && !value.eq_ignore_ascii_case("REPLACE") {
                return Some(xml_error_response(
                    StatusCode::BAD_REQUEST,
                    "InvalidArgument",
                    &format!("{header} must be COPY or REPLACE."),
                    req_id,
                ));
            }
        }
    }

    None
}

fn copy_object_metadata(
    req: &crate::server::http::Request,
    src_obj: &crate::models::Object,
) -> HashMap<String, String> {
    if copy_object_replaces_metadata(req) {
        header_utils::extract_metadata_from_http_headers(req)
    } else {
        src_obj.metadata.clone()
    }
}

fn copy_object_replaces_metadata(req: &crate::server::http::Request) -> bool {
    req.header("x-amz-metadata-directive")
        .unwrap_or("COPY")
        .eq_ignore_ascii_case("REPLACE")
}

fn copy_object_tags(
    req: &crate::server::http::Request,
    src_obj: &crate::models::Object,
) -> Result<Option<HashMap<String, String>>, String> {
    let tagging_directive = req.header("x-amz-tagging-directive").unwrap_or("COPY");
    if let Some(tagging_header) = req.header("x-amz-tagging") {
        return if tagging_directive.eq_ignore_ascii_case("REPLACE") {
            parse_tagging_header(tagging_header)
                .map(Some)
                .map_err(|err| format!("Invalid tags: {err}"))
        } else {
            Err("x-amz-tagging requires x-amz-tagging-directive: REPLACE".to_string())
        };
    }

    if tagging_directive.eq_ignore_ascii_case("COPY") {
        Ok(Some(src_obj.tags.clone()))
    } else {
        Ok(Some(HashMap::new()))
    }
}

fn store_copied_object(
    storage: &Arc<dyn Storage>,
    bucket: &str,
    key: &str,
    req: &crate::server::http::Request,
    req_id: &str,
    dest_obj: crate::models::Object,
) -> Response<Body> {
    let dest_key = dest_obj.key.clone();
    let etag = dest_obj.etag.clone();
    let dest_last_modified = dest_obj.last_modified;

    let condition = mutation_condition(req);
    match tokio::task::block_in_place(|| match condition {
        Some(condition) => storage
            .put_object_if(bucket, dest_key, dest_obj, &condition)
            .map(Some),
        None => {
            object_service::put_object(storage.as_ref(), bucket, dest_key, dest_obj).map(|()| None)
        }
    }) {
        Ok(Some(false)) => helpers::precondition_failed_response(req_id),
        Ok(Some(true) | None) => {
            copy_object_response(storage, bucket, key, req, req_id, &etag, dest_last_modified)
        }
        Err(err) => internal_error_response(&err, req_id),
    }
}

fn copy_object_response(
    storage: &Arc<dyn Storage>,
    bucket: &str,
    key: &str,
    req: &crate::server::http::Request,
    req_id: &str,
    etag: &str,
    last_modified: chrono::DateTime<chrono::Utc>,
) -> Response<Body> {
    let stored_version_id =
        tokio::task::block_in_place(|| object_service::get_object(storage.as_ref(), bucket, key))
            .ok()
            .and_then(|obj| obj.version_id);
    let xml = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<CopyObjectResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/">
    <ETag>{}</ETag>
    <LastModified>{}</LastModified>
</CopyObjectResult>"#,
        quoted_etag(etag),
        header_utils::format_last_modified_at(&last_modified)
    );
    let builder = add_version_header(
        ResponseBuilder::new(StatusCode::OK)
            .content_type("application/xml; charset=utf-8")
            .header("x-amz-request-id", req_id),
        stored_version_id.as_deref(),
    );
    cors::apply_actual_request_headers(storage.as_ref(), bucket, req, builder)
        .body(xml.into_bytes())
        .build()
}

fn put_object_body(
    storage: &Arc<dyn Storage>,
    bucket: &str,
    key: &str,
    req: &crate::server::http::Request,
    req_id: &str,
    existing: Option<&crate::models::Object>,
) -> Response<Body> {
    if let Some(response) = validate_content_md5(req, req_id) {
        return response;
    }
    if let Some(response) = validate_put_target(bucket, key, req_id) {
        return response;
    }
    let tags = match parse_optional_tagging_header(req, req_id) {
        Ok(tags) => tags,
        Err(message) => {
            return xml_error_response(StatusCode::BAD_REQUEST, "InvalidTag", &message, req_id);
        }
    };
    let content_type = req
        .header("content-type")
        .unwrap_or("application/octet-stream");
    let metadata = header_utils::extract_metadata_from_http_headers(req);
    let mut obj = crate::models::Object::new_with_metadata(
        key.to_string(),
        req.body.to_vec(),
        content_type.to_string(),
        metadata,
    );
    if let Some(existing) = existing {
        obj.provider_metadata
            .clone_from(&existing.provider_metadata);
        if s3_mutation_preserves_current_version(storage, bucket, existing) {
            clear_object_lock_metadata(&mut obj);
        }
    }
    if let Err(response) = apply_s3_request_contracts(req, &mut obj, req_id) {
        return response;
    }
    if let Some(tags) = tags {
        obj.tags = tags;
    }
    store_put_object(storage, bucket, key, req, req_id, obj)
}

fn validate_put_target(bucket: &str, key: &str, req_id: &str) -> Option<Response<Body>> {
    if let Err(err) = validation::validate_bucket_name(bucket) {
        return Some(xml_error_response(
            StatusCode::BAD_REQUEST,
            "InvalidBucketName",
            &err,
            req_id,
        ));
    }

    validation::validate_blob_key(key)
        .err()
        .map(|err| xml_error_response(StatusCode::BAD_REQUEST, "InvalidKey", &err, req_id))
}

fn parse_optional_tagging_header(
    req: &crate::server::http::Request,
    _req_id: &str,
) -> Result<Option<HashMap<String, String>>, String> {
    let Some(tagging_header) = req.header("x-amz-tagging") else {
        return Ok(None);
    };
    parse_tagging_header(tagging_header).map(Some)
}

fn store_put_object(
    storage: &Arc<dyn Storage>,
    bucket: &str,
    key: &str,
    req: &crate::server::http::Request,
    req_id: &str,
    obj: crate::models::Object,
) -> Response<Body> {
    if let Some(value) = req.header("if-none-match") {
        if value.trim() != "*" {
            return xml_error_response(
                StatusCode::BAD_REQUEST,
                "InvalidArgument",
                "If-None-Match for PutObject only supports the '*' value.",
                req_id,
            );
        }
    }
    if req.header("if-match").is_some() && storage.get_object(bucket, key).is_err() {
        return xml_error_response(
            StatusCode::NOT_FOUND,
            "NoSuchKey",
            "The specified key does not exist.",
            req_id,
        );
    }
    let obj_key = obj.key.clone();
    let etag = obj.etag.clone();
    let condition = mutation_condition(req);
    match tokio::task::block_in_place(|| match condition {
        Some(condition) => storage
            .put_object_if(bucket, obj_key, obj, &condition)
            .map(Some),
        None => object_service::put_object(storage.as_ref(), bucket, obj_key, obj).map(|()| None),
    }) {
        Ok(Some(false)) => helpers::precondition_failed_response(req_id),
        Ok(Some(true) | None) => put_object_response(storage, bucket, key, req, req_id, &etag),
        Err(err) => internal_error_response(&err, req_id),
    }
}

fn put_object_response(
    storage: &Arc<dyn Storage>,
    bucket: &str,
    key: &str,
    req: &crate::server::http::Request,
    req_id: &str,
    etag: &str,
) -> Response<Body> {
    let stored_version_id =
        tokio::task::block_in_place(|| object_service::get_object(storage.as_ref(), bucket, key))
            .ok()
            .and_then(|obj| obj.version_id);
    let builder = add_version_header(
        ResponseBuilder::new(StatusCode::OK)
            .header("Content-Length", "0")
            .header("ETag", &quoted_etag(etag))
            .header("x-amz-request-id", req_id)
            .header("x-amz-id-2", &header_utils::generate_request_id()),
        stored_version_id.as_deref(),
    );
    cors::apply_actual_request_headers(storage.as_ref(), bucket, req, builder).empty()
}

fn ok_empty_object_response(
    storage: &dyn Storage,
    bucket: &str,
    req: &crate::server::http::Request,
    req_id: &str,
) -> Response<Body> {
    let builder = ResponseBuilder::new(StatusCode::OK)
        .header("x-amz-request-id", req_id)
        .header("x-amz-id-2", &header_utils::generate_request_id());
    cors::apply_actual_request_headers(storage, bucket, req, builder).empty()
}

fn internal_error_response(err: &crate::error::Error, req_id: &str) -> Response<Body> {
    xml_error_response(
        StatusCode::INTERNAL_SERVER_ERROR,
        "InternalError",
        &err.to_string(),
        req_id,
    )
}

#[cfg(test)]
#[allow(clippy::items_after_test_module)]
mod tests {
    use super::*;
    use crate::auth::AuthConfig;
    use crate::body::Body;
    use crate::models::Object;
    use crate::server::RequestExt;
    use crate::storage::FilesystemStorage;
    use bytes::Bytes;
    use chrono::{TimeZone, Utc};
    use http_body_util::BodyExt;
    use hyper::Request as HyperRequest;
    use hyper::StatusCode;
    use std::fs;
    use std::sync::Arc;
    use std::time::Duration;

    fn temp_storage() -> Arc<dyn Storage> {
        let dir =
            std::env::temp_dir().join(format!("sqrzl-copy-range-test-{}", uuid::Uuid::new_v4()));
        let _ = fs::create_dir_all(&dir);
        Arc::new(FilesystemStorage::new(dir))
    }

    fn auth_disabled_config() -> Arc<AuthConfig> {
        Arc::new(AuthConfig {
            access_key_id: None,
            secret_access_key: None,
            enforce_auth: false,
            admin_auth_disabled: false,
            blobs_path: "./blobs".to_string(),
            lifecycle_interval: Duration::from_hours(1),
            api_port: 9000,
            ui_port: 9001,
            max_request_bytes: crate::config::DEFAULT_SQRZL_MAX_REQUEST_BYTES,
            smtp_port: crate::config::DEFAULT_SQRZL_SMTP_PORT,
        })
    }

    async fn parsed_request(headers: &[(&str, &str)]) -> RequestExt {
        let mut builder = HyperRequest::builder()
            .method("PUT")
            .uri("http://localhost/");

        for (name, value) in headers {
            builder = builder.header(*name, *value);
        }

        RequestExt::from_hyper(
            builder
                .body(Body::from(Bytes::new()))
                .expect("request should build"),
        )
        .await
        .expect("request should parse")
    }

    async fn parsed_request_with_method(method: &str, headers: &[(&str, &str)]) -> RequestExt {
        let mut builder = HyperRequest::builder()
            .method(method)
            .uri("http://localhost/");

        for (name, value) in headers {
            builder = builder.header(*name, *value);
        }

        RequestExt::from_hyper(
            builder
                .body(Body::from(Bytes::new()))
                .expect("request should build"),
        )
        .await
        .expect("request should parse")
    }

    async fn parsed_request_at(
        method: &str,
        uri: &str,
        headers: &[(&str, &str)],
        body: &[u8],
    ) -> RequestExt {
        let mut builder = HyperRequest::builder().method(method).uri(uri);
        for (name, value) in headers {
            builder = builder.header(*name, *value);
        }
        RequestExt::from_hyper(
            builder
                .body(Body::from(body.to_vec()))
                .expect("request should build"),
        )
        .await
        .expect("request should parse")
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn should_reject_unsupported_s3_object_lock_subresources_without_overwrite() {
        // Arrange
        let storage = temp_storage();
        storage.create_bucket("bucket".to_string()).unwrap();
        storage
            .put_object(
                "bucket",
                "object.txt".to_string(),
                Object::new(
                    "object.txt".to_string(),
                    b"original".to_vec(),
                    "text/plain".to_string(),
                ),
            )
            .unwrap();

        // Act
        // Assert
        for subresource in ["retention", "legal-hold"] {
            for method in ["GET", "PUT"] {
                let body = if method == "PUT" {
                    b"must-not-overwrite".as_slice()
                } else {
                    b"".as_slice()
                };
                let content_length = body.len().to_string();
                let request = parsed_request_at(
                    method,
                    &format!("http://localhost/bucket/object.txt?{subresource}"),
                    &[("content-length", content_length.as_str())],
                    body,
                )
                .await;
                let response = if method == "PUT" {
                    object_put(
                        storage.clone(),
                        auth_disabled_config(),
                        "bucket",
                        "object.txt",
                        &request,
                        "req-object-lock-subresource".to_string(),
                    )
                    .await
                } else {
                    object_get(
                        storage.clone(),
                        auth_disabled_config(),
                        "bucket",
                        "object.txt",
                        &request,
                        "req-object-lock-subresource".to_string(),
                    )
                    .await
                }
                .expect("unsupported object-lock subresource should respond");
                assert_eq!(response.status(), StatusCode::NOT_IMPLEMENTED);
                assert_eq!(
                    response.headers()["x-amz-request-id"],
                    "req-object-lock-subresource"
                );
                let body = response
                    .into_body()
                    .collect()
                    .await
                    .expect("body should read")
                    .to_bytes();
                assert!(String::from_utf8(body.to_vec())
                    .expect("response should be XML")
                    .contains("<Code>NotImplemented</Code>"));
                assert_eq!(
                    storage.get_object("bucket", "object.txt").unwrap().data,
                    b"original"
                );
            }
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn should_reject_copy_source_range_on_copy_object_without_mutation() {
        // Arrange
        let storage = temp_storage();
        storage.create_bucket("source".to_string()).unwrap();
        storage.create_bucket("dest".to_string()).unwrap();

        let mut metadata = std::collections::HashMap::new();
        metadata.insert("owner".to_string(), "alice".to_string());
        storage
            .put_object(
                "source",
                "source.txt".to_string(),
                Object::new_with_metadata(
                    "source.txt".to_string(),
                    b"abcdefghij".to_vec(),
                    "text/plain".to_string(),
                    metadata,
                ),
            )
            .unwrap();

        // Act
        let req = parsed_request(&[
            ("x-amz-copy-source", "/source/source.txt"),
            ("x-amz-copy-source-range", "bytes=2-5"),
        ])
        .await;

        let resp = object_put(
            storage.clone(),
            auth_disabled_config(),
            "dest",
            "copied.txt",
            &req,
            "req-123".to_string(),
        )
        .await
        .expect("copy should complete");

        // Assert
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        assert!(storage.get_object("dest", "copied.txt").is_err());
        let response_body = resp
            .into_body()
            .collect()
            .await
            .expect("copy response body should read")
            .to_bytes();
        assert!(String::from_utf8_lossy(&response_body).contains("InvalidRequest"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn should_reject_malformed_copy_source_encoding_without_destination_mutation() {
        // Arrange
        let storage = temp_storage();
        storage.create_bucket("source".to_string()).unwrap();
        storage.create_bucket("dest".to_string()).unwrap();
        storage
            .put_object(
                "source",
                "source.txt".to_string(),
                Object::new(
                    "source.txt".to_string(),
                    b"source".to_vec(),
                    "text/plain".to_string(),
                ),
            )
            .unwrap();

        // Act
        // Assert
        for copy_source in [
            "/source/source%ZZ.txt",
            "/source%ZZ/source.txt",
            "/source/source.txt?versionId=%ZZ",
        ] {
            let request = parsed_request(&[("x-amz-copy-source", copy_source)]).await;
            let response = object_put(
                storage.clone(),
                auth_disabled_config(),
                "dest",
                "copy.txt",
                &request,
                "req-malformed-copy-source".to_string(),
            )
            .await
            .expect("malformed copy source should respond");
            assert_eq!(response.status(), StatusCode::BAD_REQUEST);
            assert_eq!(
                response.headers()["x-amz-request-id"],
                "req-malformed-copy-source"
            );
            assert!(response.headers().contains_key("x-amz-id-2"));
            let body = response
                .into_body()
                .collect()
                .await
                .expect("body should read")
                .to_bytes();
            assert!(String::from_utf8(body.to_vec())
                .expect("response should be XML")
                .contains("<Code>InvalidArgument</Code>"));
            assert!(storage.get_object("dest", "copy.txt").is_err());
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn should_reject_out_of_bounds_copy_source_range_without_mutation() {
        // Arrange
        let storage = temp_storage();
        storage.create_bucket("source".to_string()).unwrap();
        storage.create_bucket("dest".to_string()).unwrap();

        storage
            .put_object(
                "source",
                "source.txt".to_string(),
                Object::new(
                    "source.txt".to_string(),
                    b"abcdefghij".to_vec(),
                    "text/plain".to_string(),
                ),
            )
            .unwrap();

        // Act
        let req = parsed_request(&[
            ("x-amz-copy-source", "source/source.txt"),
            ("x-amz-copy-source-range", "bytes=20-30"),
        ])
        .await;

        let resp = object_put(
            storage.clone(),
            auth_disabled_config(),
            "dest",
            "copied.txt",
            &req,
            "req-124".to_string(),
        )
        .await
        .expect("copy should return a response");

        // Assert
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        assert!(storage.get_object("dest", "copied.txt").is_err());

        let body = resp
            .into_body()
            .collect()
            .await
            .expect("body should read")
            .to_bytes();
        let body = String::from_utf8(body.to_vec()).expect("body should be utf8");
        assert!(body.contains("InvalidRequest"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn should_return_object_last_modified_from_stored_object_when_getting_the_object() {
        // Arrange
        let storage = temp_storage();
        storage.create_bucket("bucket".to_string()).unwrap();

        let mut object = Object::new(
            "object.txt".to_string(),
            b"payload".to_vec(),
            "text/plain".to_string(),
        );
        let expected_last_modified = Utc.with_ymd_and_hms(2024, 4, 10, 12, 34, 56).unwrap();
        object.last_modified = expected_last_modified;

        storage
            .put_object("bucket", "object.txt".to_string(), object)
            .unwrap();

        // Act
        let req = parsed_request_with_method("GET", &[]).await;

        let resp = object_get(
            storage.clone(),
            auth_disabled_config(),
            "bucket",
            "object.txt",
            &req,
            "req-125".to_string(),
        )
        .await
        .expect("get should complete");

        // Assert
        assert_eq!(resp.status(), StatusCode::OK);

        let last_modified = resp
            .headers()
            .get("last-modified")
            .expect("last-modified header should be present")
            .to_str()
            .expect("last-modified should be valid header value");
        let parsed = chrono::DateTime::parse_from_rfc2822(last_modified)
            .expect("last-modified should parse as RFC2822")
            .with_timezone(&Utc);
        assert_eq!(parsed, expected_last_modified);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn should_return_not_modified_when_if_none_match_matches_the_object_etag() {
        // Arrange
        let storage = temp_storage();
        storage.create_bucket("bucket".to_string()).unwrap();

        let object = Object::new(
            "object.txt".to_string(),
            b"payload".to_vec(),
            "text/plain".to_string(),
        );
        let etag = object.etag.clone();
        storage
            .put_object("bucket", "object.txt".to_string(), object)
            .unwrap();

        // Act
        let req = parsed_request_with_method("GET", &[("If-None-Match", &etag)]).await;

        let resp = object_get(
            storage.clone(),
            auth_disabled_config(),
            "bucket",
            "object.txt",
            &req,
            "req-126".to_string(),
        )
        .await
        .expect("get should complete");

        // Assert
        assert_eq!(resp.status(), StatusCode::NOT_MODIFIED);
        assert_eq!(
            resp.headers().get("etag").and_then(|v| v.to_str().ok()),
            Some(format!("\"{etag}\"").as_str())
        );
        let body = resp
            .into_body()
            .collect()
            .await
            .expect("body should read")
            .to_bytes();
        assert!(body.is_empty());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn should_return_precondition_failed_when_if_match_does_not_match_the_object_etag() {
        // Arrange
        let storage = temp_storage();
        storage.create_bucket("bucket".to_string()).unwrap();

        storage
            .put_object(
                "bucket",
                "object.txt".to_string(),
                Object::new(
                    "object.txt".to_string(),
                    b"payload".to_vec(),
                    "text/plain".to_string(),
                ),
            )
            .unwrap();

        // Act
        let req = parsed_request_with_method("GET", &[("If-Match", "not-the-etag")]).await;

        let resp = object_get(
            storage.clone(),
            auth_disabled_config(),
            "bucket",
            "object.txt",
            &req,
            "req-127".to_string(),
        )
        .await
        .expect("get should complete");

        // Assert
        assert_eq!(resp.status(), StatusCode::PRECONDITION_FAILED);
        let body = resp
            .into_body()
            .collect()
            .await
            .expect("body should read")
            .to_bytes();
        let body = String::from_utf8(body.to_vec()).expect("body should be utf8");
        assert!(body.contains("PreconditionFailed"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn should_return_not_modified_when_if_modified_since_is_after_the_object_last_modified_on_head(
    ) {
        // Arrange
        let storage = temp_storage();
        storage.create_bucket("bucket".to_string()).unwrap();

        let mut object = Object::new(
            "object.txt".to_string(),
            b"payload".to_vec(),
            "text/plain".to_string(),
        );
        let expected_last_modified = Utc.with_ymd_and_hms(2024, 4, 10, 12, 34, 56).unwrap();
        object.last_modified = expected_last_modified;
        storage
            .put_object("bucket", "object.txt".to_string(), object)
            .unwrap();

        let request_time = (expected_last_modified + chrono::Duration::days(1)).to_rfc2822();

        // Act
        let req = parsed_request_with_method("HEAD", &[("If-Modified-Since", &request_time)]).await;

        let resp = object_head(
            storage.clone(),
            auth_disabled_config(),
            "bucket",
            "object.txt",
            &req,
            "req-128".to_string(),
        )
        .await
        .expect("head should complete");

        // Assert
        assert_eq!(resp.status(), StatusCode::NOT_MODIFIED);
        let body = resp
            .into_body()
            .collect()
            .await
            .expect("body should read")
            .to_bytes();
        assert!(body.is_empty());
    }
}

pub async fn object_delete(
    storage: Arc<dyn Storage>,
    auth_config: Arc<AuthConfig>,
    bucket: &str,
    key: &str,
    req: &crate::server::http::Request,
    req_id: String,
) -> Result<Response<Body>, String> {
    if req.has_query_param("uploadId") {
        return Ok(delete_multipart_upload(
            &storage,
            &auth_config,
            bucket,
            key,
            req,
            &req_id,
        ));
    }

    if let Err(response) = check_authorization(
        req,
        &auth_config,
        &storage,
        bucket,
        Some(key),
        "s3:DeleteObject",
    ) {
        return Ok(response);
    }

    // Verify presigned URL if present
    if let Err(response) = verify_presigned_url(req, bucket, key, &auth_config) {
        return Ok(response);
    }

    if let Some(response) = object_bucket_error_response(storage.as_ref(), bucket, &req_id, false) {
        return Ok(response);
    }

    if req.has_query_param("tagging") && req.has_query_param("versionId") {
        return Ok(unsupported_version_tagging_response(&req_id));
    }

    if s3_foreign_history_conflict(storage.as_ref(), bucket) {
        return Ok(s3_foreign_history_conflict_response(&req_id));
    }

    if req.has_query_param("tagging") {
        return Ok(delete_object_tagging(&storage, bucket, key, req, &req_id));
    }

    if req.has_query_param("versionId") {
        return Ok(delete_object_version_request(
            &storage, bucket, key, req, &req_id,
        ));
    }

    Ok(delete_current_object(&storage, bucket, key, req, &req_id))
}

fn unsupported_version_tagging_response(req_id: &str) -> Response<Body> {
    xml_error_response(
        StatusCode::NOT_IMPLEMENTED,
        "NotImplemented",
        "Version-scoped object tagging is not supported by this emulator subset.",
        req_id,
    )
}

fn delete_multipart_upload(
    storage: &Arc<dyn Storage>,
    auth_config: &Arc<AuthConfig>,
    bucket: &str,
    key: &str,
    req: &crate::server::http::Request,
    req_id: &str,
) -> Response<Body> {
    let upload_id = req.query_param("uploadId").unwrap_or("");
    let upload = match tokio::task::block_in_place(|| {
        object_service::get_multipart_upload(storage.as_ref(), bucket, upload_id)
    }) {
        Ok(upload) => upload,
        Err(crate::error::Error::NoSuchUpload) => {
            return bare_no_content_response(req_id);
        }
        Err(err) => return storage_error_response(&err, req_id),
    };

    if upload.key != key {
        return upload_key_mismatch_response(req_id);
    }

    if let Err(response) = check_authorization(
        req,
        auth_config,
        storage,
        bucket,
        Some(upload.key.as_str()),
        "s3:DeleteObject",
    ) {
        return response;
    }

    if let Err(response) = verify_presigned_url(req, bucket, upload.key.as_str(), auth_config) {
        return response;
    }

    match tokio::task::block_in_place(|| {
        object_service::abort_multipart_upload(storage.as_ref(), bucket, upload_id)
    }) {
        Ok(()) | Err(crate::error::Error::NoSuchUpload) => {
            no_content_object_response(storage.as_ref(), bucket, req, req_id)
        }
        Err(err) => storage_error_response(&err, req_id),
    }
}

fn delete_object_version_request(
    storage: &Arc<dyn Storage>,
    bucket: &str,
    key: &str,
    req: &crate::server::http::Request,
    req_id: &str,
) -> Response<Body> {
    let version_id = req.query_param("versionId").unwrap_or("");
    let target = tokio::task::block_in_place(|| {
        object_service::get_object_version(storage.as_ref(), bucket, key, version_id)
    });
    if let Some(value) = req.header("x-amz-bypass-governance-retention") {
        if !value.eq_ignore_ascii_case("true") && !value.eq_ignore_ascii_case("false") {
            return xml_error_response(
                StatusCode::BAD_REQUEST,
                "InvalidArgument",
                "x-amz-bypass-governance-retention must be true or false.",
                req_id,
            );
        }
        let governance_locked = value.eq_ignore_ascii_case("true")
            && target.as_ref().is_ok_and(|object| {
                object
                    .provider_metadata
                    .get("s3_object_lock_mode")
                    .is_some_and(|mode| mode == "GOVERNANCE")
                    && object
                        .provider_metadata
                        .get("s3_object_lock_legal_hold")
                        .is_none_or(|hold| !hold.eq_ignore_ascii_case("ON"))
                    && object_is_locked(object)
            });
        if governance_locked {
            return xml_error_response(
                StatusCode::NOT_IMPLEMENTED,
                "NotImplemented",
                "Permission-aware governance retention bypass is not supported.",
                req_id,
            );
        }
    }
    if target.as_ref().is_ok_and(object_is_locked) {
        return locked_object_response(req_id);
    }
    let deleting_marker = target.as_ref().is_ok_and(is_s3_delete_marker);
    match tokio::task::block_in_place(|| {
        object_service::delete_object_version(storage.as_ref(), bucket, key, version_id)
    }) {
        Ok(()) | Err(crate::error::Error::KeyNotFound | crate::error::Error::NoSuchVersion) => {
            let mut builder = ResponseBuilder::new(StatusCode::NO_CONTENT)
                .header("x-amz-request-id", req_id)
                .header("x-amz-id-2", &header_utils::generate_request_id())
                .header("x-amz-version-id", version_id);
            if deleting_marker {
                builder = builder.header("x-amz-delete-marker", "true");
            }
            cors::apply_actual_request_headers(storage.as_ref(), bucket, req, builder).empty()
        }
        Err(err) => internal_error_response(&err, req_id),
    }
}

fn delete_object_tagging(
    storage: &Arc<dyn Storage>,
    bucket: &str,
    key: &str,
    req: &crate::server::http::Request,
    req_id: &str,
) -> Response<Body> {
    if let Ok(existing) =
        tokio::task::block_in_place(|| object_service::get_object(storage.as_ref(), bucket, key))
    {
        if object_is_locked(&existing) {
            return locked_object_response(req_id);
        }
    }

    match tokio::task::block_in_place(|| {
        object_service::delete_object_tags(storage.as_ref(), bucket, key)
    }) {
        Ok(()) | Err(crate::error::Error::KeyNotFound) => {
            no_content_object_response(storage.as_ref(), bucket, req, req_id)
        }
        Err(err) => internal_error_response(&err, req_id),
    }
}

fn delete_current_object(
    storage: &Arc<dyn Storage>,
    bucket: &str,
    key: &str,
    req: &crate::server::http::Request,
    req_id: &str,
) -> Response<Body> {
    if req.header("if-match").is_some()
        && matches!(
            tokio::task::block_in_place(|| {
                object_service::get_object(storage.as_ref(), bucket, key)
            }),
            Err(crate::error::Error::KeyNotFound)
        )
    {
        if current_delete_marker(storage, bucket, key).is_some() {
            return helpers::precondition_failed_response(req_id);
        }
        return xml_error_response(
            StatusCode::NOT_FOUND,
            "NoSuchKey",
            "The specified key does not exist.",
            req_id,
        );
    }
    let condition = mutation_if_match_condition(req);
    match tokio::task::block_in_place(|| {
        if let Ok(existing) = object_service::get_object(storage.as_ref(), bucket, key) {
            if object_is_locked(&existing)
                && !s3_mutation_preserves_current_version(storage, bucket, &existing)
            {
                return Err(crate::error::Error::AccessDenied);
            }
        }
        if let Some(condition) = condition {
            storage.delete_object_if(bucket, key, &condition).map(Some)
        } else {
            object_service::delete_object(storage.as_ref(), bucket, key).map(|()| None)
        }
    }) {
        Ok(Some(false)) => helpers::precondition_failed_response(req_id),
        Ok(Some(true) | None) => delete_current_success_response(storage, bucket, key, req, req_id),
        Err(crate::error::Error::KeyNotFound) => {
            no_content_object_response(storage.as_ref(), bucket, req, req_id)
        }
        Err(crate::error::Error::AccessDenied) => locked_object_response(req_id),
        Err(err) => internal_error_response(&err, req_id),
    }
}

fn delete_current_success_response(
    storage: &Arc<dyn Storage>,
    bucket: &str,
    key: &str,
    req: &crate::server::http::Request,
    req_id: &str,
) -> Response<Body> {
    let marker_version = tokio::task::block_in_place(|| {
        object_service::list_object_versions_for_key(storage.as_ref(), bucket, key)
    })
    .ok()
    .and_then(|versions| {
        versions
            .into_iter()
            .filter(|version| {
                version
                    .provider_metadata
                    .get("s3_delete_marker")
                    .is_some_and(|value| value == "true")
            })
            .max_by_key(|version| version.last_modified)
            .and_then(|version| version.version_id)
    });
    let mut builder = ResponseBuilder::new(StatusCode::NO_CONTENT)
        .header("x-amz-request-id", req_id)
        .header("x-amz-id-2", &header_utils::generate_request_id());
    if let Some(version_id) = marker_version {
        builder = builder
            .header("x-amz-delete-marker", "true")
            .header("x-amz-version-id", &version_id);
    }
    cors::apply_actual_request_headers(storage.as_ref(), bucket, req, builder).empty()
}

fn bare_no_content_response(req_id: &str) -> Response<Body> {
    ResponseBuilder::new(StatusCode::NO_CONTENT)
        .header("x-amz-request-id", req_id)
        .header("x-amz-id-2", &header_utils::generate_request_id())
        .empty()
}

fn no_content_object_response(
    storage: &dyn Storage,
    bucket: &str,
    req: &crate::server::http::Request,
    req_id: &str,
) -> Response<Body> {
    let builder = ResponseBuilder::new(StatusCode::NO_CONTENT)
        .header("x-amz-request-id", req_id)
        .header("x-amz-id-2", &header_utils::generate_request_id());
    cors::apply_actual_request_headers(storage, bucket, req, builder).empty()
}

pub async fn object_head(
    storage: Arc<dyn Storage>,
    auth_config: Arc<AuthConfig>,
    bucket: &str,
    key: &str,
    req: &crate::server::http::Request,
    req_id: String,
) -> Result<Response<Body>, String> {
    if let Err(response) = check_authorization(
        req,
        &auth_config,
        &storage,
        bucket,
        Some(key),
        "s3:GetObject",
    ) {
        return Ok(response);
    }

    if let Some(response) = object_bucket_error_response(storage.as_ref(), bucket, &req_id, true) {
        return Ok(response);
    }

    if s3_foreign_history_conflict(storage.as_ref(), bucket) {
        return Ok(s3_foreign_history_conflict_response(&req_id));
    }

    if let Some(version_id) = req.query_param("versionId") {
        match tokio::task::block_in_place(|| {
            object_service::get_object_version(storage.as_ref(), bucket, key, version_id)
        }) {
            Ok(obj) if is_s3_delete_marker(&obj) => {
                return Ok(delete_marker_response(
                    &storage, bucket, req, &req_id, &obj, true, true,
                ));
            }
            Ok(obj) => {
                if let Some(response) = validate_get_sse_headers(req, &obj, &req_id) {
                    return Ok(response);
                }
                if let Some(response) = check_object_conditionals(req, &obj, &req_id) {
                    return Ok(response);
                }

                let builder = object_response_headers(
                    ResponseBuilder::new(StatusCode::OK)
                        .content_type(&obj.content_type)
                        .header("Content-Length", &obj.size.to_string()),
                    &obj,
                    &req_id,
                );

                return Ok(cors::apply_actual_request_headers(
                    storage.as_ref(),
                    bucket,
                    req,
                    builder,
                )
                .empty());
            }
            Err(crate::error::Error::KeyNotFound | crate::error::Error::NoSuchVersion) => {
                let builder = ResponseBuilder::new(StatusCode::NOT_FOUND)
                    .header("x-amz-request-id", &req_id)
                    .header("x-amz-id-2", &header_utils::generate_request_id());
                return Ok(cors::apply_actual_request_headers(
                    storage.as_ref(),
                    bucket,
                    req,
                    builder,
                )
                .empty());
            }
            Err(e) => {
                return Ok(xml_error_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "InternalError",
                    &e.to_string(),
                    &req_id,
                ));
            }
        }
    }

    match tokio::task::block_in_place(|| object_service::get_object(storage.as_ref(), bucket, key))
    {
        Ok(obj) => {
            if let Some(response) = validate_get_sse_headers(req, &obj, &req_id) {
                return Ok(response);
            }
            if let Some(response) = check_object_conditionals(req, &obj, &req_id) {
                return Ok(response);
            }

            let builder = object_response_headers(
                ResponseBuilder::new(StatusCode::OK)
                    .content_type(&obj.content_type)
                    .header("Content-Length", &obj.size.to_string()),
                &obj,
                &req_id,
            );

            Ok(cors::apply_actual_request_headers(storage.as_ref(), bucket, req, builder).empty())
        }
        Err(_) => Ok(current_delete_marker(&storage, bucket, key).map_or_else(
            || {
                let builder = ResponseBuilder::new(StatusCode::NOT_FOUND)
                    .header("x-amz-request-id", &req_id)
                    .header("x-amz-id-2", &header_utils::generate_request_id());
                cors::apply_actual_request_headers(storage.as_ref(), bucket, req, builder).empty()
            },
            |marker| delete_marker_response(&storage, bucket, req, &req_id, &marker, false, true),
        )),
    }
}

pub async fn object_post(
    storage: Arc<dyn Storage>,
    auth_config: Arc<AuthConfig>,
    bucket: &str,
    key: &str,
    req: &crate::server::http::Request,
    req_id: String,
) -> Result<Response<Body>, String> {
    if req.has_query_param("uploadId") {
        return Ok(complete_multipart_upload_request(
            &storage,
            &auth_config,
            bucket,
            key,
            req,
            &req_id,
        ));
    }

    if let Err(response) = check_authorization(
        req,
        &auth_config,
        &storage,
        bucket,
        Some(key),
        "s3:PutObject",
    ) {
        return Ok(response);
    }

    if let Err(response) = verify_presigned_url(req, bucket, key, &auth_config) {
        return Ok(response);
    }

    if s3_foreign_history_conflict(storage.as_ref(), bucket) {
        return Ok(s3_foreign_history_conflict_response(&req_id));
    }

    // Handle initiate multipart upload
    if req.has_query_param("uploads") {
        return Ok(initiate_multipart_upload_request(
            &storage, bucket, key, req, &req_id,
        ));
    }

    Ok(xml_error_response(
        StatusCode::NOT_IMPLEMENTED,
        "NotImplemented",
        "Object POST operations not yet implemented",
        &req_id,
    ))
}

fn complete_multipart_upload_request(
    storage: &Arc<dyn Storage>,
    auth_config: &Arc<AuthConfig>,
    bucket: &str,
    key: &str,
    req: &crate::server::http::Request,
    req_id: &str,
) -> Response<Body> {
    if req.header("if-match").is_some() || req.header("if-none-match").is_some() {
        return xml_error_response(
            StatusCode::NOT_IMPLEMENTED,
            "NotImplemented",
            "Conditional CompleteMultipartUpload is not supported by this emulator subset.",
            req_id,
        );
    }
    let upload_id = req.query_param("uploadId").unwrap_or("");
    let upload = match tokio::task::block_in_place(|| {
        object_service::get_multipart_upload(storage.as_ref(), bucket, upload_id)
    }) {
        Ok(upload) => upload,
        Err(crate::error::Error::NoSuchUpload) => {
            return xml_error_response(
                StatusCode::NOT_FOUND,
                "NoSuchUpload",
                "Upload not found",
                req_id,
            );
        }
        Err(err) => return storage_error_response(&err, req_id),
    };

    if upload.key != key {
        return upload_key_mismatch_response(req_id);
    }

    if let Err(response) = check_authorization(
        req,
        auth_config,
        storage,
        bucket,
        Some(upload.key.as_str()),
        "s3:PutObject",
    ) {
        return response;
    }

    if let Err(response) = verify_presigned_url(req, bucket, upload.key.as_str(), auth_config) {
        return response;
    }

    if s3_foreign_history_conflict(storage.as_ref(), bucket) {
        return s3_foreign_history_conflict_response(req_id);
    }

    let manifest = match std::str::from_utf8(&req.body)
        .map_err(|error| error.to_string())
        .and_then(xml_utils::parse_complete_multipart_upload_xml)
    {
        Ok(manifest) => manifest,
        Err(message) => {
            return xml_error_response(StatusCode::BAD_REQUEST, "MalformedXML", &message, req_id);
        }
    };
    if manifest.windows(2).any(|parts| parts[0].0 >= parts[1].0) {
        return xml_error_response(
            StatusCode::BAD_REQUEST,
            "InvalidPartOrder",
            "The list of parts was not in ascending order.",
            req_id,
        );
    }
    if manifest.iter().any(|(number, etag)| {
        !upload
            .parts
            .iter()
            .any(|part| part.part_number == *number && part.etag == *etag)
    }) {
        return xml_error_response(
            StatusCode::BAD_REQUEST,
            "InvalidPart",
            "One or more of the specified parts could not be found.",
            req_id,
        );
    }
    if manifest.len() != upload.parts.len() {
        return xml_error_response(
            StatusCode::NOT_IMPLEMENTED,
            "NotImplemented",
            "Completing a selected subset of uploaded parts is not supported.",
            req_id,
        );
    }

    match tokio::task::block_in_place(|| {
        object_service::complete_multipart_upload(storage.as_ref(), bucket, upload_id)
    }) {
        Ok(etag) => complete_multipart_upload_response(storage, bucket, key, req, req_id, &etag),
        Err(err) => storage_error_response(&err, req_id),
    }
}

fn complete_multipart_upload_response(
    storage: &Arc<dyn Storage>,
    bucket: &str,
    key: &str,
    req: &crate::server::http::Request,
    req_id: &str,
    etag: &str,
) -> Response<Body> {
    let xml = xml_utils::complete_multipart_upload_xml(bucket, key, etag);
    let stored_version_id =
        tokio::task::block_in_place(|| object_service::get_object(storage.as_ref(), bucket, key))
            .ok()
            .and_then(|obj| obj.version_id);
    let builder = add_version_header(
        ResponseBuilder::new(StatusCode::OK)
            .content_type("application/xml; charset=utf-8")
            .header("x-amz-request-id", req_id)
            .header("x-amz-id-2", &header_utils::generate_request_id()),
        stored_version_id.as_deref(),
    );
    cors::apply_actual_request_headers(storage.as_ref(), bucket, req, builder)
        .body(xml.into_bytes())
        .build()
}

fn initiate_multipart_upload_request(
    storage: &Arc<dyn Storage>,
    bucket: &str,
    key: &str,
    req: &crate::server::http::Request,
    req_id: &str,
) -> Response<Body> {
    if object_lock_headers_requested(req) {
        return xml_error_response(
            StatusCode::NOT_IMPLEMENTED,
            "NotImplemented",
            "Object Lock parameters on multipart upload initiation are not supported by this emulator subset.",
            req_id,
        );
    }

    match tokio::task::block_in_place(|| {
        object_service::create_multipart_upload(storage.as_ref(), bucket, key.to_string())
    }) {
        Ok(upload) => {
            let xml = format!(
                r#"<?xml version="1.0" encoding="UTF-8"?>
<InitiateMultipartUploadResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/">
    <Bucket>{bucket}</Bucket>
    <Key>{}</Key>
    <UploadId>{}</UploadId>
</InitiateMultipartUploadResult>"#,
                upload.key, upload.upload_id
            );
            let builder = ResponseBuilder::new(StatusCode::OK)
                .content_type("application/xml; charset=utf-8")
                .header("x-amz-request-id", req_id)
                .header("x-amz-id-2", &header_utils::generate_request_id());
            cors::apply_actual_request_headers(storage.as_ref(), bucket, req, builder)
                .body(xml.into_bytes())
                .build()
        }
        Err(err) => internal_error_response(&err, req_id),
    }
}

#[cfg(test)]
mod s3_contract_tests {
    use super::*;
    use crate::auth::AuthConfig;
    use crate::body::Body;
    use crate::models::Object;
    use crate::services::bucket as bucket_service;
    use crate::storage::FilesystemStorage;
    use chrono::TimeZone;
    use http_body_util::BodyExt;
    use hyper::Request as HyperRequest;
    use std::fs;
    use std::sync::Arc;
    use std::time::Duration;

    fn temp_storage() -> Arc<dyn Storage> {
        let dir =
            std::env::temp_dir().join(format!("sqrzl-s3-contract-test-{}", uuid::Uuid::new_v4()));
        let _ = fs::create_dir_all(&dir);
        Arc::new(FilesystemStorage::new(dir))
    }

    fn auth_disabled_config() -> Arc<AuthConfig> {
        Arc::new(AuthConfig {
            access_key_id: None,
            secret_access_key: None,
            enforce_auth: false,
            admin_auth_disabled: false,
            blobs_path: "./blobs".to_string(),
            lifecycle_interval: Duration::from_hours(1),
            api_port: 9000,
            ui_port: 9001,
            max_request_bytes: crate::config::DEFAULT_SQRZL_MAX_REQUEST_BYTES,
            smtp_port: crate::config::DEFAULT_SQRZL_SMTP_PORT,
        })
    }

    fn auth_enabled_config() -> Arc<AuthConfig> {
        Arc::new(AuthConfig {
            access_key_id: Some("test-access-key".to_string()),
            secret_access_key: Some("test-secret-key".to_string()),
            enforce_auth: true,
            admin_auth_disabled: false,
            blobs_path: "./blobs".to_string(),
            lifecycle_interval: Duration::from_hours(1),
            api_port: 9000,
            ui_port: 9001,
            max_request_bytes: crate::config::DEFAULT_SQRZL_MAX_REQUEST_BYTES,
            smtp_port: crate::config::DEFAULT_SQRZL_SMTP_PORT,
        })
    }

    async fn request(
        method: &str,
        headers: &[(&str, &str)],
        body: &[u8],
    ) -> crate::server::RequestExt {
        let mut builder = HyperRequest::builder()
            .method(method)
            .uri("http://localhost/");
        for (name, value) in headers {
            builder = builder.header(*name, *value);
        }
        crate::server::RequestExt::from_hyper(
            builder
                .body(Body::from(body.to_vec()))
                .expect("request should build"),
        )
        .await
        .expect("request should parse")
    }

    async fn request_with_uri(
        method: &str,
        uri: &str,
        headers: &[(&str, &str)],
        body: &[u8],
    ) -> crate::server::RequestExt {
        let mut builder = HyperRequest::builder().method(method).uri(uri);
        for (name, value) in headers {
            builder = builder.header(*name, *value);
        }
        crate::server::RequestExt::from_hyper(
            builder
                .body(Body::from(body.to_vec()))
                .expect("request should build"),
        )
        .await
        .expect("request should parse")
    }

    fn versioned_deleted_object() -> (Arc<dyn Storage>, String) {
        let storage = temp_storage();
        storage.create_bucket("bucket".to_string()).unwrap();
        storage.enable_versioning("bucket").unwrap();
        storage
            .put_object(
                "bucket",
                "removed.txt".to_string(),
                Object::new(
                    "removed.txt".to_string(),
                    b"payload".to_vec(),
                    "text/plain".to_string(),
                ),
            )
            .unwrap();
        storage.delete_object("bucket", "removed.txt").unwrap();
        let marker_id = storage
            .list_object_versions_for_key("bucket", "removed.txt")
            .unwrap()
            .into_iter()
            .filter(is_s3_delete_marker)
            .max_by(|left, right| {
                left.last_modified
                    .cmp(&right.last_modified)
                    .then_with(|| left.version_id.cmp(&right.version_id))
            })
            .and_then(|marker| marker.version_id)
            .expect("delete should create a versioned marker");
        (storage, marker_id)
    }

    async fn versioned_locked_object() -> (Arc<dyn Storage>, String) {
        let storage = temp_storage();
        storage.create_bucket("bucket".to_string()).unwrap();
        storage.enable_versioning("bucket").unwrap();
        let mut metadata = storage.get_bucket("bucket").unwrap().metadata;
        metadata.insert("s3_object_lock_enabled".to_string(), "true".to_string());
        metadata.insert("s3_versioning_status".to_string(), "Enabled".to_string());
        storage.update_bucket_metadata("bucket", metadata).unwrap();
        let content_md5 = BASE64.encode(md5::compute(b"protected payload").0);
        let request = request(
            "PUT",
            &[
                ("content-md5", &content_md5),
                ("x-amz-object-lock-mode", "GOVERNANCE"),
                (
                    "x-amz-object-lock-retain-until-date",
                    "2099-01-01T00:00:00Z",
                ),
            ],
            b"protected payload",
        )
        .await;
        let response = object_put(
            storage.clone(),
            auth_disabled_config(),
            "bucket",
            "protected.txt",
            &request,
            "req-versioned-lock-put".to_string(),
        )
        .await
        .expect("versioned locked PUT should respond");
        assert_eq!(response.status(), StatusCode::OK);
        let version_id = response
            .headers()
            .get("x-amz-version-id")
            .and_then(|value| value.to_str().ok())
            .expect("versioned PUT should return a version id")
            .to_string();
        (storage, version_id)
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn should_reject_version_scoped_tagging_without_mutating_the_version() {
        // Arrange
        let storage = temp_storage();
        storage.create_bucket("bucket".to_string()).unwrap();
        storage.enable_versioning("bucket").unwrap();
        storage
            .put_object(
                "bucket",
                "tagged.txt".to_string(),
                Object::new(
                    "tagged.txt".to_string(),
                    b"payload".to_vec(),
                    "text/plain".to_string(),
                ),
            )
            .unwrap();
        let version_id = storage
            .get_object("bucket", "tagged.txt")
            .unwrap()
            .version_id
            .expect("versioned object should have an id");
        let uri = format!("http://localhost/?tagging&versionId={version_id}");
        let tag_xml = br#"<Tagging xmlns="http://s3.amazonaws.com/doc/2006-03-01/"><TagSet><Tag><Key>scope</Key><Value>version</Value></Tag></TagSet></Tagging>"#;

        // Act
        let get = object_get(
            storage.clone(),
            auth_disabled_config(),
            "bucket",
            "tagged.txt",
            &request_with_uri("GET", &uri, &[], b"").await,
            "req-version-tag-get".to_string(),
        )
        .await
        .expect("version tag GET should respond");
        let put = object_put(
            storage.clone(),
            auth_disabled_config(),
            "bucket",
            "tagged.txt",
            &request_with_uri("PUT", &uri, &[], tag_xml).await,
            "req-version-tag-put".to_string(),
        )
        .await
        .expect("version tag PUT should respond");
        let delete = object_delete(
            storage.clone(),
            auth_disabled_config(),
            "bucket",
            "tagged.txt",
            &request_with_uri("DELETE", &uri, &[], b"").await,
            "req-version-tag-delete".to_string(),
        )
        .await
        .expect("version tag DELETE should respond");

        // Assert
        assert_eq!(get.status(), StatusCode::NOT_IMPLEMENTED);
        assert_eq!(put.status(), StatusCode::NOT_IMPLEMENTED);
        assert_eq!(delete.status(), StatusCode::NOT_IMPLEMENTED);
        assert!(storage
            .get_object_version("bucket", "tagged.txt", &version_id)
            .is_ok());
        assert!(storage
            .get_object_tags("bucket", "tagged.txt")
            .unwrap()
            .is_empty());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn should_preserve_locked_current_version_tags_when_tag_mutations_fail() {
        // Arrange
        let (storage, _) = versioned_locked_object().await;
        storage
            .put_object_tags(
                "bucket",
                "protected.txt",
                HashMap::from([("existing".to_string(), "value".to_string())]),
            )
            .unwrap();
        let tag_xml = br#"<Tagging xmlns="http://s3.amazonaws.com/doc/2006-03-01/"><TagSet><Tag><Key>replacement</Key><Value>value</Value></Tag></TagSet></Tagging>"#;

        // Act
        let put = object_put(
            storage.clone(),
            auth_disabled_config(),
            "bucket",
            "protected.txt",
            &request_with_uri("PUT", "http://localhost/?tagging", &[], tag_xml).await,
            "req-locked-tag-put".to_string(),
        )
        .await
        .expect("locked tag PUT should respond");
        let delete = object_delete(
            storage.clone(),
            auth_disabled_config(),
            "bucket",
            "protected.txt",
            &request_with_uri("DELETE", "http://localhost/?tagging", &[], b"").await,
            "req-locked-tag-delete".to_string(),
        )
        .await
        .expect("locked tag DELETE should respond");

        // Assert
        assert_eq!(put.status(), StatusCode::FORBIDDEN);
        assert_eq!(delete.status(), StatusCode::FORBIDDEN);
        assert_eq!(
            storage.get_object_tags("bucket", "protected.txt").unwrap(),
            HashMap::from([("existing".to_string(), "value".to_string())])
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn should_return_not_found_with_marker_identity_for_current_delete_marker_get() {
        // Arrange
        let (storage, marker_id) = versioned_deleted_object();
        let request = request("GET", &[], b"").await;

        // Act
        let response = object_get(
            storage,
            auth_disabled_config(),
            "bucket",
            "removed.txt",
            &request,
            "req-current-marker-get".to_string(),
        )
        .await
        .expect("marker GET should respond");

        // Assert
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert_eq!(
            response
                .headers()
                .get("x-amz-delete-marker")
                .and_then(|value| value.to_str().ok()),
            Some("true")
        );
        assert_eq!(
            response
                .headers()
                .get("x-amz-version-id")
                .and_then(|value| value.to_str().ok()),
            Some(marker_id.as_str())
        );
        let body = response
            .into_body()
            .collect()
            .await
            .expect("marker error body should read")
            .to_bytes();
        assert!(String::from_utf8_lossy(&body).contains("NoSuchKey"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn should_return_empty_not_found_with_marker_identity_for_current_delete_marker_head() {
        // Arrange
        let (storage, marker_id) = versioned_deleted_object();
        let request = request("HEAD", &[], b"").await;

        // Act
        let response = object_head(
            storage,
            auth_disabled_config(),
            "bucket",
            "removed.txt",
            &request,
            "req-current-marker-head".to_string(),
        )
        .await
        .expect("marker HEAD should respond");

        // Assert
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert_eq!(
            response
                .headers()
                .get("x-amz-delete-marker")
                .and_then(|value| value.to_str().ok()),
            Some("true")
        );
        assert_eq!(
            response
                .headers()
                .get("x-amz-version-id")
                .and_then(|value| value.to_str().ok()),
            Some(marker_id.as_str())
        );
        let body = response
            .into_body()
            .collect()
            .await
            .expect("marker HEAD body should read")
            .to_bytes();
        assert!(body.is_empty());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn should_return_method_not_allowed_for_delete_marker_version_get() {
        // Arrange
        let (storage, marker_id) = versioned_deleted_object();
        let request = request_with_uri(
            "GET",
            &format!("http://localhost/bucket/removed.txt?versionId={marker_id}"),
            &[],
            b"",
        )
        .await;

        // Act
        let response = object_get(
            storage,
            auth_disabled_config(),
            "bucket",
            "removed.txt",
            &request,
            "req-version-marker-get".to_string(),
        )
        .await
        .expect("versioned marker GET should respond");

        // Assert
        assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
        assert_eq!(
            response
                .headers()
                .get("x-amz-delete-marker")
                .and_then(|value| value.to_str().ok()),
            Some("true")
        );
        assert_eq!(
            response
                .headers()
                .get("x-amz-version-id")
                .and_then(|value| value.to_str().ok()),
            Some(marker_id.as_str())
        );
        assert!(response.headers().contains_key("last-modified"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn should_return_empty_method_not_allowed_for_delete_marker_version_head() {
        // Arrange
        let (storage, marker_id) = versioned_deleted_object();
        let request = request_with_uri(
            "HEAD",
            &format!("http://localhost/bucket/removed.txt?versionId={marker_id}"),
            &[],
            b"",
        )
        .await;

        // Act
        let response = object_head(
            storage,
            auth_disabled_config(),
            "bucket",
            "removed.txt",
            &request,
            "req-version-marker-head".to_string(),
        )
        .await
        .expect("versioned marker HEAD should respond");

        // Assert
        assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
        assert_eq!(
            response
                .headers()
                .get("x-amz-delete-marker")
                .and_then(|value| value.to_str().ok()),
            Some("true")
        );
        assert_eq!(
            response
                .headers()
                .get("x-amz-version-id")
                .and_then(|value| value.to_str().ok()),
            Some(marker_id.as_str())
        );
        assert!(response.headers().contains_key("last-modified"));
        let body = response
            .into_body()
            .collect()
            .await
            .expect("versioned marker HEAD body should read")
            .to_bytes();
        assert!(body.is_empty());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn should_add_new_version_without_overwriting_locked_current_version() {
        // Arrange
        let (storage, locked_version_id) = versioned_locked_object().await;
        let overwrite = request("PUT", &[], b"new version").await;

        // Act
        let response = object_put(
            storage.clone(),
            auth_disabled_config(),
            "bucket",
            "protected.txt",
            &overwrite,
            "req-versioned-lock-overwrite".to_string(),
        )
        .await
        .expect("versioned overwrite should respond");

        // Assert
        assert_eq!(response.status(), StatusCode::OK);
        let current = storage.get_object("bucket", "protected.txt").unwrap();
        assert_eq!(current.data, b"new version");
        assert!(!object_is_locked(&current));
        assert_eq!(
            storage
                .get_object_version("bucket", "protected.txt", &locked_version_id)
                .unwrap()
                .data,
            b"protected payload"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn should_add_delete_marker_without_removing_locked_current_version() {
        // Arrange
        let (storage, locked_version_id) = versioned_locked_object().await;
        let delete = request("DELETE", &[], b"").await;

        // Act
        let response = object_delete(
            storage.clone(),
            auth_disabled_config(),
            "bucket",
            "protected.txt",
            &delete,
            "req-versioned-lock-marker".to_string(),
        )
        .await
        .expect("versioned delete should respond");

        // Assert
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        assert_eq!(
            response
                .headers()
                .get("x-amz-delete-marker")
                .and_then(|value| value.to_str().ok()),
            Some("true")
        );
        assert!(storage.get_object("bucket", "protected.txt").is_err());
        assert_eq!(
            storage
                .get_object_version("bucket", "protected.txt", &locked_version_id)
                .unwrap()
                .data,
            b"protected payload"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn should_reject_permanent_delete_of_locked_object_version() {
        // Arrange
        let (storage, locked_version_id) = versioned_locked_object().await;
        let delete = request_with_uri(
            "DELETE",
            &format!("http://localhost/bucket/protected.txt?versionId={locked_version_id}"),
            &[],
            b"",
        )
        .await;

        // Act
        let response = object_delete(
            storage.clone(),
            auth_disabled_config(),
            "bucket",
            "protected.txt",
            &delete,
            "req-versioned-lock-permanent-delete".to_string(),
        )
        .await
        .expect("versioned delete should respond");

        // Assert
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert!(storage
            .get_object_version("bucket", "protected.txt", &locked_version_id)
            .is_ok());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn should_explicitly_reject_unsupported_governance_bypass_without_deleting_version() {
        // Arrange
        let (storage, locked_version_id) = versioned_locked_object().await;
        let delete = request_with_uri(
            "DELETE",
            &format!("http://localhost/bucket/protected.txt?versionId={locked_version_id}"),
            &[("x-amz-bypass-governance-retention", "true")],
            b"",
        )
        .await;

        // Act
        let response = object_delete(
            storage.clone(),
            auth_disabled_config(),
            "bucket",
            "protected.txt",
            &delete,
            "req-governance-bypass".to_string(),
        )
        .await
        .expect("governance bypass should respond");

        // Assert
        assert_eq!(response.status(), StatusCode::NOT_IMPLEMENTED);
        assert!(storage
            .get_object_version("bucket", "protected.txt", &locked_version_id)
            .is_ok());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn should_reject_copying_delete_marker_version_without_destination_mutation() {
        // Arrange
        let (storage, marker_id) = versioned_deleted_object();
        storage.create_bucket("destination".to_string()).unwrap();
        let copy_source = format!("/bucket/removed.txt?versionId={marker_id}");
        let copy = request("PUT", &[("x-amz-copy-source", &copy_source)], b"").await;

        // Act
        let response = object_put(
            storage.clone(),
            auth_disabled_config(),
            "destination",
            "copy.txt",
            &copy,
            "req-copy-marker".to_string(),
        )
        .await
        .expect("copying a marker should respond");

        // Assert
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body = response
            .into_body()
            .collect()
            .await
            .expect("copy marker error should read")
            .to_bytes();
        assert!(String::from_utf8_lossy(&body).contains("InvalidRequest"));
        assert!(storage.get_object("destination", "copy.txt").is_err());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn should_reject_unsupported_copy_range_and_invalid_directives_without_mutation() {
        // Arrange
        let storage = temp_storage();
        storage.create_bucket("source".to_string()).unwrap();
        storage.create_bucket("destination".to_string()).unwrap();
        storage
            .put_object(
                "source",
                "object.txt".to_string(),
                Object::new(
                    "object.txt".to_string(),
                    b"abcdef".to_vec(),
                    "text/plain".to_string(),
                ),
            )
            .unwrap();
        let cases = [
            (
                "range.txt",
                vec![("x-amz-copy-source-range", "bytes=0-0")],
                "InvalidRequest",
            ),
            (
                "metadata.txt",
                vec![("x-amz-metadata-directive", "bogus")],
                "InvalidArgument",
            ),
            (
                "tagging.txt",
                vec![("x-amz-tagging-directive", "bogus")],
                "InvalidArgument",
            ),
        ];

        // Act
        // Assert
        for (key, extra_headers, expected_code) in cases {
            let mut headers = vec![("x-amz-copy-source", "/source/object.txt")];
            headers.extend(extra_headers);
            let copy = request("PUT", &headers, b"").await;
            let response = object_put(
                storage.clone(),
                auth_disabled_config(),
                "destination",
                key,
                &copy,
                format!("req-copy-contract-{key}"),
            )
            .await
            .expect("invalid copy request should respond");

            assert_eq!(response.status(), StatusCode::BAD_REQUEST);
            let body = response
                .into_body()
                .collect()
                .await
                .expect("invalid copy response should read")
                .to_bytes();
            assert!(String::from_utf8_lossy(&body).contains(expected_code));
            assert!(storage.get_object("destination", key).is_err());
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn should_reject_non_wildcard_copy_destination_if_none_match_without_mutation() {
        // Arrange
        let storage = temp_storage();
        storage.create_bucket("source".to_string()).unwrap();
        storage.create_bucket("destination".to_string()).unwrap();
        storage
            .put_object(
                "source",
                "object.txt".to_string(),
                Object::new(
                    "object.txt".to_string(),
                    b"source".to_vec(),
                    "text/plain".to_string(),
                ),
            )
            .unwrap();
        storage
            .put_object(
                "destination",
                "object.txt".to_string(),
                Object::new(
                    "object.txt".to_string(),
                    b"preserved".to_vec(),
                    "text/plain".to_string(),
                ),
            )
            .unwrap();
        let copy = request(
            "PUT",
            &[
                ("x-amz-copy-source", "/source/object.txt"),
                ("if-none-match", "\"not-the-destination-etag\""),
            ],
            b"",
        )
        .await;

        // Act
        let response = object_put(
            storage.clone(),
            auth_disabled_config(),
            "destination",
            "object.txt",
            &copy,
            "req-copy-invalid-if-none-match".to_string(),
        )
        .await
        .expect("invalid copy conditional should respond");

        // Assert
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body = response
            .into_body()
            .collect()
            .await
            .expect("invalid copy response should read")
            .to_bytes();
        assert!(String::from_utf8_lossy(&body).contains("InvalidArgument"));
        assert_eq!(
            storage
                .get_object("destination", "object.txt")
                .unwrap()
                .data,
            b"preserved"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn should_return_precondition_failed_when_copy_source_was_not_modified_since() {
        // Arrange
        let storage = temp_storage();
        storage.create_bucket("source".to_string()).unwrap();
        storage.create_bucket("destination".to_string()).unwrap();
        let mut source = Object::new(
            "object.txt".to_string(),
            b"source".to_vec(),
            "text/plain".to_string(),
        );
        source.last_modified = chrono::Utc.with_ymd_and_hms(2026, 8, 10, 12, 0, 0).unwrap();
        storage
            .put_object("source", "object.txt".to_string(), source)
            .unwrap();
        let copy = request(
            "PUT",
            &[
                ("x-amz-copy-source", "/source/object.txt"),
                (
                    "x-amz-copy-source-if-modified-since",
                    "Mon, 10 Aug 2026 13:00:00 GMT",
                ),
            ],
            b"",
        )
        .await;

        // Act
        let response = object_put(
            storage.clone(),
            auth_disabled_config(),
            "destination",
            "object.txt",
            &copy,
            "req-copy-not-modified".to_string(),
        )
        .await
        .expect("failed copy precondition should respond");

        // Assert
        assert_eq!(response.status(), StatusCode::PRECONDITION_FAILED);
        assert!(storage.get_object("destination", "object.txt").is_err());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn should_replace_copy_content_type_and_metadata_without_changing_copy_semantics() {
        // Arrange
        let storage = temp_storage();
        storage.create_bucket("source".to_string()).unwrap();
        storage.create_bucket("destination".to_string()).unwrap();
        storage
            .put_object(
                "source",
                "object.txt".to_string(),
                Object::new_with_metadata(
                    "object.txt".to_string(),
                    b"source bytes".to_vec(),
                    "text/plain".to_string(),
                    HashMap::from([("old".to_string(), "source".to_string())]),
                ),
            )
            .unwrap();
        let replace = request(
            "PUT",
            &[
                ("x-amz-copy-source", "/source/object.txt"),
                ("x-amz-metadata-directive", "REPLACE"),
                ("content-type", "application/json"),
                ("x-amz-meta-new", "replacement"),
            ],
            b"",
        )
        .await;
        let copy = request("PUT", &[("x-amz-copy-source", "/source/object.txt")], b"").await;

        // Act
        let replaced = object_put(
            storage.clone(),
            auth_disabled_config(),
            "destination",
            "replaced.txt",
            &replace,
            "req-replaced-copy".to_string(),
        )
        .await
        .expect("REPLACE copy should respond");
        let copied = object_put(
            storage.clone(),
            auth_disabled_config(),
            "destination",
            "copied.txt",
            &copy,
            "req-default-copy".to_string(),
        )
        .await
        .expect("COPY copy should respond");
        let replaced_head = object_head(
            storage.clone(),
            auth_disabled_config(),
            "destination",
            "replaced.txt",
            &request("HEAD", &[], b"").await,
            "req-replaced-head".to_string(),
        )
        .await
        .expect("replacement HEAD should respond");
        let copied_head = object_head(
            storage.clone(),
            auth_disabled_config(),
            "destination",
            "copied.txt",
            &request("HEAD", &[], b"").await,
            "req-copy-head".to_string(),
        )
        .await
        .expect("copy HEAD should respond");

        // Assert
        assert_eq!(replaced.status(), StatusCode::OK);
        assert_eq!(copied.status(), StatusCode::OK);
        assert_eq!(replaced_head.status(), StatusCode::OK);
        assert_eq!(replaced_head.headers()["content-type"], "application/json");
        assert_eq!(replaced_head.headers()["x-amz-meta-new"], "replacement");
        assert!(!replaced_head.headers().contains_key("x-amz-meta-old"));
        assert_eq!(copied_head.status(), StatusCode::OK);
        assert_eq!(copied_head.headers()["content-type"], "text/plain");
        assert_eq!(copied_head.headers()["x-amz-meta-old"], "source");
        assert!(!copied_head.headers().contains_key("x-amz-meta-new"));
        for key in ["replaced.txt", "copied.txt"] {
            assert_eq!(
                storage.get_object("destination", key).unwrap().data,
                b"source bytes"
            );
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn should_return_empty_not_found_for_missing_head_version() {
        // Arrange
        let storage = temp_storage();
        storage.create_bucket("bucket".to_string()).unwrap();
        storage.enable_versioning("bucket").unwrap();
        let head = request_with_uri(
            "HEAD",
            "http://localhost/bucket/missing.txt?versionId=missing-version",
            &[],
            b"",
        )
        .await;

        // Act
        let response = object_head(
            storage,
            auth_disabled_config(),
            "bucket",
            "missing.txt",
            &head,
            "req-missing-version-head".to_string(),
        )
        .await
        .expect("missing version HEAD should respond");

        // Assert
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        let body = response
            .into_body()
            .collect()
            .await
            .expect("missing version HEAD body should read")
            .to_bytes();
        assert!(body.is_empty());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn should_not_alias_or_delete_version_data_given_unsafe_version_ids() {
        // Arrange
        let storage = temp_storage();
        storage.create_bucket("bucket".to_string()).unwrap();
        storage.enable_versioning("bucket").unwrap();
        storage
            .put_object(
                "bucket",
                "doc.txt".to_string(),
                Object::new(
                    "doc.txt".to_string(),
                    b"first".to_vec(),
                    "text/plain".to_string(),
                ),
            )
            .unwrap();
        let first_version_id = storage
            .get_object("bucket", "doc.txt")
            .unwrap()
            .version_id
            .expect("first version id should exist");
        storage
            .put_object(
                "bucket",
                "doc.txt".to_string(),
                Object::new(
                    "doc.txt".to_string(),
                    b"current".to_vec(),
                    "text/plain".to_string(),
                ),
            )
            .unwrap();
        let version_count = storage
            .list_object_versions_for_key("bucket", "doc.txt")
            .unwrap()
            .len();
        let uri = "http://localhost/bucket/doc.txt?versionId=%2E%2E";
        let get_request = request_with_uri("GET", uri, &[], b"").await;
        let head_request = request_with_uri("HEAD", uri, &[], b"").await;
        let delete_request = request_with_uri("DELETE", uri, &[], b"").await;

        // Act
        let get_response = object_get(
            storage.clone(),
            auth_disabled_config(),
            "bucket",
            "doc.txt",
            &get_request,
            "req-unsafe-version-get".to_string(),
        )
        .await
        .expect("unsafe version GET should respond");
        let head_response = object_head(
            storage.clone(),
            auth_disabled_config(),
            "bucket",
            "doc.txt",
            &head_request,
            "req-unsafe-version-head".to_string(),
        )
        .await
        .expect("unsafe version HEAD should respond");
        let delete_response = object_delete(
            storage.clone(),
            auth_disabled_config(),
            "bucket",
            "doc.txt",
            &delete_request,
            "req-unsafe-version-delete".to_string(),
        )
        .await
        .expect("unsafe version DELETE should respond");

        // Assert
        assert_eq!(get_response.status(), StatusCode::NOT_FOUND);
        assert_eq!(head_response.status(), StatusCode::NOT_FOUND);
        assert_eq!(delete_response.status(), StatusCode::NO_CONTENT);
        assert_eq!(
            storage.get_object("bucket", "doc.txt").unwrap().data,
            b"current"
        );
        assert_eq!(
            storage
                .get_object_version("bucket", "doc.txt", &first_version_id)
                .unwrap()
                .data,
            b"first"
        );
        assert_eq!(
            storage
                .list_object_versions_for_key("bucket", "doc.txt")
                .unwrap()
                .len(),
            version_count
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn should_return_empty_not_found_for_missing_current_head() {
        // Arrange
        let storage = temp_storage();
        storage.create_bucket("bucket".to_string()).unwrap();
        let head = request("HEAD", &[], b"").await;

        // Act
        let response = object_head(
            storage,
            auth_disabled_config(),
            "bucket",
            "missing.txt",
            &head,
            "req-missing-current-head".to_string(),
        )
        .await
        .expect("missing current HEAD should respond");

        // Assert
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert!(response
            .into_body()
            .collect()
            .await
            .expect("missing current HEAD body should read")
            .to_bytes()
            .is_empty());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn should_return_not_found_for_ranged_get_of_missing_object() {
        // Arrange
        let storage = temp_storage();
        storage.create_bucket("bucket".to_string()).unwrap();
        let get = request("GET", &[("range", "bytes=0-0")], b"").await;

        // Act
        let response = object_get(
            storage,
            auth_disabled_config(),
            "bucket",
            "missing.txt",
            &get,
            "req-missing-range".to_string(),
        )
        .await
        .expect("missing ranged GET should respond");

        // Assert
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        let body = response
            .into_body()
            .collect()
            .await
            .expect("missing ranged GET body should read")
            .to_bytes();
        assert!(String::from_utf8_lossy(&body).contains("NoSuchKey"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn should_return_native_no_such_bucket_for_every_current_object_method() {
        // Arrange
        let storage = temp_storage();
        let get = request("GET", &[], b"").await;
        let head = request("HEAD", &[], b"").await;
        let put = request("PUT", &[], b"must-not-commit").await;
        let delete = request("DELETE", &[], b"").await;

        // Act
        let get = object_get(
            storage.clone(),
            auth_disabled_config(),
            "missing-bucket",
            "object",
            &get,
            "req-missing-bucket-get".to_string(),
        )
        .await
        .expect("GET should respond");
        let head = object_head(
            storage.clone(),
            auth_disabled_config(),
            "missing-bucket",
            "object",
            &head,
            "req-missing-bucket-head".to_string(),
        )
        .await
        .expect("HEAD should respond");
        let put = object_put(
            storage.clone(),
            auth_disabled_config(),
            "missing-bucket",
            "object",
            &put,
            "req-missing-bucket-put".to_string(),
        )
        .await
        .expect("PUT should respond");
        let delete = object_delete(
            storage.clone(),
            auth_disabled_config(),
            "missing-bucket",
            "object",
            &delete,
            "req-missing-bucket-delete".to_string(),
        )
        .await
        .expect("DELETE should respond");

        // Assert
        for (method, response) in [("GET", get), ("PUT", put), ("DELETE", delete)] {
            assert_eq!(response.status(), StatusCode::NOT_FOUND, "{method}");
            assert!(
                response.headers().contains_key("x-amz-request-id"),
                "{method}"
            );
            assert!(response.headers().contains_key("x-amz-id-2"), "{method}");
            let body = response
                .into_body()
                .collect()
                .await
                .expect("error body should read")
                .to_bytes();
            assert!(
                String::from_utf8_lossy(&body).contains("<Code>NoSuchBucket</Code>"),
                "{method} body: {}",
                String::from_utf8_lossy(&body)
            );
        }
        assert_eq!(head.status(), StatusCode::NOT_FOUND);
        assert!(head.headers().contains_key("x-amz-request-id"));
        assert!(head.headers().contains_key("x-amz-id-2"));
        assert!(head
            .into_body()
            .collect()
            .await
            .expect("HEAD body should read")
            .to_bytes()
            .is_empty());
        assert!(!storage.bucket_exists("missing-bucket").unwrap());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn should_round_trip_sse_headers_and_require_matching_sse_c_reads() {
        let storage = temp_storage();
        storage.create_bucket("bucket".to_string()).unwrap();

        let put = request(
            "PUT",
            &[
                ("x-amz-server-side-encryption-customer-algorithm", "AES256"),
                ("x-amz-server-side-encryption-customer-key", "secret"),
                ("x-amz-server-side-encryption-customer-key-MD5", "md5-value"),
            ],
            b"payload",
        )
        .await;
        let put_response = object_put(
            storage.clone(),
            auth_disabled_config(),
            "bucket",
            "locked.txt",
            &put,
            "req-sse-put".to_string(),
        )
        .await
        .expect("put should succeed");
        assert_eq!(put_response.status(), StatusCode::OK);

        let head = request(
            "HEAD",
            &[
                ("x-amz-server-side-encryption-customer-algorithm", "AES256"),
                ("x-amz-server-side-encryption-customer-key-MD5", "md5-value"),
            ],
            b"",
        )
        .await;
        let head_response = object_head(
            storage.clone(),
            auth_disabled_config(),
            "bucket",
            "locked.txt",
            &head,
            "req-sse-head".to_string(),
        )
        .await
        .expect("head should succeed");
        assert_eq!(head_response.status(), StatusCode::OK);
        assert_eq!(
            head_response
                .headers()
                .get("x-amz-server-side-encryption-customer-algorithm")
                .and_then(|value| value.to_str().ok()),
            Some("AES256")
        );

        let bad_head = request("HEAD", &[], b"").await;
        let bad_head_response = object_head(
            storage,
            auth_disabled_config(),
            "bucket",
            "locked.txt",
            &bad_head,
            "req-sse-bad".to_string(),
        )
        .await
        .expect("head should respond");
        assert_eq!(bad_head_response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn should_reject_object_lock_headers_when_bucket_mode_is_not_enabled() {
        let storage = temp_storage();
        storage.create_bucket("bucket".to_string()).unwrap();
        let content_md5 = BASE64.encode(md5::compute(b"payload").0);

        let put = request(
            "PUT",
            &[
                ("content-md5", &content_md5),
                ("x-amz-object-lock-mode", "GOVERNANCE"),
                (
                    "x-amz-object-lock-retain-until-date",
                    "2099-01-01T00:00:00Z",
                ),
                ("x-amz-object-lock-legal-hold", "ON"),
            ],
            b"payload",
        )
        .await;
        let put_response = object_put(
            storage.clone(),
            auth_disabled_config(),
            "bucket",
            "governed.txt",
            &put,
            "req-lock-put".to_string(),
        )
        .await
        .expect("put should respond");
        assert_eq!(put_response.status(), StatusCode::BAD_REQUEST);
        assert!(storage.get_object("bucket", "governed.txt").is_err());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn should_reject_orphan_or_nonfuture_object_lock_retention_headers_before_mutation() {
        // Arrange
        let storage = temp_storage();
        storage.create_bucket("bucket".to_string()).unwrap();
        let mut metadata = storage.get_bucket("bucket").unwrap().metadata;
        metadata.insert("s3_object_lock_enabled".to_string(), "true".to_string());
        storage.update_bucket_metadata("bucket", metadata).unwrap();
        let content_md5 = BASE64.encode(md5::compute(b"payload").0);
        let cases: [(&str, Vec<(&str, &str)>); 4] = [
            (
                "mode-only",
                vec![
                    ("content-md5", &content_md5),
                    ("x-amz-object-lock-mode", "COMPLIANCE"),
                ],
            ),
            (
                "date-only",
                vec![
                    ("content-md5", &content_md5),
                    (
                        "x-amz-object-lock-retain-until-date",
                        "2099-01-01T00:00:00Z",
                    ),
                ],
            ),
            (
                "past-date",
                vec![
                    ("content-md5", &content_md5),
                    ("x-amz-object-lock-mode", "GOVERNANCE"),
                    (
                        "x-amz-object-lock-retain-until-date",
                        "2000-01-01T00:00:00Z",
                    ),
                ],
            ),
            (
                "invalid-date",
                vec![
                    ("content-md5", &content_md5),
                    ("x-amz-object-lock-mode", "GOVERNANCE"),
                    ("x-amz-object-lock-retain-until-date", "not-a-date"),
                ],
            ),
        ];

        // Act
        // Assert
        for (key, headers) in cases {
            let put = request("PUT", &headers, b"payload").await;
            let response = object_put(
                storage.clone(),
                auth_disabled_config(),
                "bucket",
                key,
                &put,
                format!("req-{key}"),
            )
            .await
            .expect("invalid Object Lock headers should respond");
            assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{key}");
            assert!(storage.get_object("bucket", key).is_err(), "{key}");
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn should_reject_mismatched_content_md5_without_object_mutation() {
        // Arrange
        let storage = temp_storage();
        storage.create_bucket("bucket".to_string()).unwrap();
        let wrong_digest = BASE64.encode([0_u8; 16]);
        let put = request("PUT", &[("content-md5", &wrong_digest)], b"payload").await;

        // Act
        let response = object_put(
            storage.clone(),
            auth_disabled_config(),
            "bucket",
            "bad-digest.txt",
            &put,
            "req-bad-digest".to_string(),
        )
        .await
        .expect("bad digest PUT should respond");

        // Assert
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body = response
            .into_body()
            .collect()
            .await
            .expect("bad digest body should read")
            .to_bytes();
        assert!(String::from_utf8_lossy(&body).contains("BadDigest"));
        assert!(storage.get_object("bucket", "bad-digest.txt").is_err());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn should_return_precondition_failed_when_if_match_does_not_match_on_put() {
        let storage = temp_storage();
        storage.create_bucket("bucket".to_string()).unwrap();

        storage
            .put_object(
                "bucket",
                "notes.txt".to_string(),
                Object::new(
                    "notes.txt".to_string(),
                    b"current payload".to_vec(),
                    "text/plain".to_string(),
                ),
            )
            .unwrap();

        let put = request("PUT", &[("If-Match", "not-the-etag")], b"replacement").await;
        let response = object_put(
            storage,
            auth_disabled_config(),
            "bucket",
            "notes.txt",
            &put,
            "req-put-if-match".to_string(),
        )
        .await
        .expect("put should respond");

        assert_eq!(response.status(), StatusCode::PRECONDITION_FAILED);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn should_not_treat_weak_if_match_as_a_strong_s3_precondition() {
        // Arrange
        let storage = temp_storage();
        storage.create_bucket("bucket".to_string()).unwrap();
        let object = Object::new(
            "notes.txt".to_string(),
            b"current payload".to_vec(),
            "text/plain".to_string(),
        );
        let weak_etag = format!("W/\"{}\"", object.etag);
        storage
            .put_object("bucket", "notes.txt".to_string(), object)
            .unwrap();
        let put = request("PUT", &[("if-match", &weak_etag)], b"replacement").await;

        // Act
        let response = object_put(
            storage.clone(),
            auth_disabled_config(),
            "bucket",
            "notes.txt",
            &put,
            "req-weak-if-match".to_string(),
        )
        .await
        .expect("weak If-Match PUT should respond");

        // Assert
        assert_eq!(response.status(), StatusCode::PRECONDITION_FAILED);
        assert_eq!(
            storage.get_object("bucket", "notes.txt").unwrap().data,
            b"current payload"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn should_return_precondition_failed_when_if_none_match_matches_on_put() {
        let storage = temp_storage();
        storage.create_bucket("bucket".to_string()).unwrap();

        let object = Object::new(
            "notes.txt".to_string(),
            b"current payload".to_vec(),
            "text/plain".to_string(),
        );
        let etag = object.etag.clone();
        storage
            .put_object("bucket", "notes.txt".to_string(), object)
            .unwrap();

        let put = request("PUT", &[("If-None-Match", &etag)], b"replacement").await;
        let response = object_put(
            storage,
            auth_disabled_config(),
            "bucket",
            "notes.txt",
            &put,
            "req-put-if-none-match".to_string(),
        )
        .await
        .expect("put should respond");

        assert_eq!(response.status(), StatusCode::PRECONDITION_FAILED);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn should_return_precondition_failed_when_if_unmodified_since_is_stale_on_put() {
        let storage = temp_storage();
        storage.create_bucket("bucket".to_string()).unwrap();

        let mut object = Object::new(
            "notes.txt".to_string(),
            b"current payload".to_vec(),
            "text/plain".to_string(),
        );
        object.last_modified = chrono::Utc.with_ymd_and_hms(2024, 4, 10, 12, 0, 0).unwrap();
        storage
            .put_object("bucket", "notes.txt".to_string(), object)
            .unwrap();

        let put = request(
            "PUT",
            &[("If-Unmodified-Since", "Tue, 09 Apr 2024 12:00:00 +0000")],
            b"replacement",
        )
        .await;
        let response = object_put(
            storage,
            auth_disabled_config(),
            "bucket",
            "notes.txt",
            &put,
            "req-put-if-unmodified-since".to_string(),
        )
        .await
        .expect("put should respond");

        assert_eq!(response.status(), StatusCode::PRECONDITION_FAILED);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn should_apply_cors_headers_to_object_get_and_head_responses() {
        let storage = temp_storage();
        storage.create_bucket("bucket".to_string()).unwrap();
        storage
            .put_object(
                "bucket",
                "notes.txt".to_string(),
                crate::models::Object::new(
                    "notes.txt".to_string(),
                    b"hello cors".to_vec(),
                    "text/plain".to_string(),
                ),
            )
            .unwrap();

        let mut metadata = bucket_service::get_bucket(storage.as_ref(), "bucket")
            .unwrap()
            .metadata;
        metadata.insert(
            "s3_cors_xml".to_string(),
            r#"<?xml version="1.0" encoding="UTF-8"?><CORSConfiguration><CORSRule><AllowedOrigin>https://app.example</AllowedOrigin><AllowedMethod>GET</AllowedMethod><ExposeHeader>ETag</ExposeHeader></CORSRule></CORSConfiguration>"#
                .to_string(),
        );
        bucket_service::update_bucket_metadata(storage.as_ref(), "bucket", metadata).unwrap();

        let get_request = request_with_uri(
            "GET",
            "http://localhost/bucket/notes.txt",
            &[("Origin", "https://app.example")],
            b"",
        )
        .await;
        let get_response = object_get(
            storage.clone(),
            auth_disabled_config(),
            "bucket",
            "notes.txt",
            &get_request,
            "req-cors-get".to_string(),
        )
        .await
        .expect("get should succeed");
        assert_eq!(get_response.status(), StatusCode::OK);
        assert_eq!(
            get_response
                .headers()
                .get("Access-Control-Allow-Origin")
                .and_then(|value| value.to_str().ok()),
            Some("https://app.example")
        );
        assert_eq!(
            get_response
                .headers()
                .get("Access-Control-Expose-Headers")
                .and_then(|value| value.to_str().ok()),
            Some("ETag")
        );

        let head_request = request_with_uri(
            "HEAD",
            "http://localhost/bucket/notes.txt",
            &[("Origin", "https://app.example")],
            b"",
        )
        .await;
        let head_response = object_head(
            storage,
            auth_disabled_config(),
            "bucket",
            "notes.txt",
            &head_request,
            "req-cors-head".to_string(),
        )
        .await
        .expect("head should succeed");
        assert_eq!(head_response.status(), StatusCode::OK);
        assert_eq!(
            head_response
                .headers()
                .get("Access-Control-Allow-Origin")
                .and_then(|value| value.to_str().ok()),
            Some("https://app.example")
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn should_answer_object_preflight_requests_from_bucket_cors_configuration() {
        let storage = temp_storage();
        storage.create_bucket("bucket".to_string()).unwrap();

        let mut metadata = bucket_service::get_bucket(storage.as_ref(), "bucket")
            .unwrap()
            .metadata;
        metadata.insert(
            "s3_cors_xml".to_string(),
            r#"<?xml version="1.0" encoding="UTF-8"?><CORSConfiguration><CORSRule><AllowedOrigin>https://app.example</AllowedOrigin><AllowedMethod>PUT</AllowedMethod><AllowedHeader>content-type</AllowedHeader><AllowedHeader>x-amz-meta-demo</AllowedHeader><MaxAgeSeconds>300</MaxAgeSeconds></CORSRule></CORSConfiguration>"#
                .to_string(),
        );
        bucket_service::update_bucket_metadata(storage.as_ref(), "bucket", metadata).unwrap();

        let preflight_request = request_with_uri(
            "OPTIONS",
            "http://localhost/bucket/upload.txt",
            &[
                ("Origin", "https://app.example"),
                ("Access-Control-Request-Method", "PUT"),
                (
                    "Access-Control-Request-Headers",
                    "content-type, x-amz-meta-demo",
                ),
            ],
            b"",
        )
        .await;
        let preflight_response = object_get(
            storage,
            auth_disabled_config(),
            "bucket",
            "upload.txt",
            &preflight_request,
            "req-cors-options".to_string(),
        )
        .await
        .expect("preflight should respond");

        assert_eq!(preflight_response.status(), StatusCode::OK);
        assert_eq!(
            preflight_response
                .headers()
                .get("Access-Control-Allow-Origin")
                .and_then(|value| value.to_str().ok()),
            Some("https://app.example")
        );
        assert_eq!(
            preflight_response
                .headers()
                .get("Access-Control-Allow-Methods")
                .and_then(|value| value.to_str().ok()),
            Some("PUT")
        );
        assert_eq!(
            preflight_response
                .headers()
                .get("Access-Control-Allow-Headers")
                .and_then(|value| value.to_str().ok()),
            Some("content-type, x-amz-meta-demo")
        );
        assert_eq!(
            preflight_response
                .headers()
                .get("Access-Control-Max-Age")
                .and_then(|value| value.to_str().ok()),
            Some("300")
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn should_apply_cors_headers_to_object_put_and_delete_responses() {
        let storage = temp_storage();
        storage.create_bucket("bucket".to_string()).unwrap();

        let mut metadata = bucket_service::get_bucket(storage.as_ref(), "bucket")
            .unwrap()
            .metadata;
        metadata.insert(
            "s3_cors_xml".to_string(),
            r#"<?xml version="1.0" encoding="UTF-8"?><CORSConfiguration><CORSRule><AllowedOrigin>https://app.example</AllowedOrigin><AllowedMethod>PUT</AllowedMethod><AllowedMethod>DELETE</AllowedMethod><ExposeHeader>ETag</ExposeHeader></CORSRule></CORSConfiguration>"#
                .to_string(),
        );
        bucket_service::update_bucket_metadata(storage.as_ref(), "bucket", metadata).unwrap();

        let put_request = request_with_uri(
            "PUT",
            "http://localhost/bucket/notes.txt",
            &[
                ("Origin", "https://app.example"),
                ("Content-Type", "text/plain"),
            ],
            b"hello cors",
        )
        .await;
        let put_response = object_put(
            storage.clone(),
            auth_disabled_config(),
            "bucket",
            "notes.txt",
            &put_request,
            "req-cors-put".to_string(),
        )
        .await
        .expect("put should succeed");
        assert_eq!(put_response.status(), StatusCode::OK);
        assert_eq!(
            put_response
                .headers()
                .get("Access-Control-Allow-Origin")
                .and_then(|value| value.to_str().ok()),
            Some("https://app.example")
        );
        assert_eq!(
            put_response
                .headers()
                .get("Access-Control-Expose-Headers")
                .and_then(|value| value.to_str().ok()),
            Some("ETag")
        );

        let delete_request = request_with_uri(
            "DELETE",
            "http://localhost/bucket/notes.txt",
            &[("Origin", "https://app.example")],
            b"",
        )
        .await;
        let delete_response = object_delete(
            storage,
            auth_disabled_config(),
            "bucket",
            "notes.txt",
            &delete_request,
            "req-cors-delete".to_string(),
        )
        .await
        .expect("delete should succeed");
        assert_eq!(delete_response.status(), StatusCode::NO_CONTENT);
        assert_eq!(
            delete_response
                .headers()
                .get("Access-Control-Allow-Origin")
                .and_then(|value| value.to_str().ok()),
            Some("https://app.example")
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn should_apply_cors_headers_to_multipart_initiate_post_responses() {
        let storage = temp_storage();
        storage.create_bucket("bucket".to_string()).unwrap();

        let mut metadata = bucket_service::get_bucket(storage.as_ref(), "bucket")
            .unwrap()
            .metadata;
        metadata.insert(
            "s3_cors_xml".to_string(),
            r#"<?xml version="1.0" encoding="UTF-8"?><CORSConfiguration><CORSRule><AllowedOrigin>https://app.example</AllowedOrigin><AllowedMethod>POST</AllowedMethod></CORSRule></CORSConfiguration>"#
                .to_string(),
        );
        bucket_service::update_bucket_metadata(storage.as_ref(), "bucket", metadata).unwrap();

        let request = request_with_uri(
            "POST",
            "http://localhost/bucket/upload.txt?uploads",
            &[("Origin", "https://app.example")],
            b"",
        )
        .await;
        let response = object_post(
            storage,
            auth_disabled_config(),
            "bucket",
            "upload.txt",
            &request,
            "req-cors-initiate".to_string(),
        )
        .await
        .expect("initiate should succeed");

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response
                .headers()
                .get("Access-Control-Allow-Origin")
                .and_then(|value| value.to_str().ok()),
            Some("https://app.example")
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn should_store_object_acl_grants_from_header_inputs() {
        let storage = temp_storage();
        storage.create_bucket("bucket".to_string()).unwrap();
        storage
            .put_object(
                "bucket",
                "notes.txt".to_string(),
                crate::models::Object::new(
                    "notes.txt".to_string(),
                    b"payload".to_vec(),
                    "text/plain".to_string(),
                ),
            )
            .unwrap();

        let put_acl = request_with_uri(
            "PUT",
            "http://localhost/bucket/notes.txt?acl",
            &[("x-amz-grant-full-control", "id=\"integration-tester\"")],
            b"",
        )
        .await;
        let put_response = object_put(
            storage.clone(),
            auth_disabled_config(),
            "bucket",
            "notes.txt",
            &put_acl,
            "req-object-acl-put".to_string(),
        )
        .await
        .expect("object acl put should complete");
        assert_eq!(put_response.status(), StatusCode::OK);

        let get_acl =
            request_with_uri("GET", "http://localhost/bucket/notes.txt?acl", &[], b"").await;
        let get_response = object_get(
            storage,
            auth_disabled_config(),
            "bucket",
            "notes.txt",
            &get_acl,
            "req-object-acl-get".to_string(),
        )
        .await
        .expect("object acl get should complete");
        let body = String::from_utf8(
            get_response
                .into_body()
                .collect()
                .await
                .expect("body should read")
                .to_bytes()
                .to_vec(),
        )
        .expect("body should be utf8");
        assert!(body.contains("integration-tester"));
        assert!(
            body.matches("<Permission>FULL_CONTROL</Permission>")
                .count()
                >= 2
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn should_store_object_acl_grants_from_xml_body_inputs() {
        let storage = temp_storage();
        storage.create_bucket("bucket".to_string()).unwrap();
        storage
            .put_object(
                "bucket",
                "notes.txt".to_string(),
                crate::models::Object::new(
                    "notes.txt".to_string(),
                    b"payload".to_vec(),
                    "text/plain".to_string(),
                ),
            )
            .unwrap();

        let put_acl = request_with_uri(
            "PUT",
            "http://localhost/bucket/notes.txt?acl",
            &[],
            br#"<?xml version="1.0" encoding="UTF-8"?>
<AccessControlPolicy xmlns="http://s3.amazonaws.com/doc/2006-03-01/">
  <AccessControlList>
    <Grant>
      <Grantee xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance" xsi:type="CanonicalUser">
        <ID>integration-tester</ID>
      </Grantee>
      <Permission>FULL_CONTROL</Permission>
    </Grant>
  </AccessControlList>
</AccessControlPolicy>"#,
        )
        .await;
        let put_response = object_put(
            storage.clone(),
            auth_disabled_config(),
            "bucket",
            "notes.txt",
            &put_acl,
            "req-object-acl-xml-put".to_string(),
        )
        .await
        .expect("object acl put should complete");
        assert_eq!(put_response.status(), StatusCode::OK);

        let get_acl =
            request_with_uri("GET", "http://localhost/bucket/notes.txt?acl", &[], b"").await;
        let get_response = object_get(
            storage,
            auth_disabled_config(),
            "bucket",
            "notes.txt",
            &get_acl,
            "req-object-acl-xml-get".to_string(),
        )
        .await
        .expect("object acl get should complete");
        let body = String::from_utf8(
            get_response
                .into_body()
                .collect()
                .await
                .expect("body should read")
                .to_bytes()
                .to_vec(),
        )
        .expect("body should be utf8");
        assert!(body.contains("integration-tester"));
        assert_eq!(
            body.matches("<Permission>FULL_CONTROL</Permission>")
                .count(),
            2
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn should_return_precondition_failed_when_if_match_does_not_match_on_object_acl_put() {
        let storage = temp_storage();
        storage.create_bucket("bucket".to_string()).unwrap();
        storage
            .put_object(
                "bucket",
                "notes.txt".to_string(),
                Object::new(
                    "notes.txt".to_string(),
                    b"payload".to_vec(),
                    "text/plain".to_string(),
                ),
            )
            .unwrap();

        let put_acl = request_with_uri(
            "PUT",
            "http://localhost/bucket/notes.txt?acl",
            &[("If-Match", "not-the-etag")],
            &[],
        )
        .await;
        let response = object_put(
            storage,
            auth_disabled_config(),
            "bucket",
            "notes.txt",
            &put_acl,
            "req-object-acl-precondition".to_string(),
        )
        .await
        .expect("object acl put should complete");

        assert_eq!(response.status(), StatusCode::PRECONDITION_FAILED);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn should_return_precondition_failed_when_if_match_does_not_match_on_object_tagging_put()
    {
        let storage = temp_storage();
        storage.create_bucket("bucket".to_string()).unwrap();
        storage
            .put_object(
                "bucket",
                "notes.txt".to_string(),
                Object::new(
                    "notes.txt".to_string(),
                    b"payload".to_vec(),
                    "text/plain".to_string(),
                ),
            )
            .unwrap();

        let put_tagging = request_with_uri(
            "PUT",
            "http://localhost/bucket/notes.txt?tagging",
            &[("If-Match", "not-the-etag")],
            br#"<?xml version="1.0" encoding="UTF-8"?>
<Tagging><TagSet><Tag><Key>env</Key><Value>dev</Value></Tag></TagSet></Tagging>"#,
        )
        .await;
        let response = object_put(
            storage,
            auth_disabled_config(),
            "bucket",
            "notes.txt",
            &put_tagging,
            "req-object-tagging-precondition".to_string(),
        )
        .await
        .expect("object tagging put should complete");

        assert_eq!(response.status(), StatusCode::PRECONDITION_FAILED);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn should_reject_object_lock_multipart_initiation_without_creating_a_session() {
        // Arrange
        let storage = temp_storage();
        storage.create_bucket("bucket".to_string()).unwrap();
        storage.enable_versioning("bucket").unwrap();
        let mut metadata = storage.get_bucket("bucket").unwrap().metadata;
        metadata.insert("s3_object_lock_enabled".to_string(), "true".to_string());
        storage.update_bucket_metadata("bucket", metadata).unwrap();
        let initiate = request_with_uri(
            "POST",
            "http://localhost/bucket/object.txt?uploads",
            &[
                ("x-amz-object-lock-mode", "GOVERNANCE"),
                (
                    "x-amz-object-lock-retain-until-date",
                    "2099-01-01T00:00:00Z",
                ),
            ],
            b"",
        )
        .await;

        // Act
        let response = object_post(
            storage.clone(),
            auth_disabled_config(),
            "bucket",
            "object.txt",
            &initiate,
            "req-locked-multipart-init".to_string(),
        )
        .await
        .expect("multipart initiation should respond");

        // Assert
        assert_eq!(response.status(), StatusCode::NOT_IMPLEMENTED);
        assert!(storage.list_multipart_uploads("bucket").unwrap().is_empty());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn should_reject_upload_part_copy_without_storing_an_empty_part() {
        // Arrange
        let storage = temp_storage();
        storage.create_bucket("bucket".to_string()).unwrap();
        storage.create_bucket("source".to_string()).unwrap();
        storage
            .put_object(
                "source",
                "object.txt".to_string(),
                Object::new(
                    "object.txt".to_string(),
                    b"source bytes".to_vec(),
                    "text/plain".to_string(),
                ),
            )
            .unwrap();
        let upload = storage
            .create_multipart_upload("bucket", "object.txt".to_string())
            .expect("multipart upload should be created");
        let upload_part_copy = request_with_uri(
            "PUT",
            &format!(
                "http://localhost/bucket/object.txt?partNumber=1&uploadId={}",
                upload.upload_id
            ),
            &[("x-amz-copy-source", "/source/object.txt")],
            b"",
        )
        .await;

        // Act
        let response = object_put(
            storage.clone(),
            auth_disabled_config(),
            "bucket",
            "object.txt",
            &upload_part_copy,
            "req-upload-part-copy".to_string(),
        )
        .await
        .expect("unsupported UploadPartCopy should respond");

        // Assert
        assert_eq!(response.status(), StatusCode::NOT_IMPLEMENTED);
        let body = response
            .into_body()
            .collect()
            .await
            .expect("unsupported UploadPartCopy response should read")
            .to_bytes();
        assert!(String::from_utf8_lossy(&body).contains("NotImplemented"));
        assert!(storage
            .list_parts("bucket", &upload.upload_id)
            .expect("upload session should remain readable")
            .is_empty());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn should_require_auth_for_object_post_multipart_routes() {
        let storage = temp_storage();
        storage.create_bucket("bucket".to_string()).unwrap();
        let upload = storage
            .create_multipart_upload("bucket", "object.txt".to_string())
            .expect("multipart upload should be created");

        let initiate = request_with_uri(
            "POST",
            "http://localhost/bucket/object.txt?uploads",
            &[],
            b"",
        )
        .await;
        let initiate_response = object_post(
            storage.clone(),
            auth_enabled_config(),
            "bucket",
            "object.txt",
            &initiate,
            "req-auth-initiate".to_string(),
        )
        .await
        .expect("initiate request should respond");
        assert_eq!(initiate_response.status(), StatusCode::FORBIDDEN);

        let complete = request_with_uri(
            "POST",
            &format!(
                "http://localhost/bucket/object.txt?uploadId={}",
                upload.upload_id
            ),
            &[],
            b"<CompleteMultipartUpload />",
        )
        .await;
        let complete_response = object_post(
            storage,
            auth_enabled_config(),
            "bucket",
            "object.txt",
            &complete,
            "req-auth-complete".to_string(),
        )
        .await
        .expect("complete request should respond");
        assert_eq!(complete_response.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn should_return_invalid_part_number_for_non_numeric_upload_part_requests() {
        let storage = temp_storage();
        storage.create_bucket("bucket".to_string()).unwrap();
        let upload = storage
            .create_multipart_upload("bucket", "object.txt".to_string())
            .expect("multipart upload should be created");

        let request = request_with_uri(
            "PUT",
            &format!(
                "http://localhost/bucket/object.txt?uploadId={}&partNumber=abc",
                upload.upload_id
            ),
            &[],
            b"payload",
        )
        .await;
        let response = object_put(
            storage,
            auth_disabled_config(),
            "bucket",
            "object.txt",
            &request,
            "req-invalid-part".to_string(),
        )
        .await
        .expect("upload part request should respond");

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body = response
            .into_body()
            .collect()
            .await
            .expect("body should read")
            .to_bytes();
        let body = String::from_utf8(body.to_vec()).expect("body should be utf8");
        assert!(body.contains("<Code>InvalidPartNumber</Code>"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn should_reject_complete_multipart_when_upload_id_targets_different_key() {
        let storage = temp_storage();
        storage.create_bucket("bucket".to_string()).unwrap();
        let upload = storage
            .create_multipart_upload("bucket", "real.txt".to_string())
            .expect("multipart upload should be created");
        let etag = storage
            .upload_part("bucket", &upload.upload_id, 1, b"payload".to_vec())
            .expect("part upload should succeed");

        let mismatched = request_with_uri(
            "POST",
            &format!(
                "http://localhost/bucket/other.txt?uploadId={}",
                upload.upload_id
            ),
            &[],
            b"<CompleteMultipartUpload />",
        )
        .await;
        let mismatched_response = object_post(
            storage.clone(),
            auth_disabled_config(),
            "bucket",
            "other.txt",
            &mismatched,
            "req-mismatch-complete".to_string(),
        )
        .await
        .expect("complete request should respond");
        assert_eq!(mismatched_response.status(), StatusCode::BAD_REQUEST);

        assert!(storage
            .get_multipart_upload("bucket", &upload.upload_id)
            .is_ok());

        let complete_manifest = format!(
            "<CompleteMultipartUpload><Part><PartNumber>1</PartNumber><ETag>\"{etag}\"</ETag></Part></CompleteMultipartUpload>"
        );
        let matching = request_with_uri(
            "POST",
            &format!(
                "http://localhost/bucket/real.txt?uploadId={}",
                upload.upload_id
            ),
            &[],
            complete_manifest.as_bytes(),
        )
        .await;
        let matching_response = object_post(
            storage.clone(),
            auth_disabled_config(),
            "bucket",
            "real.txt",
            &matching,
            "req-match-complete".to_string(),
        )
        .await
        .expect("complete request should respond");
        assert_eq!(matching_response.status(), StatusCode::OK);
        assert_eq!(
            storage.get_object("bucket", "real.txt").unwrap().data,
            b"payload".to_vec()
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn should_reject_unsupported_conditional_multipart_completion_without_consuming_upload() {
        // Arrange
        let storage = temp_storage();
        storage.create_bucket("bucket".to_string()).unwrap();
        let upload = storage
            .create_multipart_upload("bucket", "object.txt".to_string())
            .expect("multipart upload should be created");
        let etag = storage
            .upload_part("bucket", &upload.upload_id, 1, b"payload".to_vec())
            .expect("multipart part should upload");
        let body = format!(
            "<CompleteMultipartUpload><Part><PartNumber>1</PartNumber><ETag>\"{etag}\"</ETag></Part></CompleteMultipartUpload>"
        );
        let request = request_with_uri(
            "POST",
            &format!(
                "http://localhost/bucket/object.txt?uploadId={}",
                upload.upload_id
            ),
            &[("if-none-match", "*")],
            body.as_bytes(),
        )
        .await;

        // Act
        let response = object_post(
            storage.clone(),
            auth_disabled_config(),
            "bucket",
            "object.txt",
            &request,
            "req-conditional-complete".to_string(),
        )
        .await
        .expect("conditional completion should respond");

        // Assert
        assert_eq!(response.status(), StatusCode::NOT_IMPLEMENTED);
        assert!(storage
            .get_multipart_upload("bucket", &upload.upload_id)
            .is_ok());
        assert!(storage.get_object("bucket", "object.txt").is_err());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn should_reject_abort_multipart_when_upload_id_targets_different_key() {
        let storage = temp_storage();
        storage.create_bucket("bucket".to_string()).unwrap();
        let upload = storage
            .create_multipart_upload("bucket", "real.txt".to_string())
            .expect("multipart upload should be created");

        let mismatched = request_with_uri(
            "DELETE",
            &format!(
                "http://localhost/bucket/other.txt?uploadId={}",
                upload.upload_id
            ),
            &[],
            b"",
        )
        .await;
        let mismatched_response = object_delete(
            storage.clone(),
            auth_disabled_config(),
            "bucket",
            "other.txt",
            &mismatched,
            "req-mismatch-abort".to_string(),
        )
        .await
        .expect("abort request should respond");
        assert_eq!(mismatched_response.status(), StatusCode::BAD_REQUEST);

        assert!(storage
            .get_multipart_upload("bucket", &upload.upload_id)
            .is_ok());

        let matching = request_with_uri(
            "DELETE",
            &format!(
                "http://localhost/bucket/real.txt?uploadId={}",
                upload.upload_id
            ),
            &[],
            b"",
        )
        .await;
        let matching_response = object_delete(
            storage.clone(),
            auth_disabled_config(),
            "bucket",
            "real.txt",
            &matching,
            "req-match-abort".to_string(),
        )
        .await
        .expect("abort request should respond");
        assert_eq!(matching_response.status(), StatusCode::NO_CONTENT);
        assert!(matches!(
            storage.get_multipart_upload("bucket", &upload.upload_id),
            Err(crate::error::Error::NoSuchUpload)
        ));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn should_distinguish_missing_key_and_delete_marker_for_if_match_star_delete() {
        // Arrange
        let missing_storage = temp_storage();
        missing_storage.create_bucket("bucket".to_string()).unwrap();
        let request = request("DELETE", &[("if-match", "*")], b"").await;

        // Act
        let missing = object_delete(
            missing_storage,
            auth_disabled_config(),
            "bucket",
            "missing.txt",
            &request,
            "req-missing-conditional-delete".to_string(),
        )
        .await
        .expect("missing conditional delete should respond");

        // Assert
        assert_eq!(missing.status(), StatusCode::NOT_FOUND);
        let body = missing
            .into_body()
            .collect()
            .await
            .expect("missing response body should read")
            .to_bytes();
        assert!(String::from_utf8_lossy(&body).contains("<Code>NoSuchKey</Code>"));

        let (marker_storage, marker_id) = versioned_deleted_object();
        let versions_before = marker_storage
            .list_object_versions_for_key("bucket", "removed.txt")
            .unwrap()
            .len();
        let marker = object_delete(
            marker_storage.clone(),
            auth_disabled_config(),
            "bucket",
            "removed.txt",
            &request,
            "req-marker-conditional-delete".to_string(),
        )
        .await
        .expect("delete-marker conditional delete should respond");
        assert_eq!(marker.status(), StatusCode::PRECONDITION_FAILED);
        let versions_after = marker_storage
            .list_object_versions_for_key("bucket", "removed.txt")
            .unwrap();
        assert_eq!(versions_after.len(), versions_before);
        assert!(versions_after
            .iter()
            .any(|version| version.version_id.as_deref() == Some(marker_id.as_str())));
    }
}
