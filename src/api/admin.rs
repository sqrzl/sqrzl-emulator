use crate::body::Body;
use crate::error::{Error, Result};
use crate::services::{bucket as bucket_service, json_response, object as object_service};
use crate::storage::Storage;
use crate::utils::validation;
use http_body_util::BodyExt;
use hyper::{Method, Request, Response, StatusCode};
use serde::{de::DeserializeOwned, Deserialize};
use std::collections::HashMap;
use std::sync::Arc;

mod dto;
mod pagination;

use dto::*;
use pagination::*;

pub async fn handle_request<B>(storage: Arc<dyn Storage>, req: Request<B>) -> Result<Response<Body>>
where
    B: hyper::body::Body<Data = bytes::Bytes> + Send + 'static,
    B::Error: std::fmt::Display,
{
    let method = req.method().clone();
    let path = req.uri().path().to_string();
    let query = req.uri().query().unwrap_or("").to_string();
    let admin_path = path
        .strip_prefix("/admin/v1")
        .ok_or_else(|| Error::RouteNotFound(path.clone()))?;

    if admin_path == "/buckets" {
        return match method {
            Method::GET => list_buckets(storage, &query),
            Method::POST => create_bucket(storage, req).await,
            _ => Err(Error::MethodNotAllowed(path)),
        };
    }

    if let Some(rest) = admin_path.strip_prefix("/buckets/") {
        let (bucket, remainder) = parse_bucket_and_remainder(rest)?;

        return match remainder {
            None => match method {
                Method::GET => get_bucket(storage, &bucket),
                Method::DELETE => delete_bucket(storage, &bucket),
                _ => Err(Error::MethodNotAllowed(format!("{} {}", method, path))),
            },
            Some("versioning") => match method {
                Method::GET => get_bucket_versioning(storage, &bucket),
                Method::PUT => set_bucket_versioning(storage, &bucket, req).await,
                _ => Err(Error::MethodNotAllowed(format!("{} {}", method, path))),
            },
            Some("acl") => match method {
                Method::GET => get_bucket_acl(storage, &bucket),
                Method::PUT => set_bucket_acl(storage, &bucket, req).await,
                _ => Err(Error::MethodNotAllowed(format!("{} {}", method, path))),
            },
            Some("policy") => match method {
                Method::GET => get_bucket_policy(storage, &bucket),
                Method::PUT => set_bucket_policy(storage, &bucket, req).await,
                Method::DELETE => delete_bucket_policy(storage, &bucket),
                _ => Err(Error::MethodNotAllowed(format!("{} {}", method, path))),
            },
            Some("lifecycle") => match method {
                Method::GET => get_bucket_lifecycle(storage, &bucket),
                Method::PUT => set_bucket_lifecycle(storage, &bucket, req).await,
                Method::DELETE => delete_bucket_lifecycle(storage, &bucket),
                _ => Err(Error::MethodNotAllowed(format!("{} {}", method, path))),
            },
            Some("multipart-uploads") => match method {
                Method::GET => list_multipart_uploads(storage, &bucket, &query),
                _ => Err(Error::MethodNotAllowed(format!("{} {}", method, path))),
            },
            Some(remainder) if remainder.starts_with("multipart-uploads/") => {
                handle_multipart_upload_request(storage, &bucket, remainder, req).await
            }
            Some("objects") => match method {
                Method::GET => list_objects(storage, &bucket, &query),
                _ => Err(Error::MethodNotAllowed(format!("{} {}", method, path))),
            },
            Some(remainder) if remainder.starts_with("objects/") => {
                handle_object_request(
                    storage,
                    &bucket,
                    remainder.trim_start_matches("objects/"),
                    &query,
                    req,
                )
                .await
            }
            _ => Err(Error::RouteNotFound(path)),
        };
    }

    Err(Error::RouteNotFound(path))
}

pub fn error_response(err: &Error) -> Response<Body> {
    let details = match err {
        Error::InvalidRequest(details)
        | Error::MethodNotAllowed(details)
        | Error::RouteNotFound(details) => Some(details.clone()),
        _ => None,
    };

    let body = crate::api::models::ErrorResponse {
        error: err.to_string(),
        code: err.error_code().to_string(),
        details,
    };

    json_response(err.status_code(), &body)
}

async fn read_json<T: DeserializeOwned, B>(req: Request<B>) -> Result<T>
where
    B: hyper::body::Body<Data = bytes::Bytes> + Send + 'static,
    B::Error: std::fmt::Display,
{
    let bytes = req
        .into_body()
        .collect()
        .await
        .map_err(|e| Error::InvalidRequest(e.to_string()))?
        .to_bytes();
    serde_json::from_slice(&bytes).map_err(|e| Error::InvalidRequest(e.to_string()))
}

fn empty_response(status: StatusCode) -> Response<Body> {
    Response::builder()
        .status(status)
        .body(Body::default())
        .unwrap_or_else(|_| Response::new(Body::default()))
}

fn parse_bucket_and_remainder(rest: &str) -> Result<(String, Option<&str>)> {
    let (bucket, remainder) = match rest.split_once('/') {
        Some((bucket, remainder)) => (bucket, Some(remainder)),
        None => (rest, None),
    };

    let bucket = decode_component(bucket);
    if bucket.is_empty() {
        return Err(Error::InvalidRequest("Missing bucket".into()));
    }

    if let Err(message) = validation::validate_bucket_name(&bucket) {
        return Err(Error::InvalidRequest(message));
    }

    Ok((bucket, remainder))
}

fn list_buckets(storage: Arc<dyn Storage>, query: &str) -> Result<Response<Body>> {
    let page = parse_page_params(query, PageTokenKind::Buckets)?;
    let mut buckets =
        tokio::task::block_in_place(|| bucket_service::list_buckets(storage.as_ref()))?;
    buckets.sort_by(|left, right| left.name.cmp(&right.name));
    let buckets = buckets
        .into_iter()
        .filter(|bucket| contains_search(&bucket.name, page.search.as_deref()))
        .map(bucket_to_info)
        .collect();
    let (items, next) = paginate(buckets, &page);

    Ok(json_response(
        StatusCode::OK,
        &crate::api::models::ListBucketsResponse {
            items,
            next: encode_next(next, PageTokenKind::Buckets),
        },
    ))
}

async fn create_bucket<B>(storage: Arc<dyn Storage>, req: Request<B>) -> Result<Response<Body>>
where
    B: hyper::body::Body<Data = bytes::Bytes> + Send + 'static,
    B::Error: std::fmt::Display,
{
    #[derive(Deserialize)]
    struct CreateReq {
        name: String,
    }

    let payload: CreateReq = read_json(req).await?;
    let name = payload.name;
    if let Err(message) = validation::validate_bucket_name(&name) {
        return Err(Error::InvalidRequest(message));
    }
    tokio::task::block_in_place(|| bucket_service::create_bucket(storage.as_ref(), name.clone()))?;
    let bucket =
        tokio::task::block_in_place(|| bucket_service::get_bucket(storage.as_ref(), &name))?;

    Ok(json_response(
        StatusCode::CREATED,
        &bucket_to_details(bucket),
    ))
}

