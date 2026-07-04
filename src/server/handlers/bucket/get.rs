use super::helpers::*;
use super::{
    bucket_service, check_authorization, cors, header_utils, object_service, xml_error_response,
    xml_utils, AuthConfig, Body, ResponseBuilder, Storage,
};
use crate::error::Error;
use crate::server::http::Request;
use http::StatusCode;
use hyper::Response;
use std::sync::Arc;

pub async fn bucket_get_or_list_objects(
    storage: Arc<dyn Storage>,
    auth_config: Arc<AuthConfig>,
    bucket: &str,
    req: &Request,
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
        None,
        bucket_get_action(req),
    ) {
        return Ok(response);
    }

    if let Some(response) = bucket_subresource_response(&storage, bucket, req, &req_id)? {
        return Ok(response);
    }

    if req.query_param("list-type") == Some("2") {
        return Ok(list_objects_v2(storage, bucket, req, &req_id));
    }

    Ok(list_objects_v1(storage, bucket, req, &req_id))
}

fn bucket_subresource_response(
    storage: &Arc<dyn Storage>,
    bucket: &str,
    req: &Request,
    req_id: &str,
) -> Result<Option<Response<Body>>, String> {
    if req.has_query_param("requestPayment") {
        return Ok(Some(get_request_payment(storage, bucket, req, req_id)));
    }

    if req.has_query_param("website") {
        return Ok(Some(get_bucket_metadata_xml(
            storage,
            bucket,
            req,
            req_id,
            S3_WEBSITE_XML_KEY,
            "NoSuchWebsiteConfiguration",
            "The specified bucket does not have a website configuration",
        )));
    }

    if req.has_query_param("cors") {
        return Ok(Some(get_bucket_metadata_xml(
            storage,
            bucket,
            req,
            req_id,
            S3_CORS_XML_KEY,
            "NoSuchCORSConfiguration",
            "The CORS configuration does not exist",
        )));
    }

    if req.has_query_param("lifecycle") {
        return Ok(Some(get_lifecycle(storage, bucket, req, req_id)));
    }

    if req.has_query_param("policy") {
        return Ok(Some(get_policy(storage, bucket, req, req_id)?));
    }

    if req.has_query_param("acl") {
        return Ok(Some(get_acl(storage, bucket, req, req_id)));
    }

    if req.has_query_param("versioning") {
        return Ok(Some(get_versioning(storage, bucket, req, req_id)));
    }

    if req.has_query_param("uploads") {
        return Ok(Some(list_multipart_uploads(storage, bucket, req, req_id)));
    }

    if req.has_query_param("versions") {
        return Ok(Some(list_object_versions(storage, bucket, req, req_id)));
    }

    Ok(None)
}

fn get_request_payment(
    storage: &Arc<dyn Storage>,
    bucket: &str,
    req: &Request,
    req_id: &str,
) -> Response<Body> {
    match tokio::task::block_in_place(|| bucket_service::get_bucket(storage.as_ref(), bucket)) {
        Ok(bucket_record) => {
            let payer = bucket_record
                .metadata
                .get(S3_REQUEST_PAYMENT_KEY)
                .map(|value| value.as_str())
                .unwrap_or("BucketOwner");
            let xml = format!(
                "{}\n<RequestPaymentConfiguration xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\">\n  <Payer>{}</Payer>\n</RequestPaymentConfiguration>",
                xml_utils::xml_declaration(),
                payer
            );
            bucket_xml_response(storage.as_ref(), bucket, req, req_id, StatusCode::OK, xml)
        }
        Err(error) => bucket_not_found_or_internal_error(error, req_id),
    }
}

