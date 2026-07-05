use crate::error::Result;
use crate::models::{Acl, Bucket, MultipartUpload, Object};
use crate::storage::{
    AclStore, BucketStore, LifecycleStore, MultipartStore, ObjectListingStore, ObjectStore,
    PolicyStore, ProviderStateStore, Storage, TagStore, VersionStore,
};
use std::collections::{BTreeSet, HashMap};
use std::sync::{Arc, RwLock};

/// In-memory index for fast lookups
#[derive(Clone)]
struct ObjectIndex {
    /// `bucket_name` -> Set of object keys
    buckets: HashMap<String, BTreeSet<String>>,
}

/// Wraps any Storage implementation with in-memory indices for O(1) list/exists operations
pub struct IndexedStorage {
    inner: Arc<dyn Storage>,
    index: Arc<RwLock<ObjectIndex>>,
}

impl IndexedStorage {
    pub fn new(inner: Arc<dyn Storage>) -> Self {
        let storage = Self {
            inner,
            index: Arc::new(RwLock::new(ObjectIndex {
                buckets: HashMap::new(),
            })),
        };
        storage.rebuild_index_from_inner();
        storage
    }

    fn update_index_put(&self, bucket: &str, key: String) {
        if let Ok(mut index) = self.index.write() {
            index
                .buckets
                .entry(bucket.to_string())
                .or_default()
                .insert(key);
        }
    }

    fn update_index_delete(&self, bucket: &str, key: &str) {
        if let Ok(mut index) = self.index.write() {
            if let Some(keys) = index.buckets.get_mut(bucket) {
                keys.retain(|k| k != key);
            }
        }
    }

    fn update_index_create_bucket(&self, bucket: String) {
        if let Ok(mut index) = self.index.write() {
            index.buckets.entry(bucket).or_default();
        }
    }

    fn update_index_delete_bucket(&self, bucket: &str) {
        if let Ok(mut index) = self.index.write() {
            index.buckets.remove(bucket);
        }
    }

    fn rebuild_index_from_inner(&self) {
        let Ok(buckets) = self.inner.list_buckets() else {
            return;
        };

        for bucket in buckets {
            self.update_index_create_bucket(bucket.name.clone());
            let mut marker = None;
            while let Ok(page) =
                self.inner
                    .list_objects(&bucket.name, None, None, marker.as_deref(), Some(1000))
            {
                let is_truncated = page.is_truncated;
                let next_marker = page.next_marker;

                for object in page.objects {
                    self.update_index_put(&bucket.name, object.key);
                }

                if !is_truncated {
                    break;
                }

                let Some(next_marker) = next_marker else {
                    break;
                };
                marker = Some(next_marker);
            }
        }
    }

    fn get_indexed_objects(&self, bucket: &str, prefix: Option<&str>) -> Option<Vec<String>> {
        let Ok(index) = self.index.read() else {
            return None;
        };
        index.buckets.get(bucket).map(|keys| {
            keys.iter()
                .filter(|k| prefix.is_none_or(|p| k.starts_with(p)))
                .cloned()
                .collect()
        })
    }
}

impl BucketStore for IndexedStorage {
    fn create_bucket(&self, name: String) -> Result<()> {
        self.inner.create_bucket(name.clone())?;
        self.update_index_create_bucket(name);
        Ok(())
    }

    fn delete_bucket(&self, name: &str) -> Result<()> {
        self.inner.delete_bucket(name)?;
        self.update_index_delete_bucket(name);
        Ok(())
    }

    fn get_bucket(&self, name: &str) -> Result<Bucket> {
        self.inner.get_bucket(name)
    }

    fn list_buckets(&self) -> Result<Vec<Bucket>> {
        self.inner.list_buckets()
    }

    fn bucket_exists(&self, name: &str) -> Result<bool> {
        self.inner.bucket_exists(name)
    }

    fn update_bucket_metadata(
        &self,
        bucket: &str,
        metadata: HashMap<String, String>,
    ) -> Result<Bucket> {
        self.inner.update_bucket_metadata(bucket, metadata)
    }
}

impl ObjectStore for IndexedStorage {
    fn put_object(&self, bucket: &str, key: String, object: Object) -> Result<()> {
        self.inner.put_object(bucket, key.clone(), object)?;
        self.update_index_put(bucket, key);
        Ok(())
    }

