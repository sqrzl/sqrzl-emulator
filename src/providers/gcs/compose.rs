//! Server-side object assembly and copy for the GCS JSON API:
//! `objects.compose`, `objects.rewrite`, and `objects.copy`.
//!
//! Sources are read as coherent snapshots, their generation preconditions are
//! checked against that snapshot, and the destination is written through the
//! same conditional write path as uploads so destination preconditions are
//! evaluated atomically with the write.
use super::{
    BlobWrite, BlobWriteOutcome, GcsAdapter, GenerationPreconditions, GCS_COMPONENT_COUNT_KEY,
};
use crate::body::Body;
use crate::server::RequestExt as Request;
use crate::storage::Storage;
use http::StatusCode;
use hyper::Response;
use std::collections::HashMap;
use std::sync::Arc;

/// Same local materialization bound as GCS media reads.
const MATERIALIZATION_LIMIT: u64 = 64 * 1024 * 1024;
const MAX_COMPOSE_SOURCES: usize = 32;
const MAX_COMPONENT_COUNT: u64 = 1024;

/// Writable object fields this emulator cannot enact on a destination.
const UNSUPPORTED_WRITABLE_FIELDS: &[&str] = &[
    "acl",
    "cacheControl",
    "contentDisposition",
    "contentEncoding",
    "contentLanguage",
    "customTime",
    "storageClass",
    "retention",
    "contexts",
    "temporaryHold",
    "eventBasedHold",
    "md5Hash",
    "crc32c",
];

/// Output-only object fields that GCS ignores when a client echoes a resource back.
const OUTPUT_ONLY_FIELDS: &[&str] = &[
    "kind",
    "id",
    "selfLink",
    "mediaLink",
    "generation",
    "metageneration",
    "etag",
    "size",
    "timeCreated",
    "updated",
    "timeStorageClassUpdated",
    "componentCount",
    "owner",
    "customerEncryption",
    "softDeleteTime",
    "hardDeleteTime",
    "timeDeleted",
    "timeFinalized",
];

type Rejection = Box<Response<Body>>;

/// The destination resource fields a compose or rewrite request may set.
#[derive(Default)]
struct DestinationFields {
    content_type: Option<String>,
    metadata: Option<HashMap<String, String>>,
    kms_key: Option<String>,
}

impl GcsAdapter {
    fn invalid_argument(message: &str) -> Rejection {
        Box::new(Self::json_error(
            StatusCode::BAD_REQUEST,
            "invalid",
            message,
        ))
    }

    fn parse_json_body(req: &Request) -> Result<serde_json::Value, Rejection> {
        if req.body.is_empty() {
            return Ok(serde_json::Value::Object(serde_json::Map::new()));
        }
        serde_json::from_slice(&req.body)
            .map_err(|error| Self::invalid_argument(&format!("Invalid request JSON: {error}")))
    }

