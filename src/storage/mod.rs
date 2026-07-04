use crate::error::Result;
use crate::models::{Bucket, ListObjectsResult, MultipartUpload, Object};
use std::collections::HashMap;

pub mod filesystem;
pub mod indexed;
pub mod lockfree_index;

pub use filesystem::FilesystemStorage;
pub use indexed::IndexedStorage;
pub use lockfree_index::{DirectoryEntry, DirectoryEntryKind, LockFreeIndex};

/// Bucket metadata and lifecycle-independent bucket operations.
pub trait BucketStore: Send + Sync {
    fn create_bucket(&self, name: String) -> Result<()>;
    fn delete_bucket(&self, name: &str) -> Result<()>;
    fn get_bucket(&self, name: &str) -> Result<Bucket>;
    fn list_buckets(&self) -> Result<Vec<Bucket>>;
    fn bucket_exists(&self, name: &str) -> Result<bool>;
    fn update_bucket_metadata(
        &self,
        bucket: &str,
        metadata: HashMap<String, String>,
    ) -> Result<Bucket>;
}

/// Object read/write operations excluding list semantics.
pub trait ObjectStore: Send + Sync {
    fn put_object(&self, bucket: &str, key: String, object: Object) -> Result<()>;
    fn get_object(&self, bucket: &str, key: &str) -> Result<Object>;
    fn get_object_range(
        &self,
        bucket: &str,
        key: &str,
        start: u64,
        end: Option<u64>,
    ) -> Result<(Object, Vec<u8>)>;
    fn delete_object(&self, bucket: &str, key: &str) -> Result<()>;
    fn update_object_storage_class(
        &self,
        bucket: &str,
        key: &str,
        storage_class: &str,
    ) -> Result<()>;
    fn object_exists(&self, bucket: &str, key: &str) -> Result<bool>;
}

/// Object listing semantics, including delimiter and marker pagination behavior.
pub trait ObjectListingStore: Send + Sync {
    fn list_objects(
        &self,
        bucket: &str,
        prefix: Option<&str>,
        delimiter: Option<&str>,
        marker: Option<&str>,
        max_keys: Option<usize>,
    ) -> Result<ListObjectsResult>;
}

/// Multipart upload state and part operations.
pub trait MultipartStore: Send + Sync {
    fn create_multipart_upload(&self, bucket: &str, key: String) -> Result<MultipartUpload>;
    fn create_multipart_upload_with_metadata(
        &self,
        bucket: &str,
        key: String,
        content_type: Option<String>,
        metadata: std::collections::HashMap<String, String>,
        provider_metadata: std::collections::HashMap<String, String>,
    ) -> Result<MultipartUpload>;
    fn upload_part(
        &self,
        bucket: &str,
        upload_id: &str,
        part_number: u32,
        data: Vec<u8>,
    ) -> Result<String>;
    fn list_multipart_uploads(&self, bucket: &str) -> Result<Vec<MultipartUpload>>;
    fn list_parts(&self, bucket: &str, upload_id: &str) -> Result<Vec<crate::models::Part>>;
    fn get_multipart_upload(&self, bucket: &str, upload_id: &str) -> Result<MultipartUpload>;
    fn complete_multipart_upload(&self, bucket: &str, upload_id: &str) -> Result<String>;
    fn abort_multipart_upload(&self, bucket: &str, upload_id: &str) -> Result<()>;
}

/// Bucket versioning and object-version operations.
pub trait VersionStore: Send + Sync {
    fn enable_versioning(&self, bucket: &str) -> Result<()>;
    fn suspend_versioning(&self, bucket: &str) -> Result<()>;
    fn get_object_version(&self, bucket: &str, key: &str, version_id: &str) -> Result<Object>;
    fn list_object_versions(&self, bucket: &str, prefix: Option<&str>) -> Result<Vec<Object>>;
    fn list_object_versions_for_key(&self, bucket: &str, key: &str) -> Result<Vec<Object>> {
        self.list_object_versions(bucket, Some(key))
    }
    fn delete_object_version(&self, bucket: &str, key: &str, version_id: &str) -> Result<()>;
}