    fn get_object(&self, bucket: &str, key: &str) -> Result<Object> {
        self.inner.get_object(bucket, key)
    }

    fn get_object_range(
        &self,
        bucket: &str,
        key: &str,
        start: u64,
        end: Option<u64>,
    ) -> Result<(Object, Vec<u8>)> {
        self.inner.get_object_range(bucket, key, start, end)
    }

    fn delete_object(&self, bucket: &str, key: &str) -> Result<()> {
        self.inner.delete_object(bucket, key)?;
        self.update_index_delete(bucket, key);
        Ok(())
    }

    fn update_object_storage_class(
        &self,
        bucket: &str,
        key: &str,
        storage_class: &str,
    ) -> Result<()> {
        self.inner
            .update_object_storage_class(bucket, key, storage_class)
    }

    fn object_exists(&self, bucket: &str, key: &str) -> Result<bool> {
        // Fast path: check index first
        if let Ok(index) = self.index.read() {
            if let Some(keys) = index.buckets.get(bucket) {
                if keys.contains(&key.to_string()) {
                    return Ok(true);
                }
            }
            drop(index);
        }
        // Fallback to storage
        self.inner.object_exists(bucket, key)
    }
}

impl ObjectListingStore for IndexedStorage {
    fn list_objects(
        &self,
        bucket: &str,
        prefix: Option<&str>,
        delimiter: Option<&str>,
        marker: Option<&str>,
        max_keys: Option<usize>,
    ) -> Result<crate::models::ListObjectsResult> {
        if delimiter.is_some_and(|value| !value.is_empty()) {
            return self
                .inner
                .list_objects(bucket, prefix, delimiter, marker, max_keys);
        }

        let Some(mut keys) = self.get_indexed_objects(bucket, prefix) else {
            return self
                .inner
                .list_objects(bucket, prefix, delimiter, marker, max_keys);
        };

        if let Some(m) = marker {
            keys.retain(|key| key.as_str() > m);
        }

        let max_keys = max_keys.unwrap_or(1000);
        let is_truncated = keys.len() > max_keys;
        let page_keys = keys.iter().take(max_keys).cloned().collect::<Vec<_>>();
        let next_marker = if is_truncated {
            if max_keys == 0 {
                keys.first().cloned()
            } else {
                page_keys.last().cloned()
            }
        } else {
            None
        };

        let mut objects = Vec::with_capacity(page_keys.len());
        for key in page_keys {
            if let Ok(obj) = self.inner.get_object(bucket, &key) {
                objects.push(obj);
            }
        }

        Ok(crate::models::ListObjectsResult {
            common_prefixes: Vec::new(),
            objects,
            is_truncated,
            next_marker,
        })
    }
}

impl MultipartStore for IndexedStorage {
    fn create_multipart_upload(&self, bucket: &str, key: String) -> Result<MultipartUpload> {
        self.inner.create_multipart_upload(bucket, key)
    }

    fn create_multipart_upload_with_metadata(
        &self,
        bucket: &str,
        key: String,
        content_type: Option<String>,
        metadata: HashMap<String, String>,
        provider_metadata: HashMap<String, String>,
    ) -> Result<MultipartUpload> {
        self.inner.create_multipart_upload_with_metadata(
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
        self.inner.upload_part(bucket, upload_id, part_number, data)
    }

    fn list_multipart_uploads(&self, bucket: &str) -> Result<Vec<MultipartUpload>> {
        self.inner.list_multipart_uploads(bucket)
    }

    fn list_parts(&self, bucket: &str, upload_id: &str) -> Result<Vec<crate::models::Part>> {
        self.inner.list_parts(bucket, upload_id)
    }

    fn get_multipart_upload(&self, bucket: &str, upload_id: &str) -> Result<MultipartUpload> {
        self.inner.get_multipart_upload(bucket, upload_id)
    }

    fn complete_multipart_upload(&self, bucket: &str, upload_id: &str) -> Result<String> {
        self.inner.complete_multipart_upload(bucket, upload_id)
    }

    fn abort_multipart_upload(&self, bucket: &str, upload_id: &str) -> Result<()> {
        self.inner.abort_multipart_upload(bucket, upload_id)
    }
}

impl VersionStore for IndexedStorage {
    fn enable_versioning(&self, bucket: &str) -> Result<()> {
        self.inner.enable_versioning(bucket)
    }

    fn suspend_versioning(&self, bucket: &str) -> Result<()> {
        self.inner.suspend_versioning(bucket)
    }