    fn parse_destination_fields(
        resource: Option<&serde_json::Value>,
        bucket: &str,
        object: &str,
    ) -> Result<DestinationFields, Rejection> {
        let Some(resource) = resource.filter(|value| !value.is_null()) else {
            return Ok(DestinationFields::default());
        };
        let Some(fields) = resource.as_object() else {
            return Err(Self::invalid_argument(
                "Destination object resource must be a JSON object",
            ));
        };
        let mut destination = DestinationFields::default();
        for (field, value) in fields {
            match field.as_str() {
                "name" if value.as_str() == Some(object) => {}
                "bucket" if value.as_str() == Some(bucket) => {}
                "name" | "bucket" => {
                    return Err(Self::invalid_argument(&format!(
                        "Destination {field} must match the request path"
                    )));
                }
                "contentType" => match value {
                    serde_json::Value::Null => {}
                    serde_json::Value::String(value) if !value.is_empty() => {
                        destination.content_type = Some(value.clone());
                    }
                    _ => {
                        return Err(Self::invalid_argument(
                            "contentType must be a non-empty string",
                        ))
                    }
                },
                "metadata" => match value {
                    serde_json::Value::Null => destination.metadata = Some(HashMap::new()),
                    serde_json::Value::Object(values) => {
                        let mut metadata = HashMap::new();
                        for (key, value) in values {
                            let Some(value) = value.as_str() else {
                                return Err(Self::invalid_argument(&format!(
                                    "Object metadata value {key} must be a string"
                                )));
                            };
                            metadata.insert(key.clone(), value.to_string());
                        }
                        destination.metadata = Some(metadata);
                    }
                    _ => return Err(Self::invalid_argument("metadata must be a JSON object")),
                },
                // A key name echoed with a version suffix is the output form of a
                // previously read resource, not a request for a key.
                "kmsKeyName" => match value.as_str() {
                    Some(name) if name.contains("/cryptoKeyVersions/") => {}
                    Some(name) => destination.kms_key = Some(name.to_string()),
                    None if value.is_null() => {}
                    None => return Err(Self::invalid_argument("kmsKeyName must be a string")),
                },
                field if OUTPUT_ONLY_FIELDS.contains(&field) => {}
                field if UNSUPPORTED_WRITABLE_FIELDS.contains(&field) => {
                    return Err(Box::new(Self::json_error(
                        StatusCode::NOT_IMPLEMENTED,
                        "notImplemented",
                        &format!(
                            "Destination object field {field} is not supported by this emulator surface"
                        ),
                    )));
                }
                field => {
                    return Err(Self::invalid_argument(&format!(
                        "Unknown destination object field: {field}"
                    )));
                }
            }
        }
        Ok(destination)
    }

    /// Reject request controls that would otherwise be silently ignored.
    fn reject_unsupported_copy_controls(req: &Request) -> Result<(), Rejection> {
        for parameter in ["destinationPredefinedAcl", "rewriteToken"] {
            if req.query_param(parameter).is_some() {
                return Err(Box::new(Self::json_error(
                    StatusCode::NOT_IMPLEMENTED,
                    "notImplemented",
                    &format!("{parameter} is not supported by this emulator surface"),
                )));
            }
        }
        if let Some(response) = Self::invalid_upload_preconditions(req) {
            return Err(Box::new(response));
        }
        Ok(())
    }

    fn select_kms_key(
        storage: &Arc<dyn Storage>,
        bucket: &str,
        query: Option<&str>,
        body: Option<&str>,
    ) -> Result<Option<String>, Rejection> {
        let requested = match (query, body) {
            (Some(query), Some(body)) if query != body => {
                return Err(Self::invalid_argument(
                    "The kmsKeyName query parameter and resource field disagree",
                ));
            }
            (Some(name), _) | (None, Some(name)) => Some(name),
            (None, None) => None,
        };
        Self::resolve_kms_key(storage, bucket, requested)
    }

