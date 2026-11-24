use super::helpers::{
    bucket_get_action, decode_list_objects_v2_token, encode_list_objects_v2_token, S3_CORS_XML_KEY,
    S3_OBJECT_LOCK_ENABLED_KEY, S3_REQUEST_PAYMENT_KEY, S3_VERSIONING_STATUS_KEY,
    S3_WEBSITE_XML_KEY,
};
use super::{
    bucket_service, check_authorization, cors, header_utils, object_service,
    s3_foreign_history_conflict, s3_foreign_history_conflict_response, xml_error_response,
    xml_utils, AuthConfig, Body, ResponseBuilder, Storage,
};
use crate::error::Error;
use crate::models::MultipartUpload;
use crate::server::http::Request;
use http::StatusCode;
use hyper::Response;
use std::collections::HashSet;
use std::sync::Arc;

enum MultipartUploadListingEntry {
    Upload(MultipartUpload),
    CommonPrefix(String),
}

impl MultipartUploadListingEntry {
    fn key(&self) -> &str {
        match self {
            Self::Upload(upload) => &upload.key,
            Self::CommonPrefix(prefix) => prefix,
        }
    }

    fn upload_id(&self) -> Option<&str> {
        match self {
            Self::Upload(upload) => Some(&upload.upload_id),
            Self::CommonPrefix(_) => None,
        }
    }
}

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
        return Ok(list_objects_v2(&storage, bucket, req, &req_id));
    }

    Ok(list_objects_v1(&storage, bucket, req, &req_id))
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

    if req.has_query_param("object-lock") {
        return Ok(Some(get_object_lock_configuration(
            storage, bucket, req, req_id,
        )));
    }

    if req.has_query_param("uploads") {
        return Ok(Some(list_multipart_uploads(storage, bucket, req, req_id)));
    }

    if req.has_query_param("versions") {
        return Ok(Some(list_object_versions(storage, bucket, req, req_id)));
    }

    Ok(None)
}

