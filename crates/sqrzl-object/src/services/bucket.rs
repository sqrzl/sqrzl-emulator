use crate::error::Result;
use crate::models::Bucket;
use crate::models::{LifecycleConfiguration, MultipartUpload};
use crate::storage::{
    AclStore, BucketStore, LifecycleStore, MultipartStore, PolicyStore, VersionStore,
};
use std::collections::HashMap;
use std::hash::BuildHasher;

///
/// # Errors
///
/// Returns an error when the underlying emulator operation fails.
pub fn list_buckets(storage: &(impl BucketStore + ?Sized)) -> Result<Vec<Bucket>> {
    storage.list_buckets()
}

///
/// # Errors
///
/// Returns an error when the underlying emulator operation fails.
pub fn create_bucket(storage: &(impl BucketStore + ?Sized), name: String) -> Result<()> {
    storage.create_bucket(name)
}

///
/// # Errors
///
/// Returns an error when the underlying emulator operation fails.
pub fn get_bucket(storage: &(impl BucketStore + ?Sized), name: &str) -> Result<Bucket> {
    storage.get_bucket(name)
}

///
/// # Errors
///
/// Returns an error when the underlying emulator operation fails.
pub fn delete_bucket(storage: &(impl BucketStore + ?Sized), name: &str) -> Result<()> {
    storage.delete_bucket(name)
}

///
/// # Errors
///
/// Returns an error when the underlying emulator operation fails.
pub fn bucket_exists(storage: &(impl BucketStore + ?Sized), name: &str) -> Result<bool> {
    storage.bucket_exists(name)
}

///
/// # Errors
///
/// Returns an error when the underlying emulator operation fails.
pub fn update_bucket_metadata(
    storage: &(impl BucketStore + ?Sized),
    bucket: &str,
    metadata: HashMap<String, String, impl BuildHasher>,
) -> Result<Bucket> {
    storage.update_bucket_metadata(bucket, metadata.into_iter().collect())
}

///
/// # Errors
///
/// Returns an error when the underlying emulator operation fails.
pub fn set_versioning(
    storage: &(impl VersionStore + ?Sized),
    bucket: &str,
    enabled: bool,
) -> Result<()> {
    if enabled {
        storage.enable_versioning(bucket)
    } else {
        storage.suspend_versioning(bucket)
    }
}

#[must_use]
pub fn versioning_enabled(bucket: &Bucket) -> bool {
    bucket.versioning_enabled
}

///
/// # Errors
///
/// Returns an error when the underlying emulator operation fails.
pub fn delete_bucket_lifecycle(
    storage: &(impl LifecycleStore + ?Sized),
    bucket: &str,
) -> Result<()> {
    storage.delete_bucket_lifecycle(bucket)
}

///
/// # Errors
///
/// Returns an error when the underlying emulator operation fails.
pub fn delete_bucket_policy(storage: &(impl PolicyStore + ?Sized), bucket: &str) -> Result<()> {
    storage.delete_bucket_policy(bucket)
}

///
/// # Errors
///
/// Returns an error when the underlying emulator operation fails.
pub fn get_bucket_lifecycle(
    storage: &(impl LifecycleStore + ?Sized),
    bucket: &str,
) -> Result<LifecycleConfiguration> {
    storage.get_bucket_lifecycle(bucket)
}

///
/// # Errors
///
/// Returns an error when the underlying emulator operation fails.
pub fn put_bucket_lifecycle(
    storage: &(impl LifecycleStore + ?Sized),
    bucket: &str,
    config: LifecycleConfiguration,
) -> Result<()> {
    storage.put_bucket_lifecycle(bucket, config)
}

///
/// # Errors
///
/// Returns an error when the underlying emulator operation fails.
pub fn get_bucket_policy(
    storage: &(impl PolicyStore + ?Sized),
    bucket: &str,
) -> Result<crate::models::policy::BucketPolicyDocument> {
    storage.get_bucket_policy(bucket)
}

///
/// # Errors
///
/// Returns an error when the underlying emulator operation fails.
pub fn put_bucket_policy(
    storage: &(impl PolicyStore + ?Sized),
    bucket: &str,
    policy: crate::models::policy::BucketPolicyDocument,
) -> Result<()> {
    storage.put_bucket_policy(bucket, policy)
}

///
/// # Errors
///
/// Returns an error when the underlying emulator operation fails.
pub fn get_bucket_acl(
    storage: &(impl AclStore + ?Sized),
    bucket: &str,
) -> Result<crate::models::policy::Acl> {
    storage.get_bucket_acl(bucket)
}

///
/// # Errors
///
/// Returns an error when the underlying emulator operation fails.
pub fn put_bucket_acl(
    storage: &(impl AclStore + ?Sized),
    bucket: &str,
    acl: crate::models::policy::Acl,
) -> Result<()> {
    storage.put_bucket_acl(bucket, acl)
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::{FilesystemStorage, Storage};
    use std::fs;
    use std::sync::Arc;

    fn temp_storage() -> Arc<dyn Storage> {
        let dir = std::env::temp_dir().join(format!(
            "sqrzl-service-bucket-test-{}",
            uuid::Uuid::new_v4()
        ));
        let _ = fs::create_dir_all(&dir);
        Arc::new(FilesystemStorage::new(dir))
    }

    #[test]
    fn should_create_list_get_bucket_through_service() {
        let storage = temp_storage();

        // Arrange
        create_bucket(storage.as_ref(), "demo".to_string()).unwrap();

        // Act
        let buckets = list_buckets(storage.as_ref()).unwrap();
        let bucket = get_bucket(storage.as_ref(), "demo").unwrap();

        // Assert
        assert_eq!(buckets.len(), 1);
        assert_eq!(buckets[0].name, "demo");
        assert_eq!(bucket.name, "demo");
        assert!(!versioning_enabled(&bucket));
    }

    #[test]
    fn should_delete_bucket_through_service() {
        let storage = temp_storage();

        // Arrange
        create_bucket(storage.as_ref(), "demo".to_string()).unwrap();

        // Act
        delete_bucket(storage.as_ref(), "demo").unwrap();

        // Assert
        assert!(list_buckets(storage.as_ref()).unwrap().is_empty());
    }

    #[test]
    fn should_enable_versioning_for_bucket() {
        let storage = temp_storage();

        // Arrange
        create_bucket(storage.as_ref(), "demo".to_string()).unwrap();

        // Act
        set_versioning(storage.as_ref(), "demo", true).unwrap();

        // Assert
        assert!(
            get_bucket(storage.as_ref(), "demo")
                .unwrap()
                .versioning_enabled
        );
    }

    #[test]
    fn should_suspend_versioning_for_bucket() {
        let storage = temp_storage();

        // Arrange
        create_bucket(storage.as_ref(), "demo".to_string()).unwrap();
        set_versioning(storage.as_ref(), "demo", true).unwrap();

        // Act
        set_versioning(storage.as_ref(), "demo", false).unwrap();

        // Assert
        assert!(
            !get_bucket(storage.as_ref(), "demo")
                .unwrap()
                .versioning_enabled
        );
    }
}
