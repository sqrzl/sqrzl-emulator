use crate::error::Result;
use crate::models::{ListObjectsResult, MultipartUpload, Object, Part};
use crate::storage::{
    AclStore, MultipartStore, ObjectListingStore, ObjectStore, TagStore, VersionStore,
};
use std::collections::HashMap;
use std::hash::BuildHasher;

///
/// # Errors
///
/// Returns an error when the underlying emulator operation fails.
pub fn list_objects(
    storage: &(impl ObjectListingStore + ?Sized),
    bucket: &str,
    prefix: Option<&str>,
    delimiter: Option<&str>,
    marker: Option<&str>,
    max_keys: Option<usize>,
) -> Result<ListObjectsResult> {
    storage.list_objects(bucket, prefix, delimiter, marker, max_keys)
}

///
/// # Errors
///
/// Returns an error when the underlying emulator operation fails.
pub fn get_object(
    storage: &(impl ObjectStore + ?Sized),
    bucket: &str,
    key: &str,
) -> Result<Object> {
    storage.get_object(bucket, key)
}

/// Reads object attributes without materializing payload bytes when the
/// selected storage backend supports a metadata-only path.
///
/// # Errors
///
/// Returns an error when the underlying emulator operation fails.
pub fn get_object_metadata(
    storage: &(impl ObjectStore + ?Sized),
    bucket: &str,
    key: &str,
) -> Result<Object> {
    storage.get_object_metadata(bucket, key)
}

///
/// # Errors
///
/// Returns an error when the underlying emulator operation fails.
pub fn get_object_version(
    storage: &(impl VersionStore + ?Sized),
    bucket: &str,
    key: &str,
    version_id: &str,
) -> Result<Object> {
    storage.get_object_version(bucket, key, version_id)
}

///
/// # Errors
///
/// Returns an error when the underlying emulator operation fails.
pub fn get_object_range(
    storage: &(impl ObjectStore + ?Sized),
    bucket: &str,
    key: &str,
    start: u64,
    end: Option<u64>,
) -> Result<(Object, Vec<u8>)> {
    storage.get_object_range(bucket, key, start, end)
}

///
/// # Errors
///
/// Returns an error when the underlying emulator operation fails.
pub fn object_exists(
    storage: &(impl ObjectStore + ?Sized),
    bucket: &str,
    key: &str,
) -> Result<bool> {
    storage.object_exists(bucket, key)
}

///
/// # Errors
///
/// Returns an error when the underlying emulator operation fails.
pub fn put_object(
    storage: &(impl ObjectStore + ?Sized),
    bucket: &str,
    key: String,
    object: Object,
) -> Result<()> {
    storage.put_object(bucket, key, object)
}

///
/// # Errors
///
/// Returns an error when the underlying emulator operation fails.
pub fn delete_object(storage: &(impl ObjectStore + ?Sized), bucket: &str, key: &str) -> Result<()> {
    storage.delete_object(bucket, key)
}

///
/// # Errors
///
/// Returns an error when the underlying emulator operation fails.
pub fn delete_object_version(
    storage: &(impl VersionStore + ?Sized),
    bucket: &str,
    key: &str,
    version_id: &str,
) -> Result<()> {
    storage.delete_object_version(bucket, key, version_id)
}

///
/// # Errors
///
/// Returns an error when the underlying emulator operation fails.
pub fn list_object_versions(
    storage: &(impl VersionStore + ?Sized),
    bucket: &str,
    prefix: Option<&str>,
) -> Result<Vec<Object>> {
    storage.list_object_versions(bucket, prefix)
}

///
/// # Errors
///
/// Returns an error when the underlying emulator operation fails.
pub fn list_object_versions_for_key(
    storage: &(impl VersionStore + ?Sized),
    bucket: &str,
    key: &str,
) -> Result<Vec<Object>> {
    storage.list_object_versions_for_key(bucket, key)
}

///
/// # Errors
///
/// Returns an error when the underlying emulator operation fails.
pub fn get_object_tags(
    storage: &(impl TagStore + ?Sized),
    bucket: &str,
    key: &str,
) -> Result<HashMap<String, String>> {
    storage.get_object_tags(bucket, key)
}