fn get_bucket(storage: Arc<dyn Storage>, bucket: &str) -> Result<Response<Body>> {
    let bucket =
        tokio::task::block_in_place(|| bucket_service::get_bucket(storage.as_ref(), bucket))?;
    Ok(json_response(StatusCode::OK, &bucket_to_details(bucket)))
}

fn delete_bucket(storage: Arc<dyn Storage>, bucket: &str) -> Result<Response<Body>> {
    tokio::task::block_in_place(|| bucket_service::delete_bucket(storage.as_ref(), bucket))?;
    Ok(empty_response(StatusCode::NO_CONTENT))
}

fn get_bucket_versioning(storage: Arc<dyn Storage>, bucket: &str) -> Result<Response<Body>> {
    let bucket =
        tokio::task::block_in_place(|| bucket_service::get_bucket(storage.as_ref(), bucket))?;
    Ok(json_response(
        StatusCode::OK,
        &crate::api::models::VersioningStatus {
            enabled: bucket_service::versioning_enabled(&bucket),
        },
    ))
}

async fn set_bucket_versioning<B>(
    storage: Arc<dyn Storage>,
    bucket: &str,
    req: Request<B>,
) -> Result<Response<Body>>
where
    B: hyper::body::Body<Data = bytes::Bytes> + Send + 'static,
    B::Error: std::fmt::Display,
{
    #[derive(Deserialize)]
    struct VersioningReq {
        enabled: bool,
    }

    let body: VersioningReq = read_json(req).await?;
    tokio::task::block_in_place(|| {
        bucket_service::set_versioning(storage.as_ref(), bucket, body.enabled)
    })?;

    Ok(json_response(
        StatusCode::OK,
        &crate::api::models::VersioningStatus {
            enabled: body.enabled,
        },
    ))
}

fn get_bucket_acl(storage: Arc<dyn Storage>, bucket: &str) -> Result<Response<Body>> {
    let acl =
        tokio::task::block_in_place(|| bucket_service::get_bucket_acl(storage.as_ref(), bucket))?;
    Ok(json_response(StatusCode::OK, &acl))
}

async fn set_bucket_acl<B>(
    storage: Arc<dyn Storage>,
    bucket: &str,
    req: Request<B>,
) -> Result<Response<Body>>
where
    B: hyper::body::Body<Data = bytes::Bytes> + Send + 'static,
    B::Error: std::fmt::Display,
{
    let acl: crate::models::policy::Acl = read_json(req).await?;
    tokio::task::block_in_place(|| {
        bucket_service::put_bucket_acl(storage.as_ref(), bucket, acl.clone())
    })?;
    Ok(json_response(StatusCode::OK, &acl))
}

fn get_bucket_policy(storage: Arc<dyn Storage>, bucket: &str) -> Result<Response<Body>> {
    let policy = tokio::task::block_in_place(|| {
        bucket_service::get_bucket_policy(storage.as_ref(), bucket)
    })?;
    Ok(json_response(StatusCode::OK, &policy))
}

async fn set_bucket_policy<B>(
    storage: Arc<dyn Storage>,
    bucket: &str,
    req: Request<B>,
) -> Result<Response<Body>>
where
    B: hyper::body::Body<Data = bytes::Bytes> + Send + 'static,
    B::Error: std::fmt::Display,
{
    let policy: crate::models::policy::BucketPolicyDocument = read_json(req).await?;
    tokio::task::block_in_place(|| {
        bucket_service::put_bucket_policy(storage.as_ref(), bucket, policy.clone())
    })?;
    Ok(json_response(StatusCode::OK, &policy))
}

fn delete_bucket_policy(storage: Arc<dyn Storage>, bucket: &str) -> Result<Response<Body>> {
    tokio::task::block_in_place(|| bucket_service::delete_bucket_policy(storage.as_ref(), bucket))?;
    Ok(empty_response(StatusCode::NO_CONTENT))
}

fn get_bucket_lifecycle(storage: Arc<dyn Storage>, bucket: &str) -> Result<Response<Body>> {
    let lifecycle = tokio::task::block_in_place(|| {
        bucket_service::get_bucket_lifecycle(storage.as_ref(), bucket)
    })?;
    Ok(json_response(StatusCode::OK, &lifecycle))
}

async fn set_bucket_lifecycle<B>(
    storage: Arc<dyn Storage>,
    bucket: &str,
    req: Request<B>,
) -> Result<Response<Body>>
where
    B: hyper::body::Body<Data = bytes::Bytes> + Send + 'static,
    B::Error: std::fmt::Display,
{
    let lifecycle: crate::models::lifecycle::LifecycleConfiguration = read_json(req).await?;
    tokio::task::block_in_place(|| {
        bucket_service::put_bucket_lifecycle(storage.as_ref(), bucket, lifecycle.clone())
    })?;
    Ok(json_response(StatusCode::OK, &lifecycle))
}

fn delete_bucket_lifecycle(storage: Arc<dyn Storage>, bucket: &str) -> Result<Response<Body>> {
    tokio::task::block_in_place(|| {
        bucket_service::delete_bucket_lifecycle(storage.as_ref(), bucket)
    })?;
    Ok(empty_response(StatusCode::NO_CONTENT))
}

fn list_objects(storage: Arc<dyn Storage>, bucket: &str, query: &str) -> Result<Response<Body>> {
    let page = parse_object_page_params(query)?;
    if page.search.is_some() {
        let (items, next) = list_matching_objects(&storage, bucket, &page)?;
        return Ok(json_response(
            StatusCode::OK,
            &crate::api::models::ListObjectsResponse {
                folders: Vec::new(),
                items,
                next: encode_object_next(next, PageTokenKind::Objects),
            },
        ));
    }

    let path_prefix = page.prefix.as_deref().unwrap_or("");
    let mut folders = Vec::new();
    let mut items = Vec::new();
    let mut marker = page.next;
    let mut next_marker: Option<String> = None;

    while folders.len() + items.len() < page.limit {
        let remaining = page.limit - folders.len() - items.len();
        let result = tokio::task::block_in_place(|| {
            object_service::list_objects(
                storage.as_ref(),
                bucket,
                page.prefix.as_deref(),
                Some("/"),
                marker.as_deref(),
                Some(remaining),
            )
        })?;

        marker = result.next_marker.clone();
        let result_has_more = result.is_truncated;

        for common_prefix in result.common_prefixes {
            if folders.len() + items.len() >= page.limit {
                break;
            }
            let folder = common_prefix_to_folder_info(common_prefix, path_prefix);
            if contains_search(&folder.name, page.search.as_deref())
                || contains_search(&folder.prefix, page.search.as_deref())
            {
                folders.push(folder);
            }
        }

        for object in result.objects {
            if folders.len() + items.len() >= page.limit {
                break;
            }
            if contains_search(&object.key, page.search.as_deref()) {
                items.push(object_to_info(object));
            }
        }

        if folders.len() + items.len() >= page.limit || !result_has_more {
            if folders.len() + items.len() >= page.limit && result_has_more {
                next_marker = marker.clone();
            } else {
                next_marker = None;
            }
            break;
        }

        if marker.is_none() {
            break;
        }

        next_marker = marker.clone();
    }

    let next = if folders.len() + items.len() >= page.limit {
        next_marker
    } else {
        None
    };

    Ok(json_response(
        StatusCode::OK,
        &crate::api::models::ListObjectsResponse {
            folders,
            items,
            next: encode_object_next(next, PageTokenKind::Objects),
        },
    ))
}