fn get_bucket_metadata_xml(
    storage: &Arc<dyn Storage>,
    bucket: &str,
    req: &Request,
    req_id: &str,
    metadata_key: &str,
    missing_code: &str,
    missing_message: &str,
) -> Response<Body> {
    match tokio::task::block_in_place(|| bucket_service::get_bucket(storage.as_ref(), bucket)) {
        Ok(bucket_record) => match bucket_record.metadata.get(metadata_key) {
            Some(xml) => bucket_xml_response(
                storage.as_ref(),
                bucket,
                req,
                req_id,
                StatusCode::OK,
                xml.clone(),
            ),
            None => {
                xml_error_response(StatusCode::NOT_FOUND, missing_code, missing_message, req_id)
            }
        },
        Err(error) => bucket_not_found_or_internal_error(error, req_id),
    }
}

fn get_lifecycle(
    storage: &Arc<dyn Storage>,
    bucket: &str,
    req: &Request,
    req_id: &str,
) -> Response<Body> {
    match tokio::task::block_in_place(|| {
        bucket_service::get_bucket_lifecycle(storage.as_ref(), bucket)
    }) {
        Ok(config) => bucket_xml_response(
            storage.as_ref(),
            bucket,
            req,
            req_id,
            StatusCode::OK,
            xml_utils::lifecycle_xml(&config),
        ),
        Err(Error::BucketNotFound) => no_such_bucket_response(req_id),
        Err(Error::KeyNotFound) => xml_error_response(
            StatusCode::NOT_FOUND,
            "NoSuchLifecycleConfiguration",
            "No lifecycle configuration present",
            req_id,
        ),
        Err(error) => internal_error_response(&error, req_id),
    }
}

fn get_policy(
    storage: &Arc<dyn Storage>,
    bucket: &str,
    req: &Request,
    req_id: &str,
) -> Result<Response<Body>, String> {
    match tokio::task::block_in_place(|| {
        bucket_service::get_bucket_policy(storage.as_ref(), bucket)
    }) {
        Ok(policy) => {
            let json = serde_json::to_string(&policy)
                .map_err(|error| format!("JSON serialization error: {}", error))?;
            Ok(bucket_body_response(
                storage.as_ref(),
                bucket,
                req,
                req_id,
                StatusCode::OK,
                "application/json; charset=utf-8",
                json.into_bytes(),
            ))
        }
        Err(Error::BucketNotFound) => Ok(no_such_bucket_response(req_id)),
        Err(Error::KeyNotFound) => Ok(xml_error_response(
            StatusCode::NOT_FOUND,
            "NoSuchBucketPolicy",
            "The bucket policy does not exist",
            req_id,
        )),
        Err(error) => Ok(internal_error_response(&error, req_id)),
    }
}

fn get_acl(
    storage: &Arc<dyn Storage>,
    bucket: &str,
    req: &Request,
    req_id: &str,
) -> Response<Body> {
    match tokio::task::block_in_place(|| bucket_service::get_bucket_acl(storage.as_ref(), bucket)) {
        Ok(acl) => {
            let owner = crate::models::policy::Owner {
                id: "sqrzl-emulator".to_string(),
                display_name: "S3 Emulator".to_string(),
            };
            bucket_xml_response(
                storage.as_ref(),
                bucket,
                req,
                req_id,
                StatusCode::OK,
                xml_utils::acl_xml(&owner, &acl),
            )
        }
        Err(error) => bucket_not_found_or_internal_error(error, req_id),
    }
}

fn get_versioning(
    storage: &Arc<dyn Storage>,
    bucket: &str,
    req: &Request,
    req_id: &str,
) -> Response<Body> {
    match tokio::task::block_in_place(|| bucket_service::get_bucket(storage.as_ref(), bucket)) {
        Ok(bucket_record) => {
            let status = if bucket_record.versioning_enabled {
                Some("Enabled")
            } else {
                Some("Suspended")
            };
            bucket_xml_response(
                storage.as_ref(),
                bucket,
                req,
                req_id,
                StatusCode::OK,
                xml_utils::versioning_status_xml(status),
            )
        }
        Err(error) => bucket_not_found_or_internal_error(error, req_id),
    }
}