///
/// # Errors
///
/// Returns an error when the underlying emulator operation fails.
pub fn put_object_tags(
    storage: &(impl TagStore + ?Sized),
    bucket: &str,
    key: &str,
    tags: HashMap<String, String, impl BuildHasher>,
) -> Result<()> {
    storage.put_object_tags(bucket, key, tags.into_iter().collect())
}

///
/// # Errors
///
/// Returns an error when the underlying emulator operation fails.
pub fn delete_object_tags(
    storage: &(impl TagStore + ?Sized),
    bucket: &str,
    key: &str,
) -> Result<()> {
    storage.delete_object_tags(bucket, key)
}

///
/// # Errors
///
/// Returns an error when the underlying emulator operation fails.
pub fn get_object_acl(
    storage: &(impl AclStore + ?Sized),
    bucket: &str,
    key: &str,
) -> Result<crate::models::policy::Acl> {
    storage.get_object_acl(bucket, key)
}

///
/// # Errors
///
/// Returns an error when the underlying emulator operation fails.
pub fn put_object_acl(
    storage: &(impl AclStore + ?Sized),
    bucket: &str,
    key: &str,
    acl: crate::models::policy::Acl,
) -> Result<()> {
    storage.put_object_acl(bucket, key, acl)
}

///
/// # Errors
///
/// Returns an error when the underlying emulator operation fails.
pub fn list_parts(
    storage: &(impl MultipartStore + ?Sized),
    bucket: &str,
    upload_id: &str,
) -> Result<Vec<Part>> {
    storage.list_parts(bucket, upload_id)
}

///
/// # Errors
///
/// Returns an error when the underlying emulator operation fails.
pub fn get_multipart_upload(
    storage: &(impl MultipartStore + ?Sized),
    bucket: &str,
    upload_id: &str,
) -> Result<MultipartUpload> {
    storage.get_multipart_upload(bucket, upload_id)
}

///
/// # Errors
///
/// Returns an error when the underlying emulator operation fails.
pub fn upload_part(
    storage: &(impl MultipartStore + ?Sized),
    bucket: &str,
    upload_id: &str,
    part_number: u32,
    data: Vec<u8>,
) -> Result<String> {
    storage.upload_part(bucket, upload_id, part_number, data)
}

///
/// # Errors
///
/// Returns an error when the underlying emulator operation fails.
pub fn upload_part_streamed(
    storage: &(impl MultipartStore + ?Sized),
    bucket: &str,
    upload_id: &str,
    part_number: u32,
    payload_path: &std::path::Path,
    len: u64,
    etag: String,
) -> Result<String> {
    storage.upload_part_streamed(bucket, upload_id, part_number, payload_path, len, etag)
}

///
/// # Errors
///
/// Returns an error when the underlying emulator operation fails.
pub fn list_multipart_uploads(
    storage: &(impl MultipartStore + ?Sized),
    bucket: &str,
) -> Result<Vec<MultipartUpload>> {
    storage.list_multipart_uploads(bucket)
}

///
/// # Errors
///
/// Returns an error when the underlying emulator operation fails.
pub fn create_multipart_upload(
    storage: &(impl MultipartStore + ?Sized),
    bucket: &str,
    key: String,
) -> Result<MultipartUpload> {
    storage.create_multipart_upload(bucket, key)
}

/// Creates an S3 multipart upload while preserving object attributes from
/// the initiation request and attaching S3 part/object size constraints.
///
/// # Errors
///
/// Returns an error when the underlying emulator operation fails.
pub fn create_s3_multipart_upload_with_metadata<MS, PS>(
    storage: &(impl MultipartStore + ?Sized),
    bucket: &str,
    key: String,
    content_type: Option<String>,
    metadata: HashMap<String, String, MS>,
    provider_metadata: HashMap<String, String, PS>,
) -> Result<MultipartUpload>
where
    MS: BuildHasher,
    PS: BuildHasher,
{
    let metadata = metadata.into_iter().collect();
    let mut provider_metadata = provider_metadata.into_iter().collect::<HashMap<_, _>>();
    provider_metadata.insert(
        crate::storage::MULTIPART_MIN_NON_FINAL_PART_SIZE_KEY.to_string(),
        crate::storage::S3_MINIMUM_NON_FINAL_PART_SIZE.to_string(),
    );
    provider_metadata.insert(
        crate::storage::MULTIPART_MAX_PART_SIZE_KEY.to_string(),
        crate::storage::S3_MAXIMUM_PART_SIZE.to_string(),
    );
    provider_metadata.insert(
        crate::storage::MULTIPART_MAX_OBJECT_SIZE_KEY.to_string(),
        crate::storage::S3_MAXIMUM_OBJECT_SIZE.to_string(),
    );
    storage.create_multipart_upload_with_metadata(
        bucket,
        key,
        content_type,
        metadata,
        provider_metadata,
    )
}