fn list_matching_objects(
    storage: &Arc<dyn Storage>,
    bucket: &str,
    page: &ObjectPageParams,
) -> Result<(Vec<crate::api::models::ObjectInfo>, Option<String>)> {
    let mut items = Vec::new();
    let mut marker = page.next.clone();
    let mut last_included_key: Option<String> = None;

    loop {
        let result = tokio::task::block_in_place(|| {
            object_service::list_objects(
                storage.as_ref(),
                bucket,
                page.prefix.as_deref(),
                None,
                marker.as_deref(),
                Some(page.limit + 1),
            )
        })?;

        marker = result.next_marker.clone();

        for object in result.objects {
            if !contains_search(&object.key, page.search.as_deref()) {
                continue;
            }

            if items.len() >= page.limit {
                return Ok((items, last_included_key));
            }

            last_included_key = Some(object.key.clone());
            items.push(object_to_info(object));
        }

        if !result.is_truncated || marker.is_none() {
            break;
        }
    }

    Ok((items, None))
}

fn list_multipart_uploads(
    storage: Arc<dyn Storage>,
    bucket: &str,
    query: &str,
) -> Result<Response<Body>> {
    let page = parse_page_params(query, PageTokenKind::MultipartUploads)?;
    let mut uploads = tokio::task::block_in_place(|| {
        bucket_service::list_multipart_uploads(storage.as_ref(), bucket)
    })?;
    uploads.sort_by_key(|upload| std::cmp::Reverse(upload.initiated));
    let uploads = uploads
        .into_iter()
        .filter(|upload| {
            contains_search(&upload.key, page.search.as_deref())
                || contains_search(&upload.upload_id, page.search.as_deref())
        })
        .collect::<Vec<_>>();
    let (items, next) = paginate(uploads, &page);

    Ok(json_response(
        StatusCode::OK,
        &crate::api::models::ListMultipartUploadsResponse {
            items,
            next: encode_next(next, PageTokenKind::MultipartUploads),
        },
    ))
}

async fn handle_multipart_upload_request<B>(
    storage: Arc<dyn Storage>,
    bucket: &str,
    remainder: &str,
    req: Request<B>,
) -> Result<Response<Body>>
where
    B: hyper::body::Body<Data = bytes::Bytes> + Send + 'static,
    B::Error: std::fmt::Display,
{
    let method = req.method().clone();
    let path = req.uri().path().to_string();
    let upload_id = remainder
        .strip_prefix("multipart-uploads/")
        .ok_or_else(|| Error::RouteNotFound(path.clone()))?;
    let upload_id = decode_component(upload_id);
    if upload_id.is_empty() {
        return Err(Error::InvalidRequest("Missing upload id".into()));
    }

    match method {
        Method::GET => get_multipart_upload(storage, bucket, &upload_id),
        Method::DELETE => abort_multipart_upload(storage, bucket, &upload_id),
        _ => Err(Error::MethodNotAllowed(format!("{} {}", method, path))),
    }
}

fn get_multipart_upload(
    storage: Arc<dyn Storage>,
    bucket: &str,
    upload_id: &str,
) -> Result<Response<Body>> {
    let upload = tokio::task::block_in_place(|| {
        object_service::get_multipart_upload(storage.as_ref(), bucket, upload_id)
    })?;
    Ok(json_response(StatusCode::OK, &upload))
}

fn abort_multipart_upload(
    storage: Arc<dyn Storage>,
    bucket: &str,
    upload_id: &str,
) -> Result<Response<Body>> {
    tokio::task::block_in_place(|| {
        object_service::abort_multipart_upload(storage.as_ref(), bucket, upload_id)
    })?;
    Ok(empty_response(StatusCode::NO_CONTENT))
}

async fn handle_object_request<B>(
    storage: Arc<dyn Storage>,
    bucket: &str,
    object_rest: &str,
    query: &str,
    req: Request<B>,
) -> Result<Response<Body>>
where
    B: hyper::body::Body<Data = bytes::Bytes> + Send + 'static,
    B::Error: std::fmt::Display,
{
    let method = req.method().clone();
    let path = req.uri().path().to_string();

    if object_rest.is_empty() {
        return Err(Error::InvalidRequest("Missing object key".into()));
    }

    if let Some(key) = object_rest.strip_suffix("/content") {
        let key = decode_component(key);
        return match method {
            Method::GET => download_object_content(storage, bucket, &key),
            Method::PUT => put_object_content(storage, bucket, &key, req).await,
            _ => Err(Error::MethodNotAllowed(format!("{} {}", method, path))),
        };
    }

    if let Some(key) = object_rest.strip_suffix("/versions") {
        let key = decode_component(key);
        return match method {
            Method::GET => list_object_versions(storage, bucket, &key, query),
            _ => Err(Error::MethodNotAllowed(format!("{} {}", method, path))),
        };
    }

    if let Some((key, version_id)) = object_rest.rsplit_once("/versions/") {
        let key = decode_component(key);
        let version_id = decode_component(version_id);
        if !key.is_empty() && !version_id.is_empty() {
            return match method {
                Method::DELETE => delete_object_version(storage, bucket, &key, &version_id),
                _ => Err(Error::MethodNotAllowed(format!("{} {}", method, path))),
            };
        }
    }

    if let Some(key) = object_rest.strip_suffix("/tags") {
        let key = decode_component(key);
        return match method {
            Method::GET => get_object_tags(storage, bucket, &key),
            Method::PUT => put_object_tags(storage, bucket, &key, req).await,
            _ => Err(Error::MethodNotAllowed(format!("{} {}", method, path))),
        };
    }

    if let Some(key) = object_rest.strip_suffix("/acl") {
        let key = decode_component(key);
        return match method {
            Method::GET => get_object_acl(storage, bucket, &key),
            Method::PUT => set_object_acl(storage, bucket, &key, req).await,
            _ => Err(Error::MethodNotAllowed(format!("{} {}", method, path))),
        };
    }

    let key = decode_component(object_rest);
    match method {
        Method::GET => get_object_metadata(storage, bucket, &key),
        Method::DELETE => delete_object(storage, bucket, &key),
        _ => Err(Error::MethodNotAllowed(format!("{} {}", method, path))),
    }
}

