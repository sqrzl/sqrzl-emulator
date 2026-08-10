use super::acl;
use super::auth::check_authorization;
use super::cors;
use super::{
    foreign_data_protection_mode_active, s3_foreign_history_conflict,
    s3_foreign_history_conflict_response, ResponseBuilder,
};
use crate::auth::AuthConfig;
use crate::body::Body;
use crate::services::{
    bucket as bucket_service, object as object_service, storage_error_response, xml_error_response,
    xml_success_response,
};
use crate::storage::{ObjectCondition, Storage};
use crate::utils::{headers as header_utils, validation, xml as xml_utils};
use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use http::StatusCode;
use hyper::Response;
use std::fmt::Write as _;
use std::sync::Arc;

mod get;
mod helpers;
mod put;

use self::helpers::{
    apply_bucket_cors_headers, bucket_cors_snapshot, escape_xml_str, parse_delete_keys,
    parse_multipart_form_upload, with_bucket_metadata, DeleteObjectEntry, S3_CORS_XML_KEY,
    S3_WEBSITE_XML_KEY,
};
pub use get::bucket_get_or_list_objects;
pub use put::bucket_put;

pub async fn list_buckets(
    storage: Arc<dyn Storage>,
    auth_config: Arc<AuthConfig>,
    req: crate::server::http::Request,
    req_id: String,
) -> Result<Response<Body>, String> {
    if let Err(response) = check_authorization(
        &req,
        &auth_config,
        &storage,
        "*",
        None,
        "s3:ListAllMyBuckets",
    ) {
        return Ok(response);
    }

    let buckets = tokio::task::block_in_place(|| bucket_service::list_buckets(storage.as_ref()))?;
    let xml = xml_utils::list_buckets_xml(&buckets);

    Ok(xml_success_response(StatusCode::OK, xml, &req_id))
}

pub async fn bucket_head(
    storage: Arc<dyn Storage>,
    auth_config: Arc<AuthConfig>,
    bucket: &str,
    req: &crate::server::http::Request,
    req_id: String,
) -> Result<Response<Body>, String> {
    if let Err(response) =
        check_authorization(req, &auth_config, &storage, bucket, None, "s3:ListBucket")
    {
        return Ok(response);
    }

    match tokio::task::block_in_place(|| bucket_service::get_bucket(storage.as_ref(), bucket)) {
        Ok(_) => Ok(cors::apply_actual_request_headers(
            storage.as_ref(),
            bucket,
            req,
            ResponseBuilder::new(StatusCode::OK)
                .header("x-amz-request-id", &req_id)
                .header("x-amz-id-2", &header_utils::generate_request_id()),
        )
        .empty()),
        Err(e) => Ok(storage_error_response(&e, &req_id)),
    }
}

pub async fn bucket_delete(
    storage: Arc<dyn Storage>,
    auth_config: Arc<AuthConfig>,
    bucket: &str,
    req: &crate::server::http::Request,
    req_id: String,
) -> Result<Response<Body>, String> {
    let cors_snapshot = if req.has_query_param("cors")
        || !req.has_query_param("website")
            && !req.has_query_param("lifecycle")
            && !req.has_query_param("policy")
    {
        bucket_cors_snapshot(storage.as_ref(), bucket)
    } else {
        None
    };

    let action = if req.has_query_param("lifecycle") {
        "s3:DeleteLifecycleConfiguration"
    } else if req.has_query_param("policy") {
        "s3:DeleteBucketPolicy"
    } else if req.has_query_param("website") {
        "s3:DeleteBucketWebsite"
    } else if req.has_query_param("cors") {
        "s3:DeleteBucketCors"
    } else {
        "s3:DeleteBucket"
    };

    if let Err(response) = check_authorization(req, &auth_config, &storage, bucket, None, action) {
        return Ok(response);
    }

    if req.has_query_param("website") {
        let result = tokio::task::block_in_place(|| {
            with_bucket_metadata(storage.as_ref(), bucket, |metadata| {
                metadata.remove(S3_WEBSITE_XML_KEY);
            })
        });
        Ok(bucket_delete_response(
            result,
            storage.as_ref(),
            bucket,
            req,
            &req_id,
            None,
        ))
    } else if req.has_query_param("cors") {
        let result = tokio::task::block_in_place(|| {
            with_bucket_metadata(storage.as_ref(), bucket, |metadata| {
                metadata.remove(S3_CORS_XML_KEY);
            })
        });
        Ok(bucket_delete_response(
            result,
            storage.as_ref(),
            bucket,
            req,
            &req_id,
            cors_snapshot.as_deref(),
        ))
    } else if req.has_query_param("lifecycle") {
        let result = tokio::task::block_in_place(|| {
            bucket_service::delete_bucket_lifecycle(storage.as_ref(), bucket)
        });
        Ok(bucket_delete_response(
            result,
            storage.as_ref(),
            bucket,
            req,
            &req_id,
            None,
        ))
    } else if req.has_query_param("policy") {
        let result = tokio::task::block_in_place(|| {
            bucket_service::delete_bucket_policy(storage.as_ref(), bucket)
        });
        Ok(bucket_delete_response(
            result,
            storage.as_ref(),
            bucket,
            req,
            &req_id,
            None,
        ))
    } else if req.has_query_param("versioning") || req.has_query_param("acl") {
        Ok(xml_error_response(
            StatusCode::BAD_REQUEST,
            "InvalidRequest",
            "Cannot delete versioning or ACL via DELETE",
            &req_id,
        ))
    } else {
        if s3_foreign_history_conflict(storage.as_ref(), bucket) {
            return Ok(s3_foreign_history_conflict_response(&req_id));
        }
        tokio::task::block_in_place(|| bucket_service::delete_bucket(storage.as_ref(), bucket))?;
        Ok(apply_bucket_cors_headers(
            storage.as_ref(),
            bucket,
            req,
            ResponseBuilder::new(StatusCode::NO_CONTENT)
                .header("x-amz-request-id", &req_id)
                .header("x-amz-id-2", &header_utils::generate_request_id()),
            cors_snapshot.as_deref(),
        )
        .empty())
    }
}

fn bucket_delete_response<T>(
    result: crate::error::Result<T>,
    storage: &dyn Storage,
    bucket: &str,
    req: &crate::server::http::Request,
    req_id: &str,
    cors_snapshot: Option<&str>,
) -> Response<Body> {
    match result {
        Ok(_) => apply_bucket_cors_headers(
            storage,
            bucket,
            req,
            ResponseBuilder::new(StatusCode::NO_CONTENT)
                .header("x-amz-request-id", req_id)
                .header("x-amz-id-2", &header_utils::generate_request_id()),
            cors_snapshot,
        )
        .empty(),
        Err(e) => storage_error_response(&e, req_id),
    }
}

pub async fn bucket_post(
    storage: Arc<dyn Storage>,
    auth_config: Arc<AuthConfig>,
    bucket: &str,
    req: &crate::server::http::Request,
    req_id: String,
) -> Result<Response<Body>, String> {
    if req.has_query_param("delete") {
        return bucket_post_multi_object_delete(&storage, &auth_config, bucket, req, &req_id);
    }

    if let Some(content_type) = req
        .header("content-type")
        .filter(|content_type| content_type.starts_with("multipart/form-data"))
    {
        return bucket_post_browser_upload(
            &storage,
            &auth_config,
            bucket,
            req,
            &req_id,
            content_type,
        );
    }

    Ok(xml_error_response(
        StatusCode::NOT_IMPLEMENTED,
        "NotImplemented",
        "Bucket POST operations not yet implemented",
        &req_id,
    ))
}

