use crate::services::bucket as bucket_service;

pub(super) fn object_to_metadata(obj: crate::models::Object) -> crate::api::models::ObjectMetadata {
    crate::api::models::ObjectMetadata {
        key: obj.key,
        size: obj.size,
        last_modified: obj.last_modified.to_rfc3339(),
        etag: obj.etag,
        content_type: Some(obj.content_type),
        metadata: obj.metadata,
        version_id: obj.version_id,
        storage_class: obj.storage_class,
    }
}

pub(super) fn bucket_to_details(
    bucket: crate::models::Bucket,
) -> crate::api::models::BucketDetails {
    let versioning_enabled = bucket_service::versioning_enabled(&bucket);
    crate::api::models::BucketDetails {
        name: bucket.name,
        created_at: bucket.created_at.to_rfc3339(),
        versioning_enabled,
    }
}

pub(super) fn bucket_to_info(bucket: crate::models::Bucket) -> crate::api::models::BucketInfo {
    let versioning_enabled = bucket_service::versioning_enabled(&bucket);
    crate::api::models::BucketInfo {
        name: bucket.name,
        created_at: bucket.created_at.to_rfc3339(),
        versioning_enabled,
    }
}

pub(super) fn object_to_info(object: crate::models::Object) -> crate::api::models::ObjectInfo {
    crate::api::models::ObjectInfo {
        key: object.key,
        size: object.size,
        last_modified: object.last_modified.to_rfc3339(),
        etag: object.etag,
        content_type: Some(object.content_type),
        storage_class: object.storage_class,
    }
}

pub(super) fn common_prefix_to_folder_info(
    prefix: String,
    path_prefix: &str,
) -> crate::api::models::ObjectFolderInfo {
    let name = prefix
        .strip_prefix(path_prefix)
        .unwrap_or(&prefix)
        .to_string();

    crate::api::models::ObjectFolderInfo { name, prefix }
}
