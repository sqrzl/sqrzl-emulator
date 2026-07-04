use super::helpers::{
    apply_bucket_cors_headers, metadata_value, with_bucket_metadata, S3_CORS_XML_KEY,
    S3_REQUEST_PAYMENT_KEY, S3_WEBSITE_XML_KEY,
};
use super::{
    acl, bucket_service, check_authorization, header_utils, validation, xml_error_response,
    xml_utils, AuthConfig, Body, ResponseBuilder, Storage,
};
use crate::error::Error;
use crate::server::http::Request;
use http::StatusCode;
use hyper::Response;
use std::sync::Arc;

pub async fn bucket_put(
    storage: Arc<dyn Storage>,
    auth_config: Arc<AuthConfig>,
    bucket: &str,
    req: &Request,
    req_id: String,
) -> Result<Response<Body>, String> {
    if let Err(response) = check_authorization(
        req,
        &auth_config,
        &storage,
        bucket,
        None,
        bucket_put_action(req),
    ) {
        return Ok(response);
    }

    if let Err(error) = validation::validate_bucket_name(bucket) {
        return Ok(xml_error_response(
            StatusCode::BAD_REQUEST,
            "InvalidBucketName",
            &error,
            &req_id,
        ));
    }

    if req.has_query_param("lifecycle") {
        return put_lifecycle(&storage, bucket, req, &req_id);
    }

    if req.has_query_param("requestPayment") {
        return put_request_payment(&storage, bucket, req, &req_id);
    }

    if req.has_query_param("website") {
        return put_metadata_document(&storage, bucket, req, &req_id, S3_WEBSITE_XML_KEY);
    }

    if req.has_query_param("cors") {
        return put_metadata_document(&storage, bucket, req, &req_id, S3_CORS_XML_KEY);
    }

    if req.has_query_param("versioning") {
        return put_versioning(&storage, bucket, req, &req_id);
    }

    if req.has_query_param("acl") {
        return Ok(put_acl(&storage, bucket, req, &req_id));
    }

    if req.has_query_param("policy") {
        return put_policy(&storage, bucket, req, &req_id);
    }

    create_bucket(&storage, bucket, &req_id)
}

fn bucket_put_action(req: &Request) -> &'static str {
    if req.has_query_param("versioning") {
        "s3:PutBucketVersioning"
    } else if req.has_query_param("lifecycle") {
        "s3:PutLifecycleConfiguration"
    } else if req.has_query_param("requestPayment") {
        "s3:PutBucketRequestPayment"
    } else if req.has_query_param("website") {
        "s3:PutBucketWebsite"
    } else if req.has_query_param("cors") {
        "s3:PutBucketCors"
    } else if req.has_query_param("acl") {
        "s3:PutBucketAcl"
    } else if req.has_query_param("policy") {
        "s3:PutBucketPolicy"
    } else {
        "s3:CreateBucket"
    }
}

fn put_lifecycle(
    storage: &Arc<dyn Storage>,
    bucket: &str,
    req: &Request,
    req_id: &str,
) -> Result<Response<Body>, String> {
    let body = request_body_text(req)?;
    let config = match xml_utils::parse_lifecycle_xml(&body) {
        Ok(config) => config,
        Err(message) => {
            return Ok(xml_error_response(
                StatusCode::BAD_REQUEST,
                "MalformedXML",
                &message,
                req_id,
            ));
        }
    };

    let result = tokio::task::block_in_place(|| {
        bucket_service::put_bucket_lifecycle(storage.as_ref(), bucket, config)
    });
    Ok(bucket_write_response(
        result,
        storage.as_ref(),
        bucket,
        req,
        req_id,
    ))
}

fn put_request_payment(
    storage: &Arc<dyn Storage>,
    bucket: &str,
    req: &Request,
    req_id: &str,
) -> Result<Response<Body>, String> {
    let body = request_body_text(req)?;
    let payer = metadata_value(&body, b"Payer").unwrap_or_default();

    if payer != "Requester" && payer != "BucketOwner" {
        return Ok(xml_error_response(
            StatusCode::BAD_REQUEST,
            "MalformedXML",
            "RequestPaymentConfiguration must contain a valid Payer value",
            req_id,
        ));
    }

    let result = tokio::task::block_in_place(|| {
        with_bucket_metadata(storage.as_ref(), bucket, |metadata| {
            metadata.insert(S3_REQUEST_PAYMENT_KEY.to_string(), payer);
        })
    });
    Ok(bucket_write_response(
        result.map(|_| ()),
        storage.as_ref(),
        bucket,
        req,
        req_id,
    ))
}