fn bucket_post_multi_object_delete(
    storage: &Arc<dyn Storage>,
    auth_config: &Arc<AuthConfig>,
    bucket: &str,
    req: &crate::server::http::Request,
    req_id: &str,
) -> Result<Response<Body>, String> {
    if !tokio::task::block_in_place(|| bucket_service::bucket_exists(storage.as_ref(), bucket))? {
        return Ok(xml_error_response(
            StatusCode::NOT_FOUND,
            "NoSuchBucket",
            "Bucket not found",
            req_id,
        ));
    }

    let objects = match multi_delete_request_objects(req, req_id) {
        Ok(objects) => objects,
        Err(response) => return Ok(*response),
    };
    if let Some(response) =
        multi_delete_authorization_error(storage, auth_config, bucket, req, &objects)
    {
        return Ok(response);
    }

    if s3_foreign_history_conflict(storage.as_ref(), bucket) {
        return Ok(s3_foreign_history_conflict_response(req_id));
    }

    let outcomes = objects
        .into_iter()
        .map(|(key, version, etag)| {
            let result =
                multi_delete_object(storage, bucket, &key, version.as_deref(), etag.as_deref());
            (key, version, result.err())
        })
        .collect::<Vec<_>>();
    let resp_xml = multi_delete_result_xml(outcomes);

    Ok(apply_bucket_cors_headers(
        storage.as_ref(),
        bucket,
        req,
        ResponseBuilder::new(StatusCode::OK)
            .content_type("application/xml; charset=utf-8")
            .header("x-amz-request-id", req_id)
            .header("x-amz-id-2", &header_utils::generate_request_id())
            .body(resp_xml.into_bytes()),
        None,
    )
    .build())
}

fn multi_delete_request_objects(
    req: &crate::server::http::Request,
    req_id: &str,
) -> Result<Vec<DeleteObjectEntry>, Box<Response<Body>>> {
    let Some(content_md5) = req.header("content-md5") else {
        return Err(Box::new(xml_error_response(
            StatusCode::BAD_REQUEST,
            "MissingContentMD5",
            "The Content-MD5 header is required for DeleteObjects requests.",
            req_id,
        )));
    };
    let Ok(supplied_md5) = BASE64.decode(content_md5) else {
        return Err(Box::new(xml_error_response(
            StatusCode::BAD_REQUEST,
            "InvalidDigest",
            "The Content-MD5 value is not a valid MD5 digest.",
            req_id,
        )));
    };
    if supplied_md5.len() != 16 {
        return Err(Box::new(xml_error_response(
            StatusCode::BAD_REQUEST,
            "InvalidDigest",
            "The Content-MD5 value is not a valid MD5 digest.",
            req_id,
        )));
    }
    if supplied_md5.as_slice() != md5::compute(&req.body).0 {
        return Err(Box::new(xml_error_response(
            StatusCode::BAD_REQUEST,
            "BadDigest",
            "The Content-MD5 value does not match the request body.",
            req_id,
        )));
    }

    String::from_utf8(req.body.to_vec())
        .map_err(|error| error.to_string())
        .and_then(|body| parse_delete_keys(&body))
        .map_err(|message| {
            Box::new(xml_error_response(
                StatusCode::OK,
                "MalformedXML",
                &message,
                req_id,
            ))
        })
}

fn multi_delete_authorization_error(
    storage: &Arc<dyn Storage>,
    auth_config: &Arc<AuthConfig>,
    bucket: &str,
    req: &crate::server::http::Request,
    objects: &[DeleteObjectEntry],
) -> Option<Response<Body>> {
    objects.iter().find_map(|(key, _, _)| {
        check_authorization(
            req,
            auth_config,
            storage,
            bucket,
            Some(key),
            "s3:DeleteObject",
        )
        .err()
    })
}

type MultiDeleteOutcome = (String, Option<String>, Option<MultiDeleteFailure>);

fn multi_delete_result_xml(outcomes: Vec<MultiDeleteOutcome>) -> String {
    let mut xml = xml_utils::xml_declaration();
    xml.push_str("<DeleteResult xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\">");
    for (key, version, failure) in outcomes {
        let element = if failure.is_some() {
            "Error"
        } else {
            "Deleted"
        };
        let _ = write!(xml, "<{element}><Key>{}</Key>", escape_xml_str(&key));
        if let Some(version) = version {
            let _ = write!(xml, "<VersionId>{}</VersionId>", escape_xml_str(&version));
        }
        if let Some(failure) = failure {
            let _ = write!(
                xml,
                "<Code>{}</Code><Message>{}</Message>",
                failure.code,
                escape_xml_str(&failure.message)
            );
        }
        let _ = write!(xml, "</{element}>");
    }
    xml.push_str("</DeleteResult>");
    xml
}

struct MultiDeleteFailure {
    code: &'static str,
    message: String,
}

fn multi_delete_object(
    storage: &Arc<dyn Storage>,
    bucket: &str,
    key: &str,
    version: Option<&str>,
    etag: Option<&str>,
) -> Result<(), MultiDeleteFailure> {
    if let Some(version) = version {
        let target = match storage.get_object_version(bucket, key, version) {
            Ok(target) => target,
            Err(crate::error::Error::KeyNotFound | crate::error::Error::NoSuchVersion)
                if etag.is_none() =>
            {
                return Ok(());
            }
            Err(error) => return Err(multi_delete_storage_failure(&error)),
        };
        if etag.is_some_and(|etag| !multi_delete_etag_matches(etag, &target.etag)) {
            return Err(MultiDeleteFailure {
                code: "PreconditionFailed",
                message: "At least one of the pre-conditions you specified did not hold"
                    .to_string(),
            });
        }
        if multi_delete_object_is_locked(&target) {
            return Err(MultiDeleteFailure {
                code: "AccessDenied",
                message: "Object is protected by an active retention policy or legal hold"
                    .to_string(),
            });
        }
        return object_service::delete_object_version(storage.as_ref(), bucket, key, version)
            .or_else(|error| match error {
                crate::error::Error::KeyNotFound | crate::error::Error::NoSuchVersion
                    if etag.is_none() =>
                {
                    Ok(())
                }
                other => Err(other),
            })
            .map_err(|error| multi_delete_storage_failure(&error));
    }

    let current = storage.get_object(bucket, key);
    if let Ok(current) = current.as_ref() {
        if multi_delete_object_is_locked(current)
            && !multi_delete_preserves_current_version(storage, bucket, current)
        {
            return Err(MultiDeleteFailure {
                code: "AccessDenied",
                message: "Object is protected by an active retention policy or legal hold"
                    .to_string(),
            });
        }
        if let Some(etag) = etag {
            if !multi_delete_etag_matches(etag, &current.etag) {
                return Err(MultiDeleteFailure {
                    code: "PreconditionFailed",
                    message: "At least one of the pre-conditions you specified did not hold"
                        .to_string(),
                });
            }
            return match storage.delete_object_if(
                bucket,
                key,
                &ObjectCondition::Etag(current.etag.clone()),
            ) {
                Ok(true) => Ok(()),
                Ok(false) => Err(MultiDeleteFailure {
                    code: "Conflict",
                    message: "A conflicting mutation changed the object.".to_string(),
                }),
                Err(error) => Err(multi_delete_storage_failure(&error)),
            };
        }
    } else if etag.is_some() {
        return Err(multi_delete_storage_failure(
            &crate::error::Error::KeyNotFound,
        ));
    }

    object_service::delete_object(storage.as_ref(), bucket, key)
        .or_else(|error| match error {
            crate::error::Error::KeyNotFound => Ok(()),
            other => Err(other),
        })
        .map_err(|error| multi_delete_storage_failure(&error))
}

fn multi_delete_etag_matches(expected: &str, actual: &str) -> bool {
    let expected = expected.trim();
    expected == "*"
        || (!expected.starts_with("W/") && expected.trim_matches('"') == actual.trim_matches('"'))
}

fn multi_delete_object_is_locked(object: &crate::models::Object) -> bool {
    object
        .provider_metadata
        .get("s3_object_lock_legal_hold")
        .is_some_and(|value| value.eq_ignore_ascii_case("ON"))
        || object
            .provider_metadata
            .get("s3_object_lock_until")
            .and_then(|value| {
                chrono::DateTime::parse_from_rfc3339(value)
                    .or_else(|_| chrono::DateTime::parse_from_rfc2822(value))
                    .ok()
            })
            .is_some_and(|value| value.with_timezone(&chrono::Utc) > chrono::Utc::now())
}