///
/// # Errors
///
/// Returns an error when the underlying emulator operation fails.
pub fn complete_multipart_upload(
    storage: &(impl MultipartStore + ?Sized),
    bucket: &str,
    upload_id: &str,
) -> Result<String> {
    storage.complete_multipart_upload(bucket, upload_id)
}

///
/// # Errors
///
/// Returns an error when the multipart completion manifest is invalid.
pub fn complete_multipart_upload_with_parts(
    storage: &(impl MultipartStore + ?Sized),
    bucket: &str,
    upload_id: &str,
    parts: &[(u32, String)],
) -> Result<String> {
    storage.complete_multipart_upload_with_parts(bucket, upload_id, parts)
}

///
/// # Errors
///
/// Returns an error when the underlying emulator operation fails.
pub fn abort_multipart_upload(
    storage: &(impl MultipartStore + ?Sized),
    bucket: &str,
    upload_id: &str,
) -> Result<()> {
    storage.abort_multipart_upload(bucket, upload_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::{FilesystemStorage, Storage};
    use std::fs;
    use std::sync::Arc;

    fn temp_storage() -> Arc<dyn Storage> {
        let dir = std::env::temp_dir().join(format!(
            "sqrzl-service-object-test-{}",
            uuid::Uuid::new_v4()
        ));
        let _ = fs::create_dir_all(&dir);
        Arc::new(FilesystemStorage::new(dir))
    }

    #[test]
    fn should_roundtrip_object_through_service() {
        let storage = temp_storage();

        // Arrange
        storage.create_bucket("bucket".to_string()).unwrap();

        let mut object = Object::new(
            "key.txt".to_string(),
            b"hello".to_vec(),
            "text/plain".to_string(),
        );
        object.tags.insert("env".to_string(), "dev".to_string());

        // Act
        put_object(storage.as_ref(), "bucket", "key.txt".to_string(), object).unwrap();

        let stored = get_object(storage.as_ref(), "bucket", "key.txt").unwrap();
        assert_eq!(stored.data, b"hello".to_vec());
        assert_eq!(stored.tags.get("env"), Some(&"dev".to_string()));

        let tags = get_object_tags(storage.as_ref(), "bucket", "key.txt").unwrap();
        assert_eq!(tags.get("env"), Some(&"dev".to_string()));

        // Assert
        delete_object_tags(storage.as_ref(), "bucket", "key.txt").unwrap();
        assert!(get_object_tags(storage.as_ref(), "bucket", "key.txt")
            .unwrap()
            .is_empty());
    }

    #[test]
    fn should_list_object_versions_through_service() {
        let storage = temp_storage();

        // Arrange
        storage.create_bucket("bucket".to_string()).unwrap();
        storage.enable_versioning("bucket").unwrap();

        // Act
        put_object(
            storage.as_ref(),
            "bucket",
            "key.txt".to_string(),
            Object::new(
                "key.txt".to_string(),
                b"v1".to_vec(),
                "text/plain".to_string(),
            ),
        )
        .unwrap();
        put_object(
            storage.as_ref(),
            "bucket",
            "key.txt".to_string(),
            Object::new(
                "key.txt".to_string(),
                b"v2".to_vec(),
                "text/plain".to_string(),
            ),
        )
        .unwrap();

        let versions = list_object_versions(storage.as_ref(), "bucket", Some("key.txt")).unwrap();

        // Assert
        assert!(versions.len() >= 2);
        assert!(versions.iter().all(|version| version.key == "key.txt"));
    }
}