fn get_object_metadata(
    storage: Arc<dyn Storage>,
    bucket: &str,
    key: &str,
) -> Result<Response<Body>> {
    let object =
        tokio::task::block_in_place(|| object_service::get_object(storage.as_ref(), bucket, key))?;
    Ok(json_response(StatusCode::OK, &object_to_metadata(object)))
}

fn delete_object(storage: Arc<dyn Storage>, bucket: &str, key: &str) -> Result<Response<Body>> {
    tokio::task::block_in_place(|| object_service::delete_object(storage.as_ref(), bucket, key))?;
    Ok(empty_response(StatusCode::NO_CONTENT))
}

fn download_object_content(
    storage: Arc<dyn Storage>,
    bucket: &str,
    key: &str,
) -> Result<Response<Body>> {
    let obj =
        tokio::task::block_in_place(|| object_service::get_object(storage.as_ref(), bucket, key))?;
    let builder = Response::builder()
        .status(StatusCode::OK)
        .header("content-type", obj.content_type);
    Ok(builder
        .body(Body::from(obj.data))
        .unwrap_or_else(|_| Response::new(Body::default())))
}

async fn put_object_content<B>(
    storage: Arc<dyn Storage>,
    bucket: &str,
    key: &str,
    req: Request<B>,
) -> Result<Response<Body>>
where
    B: hyper::body::Body<Data = bytes::Bytes> + Send + 'static,
    B::Error: std::fmt::Display,
{
    if let Err(message) = validation::validate_blob_key(key) {
        return Err(Error::InvalidRequest(message));
    }
    let existed = tokio::task::block_in_place(|| {
        object_service::object_exists(storage.as_ref(), bucket, key)
    })?;
    let headers = req.headers().clone();
    let bytes = req
        .into_body()
        .collect()
        .await
        .map_err(|e| Error::InvalidRequest(e.to_string()))?
        .to_bytes();
    let content_type = headers
        .get("content-type")
        .and_then(|value| value.to_str().ok())
        .unwrap_or("application/octet-stream")
        .to_string();

    let mut metadata = HashMap::new();
    for (name, value) in headers.iter() {
        if let Some(stripped) = name.as_str().strip_prefix("x-amz-meta-") {
            if let Ok(value) = value.to_str() {
                metadata.insert(stripped.to_string(), value.to_string());
            }
        }
    }

    let object = crate::models::Object::new_with_metadata(
        key.to_string(),
        bytes.to_vec(),
        content_type,
        metadata,
    );
    tokio::task::block_in_place(|| {
        object_service::put_object(storage.as_ref(), bucket, key.to_string(), object)
    })?;
    let stored =
        tokio::task::block_in_place(|| object_service::get_object(storage.as_ref(), bucket, key))?;
    let status = if existed {
        StatusCode::OK
    } else {
        StatusCode::CREATED
    };
    Ok(json_response(status, &object_to_metadata(stored)))
}

fn list_object_versions(
    storage: Arc<dyn Storage>,
    bucket: &str,
    key: &str,
    query: &str,
) -> Result<Response<Body>> {
    let page = parse_page_params(query, PageTokenKind::Versions)?;
    let current_version_id =
        tokio::task::block_in_place(|| object_service::get_object(storage.as_ref(), bucket, key))
            .ok()
            .and_then(|object| object.version_id);
    let mut versions = tokio::task::block_in_place(|| {
        object_service::list_object_versions_for_key(storage.as_ref(), bucket, key)
    })?;
    versions.sort_by_key(|version| std::cmp::Reverse(version.last_modified));
    let versions = versions
        .into_iter()
        .filter(|object| {
            contains_search(&object.key, page.search.as_deref())
                || object
                    .version_id
                    .as_deref()
                    .map(|version_id| contains_search(version_id, page.search.as_deref()))
                    .unwrap_or(false)
        })
        .map(|object| {
            let version_id = object.version_id.clone().unwrap_or_default();
            crate::api::models::ObjectVersionInfo {
                key: object.key,
                version_id: version_id.clone(),
                size: object.size,
                last_modified: object.last_modified.to_rfc3339(),
                etag: object.etag,
                is_latest: current_version_id.as_deref() == Some(version_id.as_str()),
            }
        })
        .collect();
    let (items, next) = paginate(versions, &page);

    Ok(json_response(
        StatusCode::OK,
        &crate::api::models::ListVersionsResponse {
            items,
            next: encode_next(next, PageTokenKind::Versions),
        },
    ))
}

fn get_object_tags(storage: Arc<dyn Storage>, bucket: &str, key: &str) -> Result<Response<Body>> {
    let tags = tokio::task::block_in_place(|| {
        object_service::get_object_tags(storage.as_ref(), bucket, key)
    })?;
    Ok(json_response(
        StatusCode::OK,
        &crate::api::models::TagsResponse { tags },
    ))
}

fn get_object_acl(storage: Arc<dyn Storage>, bucket: &str, key: &str) -> Result<Response<Body>> {
    let acl = tokio::task::block_in_place(|| {
        object_service::get_object_acl(storage.as_ref(), bucket, key)
    })?;
    Ok(json_response(StatusCode::OK, &acl))
}

async fn set_object_acl<B>(
    storage: Arc<dyn Storage>,
    bucket: &str,
    key: &str,
    req: Request<B>,
) -> Result<Response<Body>>
where
    B: hyper::body::Body<Data = bytes::Bytes> + Send + 'static,
    B::Error: std::fmt::Display,
{
    if let Err(message) = validation::validate_blob_key(key) {
        return Err(Error::InvalidRequest(message));
    }
    let acl: crate::models::policy::Acl = read_json(req).await?;
    tokio::task::block_in_place(|| {
        object_service::put_object_acl(storage.as_ref(), bucket, key, acl.clone())
    })?;
    Ok(json_response(StatusCode::OK, &acl))
}

fn delete_object_version(
    storage: Arc<dyn Storage>,
    bucket: &str,
    key: &str,
    version_id: &str,
) -> Result<Response<Body>> {
    tokio::task::block_in_place(|| {
        object_service::delete_object_version(storage.as_ref(), bucket, key, version_id)
    })?;
    Ok(empty_response(StatusCode::NO_CONTENT))
}

