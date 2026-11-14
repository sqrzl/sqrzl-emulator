//! Synchronous Copy Blob From URL (`PUT` with `x-ms-copy-source` and
//! `x-ms-requires-sync: true`) for sources in the same emulator account.
//!
//! Asynchronous Copy Blob, cross-account sources, snapshot/version sources,
//! and source OAuth (`x-ms-copy-source-authorization`) are rejected with 501.

use super::{encryption_scope, AzureBlobAdapter, AzureResource, Request, ResponseBuilder};
use crate::auth::AuthConfig;
use crate::body::Body;
use crate::storage::{ObjectCondition, Storage};
use http::{Method, StatusCode, Uri};
use hyper::Response;
use std::collections::HashMap;
use std::sync::Arc;

const AZURE_COPY_ID_KEY: &str = "azure_copy_id";
const AZURE_COPY_STATUS_KEY: &str = "azure_copy_status";
const AZURE_COPY_SOURCE_KEY: &str = "azure_copy_source";
const AZURE_COPY_PROGRESS_KEY: &str = "azure_copy_progress";
const AZURE_COPY_COMPLETION_KEY: &str = "azure_copy_completion_time";

/// Copy properties recorded on a destination blob, as (provider key, header).
pub(super) const COPY_PROPERTY_HEADERS: [(&str, &str); 5] = [
    (AZURE_COPY_ID_KEY, "x-ms-copy-id"),
    (AZURE_COPY_STATUS_KEY, "x-ms-copy-status"),
    (AZURE_COPY_SOURCE_KEY, "x-ms-copy-source"),
    (AZURE_COPY_PROGRESS_KEY, "x-ms-copy-progress"),
    (AZURE_COPY_COMPLETION_KEY, "x-ms-copy-completion-time"),
];

const UNSUPPORTED_COPY_HEADERS: [&str; 10] = [
    "x-ms-copy-source-authorization",
    "x-ms-source-if-modified-since",
    "x-ms-source-if-unmodified-since",
    "x-ms-source-if-tags",
    "x-ms-tags",
    "x-ms-copy-source-tag-option",
    "x-ms-source-content-md5",
    "x-ms-immutability-policy-until-date",
    "x-ms-legal-hold",
    "x-ms-file-request-intent",
];

fn not_implemented(message: &str) -> Response<Body> {
    AzureBlobAdapter::error_response(StatusCode::NOT_IMPLEMENTED, "FeatureNotSupported", message)
}

fn cannot_verify_source(status: StatusCode, message: &str) -> Response<Body> {
    AzureBlobAdapter::error_response(status, "CannotVerifyCopySource", message)
}

fn source_condition_failed() -> Response<Body> {
    AzureBlobAdapter::error_response(
        StatusCode::PRECONDITION_FAILED,
        "SourceConditionNotMet",
        "The source condition specified using HTTP conditional header(s) is not met.",
    )
}

/// Parses the copy source into a request-shaped value so the SAS validator
/// and resource parser can evaluate it exactly like a direct request.
fn source_request(raw: &str) -> Option<Request> {
    let uri = raw.parse::<Uri>().ok()?;
    let (parts, ()) = http::Request::builder()
        .method(Method::GET)
        .uri(uri)
        .body(())
        .ok()?
        .into_parts();
    Some(Request::from_parts(parts, bytes::Bytes::new(), None))
}

fn source_conditions_match(req: &Request, source: &crate::models::Object) -> bool {
    let strip = |value: &str| {
        value
            .trim()
            .trim_start_matches("W/")
            .trim_matches('"')
            .to_string()
    };
    if let Some(value) = req.header("x-ms-source-if-match") {
        let expected = strip(value);
        if value.trim().starts_with("W/") || (expected != "*" && expected != source.etag) {
            return false;
        }
    }
    if let Some(value) = req.header("x-ms-source-if-none-match") {
        let unexpected = strip(value);
        if unexpected == "*" || unexpected == source.etag {
            return false;
        }
    }
    true
}