    fn get_object_version(
        &self,
        bucket: &str,
        key: &str,
        version_id: &str,
    ) -> Result<crate::models::Object> {
        self.inner.get_object_version(bucket, key, version_id)
    }

    fn list_object_versions(
        &self,
        bucket: &str,
        prefix: Option<&str>,
    ) -> Result<Vec<crate::models::Object>> {
        self.inner.list_object_versions(bucket, prefix)
    }

    fn list_object_versions_for_key(
        &self,
        bucket: &str,
        key: &str,
    ) -> Result<Vec<crate::models::Object>> {
        self.inner.list_object_versions_for_key(bucket, key)
    }

    fn delete_object_version(&self, bucket: &str, key: &str, version_id: &str) -> Result<()> {
        self.inner.delete_object_version(bucket, key, version_id)
    }
}

impl TagStore for IndexedStorage {
    fn get_object_tags(
        &self,
        bucket: &str,
        key: &str,
    ) -> Result<std::collections::HashMap<String, String>> {
        self.inner.get_object_tags(bucket, key)
    }

    fn put_object_tags(
        &self,
        bucket: &str,
        key: &str,
        tags: std::collections::HashMap<String, String>,
    ) -> Result<()> {
        self.inner.put_object_tags(bucket, key, tags)
    }

    fn delete_object_tags(&self, bucket: &str, key: &str) -> Result<()> {
        self.inner.delete_object_tags(bucket, key)
    }
}

impl AclStore for IndexedStorage {
    fn get_bucket_acl(&self, name: &str) -> Result<Acl> {
        self.inner.get_bucket_acl(name)
    }

    fn put_bucket_acl(&self, name: &str, acl: Acl) -> Result<()> {
        self.inner.put_bucket_acl(name, acl)
    }

    fn get_object_acl(&self, bucket: &str, key: &str) -> Result<Acl> {
        self.inner.get_object_acl(bucket, key)
    }

    fn put_object_acl(&self, bucket: &str, key: &str, acl: Acl) -> Result<()> {
        self.inner.put_object_acl(bucket, key, acl)
    }
}

impl LifecycleStore for IndexedStorage {
    fn get_bucket_lifecycle(
        &self,
        bucket: &str,
    ) -> Result<crate::models::lifecycle::LifecycleConfiguration> {
        self.inner.get_bucket_lifecycle(bucket)
    }

    fn put_bucket_lifecycle(
        &self,
        bucket: &str,
        config: crate::models::lifecycle::LifecycleConfiguration,
    ) -> Result<()> {
        self.inner.put_bucket_lifecycle(bucket, config)
    }

    fn delete_bucket_lifecycle(&self, bucket: &str) -> Result<()> {
        self.inner.delete_bucket_lifecycle(bucket)
    }
}

impl PolicyStore for IndexedStorage {
    fn get_bucket_policy(
        &self,
        bucket: &str,
    ) -> Result<crate::models::policy::BucketPolicyDocument> {
        self.inner.get_bucket_policy(bucket)
    }

    fn put_bucket_policy(
        &self,
        bucket: &str,
        policy: crate::models::policy::BucketPolicyDocument,
    ) -> Result<()> {
        self.inner.put_bucket_policy(bucket, policy)
    }

    fn delete_bucket_policy(&self, bucket: &str) -> Result<()> {
        self.inner.delete_bucket_policy(bucket)
    }
}

impl ProviderStateStore for IndexedStorage {
    fn put_provider_state(&self, provider: &str, key: &str, data: Vec<u8>) -> Result<()> {
        self.inner.put_provider_state(provider, key, data)
    }

    fn get_provider_state(&self, provider: &str, key: &str) -> Result<Vec<u8>> {
        self.inner.get_provider_state(provider, key)
    }