async fn put_object_tags<B>(
    storage: Arc<dyn Storage>,
    bucket: &str,
    key: &str,
    req: Request<B>,
) -> Result<Response<Body>>
where
    B: hyper::body::Body<Data = bytes::Bytes> + Send + 'static,
    B::Error: std::fmt::Display,
{
    #[derive(Deserialize)]
    struct TagsReq {
        tags: HashMap<String, String>,
    }

    if let Err(message) = validation::validate_blob_key(key) {
        return Err(Error::InvalidRequest(message));
    }
    let body: TagsReq = read_json(req).await?;
    tokio::task::block_in_place(|| {
        object_service::put_object_tags(storage.as_ref(), bucket, key, body.tags.clone())
    })?;
    Ok(json_response(
        StatusCode::OK,
        &crate::api::models::TagsResponse { tags: body.tags },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::models::{
        BucketDetails, ErrorResponse, ListBucketsResponse, ListMultipartUploadsResponse,
        ListObjectsResponse, ListVersionsResponse, ObjectMetadata, TagsResponse, VersioningStatus,
    };
    use crate::storage::FilesystemStorage;
    use http_body_util::BodyExt;
    use hyper::Request;
    use serde::de::DeserializeOwned;
    use serde_json::Value;
    use std::fs;

    fn temp_storage() -> Arc<dyn Storage> {
        let dir = std::env::temp_dir().join(format!("sqrzl-test-{}", uuid::Uuid::new_v4()));
        let _ = fs::create_dir_all(&dir);
        Arc::new(FilesystemStorage::new(dir))
    }

    async fn call<B>(api_req: Request<B>, storage: Arc<dyn Storage>) -> Response<Body>
    where
        B: hyper::body::Body<Data = bytes::Bytes> + Send + 'static,
        B::Error: std::fmt::Display,
    {
        match handle_request(storage, api_req).await {
            Ok(resp) => resp,
            Err(err) => error_response(&err),
        }
    }

    async fn json_body<T: DeserializeOwned>(resp: Response<Body>) -> T {
        let bytes = resp
            .into_body()
            .collect()
            .await
            .expect("response body should read")
            .to_bytes();
        serde_json::from_slice(&bytes).expect("response body should deserialize")
    }

    async fn json_value(resp: Response<Body>) -> Value {
        json_body(resp).await
    }

    fn assert_json_content_type(resp: &Response<Body>) {
        assert_eq!(
            resp.headers()
                .get("content-type")
                .and_then(|value| value.to_str().ok()),
            Some("application/json; charset=utf-8")
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn admin_bucket_crud_json() {
        let storage = temp_storage();

        let req = Request::builder()
            .method(Method::POST)
            .uri("/admin/v1/buckets")
            .header("content-type", "application/json")
            .body(Body::from("{\"name\":\"demo\"}"))
            .unwrap();
        let resp = call(req, storage.clone()).await;

        assert_eq!(resp.status(), StatusCode::CREATED);
        assert_json_content_type(&resp);
        let created: BucketDetails = json_body(resp).await;
        assert_eq!(created.name, "demo");
        assert!(!created.versioning_enabled);

        let req = Request::builder()
            .method(Method::GET)
            .uri("/admin/v1/buckets/demo")
            .body(Body::default())
            .unwrap();
        let resp = call(req, storage.clone()).await;

        assert_eq!(resp.status(), StatusCode::OK);
        assert_json_content_type(&resp);
        let bucket: BucketDetails = json_body(resp).await;
        assert_eq!(bucket.name, "demo");
        assert!(!bucket.versioning_enabled);

        let req = Request::builder()
            .method(Method::PUT)
            .uri("/admin/v1/buckets/demo/versioning")
            .header("content-type", "application/json")
            .body(Body::from("{\"enabled\":true}"))
            .unwrap();
        let resp = call(req, storage.clone()).await;

        assert_eq!(resp.status(), StatusCode::OK);
        assert_json_content_type(&resp);
        let versioning: VersioningStatus = json_body(resp).await;
        assert!(versioning.enabled);

        let req = Request::builder()
            .method(Method::PUT)
            .uri("/admin/v1/buckets/demo/versioning")
            .header("content-type", "application/json")
            .body(Body::from("{\"enabled\":false}"))
            .unwrap();
        let resp = call(req, storage.clone()).await;

        assert_eq!(resp.status(), StatusCode::OK);
        let versioning: VersioningStatus = json_body(resp).await;
        assert!(!versioning.enabled);

        let req = Request::builder()
            .method(Method::DELETE)
            .uri("/admin/v1/buckets/demo")
            .body(Body::default())
            .unwrap();
        let resp = call(req, storage.clone()).await;

        assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn admin_object_content_and_tags() {
        let storage = temp_storage();

        let req = Request::builder()
            .method(Method::POST)
            .uri("/admin/v1/buckets")
            .header("content-type", "application/json")
            .body(Body::from("{\"name\":\"demo\"}"))
            .unwrap();
        let resp = call(req, storage.clone()).await;

        assert_eq!(resp.status(), StatusCode::CREATED);

        let req = Request::builder()
            .method(Method::PUT)
            .uri("/admin/v1/buckets/demo/objects/hello.txt/content")
            .header("content-type", "text/plain")
            .header("x-amz-meta-owner", "alice")
            .header("X-Amz-Meta-Environment", "dev")
            .body(Body::from("hello"))
            .unwrap();
        let resp = call(req, storage.clone()).await;

        assert_eq!(resp.status(), StatusCode::CREATED);
        assert_json_content_type(&resp);
        let uploaded: ObjectMetadata = json_body(resp).await;
        assert_eq!(uploaded.key, "hello.txt");

        let req = Request::builder()
            .method(Method::GET)
            .uri("/admin/v1/buckets/demo/objects/hello.txt")
            .body(Body::default())
            .unwrap();
        let resp = call(req, storage.clone()).await;

        assert_eq!(resp.status(), StatusCode::OK);
        assert_json_content_type(&resp);
        let metadata: ObjectMetadata = json_body(resp).await;
        assert_eq!(metadata.key, "hello.txt");
        assert_eq!(metadata.content_type.as_deref(), Some("text/plain"));
        assert_eq!(metadata.metadata.get("owner"), Some(&"alice".to_string()));
        assert_eq!(
            metadata.metadata.get("environment"),
            Some(&"dev".to_string())
        );

        let req = Request::builder()
            .method(Method::PUT)
            .uri("/admin/v1/buckets/demo/objects/hello.txt/tags")
            .header("content-type", "application/json")
            .body(Body::from("{\"tags\":{\"env\":\"dev\"}}"))
            .unwrap();
        let resp = call(req, storage.clone()).await;

        assert_eq!(resp.status(), StatusCode::OK);
        assert_json_content_type(&resp);
        let tags: TagsResponse = json_body(resp).await;
        assert_eq!(tags.tags.get("env"), Some(&"dev".to_string()));

        let req = Request::builder()
            .method(Method::PUT)
            .uri("/admin/v1/buckets/demo/objects/dir1/dir2/blobkey.png/content")
            .header("content-type", "text/plain")
            .body(Body::from("nested"))
            .unwrap();
        let resp = call(req, storage.clone()).await;

        assert_eq!(resp.status(), StatusCode::CREATED);
        assert_json_content_type(&resp);
        let nested: ObjectMetadata = json_body(resp).await;
        assert_eq!(nested.key, "dir1/dir2/blobkey.png");

        let req = Request::builder()
            .method(Method::GET)
            .uri("/admin/v1/buckets/demo/objects/dir1/dir2/blobkey.png")
            .body(Body::default())
            .unwrap();
        let resp = call(req, storage.clone()).await;

        assert_eq!(resp.status(), StatusCode::OK);
        assert_json_content_type(&resp);
        let nested_metadata: ObjectMetadata = json_body(resp).await;
        assert_eq!(nested_metadata.key, "dir1/dir2/blobkey.png");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn admin_bucket_control_plane_round_trips_acl_policy_lifecycle_and_multipart_uploads() {
        let storage = temp_storage();

        let req = Request::builder()
            .method(Method::POST)
            .uri("/admin/v1/buckets")
            .header("content-type", "application/json")
            .body(Body::from("{\"name\":\"demo\"}"))
            .unwrap();
        let resp = call(req, storage.clone()).await;
        assert_eq!(resp.status(), StatusCode::CREATED);

        let req = Request::builder()
            .method(Method::PUT)
            .uri("/admin/v1/buckets/demo/acl")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"canned":"public-read","grants":[]}"#))
            .unwrap();
        let resp = call(req, storage.clone()).await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_json_content_type(&resp);
        let acl = json_value(resp).await;
        assert_eq!(acl["canned"], "public-read");

        let req = Request::builder()
            .method(Method::GET)
            .uri("/admin/v1/buckets/demo/acl")
            .body(Body::default())
            .unwrap();
        let resp = call(req, storage.clone()).await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_json_content_type(&resp);
        let acl = json_value(resp).await;
        assert_eq!(acl["canned"], "public-read");

        let req = Request::builder()
            .method(Method::PUT)
            .uri("/admin/v1/buckets/demo/policy")
            .header("content-type", "application/json")
            .body(Body::from(
                r#"{"Version":"2012-10-17","Statement":[{"Sid":"allow-read","Effect":"Allow","Principal":"*","Action":"s3:GetObject","Resource":"arn:aws:s3:::demo/*"}]}"#,
            ))
            .unwrap();
        let resp = call(req, storage.clone()).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let policy = json_value(resp).await;
        assert_eq!(policy["Version"], "2012-10-17");
        assert_eq!(policy["Statement"][0]["Effect"], "Allow");

        let req = Request::builder()
            .method(Method::GET)
            .uri("/admin/v1/buckets/demo/policy")
            .body(Body::default())
            .unwrap();
        let resp = call(req, storage.clone()).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let policy = json_value(resp).await;
        assert_eq!(policy["Statement"][0]["Action"], "s3:GetObject");

        let req = Request::builder()
            .method(Method::DELETE)
            .uri("/admin/v1/buckets/demo/policy")
            .body(Body::default())
            .unwrap();
        let resp = call(req, storage.clone()).await;
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);

        let req = Request::builder()
            .method(Method::PUT)
            .uri("/admin/v1/buckets/demo/lifecycle")
            .header("content-type", "application/json")
            .body(Body::from(
                r#"{"rules":[{"id":"expire","status":"Enabled","filter":{"prefix":"logs/","tags":[]},"expiration":null,"noncurrent_version_expiration":null,"transitions":[]}]}"#,
            ))
            .unwrap();
        let resp = call(req, storage.clone()).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let lifecycle = json_value(resp).await;
        assert_eq!(lifecycle["rules"][0]["id"], "expire");

        let req = Request::builder()
            .method(Method::GET)
            .uri("/admin/v1/buckets/demo/lifecycle")
            .body(Body::default())
            .unwrap();
        let resp = call(req, storage.clone()).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let lifecycle = json_value(resp).await;
        assert_eq!(lifecycle["rules"][0]["status"], "Enabled");

        let req = Request::builder()
            .method(Method::DELETE)
            .uri("/admin/v1/buckets/demo/lifecycle")
            .body(Body::default())
            .unwrap();
        let resp = call(req, storage.clone()).await;
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);

        let upload = storage
            .as_ref()
            .create_multipart_upload("demo", "video.bin".to_string())
            .expect("multipart upload should create");
        storage
            .as_ref()
            .upload_part("demo", &upload.upload_id, 1, b"part-1".to_vec())
            .expect("multipart part should upload");

        let req = Request::builder()
            .method(Method::GET)
            .uri("/admin/v1/buckets/demo/multipart-uploads?limit=10")
            .body(Body::default())
            .unwrap();
        let resp = call(req, storage.clone()).await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_json_content_type(&resp);
        let uploads: ListMultipartUploadsResponse = json_body(resp).await;
        assert_eq!(uploads.items.len(), 1);
        assert_eq!(uploads.items[0].upload_id, upload.upload_id);

        let req = Request::builder()
            .method(Method::GET)
            .uri(format!(
                "/admin/v1/buckets/demo/multipart-uploads/{}",
                upload.upload_id
            ))
            .body(Body::default())
            .unwrap();
        let resp = call(req, storage.clone()).await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_json_content_type(&resp);
        let detailed: crate::models::MultipartUpload = json_body(resp).await;
        assert_eq!(detailed.parts.len(), 1);

        let req = Request::builder()
            .method(Method::DELETE)
            .uri(format!(
                "/admin/v1/buckets/demo/multipart-uploads/{}",
                upload.upload_id
            ))
            .body(Body::default())
            .unwrap();
        let resp = call(req, storage.clone()).await;
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn admin_object_acl_and_version_deletion_round_trip() {
        let storage = temp_storage();

        let req = Request::builder()
            .method(Method::POST)
            .uri("/admin/v1/buckets")
            .header("content-type", "application/json")
            .body(Body::from("{\"name\":\"demo\"}"))
            .unwrap();
        let resp = call(req, storage.clone()).await;
        assert_eq!(resp.status(), StatusCode::CREATED);

        let req = Request::builder()
            .method(Method::PUT)
            .uri("/admin/v1/buckets/demo/versioning")
            .header("content-type", "application/json")
            .body(Body::from("{\"enabled\":true}"))
            .unwrap();
        let resp = call(req, storage.clone()).await;
        assert_eq!(resp.status(), StatusCode::OK);

        for body in ["v1", "v2"] {
            let req = Request::builder()
                .method(Method::PUT)
                .uri("/admin/v1/buckets/demo/objects/versioned.txt/content")
                .header("content-type", "text/plain")
                .body(Body::from(body))
                .unwrap();
            let resp = call(req, storage.clone()).await;
            assert!(matches!(
                resp.status(),
                StatusCode::CREATED | StatusCode::OK
            ));
        }

        let req = Request::builder()
            .method(Method::PUT)
            .uri("/admin/v1/buckets/demo/objects/versioned.txt/acl")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"canned":"private","grants":[]}"#))
            .unwrap();
        let resp = call(req, storage.clone()).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let acl = json_value(resp).await;
        assert_eq!(acl["canned"], "private");

        let req = Request::builder()
            .method(Method::GET)
            .uri("/admin/v1/buckets/demo/objects/versioned.txt/acl")
            .body(Body::default())
            .unwrap();
        let resp = call(req, storage.clone()).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let acl = json_value(resp).await;
        assert_eq!(acl["canned"], "private");

        let req = Request::builder()
            .method(Method::GET)
            .uri("/admin/v1/buckets/demo/objects/versioned.txt/versions?limit=10")
            .body(Body::default())
            .unwrap();
        let resp = call(req, storage.clone()).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let versions: ListVersionsResponse = json_body(resp).await;
        assert!(versions.items.len() >= 2);

        let stale_version = versions
            .items
            .iter()
            .find(|version| !version.is_latest)
            .expect("expected a non-latest version")
            .version_id
            .clone();

        let req = Request::builder()
            .method(Method::DELETE)
            .uri(format!(
                "/admin/v1/buckets/demo/objects/versioned.txt/versions/{}",
                stale_version
            ))
            .body(Body::default())
            .unwrap();
        let resp = call(req, storage.clone()).await;
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);

        let req = Request::builder()
            .method(Method::GET)
            .uri("/admin/v1/buckets/demo/objects/versioned.txt/versions?limit=10")
            .body(Body::default())
            .unwrap();
        let resp = call(req, storage.clone()).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let versions_after: ListVersionsResponse = json_body(resp).await;
        assert!(versions_after.items.len() < versions.items.len());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn admin_lists_support_next_limit_and_search() {
        let storage = temp_storage();

        for bucket in ["alpha", "beta", "gamma"] {
            let req = Request::builder()
                .method(Method::POST)
                .uri("/admin/v1/buckets")
                .header("content-type", "application/json")
                .body(Body::from(format!("{{\"name\":\"{}\"}}", bucket)))
                .unwrap();
            let resp = call(req, storage.clone()).await;
            assert_eq!(resp.status(), StatusCode::CREATED);
        }

        let req = Request::builder()
            .method(Method::GET)
            .uri("/admin/v1/buckets?limit=1&search=a")
            .body(Body::default())
            .unwrap();
        let resp = call(req, storage.clone()).await;

        assert_eq!(resp.status(), StatusCode::OK);
        assert_json_content_type(&resp);
        let buckets: ListBucketsResponse = json_body(resp).await;
        assert_eq!(buckets.items.len(), 1);
        assert!(buckets.next.is_some());

        let req = Request::builder()
            .method(Method::POST)
            .uri("/admin/v1/buckets")
            .header("content-type", "application/json")
            .body(Body::from("{\"name\":\"demo\"}"))
            .unwrap();
        let resp = call(req, storage.clone()).await;
        assert_eq!(resp.status(), StatusCode::CREATED);

        let req = Request::builder()
            .method(Method::PUT)
            .uri("/admin/v1/buckets/demo/versioning")
            .header("content-type", "application/json")
            .body(Body::from("{\"enabled\":true}"))
            .unwrap();
        let resp = call(req, storage.clone()).await;
        assert_eq!(resp.status(), StatusCode::OK);

        for key in ["alpha.txt", "beta.txt", "gamma.bin"] {
            let req = Request::builder()
                .method(Method::PUT)
                .uri(format!("/admin/v1/buckets/demo/objects/{}/content", key))
                .header("content-type", "text/plain")
                .body(Body::from(key.to_string()))
                .unwrap();
            let resp = call(req, storage.clone()).await;
            assert!(matches!(
                resp.status(),
                StatusCode::CREATED | StatusCode::OK
            ));
        }

        for body in ["v1", "v2"] {
            let req = Request::builder()
                .method(Method::PUT)
                .uri("/admin/v1/buckets/demo/objects/versioned.txt/content")
                .header("content-type", "text/plain")
                .body(Body::from(body))
                .unwrap();
            let resp = call(req, storage.clone()).await;
            assert!(matches!(
                resp.status(),
                StatusCode::CREATED | StatusCode::OK
            ));
        }

        let req = Request::builder()
            .method(Method::GET)
            .uri("/admin/v1/buckets/demo/objects?limit=1&search=.txt")
            .body(Body::default())
            .unwrap();
        let resp = call(req, storage.clone()).await;

        assert_eq!(resp.status(), StatusCode::OK);
        assert_json_content_type(&resp);
        let objects: ListObjectsResponse = json_body(resp).await;
        assert!(objects.folders.is_empty());
        assert_eq!(objects.items.len(), 1);
        let next = objects.next.clone().expect("objects page should continue");

        let req = Request::builder()
            .method(Method::GET)
            .uri(format!(
                "/admin/v1/buckets/demo/objects?limit=1&search=.txt&next={}",
                next
            ))
            .body(Body::default())
            .unwrap();
        let resp = call(req, storage.clone()).await;

        assert_eq!(resp.status(), StatusCode::OK);
        let next_page: ListObjectsResponse = json_body(resp).await;
        assert!(next_page.folders.is_empty());
        assert_eq!(next_page.items.len(), 1);

        let req = Request::builder()
            .method(Method::GET)
            .uri("/admin/v1/buckets/demo/objects/versioned.txt/versions?limit=10&search=versioned")
            .body(Body::default())
            .unwrap();
        let resp = call(req, storage.clone()).await;

        assert_eq!(resp.status(), StatusCode::OK);
        assert_json_content_type(&resp);
        let versions: ListVersionsResponse = json_body(resp).await;
        assert!(versions.items.len() >= 2);
        assert!(versions.items.iter().any(|version| version.is_latest));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn admin_lists_support_prefix_filtering_and_search() {
        let storage = temp_storage();

        let req = Request::builder()
            .method(Method::POST)
            .uri("/admin/v1/buckets")
            .header("content-type", "application/json")
            .body(Body::from("{\"name\":\"demo\"}"))
            .unwrap();
        let resp = call(req, storage.clone()).await;
        assert_eq!(resp.status(), StatusCode::CREATED);

        for key in [
            "docs/api/openapi.json",
            "docs/readme.txt",
            "docs/spec.txt",
            "image.png",
        ] {
            let req = Request::builder()
                .method(Method::PUT)
                .uri(format!("/admin/v1/buckets/demo/objects/{}/content", key))
                .header("content-type", "text/plain")
                .body(Body::from(key.to_string()))
                .unwrap();
            let resp = call(req, storage.clone()).await;
            assert!(matches!(
                resp.status(),
                StatusCode::CREATED | StatusCode::OK
            ));
        }

        let req = Request::builder()
            .method(Method::GET)
            .uri("/admin/v1/buckets/demo/objects?limit=2&prefix=docs/")
            .body(Body::default())
            .unwrap();
        let resp = call(req, storage.clone()).await;

        assert_eq!(resp.status(), StatusCode::OK);
        assert_json_content_type(&resp);
        let first_prefixed: ListObjectsResponse = json_body(resp).await;
        assert_eq!(first_prefixed.folders.len(), 1);
        assert_eq!(first_prefixed.folders[0].name, "api/");
        assert_eq!(first_prefixed.folders[0].prefix, "docs/api/");
        assert_eq!(first_prefixed.items.len(), 1);
        assert_eq!(first_prefixed.items[0].key, "docs/readme.txt");
        let next = first_prefixed
            .next
            .clone()
            .expect("prefixed page should continue");

        let req = Request::builder()
            .method(Method::GET)
            .uri(format!(
                "/admin/v1/buckets/demo/objects?limit=2&prefix=docs/&next={}",
                next
            ))
            .body(Body::default())
            .unwrap();
        let resp = call(req, storage.clone()).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let second_prefixed: ListObjectsResponse = json_body(resp).await;

        assert_eq!(second_prefixed.items.len(), 1);
        assert_eq!(second_prefixed.items[0].key, "docs/spec.txt");
        assert!(second_prefixed.folders.is_empty());
        assert!(second_prefixed.next.is_none());

        let req = Request::builder()
            .method(Method::GET)
            .uri("/admin/v1/buckets/demo/objects?limit=1&prefix=docs/&search=.txt")
            .body(Body::default())
            .unwrap();
        let resp = call(req, storage.clone()).await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_json_content_type(&resp);
        let first_search: ListObjectsResponse = json_body(resp).await;
        assert!(first_search.folders.is_empty());
        assert_eq!(first_search.items.len(), 1);
        assert_eq!(first_search.items[0].key, "docs/readme.txt");
        let next = first_search
            .next
            .clone()
            .expect("search page should continue");

        let req = Request::builder()
            .method(Method::GET)
            .uri(format!(
                "/admin/v1/buckets/demo/objects?limit=1&prefix=docs/&search=.txt&next={}",
                next
            ))
            .body(Body::default())
            .unwrap();
        let resp = call(req, storage.clone()).await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_json_content_type(&resp);
        let second_search: ListObjectsResponse = json_body(resp).await;

        assert!(second_search.folders.is_empty());
        assert_eq!(second_search.items.len(), 1);
        assert_eq!(second_search.items[0].key, "docs/spec.txt");
        assert!(second_search.next.is_none());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn admin_object_search_finds_nested_matches_from_bucket_root() {
        let storage = temp_storage();

        let req = Request::builder()
            .method(Method::POST)
            .uri("/admin/v1/buckets")
            .header("content-type", "application/json")
            .body(Body::from("{\"name\":\"demo\"}"))
            .unwrap();
        let resp = call(req, storage.clone()).await;
        assert_eq!(resp.status(), StatusCode::CREATED);

        for key in [
            "archive/2025/notes.md",
            "archive/2026/report.txt",
            "root.txt",
        ] {
            let req = Request::builder()
                .method(Method::PUT)
                .uri(format!("/admin/v1/buckets/demo/objects/{}/content", key))
                .header("content-type", "text/plain")
                .body(Body::from(key.to_string()))
                .unwrap();
            let resp = call(req, storage.clone()).await;
            assert!(matches!(
                resp.status(),
                StatusCode::CREATED | StatusCode::OK
            ));
        }

        let req = Request::builder()
            .method(Method::GET)
            .uri("/admin/v1/buckets/demo/objects?limit=1&search=report")
            .body(Body::default())
            .unwrap();
        let resp = call(req, storage.clone()).await;

        assert_eq!(resp.status(), StatusCode::OK);
        assert_json_content_type(&resp);
        let search: ListObjectsResponse = json_body(resp).await;
        assert!(search.folders.is_empty());
        assert_eq!(search.items.len(), 1);
        assert_eq!(search.items[0].key, "archive/2026/report.txt");
        assert!(search.next.is_none());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn admin_lists_indexed_directories_without_scanning_first_blob_page() {
        let storage = temp_storage();

        let req = Request::builder()
            .method(Method::POST)
            .uri("/admin/v1/buckets")
            .header("content-type", "application/json")
            .body(Body::from("{\"name\":\"demo\"}"))
            .unwrap();
        let resp = call(req, storage.clone()).await;
        assert_eq!(resp.status(), StatusCode::CREATED);

        for index in 0..75 {
            let key = format!("a/blob-{index:03}.txt");
            let req = Request::builder()
                .method(Method::PUT)
                .uri(format!("/admin/v1/buckets/demo/objects/{}/content", key))
                .header("content-type", "text/plain")
                .body(Body::from(key))
                .unwrap();
            let resp = call(req, storage.clone()).await;
            assert!(matches!(
                resp.status(),
                StatusCode::CREATED | StatusCode::OK
            ));
        }

        let req = Request::builder()
            .method(Method::PUT)
            .uri("/admin/v1/buckets/demo/objects/z/blob.txt/content")
            .header("content-type", "text/plain")
            .body(Body::from("z"))
            .unwrap();
        let resp = call(req, storage.clone()).await;
        assert!(matches!(
            resp.status(),
            StatusCode::CREATED | StatusCode::OK
        ));

        let req = Request::builder()
            .method(Method::GET)
            .uri("/admin/v1/buckets/demo/objects?limit=2")
            .body(Body::default())
            .unwrap();
        let resp = call(req, storage.clone()).await;

        assert_eq!(resp.status(), StatusCode::OK);
        assert_json_content_type(&resp);
        let listing: ListObjectsResponse = json_body(resp).await;
        let folders = listing
            .folders
            .iter()
            .map(|folder| folder.prefix.as_str())
            .collect::<Vec<_>>();
        assert_eq!(folders, vec!["a/", "z/"]);
        assert!(listing.items.is_empty());
        assert!(listing.next.is_none());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn admin_reports_method_and_route_errors() {
        let storage = temp_storage();

        let req = Request::builder()
            .method(Method::POST)
            .uri("/admin/v1/buckets/demo")
            .body(Body::default())
            .unwrap();
        let resp = call(req, storage.clone()).await;

        assert_eq!(resp.status(), StatusCode::METHOD_NOT_ALLOWED);
        assert_json_content_type(&resp);
        let error: ErrorResponse = json_body(resp).await;
        assert_eq!(error.code, "MethodNotAllowed");
        assert_eq!(error.error, "Method not allowed");

        let req = Request::builder()
            .method(Method::GET)
            .uri("/admin/v1/does-not-exist")
            .body(Body::default())
            .unwrap();
        let resp = call(req, storage.clone()).await;

        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        let error: ErrorResponse = json_body(resp).await;
        assert_eq!(error.code, "NotFound");
        assert_eq!(error.error, "Route not found");
    }
}
