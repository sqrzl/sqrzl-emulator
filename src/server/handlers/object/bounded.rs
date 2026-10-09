//! Limits materialized S3 reads while retaining streamed writes and small ranges.
use crate::body::Body;
use crate::models::Object;
use crate::services::{storage_error_response, xml_error_response};
use crate::storage::Storage;
use http::StatusCode;
use hyper::Response;

pub(super) const MATERIALIZATION_LIMIT: u64 = 64 * 1024 * 1024;

fn extent_response(req_id: &str) -> Response<Body> {
    xml_error_response(StatusCode::NOT_IMPLEMENTED, "NotImplemented", "This emulator materializes at most 64 MiB per S3 GET or copy. Use a smaller byte range; streamed PUT remains supported.", req_id)
}

pub(super) fn invalid_range(req_id: &str) -> Response<Body> {
    xml_error_response(
        StatusCode::RANGE_NOT_SATISFIABLE,
        "InvalidRange",
        "The requested range is not satisfiable.",
        req_id,
    )
}

pub(super) fn metadata(
    storage: &dyn Storage,
    bucket: &str,
    key: &str,
    version: Option<&str>,
) -> crate::error::Result<Object> {
    match version {
        Some(version) => storage.get_object_version_metadata(bucket, key, version),
        None => storage.get_object_metadata(bucket, key),
    }
}

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

#[allow(clippy::result_large_err)]
pub(super) fn payload(
    storage: &dyn Storage,
    bucket: &str,
    key: &str,
    version: Option<&str>,
    range: Option<(u64, Option<u64>)>,
    req_id: &str,
) -> Result<Object, Box<Response<Body>>> {
    let initial = metadata(storage, bucket, key, version)
        .map_err(|error| Box::new(storage_error_response(&error, req_id)))?;
    if super::is_s3_delete_marker(&initial) {
        return Ok(initial);
    }
    let len = selected_len(initial.size, range).ok_or_else(|| Box::new(invalid_range(req_id)))?;
    if len > MATERIALIZATION_LIMIT {
        return Err(Box::new(extent_response(req_id)));
    }
    if len == 0 {
        return Ok(initial);
    }
    let (start, requested_end) = range.unwrap_or((0, None));
    // Cap the storage read itself, so a concurrent replacement cannot increase
    // allocation between metadata admission and the coherent range read.
    let capped_end = start.saturating_add(MATERIALIZATION_LIMIT - 1);
    let end = Some(requested_end.map_or(capped_end, |end| end.min(capped_end)));
    let read = match version {
        Some(version) => storage.get_object_version_range(bucket, key, version, start, end),
        None => storage.get_object_range(bucket, key, start, end),
    };
    let (mut object, data) = read.map_err(|error| {
        Box::new(match error {
            crate::error::Error::InvalidRequest(_) => invalid_range(req_id),
            other => storage_error_response(&other, req_id),
        })
    })?;
    let actual_len =
        selected_len(object.size, range).ok_or_else(|| Box::new(invalid_range(req_id)))?;
    if actual_len > MATERIALIZATION_LIMIT || actual_len != data.len() as u64 {
        return Err(Box::new(extent_response(req_id)));
    }
    object.data = data;
    Ok(object)
}