fn multi_delete_preserves_current_version(
    storage: &Arc<dyn Storage>,
    bucket: &str,
    current: &crate::models::Object,
) -> bool {
    let Ok(bucket) = storage.get_bucket(bucket) else {
        return false;
    };
    bucket.versioning_enabled
        || (bucket
            .metadata
            .get("s3_versioning_status")
            .is_some_and(|status| status == "Suspended")
            && current.version_id.as_deref() != Some("null"))
}

fn multi_delete_storage_failure(error: &crate::error::Error) -> MultiDeleteFailure {
    MultiDeleteFailure {
        code: error.error_code(),
        message: error.to_string(),
    }
}

fn bucket_post_browser_upload(
    storage: &Arc<dyn Storage>,
    auth_config: &Arc<AuthConfig>,
    bucket: &str,
    req: &crate::server::http::Request,
    req_id: &str,
    content_type: &str,
) -> Result<Response<Body>, String> {
    if !tokio::task::block_in_place(|| bucket_service::bucket_exists(storage.as_ref(), bucket))? {
        return Ok(xml_error_response(
            StatusCode::NOT_FOUND,
            "NoSuchBucket",
            "Bucket not found",
            req_id,
        ));
    }

    if let Some((key, data, file_content_type)) =
        parse_multipart_form_upload(content_type, &req.body)
    {
        if let Err(response) = check_authorization(
            req,
            auth_config,
            storage,
            bucket,
            Some(key.as_str()),
            "s3:PutObject",
        ) {
            return Ok(response);
        }

        if s3_foreign_history_conflict(storage.as_ref(), bucket) {
            return Ok(s3_foreign_history_conflict_response(req_id));
        }

        let object = crate::models::Object::new(key.clone(), data, file_content_type);
        tokio::task::block_in_place(|| {
            object_service::put_object(storage.as_ref(), bucket, key.clone(), object)
        })?;

        return Ok(cors::apply_actual_request_headers(
            storage.as_ref(),
            bucket,
            req,
            ResponseBuilder::new(StatusCode::NO_CONTENT)
                .header("Location", &format!("/{bucket}/{key}"))
                .header("x-amz-request-id", req_id)
                .header("x-amz-id-2", &header_utils::generate_request_id()),
        )
        .empty());
    }

    Ok(xml_error_response(
        StatusCode::BAD_REQUEST,
        "InvalidRequest",
        "Unable to parse multipart form upload",
        req_id,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::body::Body;
    use crate::config::Config;
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

    fn temp_storage() -> Arc<dyn Storage> {
        let dir =
            std::env::temp_dir().join(format!("sqrzl-list-versions-test-{}", uuid::Uuid::new_v4()));
        let _ = fs::create_dir_all(&dir);
        Arc::new(FilesystemStorage::new(dir))
    }

    fn auth_disabled_config() -> Arc<AuthConfig> {
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

    fn auth_enabled_config() -> Arc<AuthConfig> {
        Arc::new(Config {
            access_key_id: Some("test-access-key".to_string()),
            secret_access_key: Some("test-secret-key".to_string()),
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

    async fn parsed_request(uri: &str) -> RequestExt {
        let request = HyperRequest::builder()
            .method("GET")
            .uri(uri)
            .body(Body::from(Bytes::new()))
            .expect("request should build");

        RequestExt::from_hyper(request)
            .await
            .expect("request should parse")
    }

    async fn parsed_request_with_method(method: &str, uri: &str, body: &[u8]) -> RequestExt {
        let mut builder = HyperRequest::builder().method(method).uri(uri);
        if method == "POST" && uri.contains("?delete") {
            builder = builder.header("content-md5", BASE64.encode(md5::compute(body).0));
        }
        let request = builder
            .body(Body::from(body.to_vec()))
            .expect("request should build");

        RequestExt::from_hyper(request)
            .await
            .expect("request should parse")
    }

    async fn parsed_request_with_origin(method: &str, uri: &str, body: &[u8]) -> RequestExt {
        let mut builder = HyperRequest::builder()
            .method(method)
            .uri(uri)
            .header("Origin", "https://app.example");
        if method == "POST" && uri.contains("?delete") {
            builder = builder.header("content-md5", BASE64.encode(md5::compute(body).0));
        }
        let request = builder
            .body(Body::from(body.to_vec()))
            .expect("request should build");

        RequestExt::from_hyper(request)
            .await
            .expect("request should parse")
    }

    async fn response_text(response: Response<Body>) -> String {
        let body = response
            .into_body()
            .collect()
            .await
            .expect("body should read")
            .to_bytes();
        String::from_utf8(body.to_vec()).expect("body should be utf8")
    }

    fn xml_tag(body: &str, tag: &str) -> Option<String> {
        let start = format!("<{tag}>");
        let end = format!("</{tag}>");
        let value = body.split_once(&start)?.1.split_once(&end)?.0;
        Some(value.to_string())
    }

    fn cors_origin(response: &Response<Body>) -> Option<&str> {
        response
            .headers()
            .get("Access-Control-Allow-Origin")
            .and_then(|value| value.to_str().ok())
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn should_fully_parse_multi_delete_before_mutating_any_object() {
        // Arrange
        let storage = temp_storage();
        storage.create_bucket("bucket".to_string()).unwrap();
        for key in ["first.txt", "second.txt"] {
            storage
                .put_object(
                    "bucket",
                    key.to_string(),
                    Object::new(
                        key.to_string(),
                        b"preserved".to_vec(),
                        "text/plain".to_string(),
                    ),
                )
                .unwrap();
        }
        let malformed =
            br"<Delete><Object><Key>first.txt</Key></Object><Object><Key>second.txt</Key>";
        let request =
            parsed_request_with_method("POST", "http://localhost/bucket?delete", malformed).await;

        // Act
        let response = bucket_post(
            storage.clone(),
            auth_disabled_config(),
            "bucket",
            &request,
            "req-malformed-multi-delete".to_string(),
        )
        .await
        .expect("malformed multi-delete should respond");

        // Assert
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers()["x-amz-request-id"],
            "req-malformed-multi-delete"
        );
        assert!(response.headers().contains_key("x-amz-id-2"));
        assert!(response_text(response)
            .await
            .contains("<Code>MalformedXML</Code>"));
        for key in ["first.txt", "second.txt"] {
            assert_eq!(
                storage.get_object("bucket", key).unwrap().data,
                b"preserved"
            );
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn should_reject_schema_invalid_multi_delete_nesting_without_mutating() {
        // Arrange
        let storage = temp_storage();
        storage.create_bucket("bucket".to_string()).unwrap();
        storage
            .put_object(
                "bucket",
                "preserved.txt".to_string(),
                Object::new(
                    "preserved.txt".to_string(),
                    b"preserved".to_vec(),
                    "text/plain".to_string(),
                ),
            )
            .unwrap();
        let invalid =
            br"<Delete><Unexpected><Object><Key>preserved.txt</Key></Object></Unexpected></Delete>";
        let request =
            parsed_request_with_method("POST", "http://localhost/bucket?delete", invalid).await;

        // Act
        let response = bucket_post(
            storage.clone(),
            auth_disabled_config(),
            "bucket",
            &request,
            "req-schema-invalid-multi-delete".to_string(),
        )
        .await
        .expect("schema-invalid multi-delete should respond");

        // Assert
        assert_eq!(response.status(), StatusCode::OK);
        assert!(response_text(response)
            .await
            .contains("<Code>MalformedXML</Code>"));
        assert_eq!(
            storage.get_object("bucket", "preserved.txt").unwrap().data,
            b"preserved"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn should_require_valid_content_md5_before_multi_delete_mutation() {
        // Arrange
        let storage = temp_storage();
        storage.create_bucket("bucket".to_string()).unwrap();
        storage
            .put_object(
                "bucket",
                "preserved.txt".to_string(),
                Object::new(
                    "preserved.txt".to_string(),
                    b"preserved".to_vec(),
                    "text/plain".to_string(),
                ),
            )
            .unwrap();
        let body = br"<Delete><Object><Key>preserved.txt</Key></Object></Delete>";
        let missing = RequestExt::from_hyper(
            HyperRequest::builder()
                .method("POST")
                .uri("http://localhost/bucket?delete")
                .body(Body::from(body.to_vec()))
                .expect("request should build"),
        )
        .await
        .expect("request should parse");
        let mismatched = RequestExt::from_hyper(
            HyperRequest::builder()
                .method("POST")
                .uri("http://localhost/bucket?delete")
                .header("content-md5", BASE64.encode(md5::compute(b"different").0))
                .body(Body::from(body.to_vec()))
                .expect("request should build"),
        )
        .await
        .expect("request should parse");

        // Act
        let missing_response = bucket_post(
            storage.clone(),
            auth_disabled_config(),
            "bucket",
            &missing,
            "req-missing-md5".to_string(),
        )
        .await
        .expect("missing digest should respond");
        let mismatched_response = bucket_post(
            storage.clone(),
            auth_disabled_config(),
            "bucket",
            &mismatched,
            "req-bad-md5".to_string(),
        )
        .await
        .expect("bad digest should respond");

        // Assert
        assert_eq!(missing_response.status(), StatusCode::BAD_REQUEST);
        assert!(response_text(missing_response)
            .await
            .contains("<Code>MissingContentMD5</Code>"));
        assert_eq!(mismatched_response.status(), StatusCode::BAD_REQUEST);
        assert!(response_text(mismatched_response)
            .await
            .contains("<Code>BadDigest</Code>"));
        assert_eq!(
            storage
                .get_object("bucket", "preserved.txt")
                .expect("invalid digest must not delete the object")
                .data,
            b"preserved"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn should_report_multi_delete_lock_and_etag_failures_without_deleting() {
        // Arrange
        let storage = temp_storage();
        storage.create_bucket("bucket".to_string()).unwrap();
        storage.enable_versioning("bucket").unwrap();
        let mut retained = Object::new(
            "retained.txt".to_string(),
            b"retained".to_vec(),
            "text/plain".to_string(),
        );
        retained.provider_metadata.insert(
            "s3_object_lock_until".to_string(),
            "2099-01-01T00:00:00Z".to_string(),
        );
        storage
            .put_object("bucket", "retained.txt".to_string(), retained)
            .unwrap();
        let retained_version = storage
            .get_object("bucket", "retained.txt")
            .unwrap()
            .version_id
            .expect("versioned object should have a version id");
        storage
            .put_object(
                "bucket",
                "etagged.txt".to_string(),
                Object::new(
                    "etagged.txt".to_string(),
                    b"etagged".to_vec(),
                    "text/plain".to_string(),
                ),
            )
            .unwrap();
        let body = format!(
            "<Delete><Object><Key>retained.txt</Key><VersionId>{retained_version}</VersionId></Object><Object><Key>etagged.txt</Key><ETag>\"wrong\"</ETag></Object></Delete>"
        );
        let request =
            parsed_request_with_method("POST", "http://localhost/bucket?delete", body.as_bytes())
                .await;

        // Act
        let response = bucket_post(
            storage.clone(),
            auth_disabled_config(),
            "bucket",
            &request,
            "req-multi-delete-conditions".to_string(),
        )
        .await
        .expect("conditional multi-delete should respond");

        // Assert
        assert_eq!(response.status(), StatusCode::OK);
        let body = response_text(response).await;
        assert!(body.contains("<Code>AccessDenied</Code>"));
        assert!(body.contains("<Code>PreconditionFailed</Code>"));
        assert!(storage
            .get_object_version("bucket", "retained.txt", &retained_version)
            .is_ok());
        assert_eq!(
            storage.get_object("bucket", "etagged.txt").unwrap().data,
            b"etagged"
        );
    }

    async fn browser_upload_request(
        boundary: &str,
        key: &str,
        file_content_type: &str,
        file_name: &str,
        file_data: &[u8],
    ) -> RequestExt {
        let mut body = Vec::new();
        body.extend_from_slice(
            format!(
                "--{boundary}\r\nContent-Disposition: form-data; name=\"key\"\r\n\r\n{key}\r\n"
            )
            .as_bytes(),
        );
        body.extend_from_slice(format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"{file_name}\"\r\nContent-Type: {file_content_type}\r\n\r\n"
        )
        .as_bytes());
        body.extend_from_slice(file_data);
        body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());

        RequestExt::from_hyper(
            HyperRequest::builder()
                .method("POST")
                .uri("http://localhost/bucket")
                .header(
                    "content-type",
                    format!("multipart/form-data; boundary={boundary}"),
                )
                .body(Body::from(body))
                .expect("request should build"),
        )
        .await
        .expect("request should parse")
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn should_enable_object_lock_at_bucket_creation_and_prevent_versioning_suspension() {
        // Arrange
        let storage = temp_storage();
        let create = RequestExt::from_hyper(
            HyperRequest::builder()
                .method("PUT")
                .uri("http://localhost/locked-bucket")
                .header("x-amz-bucket-object-lock-enabled", "true")
                .body(Body::from(Bytes::new()))
                .expect("request should build"),
        )
        .await
        .expect("request should parse");

        // Act
        let create_response = bucket_put(
            storage.clone(),
            auth_disabled_config(),
            "locked-bucket",
            &create,
            "req-lock-create".to_string(),
        )
        .await
        .expect("locked bucket create should respond");
        let get = parsed_request("http://localhost/locked-bucket?object-lock").await;
        let get_response = bucket_get_or_list_objects(
            storage.clone(),
            auth_disabled_config(),
            "locked-bucket",
            &get,
            "req-lock-get".to_string(),
        )
        .await
        .expect("Object Lock configuration GET should respond");
        let suspend = parsed_request_with_method(
            "PUT",
            "http://localhost/locked-bucket?versioning",
            br#"<VersioningConfiguration xmlns="http://s3.amazonaws.com/doc/2006-03-01/"><Status>Suspended</Status></VersioningConfiguration>"#,
        )
        .await;
        let suspend_response = bucket_put(
            storage.clone(),
            auth_disabled_config(),
            "locked-bucket",
            &suspend,
            "req-lock-suspend".to_string(),
        )
        .await
        .expect("versioning suspension should respond");

        // Assert
        assert_eq!(create_response.status(), StatusCode::OK);
        assert_eq!(
            create_response
                .headers()
                .get("location")
                .and_then(|value| value.to_str().ok()),
            Some("/locked-bucket")
        );
        let bucket = storage.get_bucket("locked-bucket").unwrap();
        assert!(bucket.versioning_enabled);
        assert_eq!(
            bucket.metadata.get("s3_object_lock_enabled"),
            Some(&"true".to_string())
        );
        assert_eq!(get_response.status(), StatusCode::OK);
        assert!(response_text(get_response)
            .await
            .contains("<ObjectLockEnabled>Enabled</ObjectLockEnabled>"));
        assert_eq!(suspend_response.status(), StatusCode::CONFLICT);
        assert!(
            storage
                .get_bucket("locked-bucket")
                .unwrap()
                .versioning_enabled
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn should_list_version_history_when_versions_query_is_requested() {
        // Arrange
        let storage = temp_storage();
        storage.create_bucket("bucket".to_string()).unwrap();
        storage.enable_versioning("bucket").unwrap();

        let mut first = Object::new(
            "doc.txt".to_string(),
            b"v1".to_vec(),
            "text/plain".to_string(),
        );
        first.last_modified = Utc.with_ymd_and_hms(2024, 4, 10, 12, 0, 0).unwrap();
        storage
            .put_object("bucket", "doc.txt".to_string(), first)
            .unwrap();

        let first_version_id = storage
            .get_object("bucket", "doc.txt")
            .unwrap()
            .version_id
            .clone()
            .expect("first version id should exist");

        let mut second = Object::new(
            "doc.txt".to_string(),
            b"v2".to_vec(),
            "text/plain".to_string(),
        );
        second.last_modified = Utc.with_ymd_and_hms(2024, 4, 10, 12, 5, 0).unwrap();
        storage
            .put_object("bucket", "doc.txt".to_string(), second)
            .unwrap();

        let current_version_id = storage
            .get_object("bucket", "doc.txt")
            .unwrap()
            .version_id
            .clone()
            .expect("current version id should exist");

        let req = parsed_request("http://localhost/bucket?versions").await;

        // Act
        let resp = bucket_get_or_list_objects(
            storage.clone(),
            auth_disabled_config(),
            "bucket",
            &req,
            "req-129".to_string(),
        )
        .await
        .expect("versions listing should complete");

        // Assert
        assert_eq!(resp.status(), StatusCode::OK);

        let body = resp
            .into_body()
            .collect()
            .await
            .expect("body should read")
            .to_bytes();
        let body = String::from_utf8(body.to_vec()).expect("body should be utf8");
        assert!(body.contains("<ListVersionsResult"));
        assert!(body.contains(&first_version_id));
        assert!(body.contains(&current_version_id));
        assert!(body.contains("<IsLatest>true</IsLatest>"));
        assert!(body.contains("doc.txt"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn should_list_objects_v2_with_continuation_token_when_list_type_two_is_requested() {
        // Arrange
        let storage = temp_storage();
        storage.create_bucket("bucket".to_string()).unwrap();

        for (key, payload) in [
            ("alpha.txt", b"alpha".as_slice()),
            ("beta.txt", b"beta".as_slice()),
            ("gamma.txt", b"gamma".as_slice()),
        ] {
            storage
                .put_object(
                    "bucket",
                    key.to_string(),
                    Object::new(key.to_string(), payload.to_vec(), "text/plain".to_string()),
                )
                .unwrap();
        }

        let first_req = parsed_request("http://localhost/bucket?list-type=2&max-keys=2").await;

        // Act
        let first_resp = bucket_get_or_list_objects(
            storage.clone(),
            auth_disabled_config(),
            "bucket",
            &first_req,
            "req-130".to_string(),
        )
        .await
        .expect("first listing should complete");

        // Assert
        assert_eq!(first_resp.status(), StatusCode::OK);

        let first_body = first_resp
            .into_body()
            .collect()
            .await
            .expect("body should read")
            .to_bytes();
        let first_body = String::from_utf8(first_body.to_vec()).expect("body should be utf8");
        assert!(first_body.contains("<ListBucketResult"));
        assert!(first_body.contains("<KeyCount>2</KeyCount>"));
        assert!(first_body.contains("<IsTruncated>true</IsTruncated>"));
        let continuation_token = xml_tag(&first_body, "NextContinuationToken")
            .expect("truncated result should include a continuation token");
        assert_ne!(continuation_token, "beta.txt");
        assert!(first_body.contains("<ETag>\"") && !first_body.contains(r#"<ETag>\""#));
        assert!(first_body.contains("alpha.txt"));
        assert!(first_body.contains("beta.txt"));

        let second_req = parsed_request(&format!(
            "http://localhost/bucket?list-type=2&continuation-token={}",
            urlencoding::encode(&continuation_token)
        ))
        .await;
        let second_resp = bucket_get_or_list_objects(
            storage.clone(),
            auth_disabled_config(),
            "bucket",
            &second_req,
            "req-131".to_string(),
        )
        .await
        .expect("second listing should complete");

        assert_eq!(second_resp.status(), StatusCode::OK);

        let second_body = second_resp
            .into_body()
            .collect()
            .await
            .expect("body should read")
            .to_bytes();
        let second_body = String::from_utf8(second_body.to_vec()).expect("body should be utf8");
        assert!(second_body.contains("<KeyCount>1</KeyCount>"));
        assert!(second_body.contains("gamma.txt"));
        assert!(!second_body.contains("<Key>alpha.txt</Key>"));
        assert!(!second_body.contains("<Key>beta.txt</Key>"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn should_reject_unissued_list_objects_v2_continuation_token() {
        // Arrange
        let storage = temp_storage();
        storage.create_bucket("bucket".to_string()).unwrap();
        let req =
            parsed_request("http://localhost/bucket?list-type=2&continuation-token=alpha.txt")
                .await;

        // Act
        let response = bucket_get_or_list_objects(
            storage,
            auth_disabled_config(),
            "bucket",
            &req,
            "req-invalid-token".to_string(),
        )
        .await
        .expect("invalid token should produce a response");

        // Assert
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert!(response_text(response)
            .await
            .contains("<Code>InvalidArgument</Code>"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn should_reject_malformed_max_keys_for_every_s3_object_listing_shape() {
        // Arrange
        let storage = temp_storage();
        storage.create_bucket("bucket".to_string()).unwrap();
        let listing_shapes = ["", "list-type=2&", "versions&"];

        // Act
        // Assert
        for shape in listing_shapes {
            for value in ["alpha", "-1", "184467440737095516160"] {
                let req =
                    parsed_request(&format!("http://localhost/bucket?{shape}max-keys={value}"))
                        .await;
                let response = bucket_get_or_list_objects(
                    storage.clone(),
                    auth_disabled_config(),
                    "bucket",
                    &req,
                    "req-invalid-max-keys".to_string(),
                )
                .await
                .expect("invalid max-keys should produce a response");
                assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{shape}{value}");
                assert_eq!(
                    response.headers()["x-amz-request-id"],
                    "req-invalid-max-keys"
                );
                assert!(response.headers().contains_key("x-amz-id-2"));
                assert!(response_text(response)
                    .await
                    .contains("<Code>InvalidArgument</Code>"));
            }
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn should_return_no_such_bucket_for_s3_list_objects_v1() {
        // Arrange
        let storage = temp_storage();
        let req = parsed_request("http://localhost/absent?prefix=logs%2F").await;

        // Act
        let response = bucket_get_or_list_objects(
            storage,
            auth_disabled_config(),
            "absent",
            &req,
            "req-missing-list-bucket".to_string(),
        )
        .await
        .expect("missing bucket listing should produce a response");

        // Assert
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert_eq!(
            response.headers()["x-amz-request-id"],
            "req-missing-list-bucket"
        );
        assert!(response.headers().contains_key("x-amz-id-2"));
        assert!(response_text(response)
            .await
            .contains("<Code>NoSuchBucket</Code>"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn should_order_and_resume_object_versions_by_key_then_recency() {
        // Arrange
        let storage = temp_storage();
        storage.create_bucket("bucket".to_string()).unwrap();
        storage.enable_versioning("bucket").unwrap();
        put_version_at(&storage, "bravo.txt", b"b1", 12, 0);
        put_version_at(&storage, "alpha.txt", b"a1", 11, 0);
        put_version_at(&storage, "bravo.txt", b"b2", 12, 5);
        put_version_at(&storage, "alpha.txt", b"a2", 11, 5);
        let first = parsed_request("http://localhost/bucket?versions&max-keys=2").await;

        // Act
        let first_body = response_text(
            bucket_get_or_list_objects(
                storage.clone(),
                auth_disabled_config(),
                "bucket",
                &first,
                "req-version-page-one".to_string(),
            )
            .await
            .expect("first version page should complete"),
        )
        .await;

        // Assert
        assert_eq!(first_body.matches("<Key>alpha.txt</Key>").count(), 2);
        assert!(!first_body.contains("<Key>bravo.txt</Key>"));
        let key_marker = xml_tag(&first_body, "NextKeyMarker").expect("next key marker");
        let version_marker =
            xml_tag(&first_body, "NextVersionIdMarker").expect("next version marker");
        let second = parsed_request(&format!(
            "http://localhost/bucket?versions&key-marker={}&version-id-marker={}",
            urlencoding::encode(&key_marker),
            urlencoding::encode(&version_marker)
        ))
        .await;
        let second_body = response_text(
            bucket_get_or_list_objects(
                storage,
                auth_disabled_config(),
                "bucket",
                &second,
                "req-version-page-two".to_string(),
            )
            .await
            .expect("second version page should complete"),
        )
        .await;
        assert_eq!(second_body.matches("<Key>bravo.txt</Key>").count(), 2);
        assert!(!second_body.contains("<Key>alpha.txt</Key>"));
    }

    fn put_version_at(storage: &Arc<dyn Storage>, key: &str, data: &[u8], hour: u32, minute: u32) {
        let mut object = Object::new(key.to_string(), data.to_vec(), "text/plain".to_string());
        object.last_modified = Utc.with_ymd_and_hms(2024, 4, 10, hour, minute, 0).unwrap();
        storage
            .put_object("bucket", key.to_string(), object)
            .expect("version write should succeed");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn should_list_objects_v2_with_common_prefixes_when_delimiter_is_provided() {
        // Arrange
        let storage = temp_storage();
        storage.create_bucket("bucket".to_string()).unwrap();

        for (key, payload) in [
            ("docs/readme.txt", b"readme".as_slice()),
            ("docs/2024/alpha.txt", b"alpha".as_slice()),
            ("docs/2025/bravo.txt", b"bravo".as_slice()),
        ] {
            storage
                .put_object(
                    "bucket",
                    key.to_string(),
                    Object::new(key.to_string(), payload.to_vec(), "text/plain".to_string()),
                )
                .unwrap();
        }

        let req =
            parsed_request("http://localhost/bucket?list-type=2&prefix=docs%2F&delimiter=%2F")
                .await;

        // Act
        let resp = bucket_get_or_list_objects(
            storage.clone(),
            auth_disabled_config(),
            "bucket",
            &req,
            "req-132".to_string(),
        )
        .await
        .expect("delimiter listing should complete");

        // Assert
        assert_eq!(resp.status(), StatusCode::OK);

        let body = resp
            .into_body()
            .collect()
            .await
            .expect("body should read")
            .to_bytes();
        let body = String::from_utf8(body.to_vec()).expect("body should be utf8");
        assert!(body.contains("<ListBucketResult"));
        assert!(body.contains("<KeyCount>3</KeyCount>"));
        assert!(body.contains("docs/readme.txt"));
        assert!(body.contains("<CommonPrefixes>"));
        assert!(body.contains("<Prefix>docs/2024/</Prefix>"));
        assert!(body.contains("<Prefix>docs/2025/</Prefix>"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn should_round_trip_request_payment_website_and_cors_bucket_configs() {
        let storage = temp_storage();
        storage.create_bucket("bucket".to_string()).unwrap();

        put_bucket_configurations(storage.clone()).await;
        verify_bucket_configurations(storage.clone()).await;
        delete_bucket_configurations(storage).await;
    }

    async fn put_bucket_configurations(storage: Arc<dyn Storage>) {
        let request_payment_xml = br#"<?xml version="1.0" encoding="UTF-8"?><RequestPaymentConfiguration xmlns="http://s3.amazonaws.com/doc/2006-03-01/"><Payer>Requester</Payer></RequestPaymentConfiguration>"#;
        let website_xml = br#"<?xml version="1.0" encoding="UTF-8"?><WebsiteConfiguration xmlns="http://s3.amazonaws.com/doc/2006-03-01/"><IndexDocument><Suffix>index.html</Suffix></IndexDocument></WebsiteConfiguration>"#;
        let cors_xml = br#"<?xml version="1.0" encoding="UTF-8"?><CORSConfiguration><CORSRule><AllowedOrigin>*</AllowedOrigin><AllowedMethod>GET</AllowedMethod></CORSRule></CORSConfiguration>"#;

        let put_request_payment = parsed_request_with_method(
            "PUT",
            "http://localhost/bucket?requestPayment",
            request_payment_xml,
        )
        .await;
        let put_website =
            parsed_request_with_method("PUT", "http://localhost/bucket?website", website_xml).await;
        let put_cors =
            parsed_request_with_method("PUT", "http://localhost/bucket?cors", cors_xml).await;

        assert_eq!(
            bucket_put(
                storage.clone(),
                auth_disabled_config(),
                "bucket",
                &put_request_payment,
                "req-133".to_string(),
            )
            .await
            .expect("request payment put should complete")
            .status(),
            StatusCode::OK
        );
        assert_eq!(
            bucket_put(
                storage.clone(),
                auth_disabled_config(),
                "bucket",
                &put_website,
                "req-134".to_string(),
            )
            .await
            .expect("website put should complete")
            .status(),
            StatusCode::OK
        );
        assert_eq!(
            bucket_put(
                storage.clone(),
                auth_disabled_config(),
                "bucket",
                &put_cors,
                "req-135".to_string(),
            )
            .await
            .expect("cors put should complete")
            .status(),
            StatusCode::OK
        );
    }

    async fn verify_bucket_configurations(storage: Arc<dyn Storage>) {
        let request_payment = bucket_get_or_list_objects(
            storage.clone(),
            auth_disabled_config(),
            "bucket",
            &parsed_request("http://localhost/bucket?requestPayment").await,
            "req-136".to_string(),
        )
        .await
        .expect("request payment get should complete");
        let request_payment_body = response_text(request_payment).await;
        assert!(request_payment_body.contains("<Payer>Requester</Payer>"));

        let website = bucket_get_or_list_objects(
            storage.clone(),
            auth_disabled_config(),
            "bucket",
            &parsed_request("http://localhost/bucket?website").await,
            "req-137".to_string(),
        )
        .await
        .expect("website get should complete");
        let website_body = response_text(website).await;
        assert!(website_body.contains("index.html"));

        let cors = bucket_get_or_list_objects(
            storage.clone(),
            auth_disabled_config(),
            "bucket",
            &parsed_request("http://localhost/bucket?cors").await,
            "req-138".to_string(),
        )
        .await
        .expect("cors get should complete");
        let cors_body = response_text(cors).await;
        assert!(cors_body.contains("<AllowedMethod>GET</AllowedMethod>"));
    }

    async fn delete_bucket_configurations(storage: Arc<dyn Storage>) {
        assert_eq!(
            bucket_delete(
                storage.clone(),
                auth_disabled_config(),
                "bucket",
                &parsed_request("http://localhost/bucket?website").await,
                "req-139".to_string(),
            )
            .await
            .expect("website delete should complete")
            .status(),
            StatusCode::NO_CONTENT
        );
        assert_eq!(
            bucket_delete(
                storage.clone(),
                auth_disabled_config(),
                "bucket",
                &parsed_request("http://localhost/bucket?cors").await,
                "req-140".to_string(),
            )
            .await
            .expect("cors delete should complete")
            .status(),
            StatusCode::NO_CONTENT
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn should_store_bucket_acl_grants_from_header_inputs() {
        let storage = temp_storage();
        storage.create_bucket("bucket".to_string()).unwrap();

        let put_acl = RequestExt::from_hyper(
            HyperRequest::builder()
                .method("PUT")
                .uri("http://localhost/bucket?acl")
                .header(
                    "x-amz-grant-read",
                    "uri=\"http://acs.amazonaws.com/groups/global/AllUsers\"",
                )
                .body(Body::from(Bytes::new()))
                .expect("request should build"),
        )
        .await
        .expect("request should parse");

        let put_response = bucket_put(
            storage.clone(),
            auth_disabled_config(),
            "bucket",
            &put_acl,
            "req-bucket-acl-put".to_string(),
        )
        .await
        .expect("bucket acl put should complete");
        assert_eq!(put_response.status(), StatusCode::OK);

        let get_acl = parsed_request("http://localhost/bucket?acl").await;
        let get_response = bucket_get_or_list_objects(
            storage,
            auth_disabled_config(),
            "bucket",
            &get_acl,
            "req-bucket-acl-get".to_string(),
        )
        .await
        .expect("bucket acl get should complete");
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
        assert!(body.contains("http://acs.amazonaws.com/groups/global/AllUsers"));
        assert!(body.contains("<Permission>READ</Permission>"));
        assert!(body.contains("<Permission>FULL_CONTROL</Permission>"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn should_store_bucket_acl_grants_from_xml_body_inputs() {
        let storage = temp_storage();
        storage.create_bucket("bucket".to_string()).unwrap();

        let put_acl = parsed_request_with_method(
            "PUT",
            "http://localhost/bucket?acl",
            br#"<?xml version="1.0" encoding="UTF-8"?>
<AccessControlPolicy xmlns="http://s3.amazonaws.com/doc/2006-03-01/">
    <AccessControlList>
        <Grant>
            <Grantee xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance" xsi:type="Group">
                <URI>http://acs.amazonaws.com/groups/global/AllUsers</URI>
            </Grantee>
            <Permission>READ</Permission>
        </Grant>
    </AccessControlList>
</AccessControlPolicy>"#,
        )
        .await;

        let put_response = bucket_put(
            storage.clone(),
            auth_disabled_config(),
            "bucket",
            &put_acl,
            "req-bucket-acl-xml-put".to_string(),
        )
        .await
        .expect("bucket acl put should complete");
        assert_eq!(put_response.status(), StatusCode::OK);

        let get_acl = parsed_request("http://localhost/bucket?acl").await;
        let get_response = bucket_get_or_list_objects(
            storage,
            auth_disabled_config(),
            "bucket",
            &get_acl,
            "req-bucket-acl-xml-get".to_string(),
        )
        .await
        .expect("bucket acl get should complete");
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
        assert!(body.contains("http://acs.amazonaws.com/groups/global/AllUsers"));
        assert_eq!(body.matches("<Permission>READ</Permission>").count(), 1);
        assert_eq!(
            body.matches("<Permission>FULL_CONTROL</Permission>")
                .count(),
            1
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn should_apply_cors_headers_to_bucket_listing_and_head_responses() {
        let storage = temp_storage();
        storage.create_bucket("bucket".to_string()).unwrap();
        storage
            .put_object(
                "bucket",
                "hello.txt".to_string(),
                Object::new(
                    "hello.txt".to_string(),
                    b"payload".to_vec(),
                    "text/plain".to_string(),
                ),
            )
            .unwrap();

        let cors_xml = br#"<?xml version="1.0" encoding="UTF-8"?><CORSConfiguration><CORSRule><AllowedOrigin>https://app.example</AllowedOrigin><AllowedMethod>GET</AllowedMethod><AllowedMethod>HEAD</AllowedMethod></CORSRule></CORSConfiguration>"#;
        let put_cors =
            parsed_request_with_method("PUT", "http://localhost/bucket?cors", cors_xml).await;
        bucket_put(
            storage.clone(),
            auth_disabled_config(),
            "bucket",
            &put_cors,
            "req-bucket-cors-put".to_string(),
        )
        .await
        .expect("cors put should complete");

        let list_request = RequestExt::from_hyper(
            HyperRequest::builder()
                .method("GET")
                .uri("http://localhost/bucket")
                .header("Origin", "https://app.example")
                .body(Body::from(Bytes::new()))
                .expect("request should build"),
        )
        .await
        .expect("request should parse");
        let list_response = bucket_get_or_list_objects(
            storage.clone(),
            auth_disabled_config(),
            "bucket",
            &list_request,
            "req-bucket-list-cors".to_string(),
        )
        .await
        .expect("list should complete");
        assert_eq!(list_response.status(), StatusCode::OK);
        assert_eq!(
            list_response
                .headers()
                .get("Access-Control-Allow-Origin")
                .and_then(|value| value.to_str().ok()),
            Some("https://app.example")
        );

        let head_request = RequestExt::from_hyper(
            HyperRequest::builder()
                .method("HEAD")
                .uri("http://localhost/bucket")
                .header("Origin", "https://app.example")
                .body(Body::from(Bytes::new()))
                .expect("request should build"),
        )
        .await
        .expect("request should parse");
        let head_response = bucket_head(
            storage,
            auth_disabled_config(),
            "bucket",
            &head_request,
            "req-bucket-head-cors".to_string(),
        )
        .await
        .expect("head should complete");
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
    async fn should_reject_bucket_preflight_and_omit_actual_headers_when_cors_rule_does_not_match()
    {
        let storage = temp_storage();
        storage.create_bucket("bucket".to_string()).unwrap();

        let cors_xml = br#"<?xml version="1.0" encoding="UTF-8"?><CORSConfiguration><CORSRule><AllowedOrigin>https://allowed.example</AllowedOrigin><AllowedMethod>GET</AllowedMethod></CORSRule></CORSConfiguration>"#;
        let put_cors =
            parsed_request_with_method("PUT", "http://localhost/bucket?cors", cors_xml).await;
        bucket_put(
            storage.clone(),
            auth_disabled_config(),
            "bucket",
            &put_cors,
            "req-bucket-cors-put-negative".to_string(),
        )
        .await
        .expect("cors put should complete");

        let preflight_request = RequestExt::from_hyper(
            HyperRequest::builder()
                .method("OPTIONS")
                .uri("http://localhost/bucket")
                .header("Origin", "https://blocked.example")
                .header("Access-Control-Request-Method", "GET")
                .body(Body::from(Bytes::new()))
                .expect("request should build"),
        )
        .await
        .expect("request should parse");
        let preflight_response = bucket_get_or_list_objects(
            storage.clone(),
            auth_disabled_config(),
            "bucket",
            &preflight_request,
            "req-bucket-preflight-blocked".to_string(),
        )
        .await
        .expect("preflight should respond");
        assert_eq!(preflight_response.status(), StatusCode::FORBIDDEN);

        let list_request = RequestExt::from_hyper(
            HyperRequest::builder()
                .method("GET")
                .uri("http://localhost/bucket")
                .header("Origin", "https://blocked.example")
                .body(Body::from(Bytes::new()))
                .expect("request should build"),
        )
        .await
        .expect("request should parse");
        let list_response = bucket_get_or_list_objects(
            storage,
            auth_disabled_config(),
            "bucket",
            &list_request,
            "req-bucket-list-blocked".to_string(),
        )
        .await
        .expect("list should respond");
        assert_eq!(list_response.status(), StatusCode::OK);
        assert!(list_response
            .headers()
            .get("Access-Control-Allow-Origin")
            .is_none());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn should_apply_cors_headers_to_bucket_mutation_responses() {
        let storage = temp_storage();
        storage.create_bucket("bucket".to_string()).unwrap();
        storage
            .put_object(
                "bucket",
                "delete-me.txt".to_string(),
                Object::new(
                    "delete-me.txt".to_string(),
                    b"payload".to_vec(),
                    "text/plain".to_string(),
                ),
            )
            .unwrap();

        put_mutation_cors_config(storage.clone()).await;
        verify_cors_on_website_put(storage.clone()).await;
        verify_cors_on_multi_delete(storage.clone()).await;
        verify_cors_on_bucket_config_deletes(storage).await;
    }

    async fn put_mutation_cors_config(storage: Arc<dyn Storage>) {
        let cors_xml = br#"<?xml version="1.0" encoding="UTF-8"?><CORSConfiguration><CORSRule><AllowedOrigin>https://app.example</AllowedOrigin><AllowedMethod>PUT</AllowedMethod><AllowedMethod>POST</AllowedMethod><AllowedMethod>DELETE</AllowedMethod></CORSRule></CORSConfiguration>"#;
        let put_cors =
            parsed_request_with_method("PUT", "http://localhost/bucket?cors", cors_xml).await;
        bucket_put(
            storage.clone(),
            auth_disabled_config(),
            "bucket",
            &put_cors,
            "req-mutation-cors-put".to_string(),
        )
        .await
        .expect("cors put should complete");
    }

    async fn verify_cors_on_website_put(storage: Arc<dyn Storage>) {
        let put_website = parsed_request_with_origin(
            "PUT",
            "http://localhost/bucket?website",
            br#"<?xml version="1.0" encoding="UTF-8"?><WebsiteConfiguration xmlns="http://s3.amazonaws.com/doc/2006-03-01/"><IndexDocument><Suffix>index.html</Suffix></IndexDocument></WebsiteConfiguration>"#,
        )
        .await;
        let put_website_response = bucket_put(
            storage.clone(),
            auth_disabled_config(),
            "bucket",
            &put_website,
            "req-website-put-cors".to_string(),
        )
        .await
        .expect("website put should complete");
        assert_eq!(put_website_response.status(), StatusCode::OK);
        assert_eq!(
            cors_origin(&put_website_response),
            Some("https://app.example")
        );
    }

    async fn verify_cors_on_multi_delete(storage: Arc<dyn Storage>) {
        let delete_request = parsed_request_with_origin(
            "POST",
            "http://localhost/bucket?delete",
            br"<Delete><Object><Key>delete-me.txt</Key></Object></Delete>",
        )
        .await;
        let delete_response = bucket_post(
            storage.clone(),
            auth_disabled_config(),
            "bucket",
            &delete_request,
            "req-multi-delete-cors".to_string(),
        )
        .await
        .expect("multi delete should complete");
        assert_eq!(delete_response.status(), StatusCode::OK);
        assert_eq!(cors_origin(&delete_response), Some("https://app.example"));
    }

    async fn verify_cors_on_bucket_config_deletes(storage: Arc<dyn Storage>) {
        let delete_website =
            parsed_request_with_origin("DELETE", "http://localhost/bucket?website", &[]).await;
        let delete_website_response = bucket_delete(
            storage.clone(),
            auth_disabled_config(),
            "bucket",
            &delete_website,
            "req-website-delete-cors".to_string(),
        )
        .await
        .expect("website delete should complete");
        assert_eq!(delete_website_response.status(), StatusCode::NO_CONTENT);
        assert_eq!(
            cors_origin(&delete_website_response),
            Some("https://app.example")
        );

        let delete_cors =
            parsed_request_with_origin("DELETE", "http://localhost/bucket?cors", &[]).await;
        let delete_cors_response = bucket_delete(
            storage,
            auth_disabled_config(),
            "bucket",
            &delete_cors,
            "req-cors-delete-cors".to_string(),
        )
        .await
        .expect("cors delete should complete");
        assert_eq!(delete_cors_response.status(), StatusCode::NO_CONTENT);
        assert_eq!(
            cors_origin(&delete_cors_response),
            Some("https://app.example")
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn should_accept_browser_post_uploads() {
        let storage = temp_storage();
        storage.create_bucket("bucket".to_string()).unwrap();

        let boundary = "----sqrzl-boundary";
        let request = browser_upload_request(
            boundary,
            "upload.txt",
            "text/plain",
            "upload.txt",
            b"browser upload",
        )
        .await;

        let response = bucket_post(
            storage.clone(),
            auth_disabled_config(),
            "bucket",
            &request,
            "req-post".to_string(),
        )
        .await
        .expect("bucket post should succeed");

        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        let stored = storage.get_object("bucket", "upload.txt").unwrap();
        assert_eq!(stored.data, b"browser upload");
        assert_eq!(stored.content_type, "text/plain");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn should_preserve_exact_binary_bytes_for_browser_post_uploads() {
        let storage = temp_storage();
        storage.create_bucket("bucket".to_string()).unwrap();

        let request = browser_upload_request(
            "----sqrzl-boundary",
            "binary.bin",
            "application/octet-stream",
            "binary.bin",
            &[0x00, 0x7f, 0x80, 0xff, b'A', b'\r', b'\n', b' '],
        )
        .await;

        let response = bucket_post(
            storage.clone(),
            auth_disabled_config(),
            "bucket",
            &request,
            "req-post-binary".to_string(),
        )
        .await
        .expect("bucket post should succeed");

        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        let stored = storage.get_object("bucket", "binary.bin").unwrap();
        assert_eq!(
            stored.data,
            vec![0x00, 0x7f, 0x80, 0xff, b'A', b'\r', b'\n', b' ']
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn should_preserve_trailing_whitespace_for_browser_post_uploads() {
        let storage = temp_storage();
        storage.create_bucket("bucket".to_string()).unwrap();

        let payload = b"line one\r\nline two\r\n\r\n ";
        let request = browser_upload_request(
            "----sqrzl-boundary",
            "whitespace.txt",
            "text/plain",
            "whitespace.txt",
            payload,
        )
        .await;

        let response = bucket_post(
            storage.clone(),
            auth_disabled_config(),
            "bucket",
            &request,
            "req-post-whitespace".to_string(),
        )
        .await
        .expect("bucket post should succeed");

        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        let stored = storage.get_object("bucket", "whitespace.txt").unwrap();
        assert_eq!(stored.data, payload);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn should_require_auth_for_bucket_list_and_head_routes() {
        let storage = temp_storage();
        storage.create_bucket("bucket".to_string()).unwrap();

        let list_req = parsed_request("http://localhost/bucket").await;
        let list_response = bucket_get_or_list_objects(
            storage.clone(),
            auth_enabled_config(),
            "bucket",
            &list_req,
            "req-auth-list".to_string(),
        )
        .await
        .expect("list request should respond");
        assert_eq!(list_response.status(), StatusCode::FORBIDDEN);

        let head_req = parsed_request_with_method("HEAD", "http://localhost/bucket", &[]).await;
        let head_response = bucket_head(
            storage,
            auth_enabled_config(),
            "bucket",
            &head_req,
            "req-auth-head".to_string(),
        )
        .await
        .expect("head request should respond");
        assert_eq!(head_response.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn should_require_auth_for_bucket_post_delete_and_browser_upload() {
        let storage = temp_storage();
        storage.create_bucket("bucket".to_string()).unwrap();
        storage
            .put_object(
                "bucket",
                "delete-me.txt".to_string(),
                Object::new(
                    "delete-me.txt".to_string(),
                    b"payload".to_vec(),
                    "text/plain".to_string(),
                ),
            )
            .unwrap();

        let delete_body = br"<Delete><Object><Key>delete-me.txt</Key></Object></Delete>";
        let delete_request =
            parsed_request_with_method("POST", "http://localhost/bucket?delete", delete_body).await;
        let delete_response = bucket_post(
            storage.clone(),
            auth_enabled_config(),
            "bucket",
            &delete_request,
            "req-auth-delete".to_string(),
        )
        .await
        .expect("delete request should respond");
        assert_eq!(delete_response.status(), StatusCode::FORBIDDEN);

        let boundary = "----sqrzl-boundary";
        let body = format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"key\"\r\n\r\nupload.txt\r\n--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"upload.txt\"\r\nContent-Type: text/plain\r\n\r\nbrowser upload\r\n--{boundary}--\r\n"
        );
        let upload_request = crate::server::RequestExt::from_hyper(
            hyper::Request::builder()
                .method("POST")
                .uri("http://localhost/bucket")
                .header(
                    "content-type",
                    format!("multipart/form-data; boundary={boundary}"),
                )
                .body(Body::from(body))
                .expect("request should build"),
        )
        .await
        .expect("request should parse");
        let upload_response = bucket_post(
            storage,
            auth_enabled_config(),
            "bucket",
            &upload_request,
            "req-auth-upload".to_string(),
        )
        .await
        .expect("upload request should respond");
        assert_eq!(upload_response.status(), StatusCode::FORBIDDEN);
    }
}
