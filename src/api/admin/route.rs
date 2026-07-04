use super::pagination::decode_component;
use crate::error::{Error, Result};
use crate::utils::validation;

#[derive(Debug)]
pub(super) enum Route {
    Buckets,
    Bucket {
        bucket: String,
        resource: BucketResource,
    },
}

#[derive(Debug)]
pub(super) enum BucketResource {
    Root,
    Versioning,
    Acl,
    Policy,
    Lifecycle,
    MultipartUploads,
    MultipartUpload {
        upload_id: String,
    },
    Objects,
    Object {
        key: String,
        resource: ObjectResource,
    },
}

#[derive(Debug)]
pub(super) enum ObjectResource {
    Metadata,
    Content,
    Versions,
    Version { version_id: String },
    Tags,
    Acl,
}

pub(super) fn parse(path: &str) -> Result<Route> {
    let admin_path = path
        .strip_prefix("/admin/v1")
        .ok_or_else(|| Error::RouteNotFound(path.to_string()))?;

    if admin_path == "/buckets" {
        return Ok(Route::Buckets);
    }

    let Some(rest) = admin_path.strip_prefix("/buckets/") else {
        return Err(Error::RouteNotFound(path.to_string()));
    };

    let (bucket, remainder) = parse_bucket_and_remainder(rest)?;
    let resource = parse_bucket_resource(remainder, path)?;

    Ok(Route::Bucket { bucket, resource })
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

fn parse_bucket_resource(remainder: Option<&str>, path: &str) -> Result<BucketResource> {
    let Some(remainder) = remainder else {
        return Ok(BucketResource::Root);
    };

    match remainder {
        "versioning" => Ok(BucketResource::Versioning),
        "acl" => Ok(BucketResource::Acl),
        "policy" => Ok(BucketResource::Policy),
        "lifecycle" => Ok(BucketResource::Lifecycle),
        "multipart-uploads" => Ok(BucketResource::MultipartUploads),
        "objects" => Ok(BucketResource::Objects),
        _ if remainder.starts_with("multipart-uploads/") => {
            parse_multipart_upload_resource(remainder, path)
        }
        _ if remainder.starts_with("objects/") => {
            parse_object_resource(remainder.trim_start_matches("objects/"))
        }
        _ => Err(Error::RouteNotFound(path.to_string())),
    }
}

fn parse_multipart_upload_resource(remainder: &str, path: &str) -> Result<BucketResource> {
    let upload_id = remainder
        .strip_prefix("multipart-uploads/")
        .ok_or_else(|| Error::RouteNotFound(path.to_string()))?;
    let upload_id = decode_component(upload_id);
    if upload_id.is_empty() {
        return Err(Error::InvalidRequest("Missing upload id".into()));
    }

    Ok(BucketResource::MultipartUpload { upload_id })
}

fn parse_object_resource(object_rest: &str) -> Result<BucketResource> {
    if object_rest.is_empty() {
        return Err(Error::InvalidRequest("Missing object key".into()));
    }

    if let Some(key) = object_rest.strip_suffix("/content") {
        return Ok(BucketResource::Object {
            key: decode_component(key),
            resource: ObjectResource::Content,
        });
    }

    if let Some(key) = object_rest.strip_suffix("/versions") {
        return Ok(BucketResource::Object {
            key: decode_component(key),
            resource: ObjectResource::Versions,
        });
    }

    if let Some((key, version_id)) = object_rest.rsplit_once("/versions/") {
        let key = decode_component(key);
        let version_id = decode_component(version_id);
        if !key.is_empty() && !version_id.is_empty() {
            return Ok(BucketResource::Object {
                key,
                resource: ObjectResource::Version { version_id },
            });
        }
    }

    if let Some(key) = object_rest.strip_suffix("/tags") {
        return Ok(BucketResource::Object {
            key: decode_component(key),
            resource: ObjectResource::Tags,
        });
    }

    if let Some(key) = object_rest.strip_suffix("/acl") {
        return Ok(BucketResource::Object {
            key: decode_component(key),
            resource: ObjectResource::Acl,
        });
    }

    Ok(BucketResource::Object {
        key: decode_component(object_rest),
        resource: ObjectResource::Metadata,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_bucket_control_plane_route() {
        let route = parse("/admin/v1/buckets/demo/versioning").unwrap();

        let Route::Bucket { bucket, resource } = route else {
            panic!("expected bucket route");
        };
        assert_eq!(bucket, "demo");
        assert!(matches!(resource, BucketResource::Versioning));
    }

    #[test]
    fn parses_object_content_route_with_decoded_key() {
        let route = parse("/admin/v1/buckets/demo/objects/folder%2Ffile.txt/content").unwrap();

        let Route::Bucket {
            bucket,
            resource:
                BucketResource::Object {
                    key,
                    resource: ObjectResource::Content,
                },
        } = route
        else {
            panic!("expected object content route");
        };
        assert_eq!(bucket, "demo");
        assert_eq!(key, "folder/file.txt");
    }

    #[test]
    fn rejects_missing_multipart_upload_id() {
        let err = parse("/admin/v1/buckets/demo/multipart-uploads/").unwrap_err();

        assert!(matches!(err, Error::InvalidRequest(message) if message == "Missing upload id"));
    }
}