fn put_metadata_document(
    storage: &Arc<dyn Storage>,
    bucket: &str,
    req: &Request,
    req_id: &str,
    metadata_key: &'static str,
) -> Result<Response<Body>, String> {
    let body = request_body_text(req)?;
    let result = tokio::task::block_in_place(|| {
        with_bucket_metadata(storage.as_ref(), bucket, |metadata| {
            metadata.insert(metadata_key.to_string(), body);
        })
    });
    Ok(bucket_write_response(
        result.map(|_| ()),
        storage.as_ref(),
        bucket,
        req,
        req_id,
    ))
}

fn put_versioning(
    storage: &Arc<dyn Storage>,
    bucket: &str,
    req: &Request,
    req_id: &str,
) -> Result<Response<Body>, String> {
    let body = request_body_text(req)?;
    let enabled = match xml_utils::parse_versioning_xml(&body) {
        Ok(enabled) => enabled,
        Err(message) => {
            return Ok(xml_error_response(
                StatusCode::BAD_REQUEST,
                "MalformedXML",
                &message,
                req_id,
            ));
        }
    };

    let result = tokio::task::block_in_place(|| {
        bucket_service::set_versioning(storage.as_ref(), bucket, enabled)
    });
    Ok(bucket_write_response(
        result,
        storage.as_ref(),
        bucket,
        req,
        req_id,
    ))
}

fn put_acl(
    storage: &Arc<dyn Storage>,
    bucket: &str,
    req: &Request,
    req_id: &str,
) -> Response<Body> {
    let acl = match if req.body.is_empty() {
        acl::acl_from_headers(req).map_err(|message| ("InvalidArgument", message))
    } else {
        acl::acl_from_xml_body(&req.body).map_err(|message| ("MalformedXML", message))
    } {
        Ok(acl) => acl,
        Err((code, message)) => {
            return xml_error_response(StatusCode::BAD_REQUEST, code, &message, req_id);
        }
    };

    let result = tokio::task::block_in_place(|| {
        bucket_service::put_bucket_acl(storage.as_ref(), bucket, acl)
    });
    bucket_write_response(result, storage.as_ref(), bucket, req, req_id)
}

fn put_policy(
    storage: &Arc<dyn Storage>,
    bucket: &str,
    req: &Request,
    req_id: &str,
) -> Result<Response<Body>, String> {
    let body = request_body_text(req)?;
    let policy: crate::models::policy::BucketPolicyDocument =
        serde_json::from_str(&body).map_err(|error| format!("Invalid JSON policy: {error}"))?;
    let result = tokio::task::block_in_place(|| {
        bucket_service::put_bucket_policy(storage.as_ref(), bucket, policy)
    });
    Ok(bucket_write_response(
        result,
        storage.as_ref(),
        bucket,
        req,
        req_id,
    ))
}

fn create_bucket(
    storage: &Arc<dyn Storage>,
    bucket: &str,
    req_id: &str,
) -> Result<Response<Body>, String> {
    tokio::task::block_in_place(|| {
        bucket_service::create_bucket(storage.as_ref(), bucket.to_string())
    })?;
    Ok(ResponseBuilder::new(StatusCode::OK)
        .header("x-amz-request-id", req_id)
        .header("x-amz-id-2", &header_utils::generate_request_id())
        .empty())
}

fn bucket_write_response<T>(
    result: crate::error::Result<T>,
    storage: &dyn Storage,
    bucket: &str,
    req: &Request,
    req_id: &str,
) -> Response<Body> {
    match result {
        Ok(_) => empty_bucket_success(storage, bucket, req, req_id),
        Err(Error::BucketNotFound) => xml_error_response(
            StatusCode::NOT_FOUND,
            "NoSuchBucket",
            "Bucket not found",
            req_id,
        ),
        Err(error) => xml_error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "InternalError",
            &error.to_string(),
            req_id,
        ),
    }
}

fn empty_bucket_success(
    storage: &dyn Storage,
    bucket: &str,
    req: &Request,
    req_id: &str,
) -> Response<Body> {
    apply_bucket_cors_headers(
        storage,
        bucket,
        req,
        ResponseBuilder::new(StatusCode::OK)
            .header("x-amz-request-id", req_id)
            .header("x-amz-id-2", &header_utils::generate_request_id()),
        None,
    )
    .empty()
}

fn request_body_text(req: &Request) -> Result<String, String> {
    String::from_utf8(req.body.to_vec()).map_err(|error| format!("Invalid UTF-8 body: {error}"))
}