    fn delete_provider_state(&self, provider: &str, key: &str) -> Result<()> {
        self.inner.delete_provider_state(provider, key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::FilesystemStorage;
    use std::path::PathBuf;
    use uuid::Uuid;

    fn temp_path() -> PathBuf {
        std::env::temp_dir().join(format!("sqrzl_indexed_test_{}", Uuid::new_v4()))
    }

    fn object(key: &str, data: &[u8]) -> Object {
        Object::new(key.to_string(), data.to_vec(), "text/plain".to_string())
    }

    fn keys(result: &crate::models::ListObjectsResult) -> Vec<String> {
        result
            .objects
            .iter()
            .map(|object| object.key.clone())
            .collect()
    }

    fn assert_listing_parity(
        expected: &crate::models::ListObjectsResult,
        actual: &crate::models::ListObjectsResult,
    ) {
        assert_eq!(keys(actual), keys(expected));
        assert_eq!(actual.common_prefixes, expected.common_prefixes);
        assert_eq!(actual.is_truncated, expected.is_truncated);
        assert_eq!(actual.next_marker, expected.next_marker);
    }

    #[test]
    fn should_not_duplicate_listings_after_overwrite() {
        // Arrange
        let base = temp_path();
        let inner: Arc<dyn Storage> = Arc::new(FilesystemStorage::new(&base));
        let storage: Arc<dyn Storage> = Arc::new(IndexedStorage::new(inner));

        storage.create_bucket("bucket".to_string()).unwrap();
        storage
            .put_object("bucket", "same.txt".to_string(), object("same.txt", b"one"))
            .unwrap();
        storage
            .put_object("bucket", "same.txt".to_string(), object("same.txt", b"two"))
            .unwrap();

        // Act
        let listed = storage
            .list_objects("bucket", None, None, None, Some(10))
            .unwrap();

        // Assert
        assert_eq!(keys(&listed), vec!["same.txt"]);
        assert_eq!(
            storage.get_object("bucket", "same.txt").unwrap().data,
            b"two"
        );

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn should_match_filesystem_delimiter_common_prefix_listing() {
        // Arrange
        let base = temp_path();
        let inner: Arc<dyn Storage> = Arc::new(FilesystemStorage::new(&base));
        let storage: Arc<dyn Storage> = Arc::new(IndexedStorage::new(inner.clone()));

        storage.create_bucket("bucket".to_string()).unwrap();
        for key in ["docs/api/openapi.json", "docs/readme.txt", "image.png"] {
            storage
                .put_object("bucket", key.to_string(), object(key, b"payload"))
                .unwrap();
        }

        // Act
        let expected = inner
            .list_objects("bucket", Some(""), Some("/"), None, Some(10))
            .unwrap();
        let actual = storage
            .list_objects("bucket", Some(""), Some("/"), None, Some(10))
            .unwrap();

        // Assert
        assert_listing_parity(&expected, &actual);

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn should_match_filesystem_marker_pagination() {
        // Arrange
        let base = temp_path();
        let inner: Arc<dyn Storage> = Arc::new(FilesystemStorage::new(&base));
        let storage: Arc<dyn Storage> = Arc::new(IndexedStorage::new(inner.clone()));

        storage.create_bucket("bucket".to_string()).unwrap();
        for key in ["a.txt", "b.txt", "c.txt"] {
            storage
                .put_object("bucket", key.to_string(), object(key, b"payload"))
                .unwrap();
        }

        // Act
        let expected_first = inner
            .list_objects("bucket", None, None, None, Some(2))
            .unwrap();
        let actual_first = storage
            .list_objects("bucket", None, None, None, Some(2))
            .unwrap();
        assert_listing_parity(&expected_first, &actual_first);

        let marker = actual_first
            .next_marker
            .as_deref()
            .expect("first page should be truncated");
        let expected_second = inner
            .list_objects("bucket", None, None, Some(marker), Some(2))
            .unwrap();
        let actual_second = storage
            .list_objects("bucket", None, None, Some(marker), Some(2))
            .unwrap();

        // Assert
        assert_listing_parity(&expected_second, &actual_second);

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn should_rebuild_index_from_existing_inner_storage() {
        // Arrange
        let base = temp_path();
        let inner: Arc<dyn Storage> = Arc::new(FilesystemStorage::new(&base));
        inner.create_bucket("bucket".to_string()).unwrap();
        for key in ["alpha.txt", "beta.txt"] {
            inner
                .put_object("bucket", key.to_string(), object(key, b"payload"))
                .unwrap();
        }

        // Act
        let storage: Arc<dyn Storage> = Arc::new(IndexedStorage::new(inner.clone()));
        let expected = inner
            .list_objects("bucket", None, None, None, Some(10))
            .unwrap();
        let actual = storage
            .list_objects("bucket", None, None, None, Some(10))
            .unwrap();

        // Assert
        assert_listing_parity(&expected, &actual);

        let _ = std::fs::remove_dir_all(&base);
    }
}