/// Object tag operations.
pub trait TagStore: Send + Sync {
    fn get_object_tags(&self, bucket: &str, key: &str) -> Result<HashMap<String, String>>;
    fn put_object_tags(&self, bucket: &str, key: &str, tags: HashMap<String, String>)
        -> Result<()>;
    fn delete_object_tags(&self, bucket: &str, key: &str) -> Result<()>;
}

/// Bucket and object ACL operations.
pub trait AclStore: Send + Sync {
    fn get_bucket_acl(&self, bucket: &str) -> Result<crate::models::policy::Acl>;
    fn put_bucket_acl(&self, bucket: &str, acl: crate::models::policy::Acl) -> Result<()>;
    fn get_object_acl(&self, bucket: &str, key: &str) -> Result<crate::models::policy::Acl>;
    fn put_object_acl(
        &self,
        bucket: &str,
        key: &str,
        acl: crate::models::policy::Acl,
    ) -> Result<()>;
}

/// Bucket lifecycle configuration operations.
pub trait LifecycleStore: Send + Sync {
    fn get_bucket_lifecycle(
        &self,
        bucket: &str,
    ) -> Result<crate::models::lifecycle::LifecycleConfiguration>;
    fn put_bucket_lifecycle(
        &self,
        bucket: &str,
        config: crate::models::lifecycle::LifecycleConfiguration,
    ) -> Result<()>;
    fn delete_bucket_lifecycle(&self, bucket: &str) -> Result<()>;
}

/// Bucket policy operations.
pub trait PolicyStore: Send + Sync {
    fn get_bucket_policy(
        &self,
        bucket: &str,
    ) -> Result<crate::models::policy::BucketPolicyDocument>;
    fn put_bucket_policy(
        &self,
        bucket: &str,
        policy: crate::models::policy::BucketPolicyDocument,
    ) -> Result<()>;
    fn delete_bucket_policy(&self, bucket: &str) -> Result<()>;
}

/// Provider session/state sidecars for restart-safe emulator workflows.
pub trait ProviderStateStore: Send + Sync {
    fn put_provider_state(&self, provider: &str, key: &str, data: Vec<u8>) -> Result<()>;
    fn get_provider_state(&self, provider: &str, key: &str) -> Result<Vec<u8>>;
    fn delete_provider_state(&self, provider: &str, key: &str) -> Result<()>;
}

/// Storage backend aggregate - synchronous operations.
/// HTTP layers handle async/await by calling these operations on request paths.
pub trait Storage: Send + Sync {
    fn create_bucket(&self, name: String) -> Result<()>;
    fn delete_bucket(&self, name: &str) -> Result<()>;
    fn get_bucket(&self, name: &str) -> Result<Bucket>;
    fn list_buckets(&self) -> Result<Vec<Bucket>>;
    fn bucket_exists(&self, name: &str) -> Result<bool>;
    fn update_bucket_metadata(
        &self,
        bucket: &str,
        metadata: HashMap<String, String>,
    ) -> Result<Bucket>;

    fn put_object(&self, bucket: &str, key: String, object: Object) -> Result<()>;
    fn get_object(&self, bucket: &str, key: &str) -> Result<Object>;
    fn get_object_range(
        &self,
        bucket: &str,
        key: &str,
        start: u64,
        end: Option<u64>,
    ) -> Result<(Object, Vec<u8>)>;
    fn delete_object(&self, bucket: &str, key: &str) -> Result<()>;
    fn update_object_storage_class(
        &self,
        bucket: &str,
        key: &str,
        storage_class: &str,
    ) -> Result<()>;
    fn object_exists(&self, bucket: &str, key: &str) -> Result<bool>;
    fn list_objects(
        &self,
        bucket: &str,
        prefix: Option<&str>,
        delimiter: Option<&str>,
        marker: Option<&str>,
        max_keys: Option<usize>,
    ) -> Result<ListObjectsResult>;

