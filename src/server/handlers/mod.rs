use super::http::{Request, ResponseBuilder, RouteMatch, Router};
use crate::auth::AuthConfig;
use crate::body::Body;
use crate::services::xml_error_response;
use crate::storage::Storage;
use crate::utils::headers as header_utils;
use http::StatusCode;
use hyper::Response;
use std::sync::Arc;

mod acl;
mod auth;
mod bucket;
mod cors;
mod object;

#[allow(unused_imports)]
pub(crate) use auth::{
    build_canonical_request, check_authorization, extract_credential_scope, extract_signed_headers,
    extract_sigv4_signature, verify_sigv4_signature,
};
pub use bucket::{
    bucket_delete, bucket_get_or_list_objects, bucket_head, bucket_post, bucket_put, list_buckets,
};
pub use object::{object_delete, object_get, object_head, object_post, object_put};

const GCS_SOFT_DELETE_SECONDS_KEY: &str = "gcs_soft_delete_seconds";
const GCS_RETENTION_SECONDS_KEY: &str = "gcs_retention_seconds";
const AZURE_VERSIONING_KEY: &str = "azure_versioning_enabled";
const AZURE_SOFT_DELETE_DAYS_KEY: &str = "azure_soft_delete_days";

fn bucket_has_foreign_data_protection(bucket: &crate::models::Bucket) -> bool {
    bucket
        .metadata
        .get(GCS_SOFT_DELETE_SECONDS_KEY)
        .and_then(|value| value.parse::<u64>().ok())
        .is_some_and(|seconds| seconds > 0)
        || bucket
            .metadata
            .get(GCS_RETENTION_SECONDS_KEY)
            .and_then(|value| value.parse::<u64>().ok())
            .is_some_and(|seconds| seconds > 0)
        || bucket
            .metadata
            .get(AZURE_VERSIONING_KEY)
            .is_some_and(|value| value == "true")
        || bucket
            .metadata
            .get(AZURE_SOFT_DELETE_DAYS_KEY)
            .and_then(|value| value.parse::<u64>().ok())
            .is_some_and(|days| days > 0)
}

fn foreign_data_protection_mode_active(storage: &dyn Storage, bucket: &str) -> bool {
    storage
        .get_bucket(bucket)
        .ok()
        .is_some_and(|bucket| bucket_has_foreign_data_protection(&bucket))
}

fn s3_foreign_history_conflict(storage: &dyn Storage, bucket: &str) -> bool {
    foreign_data_protection_mode_active(storage, bucket)
}

fn s3_foreign_history_conflict_response(req_id: &str) -> Response<Body> {
    xml_error_response(
        StatusCode::CONFLICT,
        "InvalidBucketState",
        "S3 versioning and object mutations are unavailable while a foreign-provider data-protection mode is active.",
        req_id,
    )
}

pub async fn handle_request(
    storage: Arc<dyn Storage>,
    auth_config: Arc<AuthConfig>,
    req: Request,
) -> Result<Response<Body>, String> {
    let route = Router::route(&req);
    let req_id = header_utils::generate_request_id();

    match route {
        RouteMatch::ListBuckets => list_buckets(storage, auth_config, req, req_id).await,

        RouteMatch::BucketGet(bucket) => {
            bucket_get_or_list_objects(storage, auth_config, &bucket, &req, req_id).await
        }

        RouteMatch::BucketPut(bucket) => {
            bucket_put(storage, auth_config, &bucket, &req, req_id).await
        }

        RouteMatch::BucketDelete(bucket) => {
            bucket_delete(storage, auth_config, &bucket, &req, req_id).await
        }

        RouteMatch::BucketHead(bucket) => {
            bucket_head(storage, auth_config, &bucket, &req, req_id).await
        }

        RouteMatch::BucketPost(bucket) => {
            bucket_post(storage, auth_config, &bucket, &req, req_id).await
        }

        RouteMatch::ObjectGet(bucket, key) => {
            object_get(storage, auth_config, &bucket, &key, &req, req_id).await
        }

        RouteMatch::ObjectPut(bucket, key) => {
            object_put(storage, auth_config, &bucket, &key, &req, req_id).await
        }

        RouteMatch::ObjectDelete(bucket, key) => {
            object_delete(storage, auth_config, &bucket, &key, &req, req_id).await
        }

        RouteMatch::ObjectHead(bucket, key) => {
            object_head(storage, auth_config, &bucket, &key, &req, req_id).await
        }

        RouteMatch::ObjectPost(bucket, key) => {
            object_post(storage, auth_config, &bucket, &key, &req, req_id).await
        }

        RouteMatch::InvalidObjectPath => Ok(xml_error_response(
            StatusCode::BAD_REQUEST,
            "InvalidURI",
            "Couldn't parse the specified URI.",
            &req_id,
        )),

        RouteMatch::NotFound => Ok(xml_error_response(
            StatusCode::NOT_FOUND,
            "NotFound",
            "Not Found",
            &req_id,
        )),
    }
}
