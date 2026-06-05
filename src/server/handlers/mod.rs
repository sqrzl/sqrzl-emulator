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

        RouteMatch::NotFound => Ok(xml_error_response(
            StatusCode::NOT_FOUND,
            "NotFound",
            "Not Found",
            &req_id,
        )),
    }
}