    fn create_multipart_upload(&self, bucket: &str, key: String) -> Result<MultipartUpload>;
    fn create_multipart_upload_with_metadata(
        &self,
        bucket: &str,
        key: String,
        content_type: Option<String>,
        metadata: HashMap<String, String>,
        provider_metadata: HashMap<String, String>,
    ) -> Result<MultipartUpload>;
    fn upload_part(
        &self,
        bucket: &str,
        upload_id: &str,
        part_number: u32,
        data: Vec<u8>,
    ) -> Result<String>;
    fn list_multipart_uploads(&self, bucket: &str) -> Result<Vec<MultipartUpload>>;
    fn list_parts(&self, bucket: &str, upload_id: &str) -> Result<Vec<crate::models::Part>>;
    fn get_multipart_upload(&self, bucket: &str, upload_id: &str) -> Result<MultipartUpload>;
    fn complete_multipart_upload(&self, bucket: &str, upload_id: &str) -> Result<String>;
    fn abort_multipart_upload(&self, bucket: &str, upload_id: &str) -> Result<()>;

    fn enable_versioning(&self, bucket: &str) -> Result<()>;
    fn suspend_versioning(&self, bucket: &str) -> Result<()>;
    fn get_object_version(&self, bucket: &str, key: &str, version_id: &str) -> Result<Object>;
    fn list_object_versions(&self, bucket: &str, prefix: Option<&str>) -> Result<Vec<Object>>;
    fn list_object_versions_for_key(&self, bucket: &str, key: &str) -> Result<Vec<Object>>;
    fn delete_object_version(&self, bucket: &str, key: &str, version_id: &str) -> Result<()>;

    fn get_object_tags(&self, bucket: &str, key: &str) -> Result<HashMap<String, String>>;
    fn put_object_tags(&self, bucket: &str, key: &str, tags: HashMap<String, String>)
        -> Result<()>;
    fn delete_object_tags(&self, bucket: &str, key: &str) -> Result<()>;

    fn get_bucket_acl(&self, bucket: &str) -> Result<crate::models::policy::Acl>;
    fn put_bucket_acl(&self, bucket: &str, acl: crate::models::policy::Acl) -> Result<()>;
    fn get_object_acl(&self, bucket: &str, key: &str) -> Result<crate::models::policy::Acl>;
    fn put_object_acl(
        &self,
        bucket: &str,
        key: &str,
        acl: crate::models::policy::Acl,
    ) -> Result<()>;

    fn get_bucket_lifecycle(
        &self,
        bucket: &str,
    ) -> Result<crate::models::lifecycle::LifecycleConfiguration>;
    fn put_bucket_lifecycle(
        &self,
        bucket: &str,
        config: crate::models::lifecycle::LifecycleConfiguration,
    ) -> Result<()>;
    fn delete_bucket_lifecycle(&self, bucket: &str) -> Result<()>;

    fn get_bucket_policy(
        &self,
        bucket: &str,
    ) -> Result<crate::models::policy::BucketPolicyDocument>;
    fn put_bucket_policy(
        &self,
        bucket: &str,
        policy: crate::models::policy::BucketPolicyDocument,
    ) -> Result<()>;
    fn delete_bucket_policy(&self, bucket: &str) -> Result<()>;

    fn put_provider_state(&self, provider: &str, key: &str, data: Vec<u8>) -> Result<()>;
    fn get_provider_state(&self, provider: &str, key: &str) -> Result<Vec<u8>>;
    fn delete_provider_state(&self, provider: &str, key: &str) -> Result<()>;
}

