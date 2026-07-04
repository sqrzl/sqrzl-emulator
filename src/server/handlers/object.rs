use super::acl;
use super::auth::{check_authorization, verify_presigned_url};
use super::cors;
use super::ResponseBuilder;
use crate::auth::AuthConfig;
use crate::body::Body;
use crate::services::{
    object as object_service, storage_error_response, xml_error_response, xml_success_response,
};
use crate::storage::Storage;
use crate::utils::{headers as header_utils, validation, xml as xml_utils};
use http::StatusCode;
use hyper::Response;
use std::collections::HashMap;
use std::sync::Arc;

mod helpers;

use self::helpers::{
    add_version_header, apply_s3_request_contracts, check_copy_conditionals,
    check_object_conditionals, check_put_conditionals, copy_source_range_data,
    locked_object_response, object_is_locked, object_response_headers, parse_range,
    parse_tagging_header, upload_key_mismatch_response, validate_get_sse_headers,
};

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
        Ok(obj) => object_payload_response(storage, bucket, req, req_id, obj, StatusCode::OK, None),
        Err(e) => storage_error_response(&e, req_id),
    }
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
        Err(e) => xml_error_response(
            StatusCode::RANGE_NOT_SATISFIABLE,
            "InvalidRange",
            &e.to_string(),
            req_id,
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
        Err(e) => xml_error_response(StatusCode::NOT_FOUND, "NoSuchKey", &e.to_string(), req_id),
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

    let existing =
        tokio::task::block_in_place(|| object_service::get_object(storage.as_ref(), bucket, key))
            .ok();
    if let Some(existing) = existing.as_ref() {
        if object_is_locked(existing) {
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
        return Ok(upload_multipart_part(&storage, bucket, req, &req_id));
    }

    if let Some(copy_source) = req.header("x-amz-copy-source") {
        return copy_object(&storage, bucket, key, copy_source, req, &req_id);
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

fn put_object_tagging(
    storage: &Arc<dyn Storage>,
    bucket: &str,
    key: &str,
    req: &crate::server::http::Request,
    req_id: &str,
) -> Result<Response<Body>, String> {
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
                .header("ETag", &etag)
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
) -> Result<Response<Body>, String> {
    let Some((source_bucket, source_key)) = copy_source.split_once('/') else {
        return Ok(xml_error_response(
            StatusCode::BAD_REQUEST,
            "InvalidArgument",
            "Invalid copy source format",
            req_id,
        ));
    };

    match tokio::task::block_in_place(|| {
        object_service::get_object(storage.as_ref(), source_bucket, source_key)
    }) {
        Ok(src_obj) => copy_loaded_object(storage, bucket, key, req, req_id, &src_obj),
        Err(crate::error::Error::KeyNotFound) => Ok(xml_error_response(
            StatusCode::NOT_FOUND,
            "NoSuchKey",
            "Copy source not found",
            req_id,
        )),
        Err(err) => Ok(internal_error_response(&err, req_id)),
    }
}

fn copy_loaded_object(
    storage: &Arc<dyn Storage>,
    bucket: &str,
    key: &str,
    req: &crate::server::http::Request,
    req_id: &str,
    src_obj: &crate::models::Object,
) -> Result<Response<Body>, String> {
    if let Some(response) = check_copy_conditionals(req, src_obj, req_id) {
        return Ok(response);
    }

    let copy_data = match copy_object_data(req, src_obj) {
        Ok(data) => data,
        Err(message) => {
            return Ok(xml_error_response(
                StatusCode::RANGE_NOT_SATISFIABLE,
                "InvalidRange",
                &message,
                req_id,
            ));
        }
    };
    let metadata = copy_object_metadata(req, src_obj);
    let tags = copy_object_tags(req, src_obj)?;
    let mut dest_obj = crate::models::Object::new_with_metadata(
        key.to_string(),
        copy_data,
        src_obj.content_type.clone(),
        metadata,
    );
    dest_obj
        .provider_metadata
        .clone_from(&src_obj.provider_metadata);
    if let Err(response) = apply_s3_request_contracts(req, &mut dest_obj, req_id) {
        return Ok(response);
    }
    if let Some(tags) = tags {
        dest_obj.tags = tags;
    } else {
        dest_obj.tags.clone_from(&src_obj.tags);
    }

    Ok(store_copied_object(
        storage, bucket, key, req, req_id, dest_obj,
    ))
}

fn copy_object_data(
    req: &crate::server::http::Request,
    src_obj: &crate::models::Object,
) -> Result<Vec<u8>, String> {
    let Some(range_header) = req.header("x-amz-copy-source-range") else {
        return Ok(src_obj.data.clone());
    };

    copy_source_range_data(src_obj, range_header)
}

fn copy_object_metadata(
    req: &crate::server::http::Request,
    src_obj: &crate::models::Object,
) -> HashMap<String, String> {
    if req
        .header("x-amz-metadata-directive")
        .unwrap_or("COPY")
        .eq_ignore_ascii_case("REPLACE")
    {
        header_utils::extract_metadata_from_http_headers(req)
    } else {
        src_obj.metadata.clone()
    }
}

fn copy_object_tags(
    req: &crate::server::http::Request,
    src_obj: &crate::models::Object,
) -> Result<Option<HashMap<String, String>>, String> {
    let tagging_directive = req.header("x-amz-tagging-directive").unwrap_or("COPY");
    if let Some(tagging_header) = req.header("x-amz-tagging") {
        return if tagging_directive.eq_ignore_ascii_case("REPLACE")
            || tagging_directive.eq_ignore_ascii_case("COPY")
        {
            parse_tagging_header(tagging_header)
                .map(Some)
                .map_err(|err| format!("Invalid tags: {err}"))
        } else {
            Ok(None)
        };
    }

    if tagging_directive.eq_ignore_ascii_case("COPY") {
        Ok(Some(src_obj.tags.clone()))
    } else {
        Ok(None)
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

    match tokio::task::block_in_place(|| {
        object_service::put_object(storage.as_ref(), bucket, dest_key, dest_obj)
    }) {
        Ok(()) => {
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
        etag,
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
    let obj_key = obj.key.clone();
    let etag = obj.etag.clone();
    match tokio::task::block_in_place(|| {
        object_service::put_object(storage.as_ref(), bucket, obj_key, obj)
    }) {
        Ok(()) => put_object_response(storage, bucket, key, req, req_id, &etag),
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
            .header("ETag", etag)
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

    #[tokio::test(flavor = "multi_thread")]
    async fn should_copy_only_requested_range_when_copy_source_range_is_provided() {
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
            ("x-amz-copy-source", "source/source.txt"),
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
        assert_eq!(resp.status(), StatusCode::OK);

        let copied = storage.get_object("dest", "copied.txt").unwrap();
        assert_eq!(copied.data, b"cdef".to_vec());
        assert_eq!(copied.size, 4);
        assert_eq!(copied.content_type, "text/plain");
        assert_eq!(copied.metadata.get("owner"), Some(&"alice".to_string()));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn should_reject_invalid_copy_source_range_when_range_exceeds_source_size() {
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
        assert_eq!(resp.status(), StatusCode::RANGE_NOT_SATISFIABLE);

        let body = resp
            .into_body()
            .collect()
            .await
            .expect("body should read")
            .to_bytes();
        let body = String::from_utf8(body.to_vec()).expect("body should be utf8");
        assert!(body.contains("InvalidRange"));
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
            Some(etag.as_str())
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

    if req.has_query_param("versionId") {
        return Ok(delete_object_version_request(
            &storage, bucket, key, req, &req_id,
        ));
    }

    if req.has_query_param("tagging") {
        return Ok(delete_object_tagging(&storage, bucket, key, req, &req_id));
    }

    Ok(delete_current_object(&storage, bucket, key, req, &req_id))
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
    match tokio::task::block_in_place(|| {
        object_service::delete_object_version(storage.as_ref(), bucket, key, version_id)
    }) {
        Ok(()) | Err(crate::error::Error::KeyNotFound) => {
            let builder = ResponseBuilder::new(StatusCode::NO_CONTENT)
                .header("x-amz-request-id", req_id)
                .header("x-amz-id-2", &header_utils::generate_request_id())
                .header("x-amz-version-id", version_id);
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
    match tokio::task::block_in_place(|| {
        if let Ok(existing) = object_service::get_object(storage.as_ref(), bucket, key) {
            if object_is_locked(&existing) {
                return Err(crate::error::Error::AccessDenied);
            }
        }
        object_service::delete_object(storage.as_ref(), bucket, key)
    }) {
        Ok(()) | Err(crate::error::Error::KeyNotFound) => {
            no_content_object_response(storage.as_ref(), bucket, req, req_id)
        }
        Err(crate::error::Error::AccessDenied) => locked_object_response(req_id),
        Err(err) => internal_error_response(&err, req_id),
    }
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

    if let Some(version_id) = req.query_param("versionId") {
        match tokio::task::block_in_place(|| {
            object_service::get_object_version(storage.as_ref(), bucket, key, version_id)
        }) {
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
            Err(crate::error::Error::KeyNotFound) => {
                return Ok(xml_error_response(
                    StatusCode::NOT_FOUND,
                    "NoSuchKey",
                    "Key not found",
                    &req_id,
                ));
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
        Err(e) => {
            let xml = xml_utils::error_xml("NoSuchKey", &e.to_string(), &req_id);
            Ok(ResponseBuilder::new(StatusCode::NOT_FOUND)
                .content_type("application/xml; charset=utf-8")
                .header("x-amz-request-id", &req_id)
                .body(xml.into_bytes())
                .build())
        }
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
    async fn should_block_mutation_when_object_lock_headers_are_active() {
        let storage = temp_storage();
        storage.create_bucket("bucket".to_string()).unwrap();

        let put = request(
            "PUT",
            &[
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
        .expect("put should succeed");
        assert_eq!(put_response.status(), StatusCode::OK);

        let head = request("HEAD", &[], b"").await;
        let head_response = object_head(
            storage.clone(),
            auth_disabled_config(),
            "bucket",
            "governed.txt",
            &head,
            "req-lock-head".to_string(),
        )
        .await
        .expect("head should succeed");
        assert_eq!(
            head_response
                .headers()
                .get("x-amz-object-lock-mode")
                .and_then(|value| value.to_str().ok()),
            Some("GOVERNANCE")
        );

        let delete = request("DELETE", &[], b"").await;
        let delete_response = object_delete(
            storage.clone(),
            auth_disabled_config(),
            "bucket",
            "governed.txt",
            &delete,
            "req-lock-delete".to_string(),
        )
        .await
        .expect("delete should respond");
        assert_eq!(delete_response.status(), StatusCode::FORBIDDEN);
        let overwrite = request("PUT", &[], b"new payload").await;
        let overwrite_response = object_put(
            storage,
            auth_disabled_config(),
            "bucket",
            "governed.txt",
            &overwrite,
            "req-lock-overwrite".to_string(),
        )
        .await
        .expect("overwrite should respond");
        assert_eq!(overwrite_response.status(), StatusCode::FORBIDDEN);
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
        storage
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

        let matching = request_with_uri(
            "POST",
            &format!(
                "http://localhost/bucket/real.txt?uploadId={}",
                upload.upload_id
            ),
            &[],
            b"<CompleteMultipartUpload />",
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
}
