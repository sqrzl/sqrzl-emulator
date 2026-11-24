//! Coherent, bounded materialization for native GCS media download surfaces.
use super::GcsAdapter;
use crate::body::Body;
use crate::server::RequestExt as Request;
use crate::storage::Storage;
use http::StatusCode;
use hyper::Response;
use std::sync::Arc;

const MATERIALIZATION_LIMIT: u64 = 64 * 1024 * 1024;

fn selected_len(size: u64, range: Option<(u64, Option<u64>)>) -> Option<u64> {
    match range {
        None => Some(size),
        Some((start, end)) if start < size => end
            .unwrap_or(size - 1)
            .min(size - 1)
            .checked_sub(start)?
            .checked_add(1),
        _ => None,
    }
}

fn extent_response(json_request: bool) -> Response<Body> {
    let message = "This emulator materializes at most 64 MiB per GCS media request. Use a smaller byte range; streamed uploads remain supported.";
    if json_request {
        GcsAdapter::json_error(StatusCode::NOT_IMPLEMENTED, "notImplemented", message)
    } else {
        GcsAdapter::error_response(StatusCode::NOT_IMPLEMENTED, "NotImplemented", message)
    }
}

fn invalid_range(json_request: bool) -> Response<Body> {
    if json_request {
        GcsAdapter::json_error(
            StatusCode::RANGE_NOT_SATISFIABLE,
            "requestedRangeNotSatisfiable",
            "The requested range is not satisfiable",
        )
    } else {
        GcsAdapter::invalid_range_response()
    }
}

fn storage_error(
    error: crate::error::Error,
    bucket: &str,
    key: &str,
    json: bool,
) -> Response<Body> {
    match error {
        crate::error::Error::InvalidRequest(_) => invalid_range(json),
        crate::error::Error::KeyNotFound if json => GcsAdapter::json_not_found(key),
        crate::error::Error::KeyNotFound => GcsAdapter::error_response(
            StatusCode::NOT_FOUND,
            "NoSuchKey",
            "The specified key does not exist.",
        ),
        crate::error::Error::BucketNotFound if json => GcsAdapter::json_bucket_not_found(bucket),
        crate::error::Error::BucketNotFound => GcsAdapter::xml_bucket_not_found(bucket),
        error if json => GcsAdapter::json_upload_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "INTERNAL",
            &error.to_string(),
        ),
        error => GcsAdapter::error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "InternalError",
            &error.to_string(),
        ),
    }
}

impl GcsAdapter {
    pub(super) fn bounded_media_response(
        storage: &Arc<dyn Storage>,
        req: &Request,
        bucket: &str,
        key: &str,
        json_request: bool,
    ) -> Result<Response<Body>, String> {
        let initial = if json_request {
            match Self::checked_json_blob(storage, req, bucket, key) {
                Ok(object) => object,
                Err(response) => return Ok(*response),
            }
        } else {
            match storage.get_object_metadata(bucket, key) {
                Ok(object) => object,
                Err(error) => return Ok(storage_error(error, bucket, key, false)),
            }
        };
        let range = match req.header("range") {
            Some(value) => match crate::utils::request::parse_byte_range(value) {
                Some(range) => Some(range),
                None => return Ok(invalid_range(json_request)),
            },
            None => None,
        };
        let Some(len) = selected_len(initial.size, range) else {
            return Ok(invalid_range(json_request));
        };
        if len > MATERIALIZATION_LIMIT {
            return Ok(extent_response(json_request));
        }
        if len == 0 {
            return Ok(Self::object_response(StatusCode::OK, &initial, 0, None).empty());
        }
        let (start, requested_end) = range.unwrap_or((0, None));
        // Preserve the client's extent, but cap allocation before entering the
        // coherent storage read if another writer replaces the admitted object.
        let capped_end = start.saturating_add(MATERIALIZATION_LIMIT - 1);
        let end = Some(requested_end.map_or(capped_end, |end| end.min(capped_end)));
        let (object, data) = match storage.get_object_range(bucket, key, start, end) {
            Ok(payload) => payload,
            Err(error) => return Ok(storage_error(error, bucket, key, json_request)),
        };
        if json_request {
            if let Err(response) = Self::check_current_generation_selector(req, &object) {
                return Ok(*response);
            }
            if let Err(response) = Self::check_gcs_preconditions(req, &object) {
                return Ok(response);
            }
        }
        let Some(actual_len) = selected_len(object.size, range) else {
            return Ok(invalid_range(json_request));
        };
        if actual_len > MATERIALIZATION_LIMIT || actual_len != data.len() as u64 {
            return Ok(extent_response(json_request));
        }
        let (status, content_range) = if range.is_some() {
            (
                StatusCode::PARTIAL_CONTENT,
                Some(format!(
                    "bytes {start}-{}/{}",
                    start + actual_len - 1,
                    object.size
                )),
            )
        } else {
            (StatusCode::OK, None)
        };
        let body_len = Self::response_body_len(actual_len)?;
        Ok(
            Self::object_response(status, &object, body_len, content_range)
                .body(data)
                .build(),
        )
    }
}