fn list_multipart_uploads(
    storage: &Arc<dyn Storage>,
    bucket: &str,
    req: &Request,
    req_id: &str,
) -> Response<Body> {
    match tokio::task::block_in_place(|| {
        bucket_service::list_multipart_uploads(storage.as_ref(), bucket)
    }) {
        Ok(uploads) => bucket_xml_response(
            storage.as_ref(),
            bucket,
            req,
            req_id,
            StatusCode::OK,
            xml_utils::list_multipart_uploads_xml(&uploads, bucket),
        ),
        Err(Error::BucketNotFound) => no_such_bucket_response(req_id),
        Err(Error::NoSuchUpload) => xml_error_response(
            StatusCode::NOT_FOUND,
            "NoSuchUpload",
            "Upload not found",
            req_id,
        ),
        Err(error) => internal_error_response(&error, req_id),
    }
}

fn list_object_versions(
    storage: &Arc<dyn Storage>,
    bucket: &str,
    req: &Request,
    req_id: &str,
) -> Response<Body> {
    let prefix = req.query_param("prefix");
    let key_marker = req.query_param("key-marker");
    let version_id_marker = req.query_param("version-id-marker");
    let max_keys = max_keys_or_default(req);

    match tokio::task::block_in_place(|| {
        object_service::list_object_versions(storage.as_ref(), bucket, prefix)
    }) {
        Ok(mut versions) => {
            versions.sort_unstable_by(|left, right| {
                right
                    .last_modified
                    .cmp(&left.last_modified)
                    .then_with(|| left.key.cmp(&right.key))
                    .then_with(|| left.version_id.cmp(&right.version_id))
            });

            let truncated = versions.len() > max_keys;
            if truncated {
                versions.truncate(max_keys);
            }

            let next_key_marker = truncated
                .then(|| versions.last().map(|version| version.key.as_str()))
                .flatten();
            let next_version_id_marker = truncated
                .then(|| {
                    versions
                        .last()
                        .and_then(|version| version.version_id.as_deref())
                })
                .flatten();

            let xml = xml_utils::list_versions_xml(
                bucket,
                &versions,
                prefix.unwrap_or(""),
                key_marker,
                version_id_marker,
                max_keys,
                truncated,
                next_key_marker,
                next_version_id_marker,
            );
            bucket_xml_response(storage.as_ref(), bucket, req, req_id, StatusCode::OK, xml)
        }
        Err(Error::BucketNotFound) => no_such_bucket_response(req_id),
        Err(error) => internal_error_response(&error, req_id),
    }
}

fn list_objects_v2(
    storage: Arc<dyn Storage>,
    bucket: &str,
    req: &Request,
    req_id: &str,
) -> Response<Body> {
    let prefix = req.query_param("prefix").unwrap_or("");
    let delimiter = req
        .query_param("delimiter")
        .filter(|value| !value.is_empty());
    let continuation_token = req
        .query_param("continuation-token")
        .filter(|value| !value.is_empty());
    let start_after = req
        .query_param("start-after")
        .filter(|value| !value.is_empty());
    let max_keys = max_keys_or_default(req);
    let encoding_type = req.query_param("encoding-type");
    let fetch_owner = matches!(
        req.query_param("fetch-owner"),
        Some(value) if value.is_empty() || value.eq_ignore_ascii_case("true")
    );

    match tokio::task::block_in_place(|| {
        object_service::list_objects(storage.as_ref(), bucket, Some(prefix), None, None, None)
    }) {
        Ok(result) => {
            let entries = build_list_objects_v2_entries(result.objects, prefix, delimiter);
            let start_index =
                list_objects_v2_start_index(&entries, continuation_token, start_after);
            let page_end = (start_index.saturating_add(max_keys)).min(entries.len());
            let page_entries = entries.get(start_index..page_end).unwrap_or_default();
            let truncated = page_end < entries.len();
            let next_continuation_token =
                next_continuation_token(&entries, page_entries, start_index, page_end, truncated);

            let xml = xml_utils::list_objects_v2_xml(
                page_entries,
                bucket,
                prefix,
                delimiter,
                max_keys,
                page_entries.len(),
                truncated,
                continuation_token,
                next_continuation_token,
                start_after,
                encoding_type,
                fetch_owner,
            );
            bucket_xml_response(storage.as_ref(), bucket, req, req_id, StatusCode::OK, xml)
        }
        Err(Error::BucketNotFound) => no_such_bucket_response(req_id),
        Err(error) => internal_error_response(&error, req_id),
    }
}

