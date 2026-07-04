use crate::error::Result;
use crate::models::{MultipartUpload, Object};
use crate::storage::{BucketStore, MultipartStore, ObjectListingStore, ObjectStore, VersionStore};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TenantContext {
    pub account_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Namespace {
    pub name: String,
    pub created_at: DateTime<Utc>,
    pub metadata: HashMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BlobChecksums {
    pub etag: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BlobRecord {
    pub namespace: String,
    pub key: String,
    pub size: u64,
    pub etag: String,
    pub content_type: String,
    pub last_modified: DateTime<Utc>,
    pub version_id: Option<String>,
    pub storage_class: String,
    pub metadata: HashMap<String, String>,
    pub tags: HashMap<String, String>,
    pub provider_metadata: HashMap<String, String>,
}

impl BlobRecord {
    #[must_use]
    pub fn from_object(namespace: &str, object: &Object) -> Self {
        Self {
            namespace: namespace.to_string(),
            key: object.key.clone(),
            size: object.size,
            etag: object.etag.clone(),
            content_type: object.content_type.clone(),
            last_modified: object.last_modified,
            version_id: object.version_id.clone(),
            storage_class: object.storage_class.clone(),
            metadata: object.metadata.clone(),
            tags: object.tags.clone(),
            provider_metadata: object.provider_metadata.clone(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PutBlobRequest {
    pub namespace: String,
    pub key: String,
    pub data: Vec<u8>,
    pub content_type: String,
    pub metadata: HashMap<String, String>,
    pub tags: HashMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct UpdateBlobMetadataRequest {
    pub namespace: String,
    pub key: String,
    pub metadata: HashMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BlobRange {
    pub start: u64,
    pub end: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BlobPayload {
    pub blob: Object,
    pub data: Vec<u8>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CreateUploadSessionRequest {
    pub namespace: String,
    pub key: String,
    pub content_type: Option<String>,
    pub metadata: HashMap<String, String>,
    pub provider_metadata: HashMap<String, String>,
}

pub trait BlobBackend: Send + Sync {
    ///
    /// # Errors
    ///
    /// Returns an error when the underlying emulator operation fails.
    fn create_namespace(&self, name: String) -> Result<Namespace>;
    ///
    /// # Errors
    ///
    /// Returns an error when the underlying emulator operation fails.
    fn get_namespace(&self, name: &str) -> Result<Namespace>;
    ///
    /// # Errors
    ///
    /// Returns an error when the underlying emulator operation fails.
    fn list_namespaces(&self) -> Result<Vec<Namespace>>;
    ///
    /// # Errors
    ///
    /// Returns an error when the underlying emulator operation fails.
    fn delete_namespace(&self, name: &str) -> Result<()>;

    ///
    /// # Errors
    ///
    /// Returns an error when the underlying emulator operation fails.
    fn put_blob(&self, request: PutBlobRequest) -> Result<BlobRecord>;
    ///
    /// # Errors
    ///
    /// Returns an error when the underlying emulator operation fails.
    fn get_blob(&self, namespace: &str, key: &str) -> Result<Object>;
    ///
    /// # Errors
    ///
    /// Returns an error when the underlying emulator operation fails.
    fn get_blob_version(&self, namespace: &str, key: &str, version_id: &str) -> Result<Object>;
    ///
    /// # Errors
    ///
    /// Returns an error when the underlying emulator operation fails.
    fn get_blob_range(&self, namespace: &str, key: &str, range: BlobRange) -> Result<BlobPayload>;
    ///
    /// # Errors
    ///
    /// Returns an error when the underlying emulator operation fails.
    fn delete_blob(&self, namespace: &str, key: &str) -> Result<()>;
    ///
    /// # Errors
    ///
    /// Returns an error when the underlying emulator operation fails.
    fn update_blob_metadata(&self, request: UpdateBlobMetadataRequest) -> Result<BlobRecord>;
    ///
    /// # Errors
    ///
    /// Returns an error when the underlying emulator operation fails.
    fn list_blobs(
        &self,
        namespace: &str,
        prefix: Option<&str>,
        delimiter: Option<&str>,
        marker: Option<&str>,
        max_keys: Option<usize>,
    ) -> Result<Vec<BlobRecord>>;
    ///
    /// # Errors
    ///
    /// Returns an error when the underlying emulator operation fails.
    fn list_blob_versions(&self, namespace: &str, prefix: Option<&str>) -> Result<Vec<BlobRecord>>;

    ///
    /// # Errors
    ///
    /// Returns an error when the underlying emulator operation fails.
    fn create_upload_session(&self, request: CreateUploadSessionRequest)
        -> Result<MultipartUpload>;
    ///
    /// # Errors
    ///
    /// Returns an error when the underlying emulator operation fails.
    fn upload_session_part(
        &self,
        namespace: &str,
        upload_id: &str,
        part_number: u32,
        data: Vec<u8>,
    ) -> Result<String>;
    ///
    /// # Errors
    ///
    /// Returns an error when the underlying emulator operation fails.
    fn complete_upload_session(&self, namespace: &str, upload_id: &str) -> Result<String>;
}

impl<T> BlobBackend for T
where
    T: BucketStore + ObjectStore + ObjectListingStore + MultipartStore + VersionStore + ?Sized,
{
    fn create_namespace(&self, name: String) -> Result<Namespace> {
        self.create_bucket(name.clone())?;
        self.get_namespace(&name)
    }

    fn get_namespace(&self, name: &str) -> Result<Namespace> {
        let bucket = self.get_bucket(name)?;
        Ok(Namespace {
            name: bucket.name,
            created_at: bucket.created_at,
            metadata: bucket.metadata,
        })
    }

    fn list_namespaces(&self) -> Result<Vec<Namespace>> {
        Ok(self
            .list_buckets()?
            .into_iter()
            .map(|bucket| Namespace {
                name: bucket.name,
                created_at: bucket.created_at,
                metadata: bucket.metadata,
            })
            .collect())
    }

    fn delete_namespace(&self, name: &str) -> Result<()> {
        self.delete_bucket(name)
    }

    fn put_blob(&self, request: PutBlobRequest) -> Result<BlobRecord> {
        let mut object = Object::new_with_metadata(
            request.key.clone(),
            request.data,
            request.content_type,
            request.metadata,
        );
        object.tags = request.tags;

        let versioning_enabled = self.get_bucket(&request.namespace)?.versioning_enabled;
        let record = BlobRecord::from_object(&request.namespace, &object);

        self.put_object(&request.namespace, request.key.clone(), object)?;

        if versioning_enabled {
            let stored = self.get_object(&request.namespace, &request.key)?;
            Ok(BlobRecord::from_object(&request.namespace, &stored))
        } else {
            Ok(record)
        }
    }

    fn get_blob(&self, namespace: &str, key: &str) -> Result<Object> {
        self.get_object(namespace, key)
    }

    fn get_blob_version(&self, namespace: &str, key: &str, version_id: &str) -> Result<Object> {
        self.get_object_version(namespace, key, version_id)
    }

    fn get_blob_range(&self, namespace: &str, key: &str, range: BlobRange) -> Result<BlobPayload> {
        let (blob, data) = self.get_object_range(namespace, key, range.start, Some(range.end))?;
        Ok(BlobPayload { blob, data })
    }

    fn delete_blob(&self, namespace: &str, key: &str) -> Result<()> {
        self.delete_object(namespace, key)
    }

    fn update_blob_metadata(&self, request: UpdateBlobMetadataRequest) -> Result<BlobRecord> {
        let existing = self.get_object(&request.namespace, &request.key)?;
        let mut object = Object::new_with_metadata(
            request.key.clone(),
            existing.data,
            existing.content_type,
            request.metadata,
        );
        object.etag = existing.etag;
        object.last_modified = existing.last_modified;
        object.version_id = existing.version_id;
        object.storage_class = existing.storage_class;
        object.tags = existing.tags;
        object.acl = existing.acl;
        object.provider_metadata = existing.provider_metadata;
        self.put_object(&request.namespace, request.key.clone(), object)?;
        let stored = self.get_object(&request.namespace, &request.key)?;
        Ok(BlobRecord::from_object(&request.namespace, &stored))
    }

    fn list_blobs(
        &self,
        namespace: &str,
        prefix: Option<&str>,
        delimiter: Option<&str>,
        marker: Option<&str>,
        max_keys: Option<usize>,
    ) -> Result<Vec<BlobRecord>> {
        Ok(self
            .list_objects(namespace, prefix, delimiter, marker, max_keys)?
            .objects
            .iter()
            .map(|object| BlobRecord::from_object(namespace, object))
            .collect())
    }

    fn list_blob_versions(&self, namespace: &str, prefix: Option<&str>) -> Result<Vec<BlobRecord>> {
        Ok(self
            .list_object_versions(namespace, prefix)?
            .iter()
            .map(|object| BlobRecord::from_object(namespace, object))
            .collect())
    }

    fn create_upload_session(
        &self,
        request: CreateUploadSessionRequest,
    ) -> Result<MultipartUpload> {
        self.create_multipart_upload_with_metadata(
            &request.namespace,
            request.key,
            request.content_type,
            request.metadata,
            request.provider_metadata,
        )
    }

    fn upload_session_part(
        &self,
        namespace: &str,
        upload_id: &str,
        part_number: u32,
        data: Vec<u8>,
    ) -> Result<String> {
        self.upload_part(namespace, upload_id, part_number, data)
    }

    fn complete_upload_session(&self, namespace: &str, upload_id: &str) -> Result<String> {
        self.complete_multipart_upload(namespace, upload_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::{FilesystemStorage, ObjectStore, VersionStore};
    use std::collections::HashMap;
    use std::path::PathBuf;
    use uuid::Uuid;

    fn temp_path() -> PathBuf {
        std::env::temp_dir().join(format!("sqrzl_blob_core_test_{}", Uuid::new_v4()))
    }

    #[test]
    fn should_read_a_blob_range_through_blob_backend() {
        // Arrange
        let base = temp_path();
        let storage = FilesystemStorage::new(&base);
        let backend: &dyn BlobBackend = &storage;

        backend
            .create_namespace("docs".to_string())
            .expect("namespace create should succeed");

        backend
            .put_blob(PutBlobRequest {
                namespace: "docs".to_string(),
                key: "guide.txt".to_string(),
                data: b"hello backend".to_vec(),
                content_type: "text/plain".to_string(),
                metadata: HashMap::from([(String::from("owner"), String::from("alice"))]),
                tags: HashMap::from([(String::from("env"), String::from("test"))]),
            })
            .expect("put should succeed");

        // Act
        let range = backend
            .get_blob_range("docs", "guide.txt", BlobRange { start: 6, end: 12 })
            .expect("range get should succeed");

        // Assert
        assert_eq!(range.data, b"backend".to_vec());

        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn should_update_blob_metadata_through_blob_backend() {
        // Arrange
        let base = temp_path();
        let storage = FilesystemStorage::new(&base);
        let backend: &dyn BlobBackend = &storage;

        backend
            .create_namespace("docs".to_string())
            .expect("namespace create should succeed");

        backend
            .put_blob(PutBlobRequest {
                namespace: "docs".to_string(),
                key: "guide.txt".to_string(),
                data: b"hello backend".to_vec(),
                content_type: "text/plain".to_string(),
                metadata: HashMap::from([(String::from("owner"), String::from("alice"))]),
                tags: HashMap::from([(String::from("env"), String::from("test"))]),
            })
            .expect("put should succeed");

        // Act
        let updated = backend
            .update_blob_metadata(UpdateBlobMetadataRequest {
                namespace: "docs".to_string(),
                key: "guide.txt".to_string(),
                metadata: HashMap::from([(String::from("owner"), String::from("bob"))]),
            })
            .expect("metadata update should succeed");

        // Assert
        assert_eq!(updated.metadata.get("owner"), Some(&"bob".to_string()));

        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn should_list_blob_versions_after_versioned_overwrite() {
        // Arrange
        let base = temp_path();
        let storage = FilesystemStorage::new(&base);
        let backend: &dyn BlobBackend = &storage;

        backend
            .create_namespace("docs".to_string())
            .expect("namespace create should succeed");
        storage
            .enable_versioning("docs")
            .expect("versioning should enable");

        backend
            .put_blob(PutBlobRequest {
                namespace: "docs".to_string(),
                key: "guide.txt".to_string(),
                data: b"hello backend".to_vec(),
                content_type: "text/plain".to_string(),
                metadata: HashMap::from([(String::from("owner"), String::from("alice"))]),
                tags: HashMap::from([(String::from("env"), String::from("test"))]),
            })
            .expect("versioned put should succeed");
        backend
            .put_blob(PutBlobRequest {
                namespace: "docs".to_string(),
                key: "guide.txt".to_string(),
                data: b"hello versions".to_vec(),
                content_type: "text/plain".to_string(),
                metadata: HashMap::new(),
                tags: HashMap::new(),
            })
            .expect("versioned overwrite should succeed");

        // Act
        let versions = backend
            .list_blob_versions("docs", Some("guide"))
            .expect("version list should succeed");

        // Assert
        assert!(
            versions.len() >= 2,
            "expected current and historical versions after overwrite"
        );

        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn should_fetch_a_blob_version_by_id() {
        // Arrange
        let base = temp_path();
        let storage = FilesystemStorage::new(&base);
        let backend: &dyn BlobBackend = &storage;

        backend
            .create_namespace("docs".to_string())
            .expect("namespace create should succeed");
        storage
            .enable_versioning("docs")
            .expect("versioning should enable");

        let created_version = backend
            .put_blob(PutBlobRequest {
                namespace: "docs".to_string(),
                key: "guide.txt".to_string(),
                data: b"hello backend".to_vec(),
                content_type: "text/plain".to_string(),
                metadata: HashMap::from([(String::from("owner"), String::from("alice"))]),
                tags: HashMap::from([(String::from("env"), String::from("test"))]),
            })
            .expect("versioned put should succeed");
        let version_id = created_version
            .version_id
            .expect("version id should exist when versioning is enabled");
        backend
            .put_blob(PutBlobRequest {
                namespace: "docs".to_string(),
                key: "guide.txt".to_string(),
                data: b"hello versions".to_vec(),
                content_type: "text/plain".to_string(),
                metadata: HashMap::new(),
                tags: HashMap::new(),
            })
            .expect("versioned overwrite should succeed");

        // Act
        let version = backend
            .get_blob_version("docs", "guide.txt", &version_id)
            .expect("version fetch should succeed");

        // Assert
        assert_eq!(version.version_id.as_deref(), Some(version_id.as_str()));
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn should_preserve_provider_metadata_when_updating_blob_metadata() {
        // Arrange
        let base = temp_path();
        let storage = FilesystemStorage::new(&base);
        let backend: &dyn BlobBackend = &storage;

        backend
            .create_namespace("azure".to_string())
            .expect("namespace create should succeed");

        let mut object = Object::new(
            "append.log".to_string(),
            b"hello".to_vec(),
            "text/plain".to_string(),
        );
        object
            .provider_metadata
            .insert("azure_blob_type".to_string(), "AppendBlob".to_string());
        storage
            .put_object("azure", "append.log".to_string(), object)
            .expect("put should succeed");

        backend
            .update_blob_metadata(UpdateBlobMetadataRequest {
                namespace: "azure".to_string(),
                key: "append.log".to_string(),
                metadata: HashMap::from([(String::from("owner"), String::from("alice"))]),
            })
            .expect("metadata update should succeed");

        // Act
        let stored = backend
            .get_blob("azure", "append.log")
            .expect("blob fetch should succeed");

        // Assert
        assert_eq!(
            stored.provider_metadata.get("azure_blob_type"),
            Some(&"AppendBlob".to_string())
        );

        let _ = std::fs::remove_dir_all(base);
    }
}