impl AzureBlobAdapter {
    /// Handles synchronous Copy Blob From URL.
    ///
    /// # Errors
    /// Returns an error when storage access fails.
    #[allow(clippy::too_many_lines)]
    pub(super) fn copy_blob_from_url(
        &self,
        storage: &Arc<dyn Storage>,
        config: &AuthConfig,
        req: &Request,
        resource: &AzureResource,
        container: &str,
        blob_key: &str,
    ) -> Result<Response<Body>, String> {
        if !req
            .header("x-ms-requires-sync")
            .is_some_and(|value| value.eq_ignore_ascii_case("true"))
        {
            return Ok(not_implemented(
                "Asynchronous Copy Blob is not implemented by this emulator; send x-ms-requires-sync: true.",
            ));
        }
        if req.query_param("comp").is_some() {
            return Ok(not_implemented(
                "Copy sources are supported only on Put Blob From URL style requests.",
            ));
        }
        if let Some(header) = UNSUPPORTED_COPY_HEADERS
            .iter()
            .find(|header| req.header(header).is_some())
        {
            return Ok(not_implemented(&format!(
                "The {header} header is not implemented for Copy Blob From URL."
            )));
        }
        let raw_source = req.header("x-ms-copy-source").unwrap_or_default();
        let Some(source_req) = source_request(raw_source) else {
            return Ok(AzureBlobAdapter::error_response(
                StatusCode::BAD_REQUEST,
                "InvalidHeaderValue",
                "The x-ms-copy-source header is not a valid URL.",
            ));
        };
        let source = match Self::parse_resource(&source_req) {
            Ok(source) if source.account == resource.account => source,
            Ok(_) => {
                return Ok(not_implemented(
                    "Copy sources from another account are not implemented by this emulator.",
                ))
            }
            Err(message) => {
                return Ok(AzureBlobAdapter::error_response(
                    StatusCode::BAD_REQUEST,
                    "InvalidHeaderValue",
                    &message,
                ))
            }
        };
        let (Some(source_container), Some(source_blob)) =
            (source.container.as_deref(), source.blob.as_deref())
        else {
            return Ok(AzureBlobAdapter::error_response(
                StatusCode::BAD_REQUEST,
                "InvalidHeaderValue",
                "The copy source must address a blob.",
            ));
        };
        if source_req.query_param("snapshot").is_some()
            || source_req.query_param("versionid").is_some()
        {
            return Ok(not_implemented(
                "Snapshot and version copy sources are not implemented by this emulator.",
            ));
        }
        if config.azure_auth_enforced() {
            if source_req.query_param("sig").is_none() {
                return Ok(cannot_verify_source(
                    StatusCode::FORBIDDEN,
                    "The copy source must be authorized with a SAS that grants read permission.",
                ));
            }
            if let Err(message) = Self::validate_sas(&source_req, config, &source) {
                return Ok(cannot_verify_source(StatusCode::FORBIDDEN, &message));
            }
            if !source_req
                .query_param("sp")
                .is_some_and(|permissions| permissions.contains('r'))
            {
                return Ok(cannot_verify_source(
                    StatusCode::FORBIDDEN,
                    "The copy source SAS does not grant read permission.",
                ));
            }
        }

        let source_metadata = match storage.get_object_metadata(source_container, source_blob) {
            Ok(metadata) => metadata,
            Err(crate::error::Error::KeyNotFound | crate::error::Error::BucketNotFound) => {
                return Ok(cannot_verify_source(
                    StatusCode::NOT_FOUND,
                    "The specified copy source blob does not exist.",
                ))
            }
            Err(error) => return Err(error.to_string()),
        };
        if !source_conditions_match(req, &source_metadata) {
            return Ok(source_condition_failed());
        }
        if source_metadata.size > super::AZURE_MAX_MATERIALIZED_MUTATION_BYTES {
            return Ok(Self::materialized_extent_unsupported());
        }

        let existing_blob = match storage.get_object_metadata(container, blob_key) {
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
        let mut condition = match Self::write_condition(req) {
            Ok(condition) => condition,
            Err(response) => return Ok(response),
        };
        if Self::create_only_sas(req) {
            condition = Some(ObjectCondition::Missing);
        }

        let source_object = match storage.get_object(source_container, source_blob) {
            Ok(object) => object,
            Err(crate::error::Error::KeyNotFound | crate::error::Error::BucketNotFound) => {
                return Ok(cannot_verify_source(
                    StatusCode::NOT_FOUND,
                    "The specified copy source blob does not exist.",
                ))
            }
            Err(error) => return Err(error.to_string()),
        };
        // The source may have changed between the condition check and the read.
        if source_object.etag != source_metadata.etag
            && !source_conditions_match(req, &source_object)
        {
            return Ok(source_condition_failed());
        }
        let requested_metadata = Self::metadata_from_headers(req);
        let metadata = if requested_metadata.is_empty() {
            source_object
                .metadata
                .iter()
                .filter(|(key, _)| !key.starts_with("__sqrzl_"))
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect::<HashMap<_, _>>()
        } else {
            requested_metadata
        };
        let mut object = crate::models::Object::new_with_metadata_and_etag(
            blob_key.to_string(),
            source_object.data,
            source_object.content_type.clone(),
            metadata,
            uuid::Uuid::new_v4().simple().to_string(),
        );
        Self::set_blob_type(&mut object, "BlockBlob");
        for (_, key, _) in super::AZURE_CONTENT_PROPERTIES {
            if let Some(value) = source_object.provider_metadata.get(*key) {
                object
                    .provider_metadata
                    .insert((*key).to_string(), value.clone());
            }
        }
        encryption_scope::record_scope(&mut object, encryption_scope::write_scope(req).as_deref());
        if let Some(existing) = existing_blob.as_ref() {
            Self::preserve_active_lease(existing, &mut object);
        }
        let copy_id = uuid::Uuid::new_v4().to_string();
        let size = object.size;
        for (key, value) in [
            (AZURE_COPY_ID_KEY, copy_id.clone()),
            (AZURE_COPY_STATUS_KEY, "success".to_string()),
            (
                AZURE_COPY_SOURCE_KEY,
                raw_source.split('?').next().unwrap_or_default().to_string(),
            ),
            (AZURE_COPY_PROGRESS_KEY, format!("{size}/{size}")),
            (
                AZURE_COPY_COMPLETION_KEY,
                crate::utils::headers::format_last_modified(),
            ),
        ] {
            object.provider_metadata.insert(key.to_string(), value);
        }

        let written = if let Some(condition) = condition.as_ref() {
            storage
                .put_object_if(container, blob_key.to_string(), object, condition)
                .map_err(|error| error.to_string())?
        } else {
            storage
                .put_object(container, blob_key.to_string(), object)
                .map_err(|error| error.to_string())?;
            true
        };
        if !written {
            return Ok(if Self::create_only_sas(req) {
                Self::error_response(
                    StatusCode::FORBIDDEN,
                    "AuthorizationPermissionMismatch",
                    "Create-only SAS cannot overwrite an existing blob.",
                )
            } else if matches!(condition, Some(ObjectCondition::Missing)) {
                Self::error_response(
                    StatusCode::CONFLICT,
                    "BlobAlreadyExists",
                    "The specified blob already exists.",
                )
            } else {
                Self::condition_failed()
            });
        }
        self.clear_block_upload_state(
            storage,
            &Self::blob_state_key(&resource.account, container, blob_key),
        )?;
        let stored = storage
            .get_object_metadata(container, blob_key)
            .map_err(|error| error.to_string())?;
        let mut response: ResponseBuilder = Self::response(StatusCode::ACCEPTED)
            .header("etag", &format!("\"{}\"", stored.etag))
            .header(
                "last-modified",
                &crate::utils::headers::format_last_modified_at(&stored.last_modified),
            )
            .header("x-ms-copy-id", &copy_id)
            .header("x-ms-copy-status", "success");
        if let Some(version_id) = stored.version_id.as_deref() {
            response = response.header("x-ms-version-id", version_id);
        }
        Ok(
            Self::with_encryption_scope_header(response, encryption_scope::recorded_scope(&stored))
                .empty(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_evaluate_source_conditions_against_the_source_etag() {
        // Arrange
        let source = crate::models::Object::new_with_metadata_and_etag(
            "staged".to_string(),
            b"data".to_vec(),
            "text/plain".to_string(),
            HashMap::new(),
            "abc".to_string(),
        );
        let request = |name: &str, value: &str| {
            let (mut parts, ()) = http::Request::builder()
                .method(Method::PUT)
                .uri("http://localhost/a/c/b")
                .body(())
                .unwrap()
                .into_parts();
            parts.headers.insert(
                http::HeaderName::from_bytes(name.as_bytes()).unwrap(),
                value.parse().unwrap(),
            );
            Request::from_parts(parts, bytes::Bytes::new(), None)
        };

        // Act
        let outcomes = [
            source_conditions_match(&request("x-ms-source-if-match", "\"abc\""), &source),
            source_conditions_match(&request("x-ms-source-if-match", "\"other\""), &source),
            source_conditions_match(&request("x-ms-source-if-match", "W/\"abc\""), &source),
            source_conditions_match(&request("x-ms-source-if-none-match", "\"abc\""), &source),
            source_conditions_match(&request("x-ms-source-if-none-match", "*"), &source),
            source_conditions_match(&request("x-ms-source-if-none-match", "\"other\""), &source),
        ];

        // Assert
        assert_eq!(outcomes, [true, false, false, false, false, true]);
        assert!(source_request("http://localhost:9000/acct/c/b?sp=r&sig=x").is_some());
    }
}