    /// Read a source object coherently, enforcing the size bound and its
    /// generation preconditions against the snapshot that will be copied.
    fn read_source(
        storage: &Arc<dyn Storage>,
        bucket: &str,
        name: &str,
        generation: Option<&str>,
        if_generation_match: Option<&str>,
        if_generation_not_match: Option<&str>,
    ) -> Result<crate::models::Object, Rejection> {
        let head = match storage.get_object_metadata(bucket, name) {
            Ok(head) => head,
            Err(crate::error::Error::KeyNotFound) => {
                return Err(Box::new(Self::json_not_found(name)));
            }
            Err(crate::error::Error::BucketNotFound) => {
                return Err(Box::new(Self::json_bucket_not_found(bucket)));
            }
            Err(error) => {
                return Err(Box::new(Self::json_error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "backendError",
                    &error.to_string(),
                )));
            }
        };
        if head.size > MATERIALIZATION_LIMIT {
            return Err(Box::new(Self::json_error(
                StatusCode::NOT_IMPLEMENTED,
                "notImplemented",
                "This emulator copies at most 64 MiB per GCS compose or rewrite request.",
            )));
        }
        let object = storage
            .get_object(bucket, name)
            .map_err(|error| match error {
                crate::error::Error::KeyNotFound => Box::new(Self::json_not_found(name)),
                error => Box::new(Self::json_error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "backendError",
                    &error.to_string(),
                )),
            })?;
        let current = Self::generation(&object);
        if let Some(selected) = generation {
            if selected.parse::<u64>().is_err() {
                return Err(Self::invalid_argument(
                    "Source generations must be unsigned integers",
                ));
            }
            if selected != current {
                // Only the live generation is retained by this emulator.
                return Err(Box::new(Self::json_not_found(name)));
            }
        }
        for value in [if_generation_match, if_generation_not_match]
            .into_iter()
            .flatten()
        {
            if value.parse::<u64>().is_err() {
                return Err(Self::invalid_argument(
                    "Source generation preconditions must be unsigned integers",
                ));
            }
        }
        if if_generation_match.is_some_and(|expected| expected != current)
            || if_generation_not_match.is_some_and(|rejected| rejected == current)
        {
            return Err(Box::new(Self::generation_precondition_failed()));
        }
        Ok(object)
    }

    fn component_count(object: &crate::models::Object) -> u64 {
        object
            .metadata
            .get(GCS_COMPONENT_COUNT_KEY)
            .and_then(|count| count.parse().ok())
            .unwrap_or(1)
    }

    fn write_destination(
        &self,
        storage: &Arc<dyn Storage>,
        bucket: &str,
        key: &str,
        write: BlobWrite<'_>,
    ) -> Result<Result<crate::blob::BlobRecord, Rejection>, String> {
        let preconditions = write.preconditions;
        Ok(
            match self.put_blob_with_generation_match(storage, bucket, key, write)? {
                BlobWriteOutcome::Stored(stored) => Ok(*stored),
                BlobWriteOutcome::PreconditionFailed => {
                    Err(Box::new(Self::upload_precondition_failed(preconditions)))
                }
                BlobWriteOutcome::RetentionPolicyNotMet => {
                    Err(Box::new(Self::retention_policy_not_met_response()))
                }
            },
        )
    }

    fn destination_metadata(
        metadata: HashMap<String, String>,
        kms_key: Option<&str>,
        component_count: Option<u64>,
    ) -> HashMap<String, String> {
        let mut metadata = Self::with_kms_key(metadata, kms_key);
        metadata.remove(GCS_COMPONENT_COUNT_KEY);
        if let Some(count) = component_count {
            metadata.insert(GCS_COMPONENT_COUNT_KEY.to_string(), count.to_string());
        }
        metadata
    }

    /// `POST /storage/v1/b/{bucket}/o/{object}/compose`
    pub(super) fn compose_json_object(
        &self,
        storage: &Arc<dyn Storage>,
        req: &Request,
        bucket: &str,
        object: &str,
    ) -> Result<Response<Body>, String> {
        Ok(match self.compose(storage, req, bucket, object)? {
            Ok(response) | Err(response) => response,
        })
    }

    fn compose(
        &self,
        storage: &Arc<dyn Storage>,
        req: &Request,
        bucket: &str,
        object: &str,
    ) -> Result<Result<Response<Body>, Response<Body>>, String> {
        macro_rules! reject {
            ($result:expr) => {
                match $result {
                    Ok(value) => value,
                    Err(response) => return Ok(Err(*response)),
                }
            };
        }
        reject!(Self::reject_unsupported_copy_controls(req));
        let payload = reject!(Self::parse_json_body(req));
        let Some(request) = payload.as_object() else {
            return Ok(Err(*Self::invalid_argument(
                "Compose request must be a JSON object",
            )));
        };
        if let Some(field) = request
            .keys()
            .find(|field| !matches!(field.as_str(), "kind" | "sourceObjects" | "destination"))
        {
            return Ok(Err(*Self::invalid_argument(&format!(
                "Unknown compose request field: {field}"
            ))));
        }
        let Some(sources) = request
            .get("sourceObjects")
            .and_then(serde_json::Value::as_array)
            .filter(|sources| !sources.is_empty())
        else {
            return Ok(Err(*Self::invalid_argument(
                "Required field: sourceObjects",
            )));
        };
        if sources.len() > MAX_COMPOSE_SOURCES {
            return Ok(Err(*Self::invalid_argument(
                "The number of source components provided (more than 32) exceeds the maximum (32)",
            )));
        }
        let destination = reject!(Self::parse_destination_fields(
            request.get("destination"),
            bucket,
            object
        ));
        let kms_key = reject!(Self::select_kms_key(
            storage,
            bucket,
            req.query_param("kmsKeyName"),
            destination.kms_key.as_deref()
        ));

        let (data, component_count) = reject!(Self::read_compose_sources(storage, bucket, sources));

        let preconditions = GenerationPreconditions {
            expected: req.query_param("ifGenerationMatch"),
            rejected: req.query_param("ifGenerationNotMatch"),
        };
        let stored = reject!(self.write_destination(
            storage,
            bucket,
            object,
            BlobWrite {
                data,
                content_type: destination
                    .content_type
                    .unwrap_or_else(|| "application/octet-stream".to_string()),
                metadata: Self::destination_metadata(
                    destination.metadata.unwrap_or_default(),
                    kms_key.as_deref(),
                    Some(component_count),
                ),
                preconditions,
            },
        )?);
        Ok(Ok(Self::gcs_object_json_response(StatusCode::OK, &stored)))
    }

    /// Read compose sources in order, checking each against its preconditions.
    fn read_compose_sources(
        storage: &Arc<dyn Storage>,
        bucket: &str,
        sources: &[serde_json::Value],
    ) -> Result<(Vec<u8>, u64), Rejection> {
        let mut data = Vec::new();
        let mut component_count = 0_u64;
        for source in sources {
            let Some(source) = source.as_object() else {
                return Err(Self::invalid_argument(
                    "Each source object must be a JSON object",
                ));
            };
            if let Some(field) = source.keys().find(|field| {
                !matches!(
                    field.as_str(),
                    "name" | "generation" | "objectPreconditions"
                )
            }) {
                return Err(Self::invalid_argument(&format!(
                    "Unknown source object field: {field}"
                )));
            }
            let Some(name) = source.get("name").and_then(serde_json::Value::as_str) else {
                return Err(Self::invalid_argument("Required field: sourceObjects.name"));
            };
            let generation = Self::json_integer_text(source.get("generation"))?;
            let preconditions = source
                .get("objectPreconditions")
                .filter(|value| !value.is_null());
            if let Some(field) = preconditions
                .and_then(serde_json::Value::as_object)
                .and_then(|fields| fields.keys().find(|field| *field != "ifGenerationMatch"))
            {
                return Err(Self::invalid_argument(&format!(
                    "Unknown objectPreconditions field: {field}"
                )));
            }
            let if_generation_match = Self::json_integer_text(
                preconditions.and_then(|value| value.get("ifGenerationMatch")),
            )?;
            let source = Self::read_source(
                storage,
                bucket,
                name,
                generation.as_deref(),
                if_generation_match.as_deref(),
                None,
            )?;
            component_count = component_count.saturating_add(Self::component_count(&source));
            if (data.len() as u64).saturating_add(source.data.len() as u64) > MATERIALIZATION_LIMIT
            {
                return Err(Box::new(Self::json_error(
                    StatusCode::NOT_IMPLEMENTED,
                    "notImplemented",
                    "This emulator composes at most 64 MiB per request.",
                )));
            }
            data.extend_from_slice(&source.data);
        }
        if component_count > MAX_COMPONENT_COUNT {
            return Err(Self::invalid_argument(
                "The composed object would exceed the maximum component count (1024)",
            ));
        }
        Ok((data, component_count))
    }

    /// Accept a JSON integer or decimal string; GCS sends generations as strings
    /// but older clients send numbers.
    fn json_integer_text(value: Option<&serde_json::Value>) -> Result<Option<String>, Rejection> {
        match value {
            None | Some(serde_json::Value::Null) => Ok(None),
            Some(serde_json::Value::String(text)) if text.parse::<u64>().is_ok() => {
                Ok(Some(text.clone()))
            }
            Some(serde_json::Value::Number(number)) if number.as_u64().is_some() => {
                Ok(Some(number.to_string()))
            }
            Some(_) => Err(Self::invalid_argument(
                "Generations must be unsigned integers",
            )),
        }
    }

    /// `POST .../o/{source}/rewriteTo/b/{bucket}/o/{object}` and `.../copyTo/...`.
    /// Rewrites always complete in a single call (`done: true`).
    #[allow(clippy::too_many_arguments)]
    pub(super) fn rewrite_json_object(
        &self,
        storage: &Arc<dyn Storage>,
        req: &Request,
        source_bucket: &str,
        source_object: &str,
        bucket: &str,
        object: &str,
        rewrite: bool,
    ) -> Result<Response<Body>, String> {
        Ok(
            match self.rewrite(
                storage,
                req,
                (source_bucket, source_object),
                (bucket, object),
                rewrite,
            )? {
                Ok(response) | Err(response) => response,
            },
        )
    }

    fn rewrite(
        &self,
        storage: &Arc<dyn Storage>,
        req: &Request,
        (source_bucket, source_object): (&str, &str),
        (bucket, object): (&str, &str),
        rewrite: bool,
    ) -> Result<Result<Response<Body>, Response<Body>>, String> {
        macro_rules! reject {
            ($result:expr) => {
                match $result {
                    Ok(value) => value,
                    Err(response) => return Ok(Err(*response)),
                }
            };
        }
        reject!(Self::reject_unsupported_copy_controls(req));
        if req.query_param("ifSourceMetagenerationMatch").is_some()
            || req.query_param("ifSourceMetagenerationNotMatch").is_some()
        {
            return Ok(Err(*Self::invalid_argument(
                "Source metageneration preconditions are not supported by this emulator surface",
            )));
        }
        if req.query_param("ifSourceGenerationMatch").is_some()
            && req.query_param("ifSourceGenerationNotMatch").is_some()
        {
            return Ok(Err(*Self::invalid_argument(
                "ifSourceGenerationMatch and ifSourceGenerationNotMatch cannot be combined",
            )));
        }
        match storage.get_bucket(bucket) {
            Ok(_) => {}
            Err(crate::error::Error::BucketNotFound) => {
                return Ok(Err(Self::json_bucket_not_found(bucket)));
            }
            Err(error) => return Err(error.to_string()),
        }
        if Self::foreign_data_protection_active(storage, bucket) {
            return Ok(Err(Self::foreign_data_protection_json_response()));
        }
        let payload = reject!(Self::parse_json_body(req));
        let destination = reject!(Self::parse_destination_fields(
            Some(&payload),
            bucket,
            object
        ));
        let kms_key = reject!(Self::select_kms_key(
            storage,
            bucket,
            req.query_param(if rewrite {
                "destinationKmsKeyName"
            } else {
                "kmsKeyName"
            }),
            destination.kms_key.as_deref()
        ));
        let source = reject!(Self::read_source(
            storage,
            source_bucket,
            source_object,
            req.query_param("sourceGeneration"),
            req.query_param("ifSourceGenerationMatch"),
            req.query_param("ifSourceGenerationNotMatch")
        ));
        let size = source.data.len();
        let component_count = source
            .metadata
            .get(GCS_COMPONENT_COUNT_KEY)
            .and_then(|count| count.parse().ok());
        let metadata = destination
            .metadata
            .unwrap_or_else(|| Self::public_metadata(&source.metadata));
        let preconditions = GenerationPreconditions {
            expected: req.query_param("ifGenerationMatch"),
            rejected: req.query_param("ifGenerationNotMatch"),
        };
        let stored =
            reject!(self.write_destination(
                storage,
                bucket,
                object,
                BlobWrite {
                    data: source.data,
                    content_type: destination.content_type.unwrap_or(source.content_type),
                    metadata: Self::destination_metadata(
                        metadata,
                        kms_key.as_deref(),
                        component_count,
                    ),
                    preconditions,
                },
            )?);
        if !rewrite {
            return Ok(Ok(Self::gcs_object_json_response(StatusCode::OK, &stored)));
        }
        let resource = Self::omit_absent_kms_key(Self::json_blob_record_metadata(bucket, &stored));
        Ok(Ok(Self::json_response(
            StatusCode::OK,
            &serde_json::json!({
                "kind": "storage#rewriteResponse",
                "totalBytesRewritten": size.to_string(),
                "objectSize": size.to_string(),
                "done": true,
                "resource": resource,
            })
            .to_string(),
        )))
    }
}