fn get_object_lock_configuration(
    storage: &Arc<dyn Storage>,
    bucket: &str,
    req: &Request,
    req_id: &str,
) -> Response<Body> {
    match tokio::task::block_in_place(|| bucket_service::get_bucket(storage.as_ref(), bucket)) {
        Ok(bucket_record)
            if bucket_record
                .metadata
                .get(S3_OBJECT_LOCK_ENABLED_KEY)
                .is_some_and(|value| value == "true") =>
        {
            let xml = format!(
                "{}\n<ObjectLockConfiguration xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\"><ObjectLockEnabled>Enabled</ObjectLockEnabled></ObjectLockConfiguration>",
                xml_utils::xml_declaration()
            );
            cors::apply_actual_request_headers(
                storage.as_ref(),
                bucket,
                req,
                ResponseBuilder::new(StatusCode::OK)
                    .content_type("application/xml; charset=utf-8")
                    .header("x-amz-request-id", req_id)
                    .header("x-amz-id-2", &header_utils::generate_request_id()),
            )
            .body(xml.into_bytes())
            .build()
        }
        Ok(_) => xml_error_response(
            StatusCode::NOT_FOUND,
            "ObjectLockConfigurationNotFoundError",
            "Object Lock configuration does not exist for this bucket.",
            req_id,
        ),
        Err(error) => crate::services::storage_error_response(&error, req_id),
    }
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
                .map_or("BucketOwner", std::string::String::as_str);
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
                .map_err(|error| format!("JSON serialization error: {error}"))?;
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
            let status = bucket_record
                .metadata
                .get(S3_VERSIONING_STATUS_KEY)
                .map(String::as_str)
                .filter(|status| matches!(*status, "Enabled" | "Suspended"));
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

#[allow(clippy::too_many_lines)]
fn list_multipart_uploads(
    storage: &Arc<dyn Storage>,
    bucket: &str,
    req: &Request,
    req_id: &str,
) -> Response<Body> {
    let prefix = req.query_param("prefix").unwrap_or("");
    let delimiter = req
        .query_param("delimiter")
        .filter(|value| !value.is_empty());
    let key_marker = req
        .query_param("key-marker")
        .filter(|value| !value.is_empty());
    let upload_id_marker = req
        .query_param("upload-id-marker")
        .filter(|value| !value.is_empty());
    if upload_id_marker.is_some() && key_marker.is_none() {
        return xml_error_response(
            StatusCode::BAD_REQUEST,
            "InvalidArgument",
            "upload-id-marker requires key-marker.",
            req_id,
        );
    }
    let max_uploads = match req.query_param("max-uploads") {
        Some(value) => match value.parse::<u32>() {
            Ok(value) => usize::try_from(value.min(1_000)).unwrap_or(1_000),
            Err(_) => {
                return xml_error_response(
                    StatusCode::BAD_REQUEST,
                    "InvalidArgument",
                    "max-uploads must be a non-negative integer.",
                    req_id,
                );
            }
        },
        None => 1_000,
    };
    let encoding_type = req.query_param("encoding-type");
    if encoding_type.is_some_and(|value| !value.eq_ignore_ascii_case("url")) {
        return xml_error_response(
            StatusCode::BAD_REQUEST,
            "InvalidArgument",
            "encoding-type must be url when provided.",
            req_id,
        );
    }

    match tokio::task::block_in_place(|| {
        bucket_service::list_multipart_uploads(storage.as_ref(), bucket)
    }) {
        Ok(uploads) => {
            let mut seen_prefixes = HashSet::new();
            let mut entries = uploads
                .into_iter()
                .filter(|upload| upload.key.starts_with(prefix))
                .filter_map(|upload| {
                    let Some(delimiter) = delimiter else {
                        return Some(MultipartUploadListingEntry::Upload(upload));
                    };
                    let remainder = &upload.key[prefix.len()..];
                    let Some(index) = remainder.find(delimiter) else {
                        return Some(MultipartUploadListingEntry::Upload(upload));
                    };
                    let common_prefix =
                        upload.key[..prefix.len() + index + delimiter.len()].to_string();
                    seen_prefixes
                        .insert(common_prefix.clone())
                        .then_some(MultipartUploadListingEntry::CommonPrefix(common_prefix))
                })
                .collect::<Vec<_>>();
            entries.sort_by(|left, right| {
                left.key()
                    .cmp(right.key())
                    .then_with(|| left.upload_id().cmp(&right.upload_id()))
            });
            entries.retain(|entry| match key_marker {
                None => true,
                Some(marker) if entry.key() > marker => true,
                Some(marker) if entry.key() == marker => upload_id_marker
                    .is_some_and(|id| entry.upload_id().is_some_and(|entry_id| entry_id > id)),
                Some(_) => false,
            });

            let is_truncated = entries.len() > max_uploads;
            entries.truncate(max_uploads);
            let next_key_marker = is_truncated
                .then(|| entries.last().map(|entry| entry.key().to_string()))
                .flatten();
            let next_upload_id_marker = is_truncated
                .then(|| {
                    entries
                        .last()
                        .and_then(MultipartUploadListingEntry::upload_id)
                        .map(str::to_string)
                })
                .flatten();
            let mut page_uploads = Vec::new();
            let mut common_prefixes = Vec::new();
            for entry in entries {
                match entry {
                    MultipartUploadListingEntry::Upload(upload) => page_uploads.push(upload),
                    MultipartUploadListingEntry::CommonPrefix(prefix) => {
                        common_prefixes.push(prefix);
                    }
                }
            }
            let xml = xml_utils::list_multipart_uploads_xml(&xml_utils::ListMultipartUploadsXml {
                uploads: &page_uploads,
                common_prefixes: &common_prefixes,
                bucket,
                key_marker,
                upload_id_marker,
                next_key_marker: next_key_marker.as_deref(),
                next_upload_id_marker: next_upload_id_marker.as_deref(),
                prefix,
                delimiter,
                encoding_type,
                max_uploads,
                is_truncated,
            });
            bucket_xml_response(storage.as_ref(), bucket, req, req_id, StatusCode::OK, xml)
        }
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
    if s3_foreign_history_conflict(storage.as_ref(), bucket) {
        return s3_foreign_history_conflict_response(req_id);
    }

    let prefix = req.query_param("prefix");
    let key_marker = req.query_param("key-marker");
    let version_id_marker = req.query_param("version-id-marker");
    let max_keys = match max_keys_or_error(req, req_id) {
        Ok(value) => value,
        Err(response) => return *response,
    };

    match tokio::task::block_in_place(|| {
        object_service::list_object_versions(storage.as_ref(), bucket, prefix)
    }) {
        Ok(mut versions) => {
            versions.sort_unstable_by(|left, right| {
                left.key
                    .cmp(&right.key)
                    .then_with(|| right.last_modified.cmp(&left.last_modified))
                    .then_with(|| left.version_id.cmp(&right.version_id))
            });

            let mut latest_key = None;
            for version in &mut versions {
                let is_latest = latest_key.as_deref() != Some(version.key.as_str());
                if is_latest {
                    latest_key = Some(version.key.clone());
                }
                version
                    .provider_metadata
                    .insert("s3_is_latest".to_string(), is_latest.to_string());
            }

            let start_index = match (key_marker, version_id_marker) {
                (None, Some(_)) => {
                    return xml_error_response(
                        StatusCode::BAD_REQUEST,
                        "InvalidArgument",
                        "A version-id-marker cannot be specified without a key-marker.",
                        req_id,
                    );
                }
                (Some(key), Some(version)) => versions
                    .iter()
                    .position(|item| item.key == key && item.version_id.as_deref() == Some(version))
                    .map_or_else(
                        || versions.partition_point(|item| item.key.as_str() <= key),
                        |index| index + 1,
                    ),
                (Some(key), None) => versions.partition_point(|item| item.key.as_str() <= key),
                (None, None) => 0,
            };
            let mut versions = versions.split_off(start_index.min(versions.len()));

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
    storage: &Arc<dyn Storage>,
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
    let continuation_marker = match continuation_token {
        Some(token) => match decode_list_objects_v2_token(token) {
            Some(marker) => Some(marker),
            None => {
                return xml_error_response(
                    StatusCode::BAD_REQUEST,
                    "InvalidArgument",
                    "The continuation token provided is incorrect.",
                    req_id,
                );
            }
        },
        None => None,
    };
    let start_after = req
        .query_param("start-after")
        .filter(|value| !value.is_empty());
    let max_keys = match max_keys_or_error(req, req_id) {
        Ok(value) => value,
        Err(response) => return *response,
    };
    let encoding_type = req.query_param("encoding-type");
    let fetch_owner = matches!(
        req.query_param("fetch-owner"),
        Some(value) if value.is_empty() || value.eq_ignore_ascii_case("true")
    );

    let marker = continuation_marker.as_deref().or(start_after);
    match tokio::task::block_in_place(|| {
        object_service::list_objects(
            storage.as_ref(),
            bucket,
            Some(prefix),
            delimiter,
            marker,
            Some(max_keys),
        )
    }) {
        Ok(result) => {
            let mut entries = result
                .objects
                .into_iter()
                .map(xml_utils::ListObjectsV2Entry::Object)
                .chain(
                    result
                        .common_prefixes
                        .into_iter()
                        .map(xml_utils::ListObjectsV2Entry::CommonPrefix),
                )
                .collect::<Vec<_>>();
            entries.sort_by(|left, right| left.token().cmp(right.token()));
            let next_continuation_token = result
                .is_truncated
                .then_some(result.next_marker)
                .flatten()
                .map(|marker| encode_list_objects_v2_token(&marker));

            let xml = xml_utils::list_objects_v2_xml(
                &entries,
                bucket,
                prefix,
                delimiter,
                max_keys,
                entries.len(),
                result.is_truncated,
                continuation_token,
                next_continuation_token.as_deref(),
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
    storage: &Arc<dyn Storage>,
    bucket: &str,
    req: &Request,
    req_id: &str,
) -> Response<Body> {
    let prefix = req.query_param("prefix");
    let delimiter = req.query_param("delimiter");
    let marker = req.query_param("marker");
    let max_keys = match max_keys_or_error(req, req_id) {
        Ok(value) => value,
        Err(response) => return *response,
    };

    match tokio::task::block_in_place(|| {
        object_service::list_objects(
            storage.as_ref(),
            bucket,
            prefix,
            delimiter,
            marker,
            Some(max_keys),
        )
    }) {
        Ok(mut result) => {
            result.objects.retain(|object| {
                !tokio::task::block_in_place(|| {
                    crate::lifecycle::check_object_expiration(storage, bucket, &object.key)
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
        Err(Error::BucketNotFound) => no_such_bucket_response(req_id),
        Err(error) => internal_error_response(&error, req_id),
    }
}

fn max_keys_or_error(req: &Request, req_id: &str) -> Result<usize, Box<Response<Body>>> {
    let Some(raw) = req.query_param("max-keys") else {
        return Ok(1_000);
    };
    raw.parse::<u32>()
        .map(|value| usize::try_from(value.min(1_000)).unwrap_or(1_000))
        .map_err(|_| {
            Box::new(xml_error_response(
                StatusCode::BAD_REQUEST,
                "InvalidArgument",
                "max-keys must be a non-negative integer.",
                req_id,
            ))
        })
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