fn list_objects_v1(
    storage: Arc<dyn Storage>,
    bucket: &str,
    req: &Request,
    req_id: &str,
) -> Response<Body> {
    let prefix = req.query_param("prefix");
    let delimiter = req.query_param("delimiter");
    let marker = req.query_param("marker");
    let max_keys = req
        .query_param("max-keys")
        .and_then(|value| value.parse::<usize>().ok());

    match tokio::task::block_in_place(|| {
        object_service::list_objects(
            storage.as_ref(),
            bucket,
            prefix,
            delimiter,
            marker,
            max_keys,
        )
    }) {
        Ok(mut result) => {
            result.objects.retain(|object| {
                !tokio::task::block_in_place(|| {
                    crate::lifecycle::check_object_expiration(&storage, bucket, &object.key)
                })
                .unwrap_or(false)
            });

            let xml = xml_utils::list_objects_xml(
                &result.objects,
                &result.common_prefixes,
                bucket,
                prefix.unwrap_or(""),
                delimiter,
                marker,
                result.objects.len(),
                result.is_truncated,
                result.next_marker.as_deref(),
            );
            bucket_xml_response(storage.as_ref(), bucket, req, req_id, StatusCode::OK, xml)
        }
        Err(error) => internal_error_response(&error, req_id),
    }
}

fn max_keys_or_default(req: &Request) -> usize {
    req.query_param("max-keys")
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(1000)
}

fn next_continuation_token<'a>(
    entries: &'a [xml_utils::ListObjectsV2Entry],
    page_entries: &'a [xml_utils::ListObjectsV2Entry],
    start_index: usize,
    page_end: usize,
    truncated: bool,
) -> Option<&'a str> {
    if !truncated {
        return None;
    }

    if page_end > start_index {
        return page_entries.last().map(|entry| entry.token());
    }

    entries.get(start_index).map(|entry| entry.token())
}

fn bucket_xml_response(
    storage: &dyn Storage,
    bucket: &str,
    req: &Request,
    req_id: &str,
    status: StatusCode,
    xml: String,
) -> Response<Body> {
    bucket_body_response(
        storage,
        bucket,
        req,
        req_id,
        status,
        "application/xml; charset=utf-8",
        xml.into_bytes(),
    )
}

fn bucket_body_response(
    storage: &dyn Storage,
    bucket: &str,
    req: &Request,
    req_id: &str,
    status: StatusCode,
    content_type: &str,
    body: Vec<u8>,
) -> Response<Body> {
    cors::apply_actual_request_headers(
        storage,
        bucket,
        req,
        ResponseBuilder::new(status)
            .content_type(content_type)
            .header("x-amz-request-id", req_id)
            .header("x-amz-id-2", &header_utils::generate_request_id()),
    )
    .body(body)
    .build()
}

fn bucket_not_found_or_internal_error(error: Error, req_id: &str) -> Response<Body> {
    match error {
        Error::BucketNotFound => no_such_bucket_response(req_id),
        error => internal_error_response(&error, req_id),
    }
}

fn no_such_bucket_response(req_id: &str) -> Response<Body> {
    xml_error_response(
        StatusCode::NOT_FOUND,
        "NoSuchBucket",
        "Bucket not found",
        req_id,
    )
}

fn internal_error_response(error: &Error, req_id: &str) -> Response<Body> {
    xml_error_response(
        StatusCode::INTERNAL_SERVER_ERROR,
        "InternalError",
        &error.to_string(),
        req_id,
    )
}