impl<T> Storage for T
where
    T: BucketStore
        + ObjectStore
        + ObjectListingStore
        + MultipartStore
        + VersionStore
        + TagStore
        + AclStore
        + LifecycleStore
        + PolicyStore
        + ProviderStateStore
        + Send
        + Sync,
{
    fn create_bucket(&self, name: String) -> Result<()> {
        BucketStore::create_bucket(self, name)
    }

    fn delete_bucket(&self, name: &str) -> Result<()> {
        BucketStore::delete_bucket(self, name)
    }

    fn get_bucket(&self, name: &str) -> Result<Bucket> {
        BucketStore::get_bucket(self, name)
    }

    fn list_buckets(&self) -> Result<Vec<Bucket>> {
        BucketStore::list_buckets(self)
    }

    fn bucket_exists(&self, name: &str) -> Result<bool> {
        BucketStore::bucket_exists(self, name)
    }

    fn update_bucket_metadata(
        &self,
        bucket: &str,
        metadata: HashMap<String, String>,
    ) -> Result<Bucket> {
        BucketStore::update_bucket_metadata(self, bucket, metadata)
    }

    fn put_object(&self, bucket: &str, key: String, object: Object) -> Result<()> {
        ObjectStore::put_object(self, bucket, key, object)
    }

    fn get_object(&self, bucket: &str, key: &str) -> Result<Object> {
        ObjectStore::get_object(self, bucket, key)
    }

    fn get_object_range(
        &self,
        bucket: &str,
        key: &str,
        start: u64,
        end: Option<u64>,
    ) -> Result<(Object, Vec<u8>)> {
        ObjectStore::get_object_range(self, bucket, key, start, end)
    }

    fn delete_object(&self, bucket: &str, key: &str) -> Result<()> {
        ObjectStore::delete_object(self, bucket, key)
    }

    fn update_object_storage_class(
        &self,
        bucket: &str,
        key: &str,
        storage_class: &str,
    ) -> Result<()> {
        ObjectStore::update_object_storage_class(self, bucket, key, storage_class)
    }

    fn object_exists(&self, bucket: &str, key: &str) -> Result<bool> {
        ObjectStore::object_exists(self, bucket, key)
    }

    fn list_objects(
        &self,
        bucket: &str,
        prefix: Option<&str>,
        delimiter: Option<&str>,
        marker: Option<&str>,
        max_keys: Option<usize>,
    ) -> Result<ListObjectsResult> {
        ObjectListingStore::list_objects(self, bucket, prefix, delimiter, marker, max_keys)
    }

    fn create_multipart_upload(&self, bucket: &str, key: String) -> Result<MultipartUpload> {
        MultipartStore::create_multipart_upload(self, bucket, key)
    }

    fn create_multipart_upload_with_metadata(
        &self,
        bucket: &str,
        key: String,
        content_type: Option<String>,
        metadata: HashMap<String, String>,
        provider_metadata: HashMap<String, String>,
    ) -> Result<MultipartUpload> {
        MultipartStore::create_multipart_upload_with_metadata(
            self,
            bucket,
            key,
            content_type,
            metadata,
            provider_metadata,
        )
    }

    fn upload_part(
        &self,
        bucket: &str,
        upload_id: &str,
        part_number: u32,
        data: Vec<u8>,
    ) -> Result<String> {
        MultipartStore::upload_part(self, bucket, upload_id, part_number, data)
    }

    fn list_multipart_uploads(&self, bucket: &str) -> Result<Vec<MultipartUpload>> {
        MultipartStore::list_multipart_uploads(self, bucket)
    }

    fn list_parts(&self, bucket: &str, upload_id: &str) -> Result<Vec<crate::models::Part>> {
        MultipartStore::list_parts(self, bucket, upload_id)
    }

    fn get_multipart_upload(&self, bucket: &str, upload_id: &str) -> Result<MultipartUpload> {
        MultipartStore::get_multipart_upload(self, bucket, upload_id)
    }

    fn complete_multipart_upload(&self, bucket: &str, upload_id: &str) -> Result<String> {
        MultipartStore::complete_multipart_upload(self, bucket, upload_id)
    }

    fn abort_multipart_upload(&self, bucket: &str, upload_id: &str) -> Result<()> {
        MultipartStore::abort_multipart_upload(self, bucket, upload_id)
    }

    fn enable_versioning(&self, bucket: &str) -> Result<()> {
        VersionStore::enable_versioning(self, bucket)
    }

    fn suspend_versioning(&self, bucket: &str) -> Result<()> {
        VersionStore::suspend_versioning(self, bucket)
    }

    fn get_object_version(&self, bucket: &str, key: &str, version_id: &str) -> Result<Object> {
        VersionStore::get_object_version(self, bucket, key, version_id)
    }

    fn list_object_versions(&self, bucket: &str, prefix: Option<&str>) -> Result<Vec<Object>> {
        VersionStore::list_object_versions(self, bucket, prefix)
    }

    fn list_object_versions_for_key(&self, bucket: &str, key: &str) -> Result<Vec<Object>> {
        VersionStore::list_object_versions_for_key(self, bucket, key)
    }

    fn delete_object_version(&self, bucket: &str, key: &str, version_id: &str) -> Result<()> {
        VersionStore::delete_object_version(self, bucket, key, version_id)
    }

    fn get_object_tags(&self, bucket: &str, key: &str) -> Result<HashMap<String, String>> {
        TagStore::get_object_tags(self, bucket, key)
    }

    fn put_object_tags(
        &self,
        bucket: &str,
        key: &str,
        tags: HashMap<String, String>,
    ) -> Result<()> {
        TagStore::put_object_tags(self, bucket, key, tags)
    }

    fn delete_object_tags(&self, bucket: &str, key: &str) -> Result<()> {
        TagStore::delete_object_tags(self, bucket, key)
    }

    fn get_bucket_acl(&self, bucket: &str) -> Result<crate::models::policy::Acl> {
        AclStore::get_bucket_acl(self, bucket)
    }

    fn put_bucket_acl(&self, bucket: &str, acl: crate::models::policy::Acl) -> Result<()> {
        AclStore::put_bucket_acl(self, bucket, acl)
    }

    fn get_object_acl(&self, bucket: &str, key: &str) -> Result<crate::models::policy::Acl> {
        AclStore::get_object_acl(self, bucket, key)
    }

    fn put_object_acl(
        &self,
        bucket: &str,
        key: &str,
        acl: crate::models::policy::Acl,
    ) -> Result<()> {
        AclStore::put_object_acl(self, bucket, key, acl)
    }

    fn get_bucket_lifecycle(
        &self,
        bucket: &str,
    ) -> Result<crate::models::lifecycle::LifecycleConfiguration> {
        LifecycleStore::get_bucket_lifecycle(self, bucket)
    }

    fn put_bucket_lifecycle(
        &self,
        bucket: &str,
        config: crate::models::lifecycle::LifecycleConfiguration,
    ) -> Result<()> {
        LifecycleStore::put_bucket_lifecycle(self, bucket, config)
    }

    fn delete_bucket_lifecycle(&self, bucket: &str) -> Result<()> {
        LifecycleStore::delete_bucket_lifecycle(self, bucket)
    }

    fn get_bucket_policy(
        &self,
        bucket: &str,
    ) -> Result<crate::models::policy::BucketPolicyDocument> {
        PolicyStore::get_bucket_policy(self, bucket)
    }

    fn put_bucket_policy(
        &self,
        bucket: &str,
        policy: crate::models::policy::BucketPolicyDocument,
    ) -> Result<()> {
        PolicyStore::put_bucket_policy(self, bucket, policy)
    }

    fn delete_bucket_policy(&self, bucket: &str) -> Result<()> {
        PolicyStore::delete_bucket_policy(self, bucket)
    }

    fn put_provider_state(&self, provider: &str, key: &str, data: Vec<u8>) -> Result<()> {
        ProviderStateStore::put_provider_state(self, provider, key, data)
    }

    fn get_provider_state(&self, provider: &str, key: &str) -> Result<Vec<u8>> {
        ProviderStateStore::get_provider_state(self, provider, key)
    }

    fn delete_provider_state(&self, provider: &str, key: &str) -> Result<()> {
        ProviderStateStore::delete_provider_state(self, provider, key)
    }
}

#[cfg(test)]
mod tests {
    use super::{BucketStore, FilesystemStorage, ObjectListingStore, ObjectStore};
    use crate::models::Object;
    use std::path::PathBuf;
    use uuid::Uuid;

    fn temp_path() -> PathBuf {
        std::env::temp_dir().join(format!("sqrzl_storage_traits_{}", Uuid::new_v4()))
    }

    #[test]
    fn focused_traits_can_drive_basic_bucket_object_listing_flow() {
        let base = temp_path();
        let storage = FilesystemStorage::new(&base);

        let buckets: &dyn BucketStore = &storage;
        buckets.create_bucket("capability".to_string()).unwrap();

        let objects: &dyn ObjectStore = &storage;
        objects
            .put_object(
                "capability",
                "docs/readme.txt".to_string(),
                Object::new(
                    "docs/readme.txt".to_string(),
                    b"hello".to_vec(),
                    "text/plain".to_string(),
                ),
            )
            .unwrap();

        let listing: &dyn ObjectListingStore = &storage;
        let result = listing
            .list_objects("capability", Some("docs/"), None, None, Some(10))
            .unwrap();

        assert_eq!(result.objects.len(), 1);
        assert_eq!(result.objects[0].key, "docs/readme.txt");

        let _ = std::fs::remove_dir_all(&base);
    }
}
