use crate::error::{Error, Result};
use crate::models::{Bucket, ListObjectsResult, MultipartUpload, Object};
use std::collections::HashMap;
use std::path::Path;

pub mod filesystem;
pub mod indexed;
pub mod lockfree_index;

pub use filesystem::FilesystemStorage;
pub use indexed::IndexedStorage;
pub use lockfree_index::{DirectoryEntry, DirectoryEntryKind, LockFreeIndex};

pub(crate) const MULTIPART_MIN_NON_FINAL_PART_SIZE_KEY: &str =
    "__sqrzl_multipart_min_non_final_part_size";
pub(crate) const MULTIPART_MAX_PART_SIZE_KEY: &str = "__sqrzl_multipart_max_part_size";
pub(crate) const MULTIPART_MAX_OBJECT_SIZE_KEY: &str = "__sqrzl_multipart_max_object_size";
pub(crate) const MULTIPART_TAGS_KEY: &str = "__sqrzl_multipart_tags";
pub(crate) const S3_MINIMUM_NON_FINAL_PART_SIZE: u64 = 5 * 1024 * 1024;
pub(crate) const S3_MAXIMUM_PART_SIZE: u64 = 5 * 1024 * 1024 * 1024;
pub(crate) const S3_MAXIMUM_OBJECT_SIZE: u64 = S3_MAXIMUM_PART_SIZE * 10_000;

/// A predicate evaluated while holding the per-object mutation lock.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ObjectCondition {
    All(Vec<ObjectCondition>),
    Missing,
    Etag(String),
    EtagIn(Vec<String>),
    EtagNotIn(Vec<String>),
    MissingOrEtagNotIn(Vec<String>),
    Metadata { key: String, value: String },
    MetadataNot { key: String, value: String },
}

/// Bucket metadata and lifecycle-independent bucket operations.
pub trait BucketStore: Send + Sync {
    ///
    /// # Errors
    ///
    /// Returns an error when the underlying emulator operation fails.
    fn create_bucket(&self, name: String) -> Result<()>;
    ///
    /// # Errors
    ///
    /// Returns an error when the underlying emulator operation fails.
    fn delete_bucket(&self, name: &str) -> Result<()>;
    ///
    /// # Errors
    ///
    /// Returns an error when the underlying emulator operation fails.
    fn get_bucket(&self, name: &str) -> Result<Bucket>;
    ///
    /// # Errors
    ///
    /// Returns an error when the underlying emulator operation fails.
    fn list_buckets(&self) -> Result<Vec<Bucket>>;
    ///
    /// # Errors
    ///
    /// Returns an error when the underlying emulator operation fails.
    fn bucket_exists(&self, name: &str) -> Result<bool>;
    ///
    /// # Errors
    ///
    /// Returns an error when the underlying emulator operation fails.
    fn update_bucket_metadata(
        &self,
        bucket: &str,
        metadata: HashMap<String, String>,
    ) -> Result<Bucket>;
}

/// Object read/write operations excluding list semantics.
pub trait ObjectStore: Send + Sync {
    ///
    /// # Errors
    ///
    /// Returns an error when the underlying emulator operation fails.
    fn put_object(&self, bucket: &str, key: String, object: Object) -> Result<()>;
    /// Atomically writes an object only when the current object matches `condition`.
    ///
    /// # Errors
    ///
    /// Returns an error when the underlying emulator operation fails.
    fn put_object_if(
        &self,
        bucket: &str,
        key: String,
        object: Object,
        condition: &ObjectCondition,
    ) -> Result<bool>;
    /// Atomically replaces content type, user metadata, and provider metadata
    /// when the currently stored object still has the observed identity and
    /// metadata.
    ///
    /// This preserves object bytes, `ETag`, last-modified time, version ID, and
    /// version history.
    ///
    /// # Errors
    ///
    /// Returns an error when the current metadata cannot be read or persisted.
    fn replace_object_metadata_if_unchanged(
        &self,
        bucket: &str,
        key: &str,
        observed: &Object,
        updated: &Object,
    ) -> Result<bool>;
    ///
    /// # Errors
    ///
    /// Returns an error when the underlying emulator operation fails.
    fn get_object(&self, bucket: &str, key: &str) -> Result<Object>;
    /// Reads an object's persisted attributes without loading its payload bytes.
    ///
    /// The default implementation preserves compatibility for backends without
    /// a metadata-specific read path, but may materialize the payload. Backends
    /// that store payload and metadata separately should override it.
    ///
    /// # Errors
    ///
    /// Returns an error when the underlying emulator operation fails.
    fn get_object_metadata(&self, bucket: &str, key: &str) -> Result<Object> {
        self.get_object(bucket, key)
    }
    ///
    /// # Errors
    ///
    /// Returns an error when the underlying emulator operation fails.
    fn get_object_range(
        &self,
        bucket: &str,
        key: &str,
        start: u64,
        end: Option<u64>,
    ) -> Result<(Object, Vec<u8>)>;
    ///
    /// # Errors
    ///
    /// Returns an error when the underlying emulator operation fails.
    fn delete_object(&self, bucket: &str, key: &str) -> Result<()>;
    /// Atomically deletes an object only when the current object matches `condition`.
    ///
    /// # Errors
    ///
    /// Returns an error when the underlying emulator operation fails.
    fn delete_object_if(
        &self,
        bucket: &str,
        key: &str,
        condition: &ObjectCondition,
    ) -> Result<bool>;
    ///
    /// # Errors
    ///
    /// Returns an error when the underlying emulator operation fails.
    fn update_object_storage_class(
        &self,
        bucket: &str,
        key: &str,
        storage_class: &str,
    ) -> Result<()>;
    ///
    /// # Errors
    ///
    /// Returns an error when the underlying emulator operation fails.
    fn object_exists(&self, bucket: &str, key: &str) -> Result<bool>;
    /// Persists `object` with its payload read from `payload_path` (a file
    /// already fully written to disk) rather than from `object.data`, so a
    /// backend that supports it can move the file into place instead of
    /// buffering the whole payload in memory. `object.size` and
    /// `object.etag` must already reflect the file's contents; `object.data`
    /// is ignored.
    ///
    /// The default implementation is a correctness fallback only — it reads
    /// `payload_path` fully into memory and delegates to [`Self::put_object`],
    /// so it does not itself avoid buffering. Backends that can move or
    /// stream the file directly (e.g. [`crate::storage::FilesystemStorage`])
    /// should override this.
    ///
    /// # Errors
    ///
    /// Returns an error when `payload_path` cannot be read or the write fails.
    fn put_object_streamed(
        &self,
        bucket: &str,
        key: String,
        mut object: Object,
        payload_path: &Path,
    ) -> Result<()> {
        object.data = std::fs::read(payload_path)
            .map_err(|e| Error::InternalError(format!("Failed to read spooled payload: {e}")))?;
        let _ = std::fs::remove_file(payload_path);
        self.put_object(bucket, key, object)
    }

    /// Atomically writes a spooled object when the current object matches
    /// `condition`. The default implementation is a compatibility fallback
    /// that materializes the payload; filesystem storage overrides it.
    ///
    /// # Errors
    ///
    /// Returns an error when the payload cannot be read or persisted.
    fn put_object_streamed_if(
        &self,
        bucket: &str,
        key: String,
        mut object: Object,
        payload_path: &Path,
        condition: &ObjectCondition,
    ) -> Result<bool> {
        object.data = std::fs::read(payload_path)
            .map_err(|e| Error::InternalError(format!("Failed to read spooled payload: {e}")))?;
        let _ = std::fs::remove_file(payload_path);
        self.put_object_if(bucket, key, object, condition)
    }
}

/// Object listing semantics, including delimiter and marker pagination behavior.
pub trait ObjectListingStore: Send + Sync {
    ///
    /// # Errors
    ///
    /// Returns an error when the underlying emulator operation fails.
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
    ///
    /// # Errors
    ///
    /// Returns an error when the underlying emulator operation fails.
    fn create_multipart_upload(&self, bucket: &str, key: String) -> Result<MultipartUpload>;
    ///
    /// # Errors
    ///
    /// Returns an error when the underlying emulator operation fails.
    fn create_multipart_upload_with_metadata(
        &self,
        bucket: &str,
        key: String,
        content_type: Option<String>,
        metadata: std::collections::HashMap<String, String>,
        provider_metadata: std::collections::HashMap<String, String>,
    ) -> Result<MultipartUpload>;
    ///
    /// # Errors
    ///
    /// Returns an error when the underlying emulator operation fails.
    fn upload_part(
        &self,
        bucket: &str,
        upload_id: &str,
        part_number: u32,
        data: Vec<u8>,
    ) -> Result<String>;
    ///
    /// # Errors
    ///
    /// Returns an error when the underlying emulator operation fails.
    fn list_multipart_uploads(&self, bucket: &str) -> Result<Vec<MultipartUpload>>;
    ///
    /// # Errors
    ///
    /// Returns an error when the underlying emulator operation fails.
    fn list_parts(&self, bucket: &str, upload_id: &str) -> Result<Vec<crate::models::Part>>;
    ///
    /// # Errors
    ///
    /// Returns an error when the underlying emulator operation fails.
    fn get_multipart_upload(&self, bucket: &str, upload_id: &str) -> Result<MultipartUpload>;
    ///
    /// # Errors
    ///
    /// Returns an error when the underlying emulator operation fails.
    fn complete_multipart_upload(&self, bucket: &str, upload_id: &str) -> Result<String>;
    /// Completes a multipart upload using only the ordered parts in the S3
    /// completion manifest. Parts uploaded but omitted from `parts` are
    /// discarded after the completed object is committed.
    ///
    /// # Errors
    ///
    /// Returns an error when a selected part is missing, its `ETag` changed, or
    /// the completed object violates provider size constraints.
    fn complete_multipart_upload_with_parts(
        &self,
        bucket: &str,
        upload_id: &str,
        parts: &[(u32, String)],
    ) -> Result<String> {
        let upload = self.get_multipart_upload(bucket, upload_id)?;
        let all_parts = upload
            .parts
            .iter()
            .map(|part| (part.part_number, part.etag.clone()))
            .collect::<Vec<_>>();
        if parts != all_parts {
            return Err(Error::IncompleteMultipartUpload);
        }
        self.complete_multipart_upload(bucket, upload_id)
    }
    ///
    /// # Errors
    ///
    /// Returns an error when the underlying emulator operation fails.
    fn abort_multipart_upload(&self, bucket: &str, upload_id: &str) -> Result<()>;
    /// Stores a part with its bytes read from `payload_path` (a file already
    /// fully written to disk) rather than passed in memory as `Vec<u8>`.
    /// `len` and `etag` must already reflect the file's contents.
    ///
    /// The default implementation is a correctness fallback only — it reads
    /// `payload_path` fully into memory and delegates to [`Self::upload_part`]
    /// (recomputing the `ETag` from those bytes), so it does not itself avoid
    /// buffering. Backends that can move or stream the file directly (e.g.
    /// [`crate::storage::FilesystemStorage`]) should override this.
    ///
    /// # Errors
    ///
    /// Returns an error when `payload_path` cannot be read or the write fails.
    fn upload_part_streamed(
        &self,
        bucket: &str,
        upload_id: &str,
        part_number: u32,
        payload_path: &Path,
        _len: u64,
        _etag: String,
    ) -> Result<String> {
        let data = std::fs::read(payload_path)
            .map_err(|e| Error::InternalError(format!("Failed to read spooled payload: {e}")))?;
        let _ = std::fs::remove_file(payload_path);
        self.upload_part(bucket, upload_id, part_number, data)
    }
}

/// Bucket versioning and object-version operations.
pub trait VersionStore: Send + Sync {
    ///
    /// # Errors
    ///
    /// Returns an error when the underlying emulator operation fails.
    fn enable_versioning(&self, bucket: &str) -> Result<()>;
    ///
    /// # Errors
    ///
    /// Returns an error when the underlying emulator operation fails.
    fn suspend_versioning(&self, bucket: &str) -> Result<()>;
    ///
    /// # Errors
    ///
    /// Returns an error when the underlying emulator operation fails.
    fn get_object_version(&self, bucket: &str, key: &str, version_id: &str) -> Result<Object>;
    ///
    /// # Errors
    ///
    /// Returns an error when the underlying emulator operation fails.
    fn list_object_versions(&self, bucket: &str, prefix: Option<&str>) -> Result<Vec<Object>>;
    ///
    /// # Errors
    ///
    /// Returns an error when the underlying emulator operation fails.
    fn list_object_versions_for_key(&self, bucket: &str, key: &str) -> Result<Vec<Object>> {
        self.list_object_versions(bucket, Some(key))
    }
    ///
    /// # Errors
    ///
    /// Returns an error when the underlying emulator operation fails.
    fn delete_object_version(&self, bucket: &str, key: &str, version_id: &str) -> Result<()>;
}

/// Object tag operations.
pub trait TagStore: Send + Sync {
    ///
    /// # Errors
    ///
    /// Returns an error when the underlying emulator operation fails.
    fn get_object_tags(&self, bucket: &str, key: &str) -> Result<HashMap<String, String>>;
    ///
    /// # Errors
    ///
    /// Returns an error when the underlying emulator operation fails.
    fn put_object_tags(&self, bucket: &str, key: &str, tags: HashMap<String, String>)
        -> Result<()>;
    ///
    /// # Errors
    ///
    /// Returns an error when the underlying emulator operation fails.
    fn delete_object_tags(&self, bucket: &str, key: &str) -> Result<()>;
}

/// Bucket and object ACL operations.
pub trait AclStore: Send + Sync {
    ///
    /// # Errors
    ///
    /// Returns an error when the underlying emulator operation fails.
    fn get_bucket_acl(&self, bucket: &str) -> Result<crate::models::policy::Acl>;
    ///
    /// # Errors
    ///
    /// Returns an error when the underlying emulator operation fails.
    fn put_bucket_acl(&self, bucket: &str, acl: crate::models::policy::Acl) -> Result<()>;
    ///
    /// # Errors
    ///
    /// Returns an error when the underlying emulator operation fails.
    fn get_object_acl(&self, bucket: &str, key: &str) -> Result<crate::models::policy::Acl>;
    ///
    /// # Errors
    ///
    /// Returns an error when the underlying emulator operation fails.
    fn put_object_acl(
        &self,
        bucket: &str,
        key: &str,
        acl: crate::models::policy::Acl,
    ) -> Result<()>;
}

/// Bucket lifecycle configuration operations.
pub trait LifecycleStore: Send + Sync {
    ///
    /// # Errors
    ///
    /// Returns an error when the underlying emulator operation fails.
    fn get_bucket_lifecycle(
        &self,
        bucket: &str,
    ) -> Result<crate::models::lifecycle::LifecycleConfiguration>;
    ///
    /// # Errors
    ///
    /// Returns an error when the underlying emulator operation fails.
    fn put_bucket_lifecycle(
        &self,
        bucket: &str,
        config: crate::models::lifecycle::LifecycleConfiguration,
    ) -> Result<()>;
    ///
    /// # Errors
    ///
    /// Returns an error when the underlying emulator operation fails.
    fn delete_bucket_lifecycle(&self, bucket: &str) -> Result<()>;
}

/// Bucket policy operations.
pub trait PolicyStore: Send + Sync {
    ///
    /// # Errors
    ///
    /// Returns an error when the underlying emulator operation fails.
    fn get_bucket_policy(
        &self,
        bucket: &str,
    ) -> Result<crate::models::policy::BucketPolicyDocument>;
    ///
    /// # Errors
    ///
    /// Returns an error when the underlying emulator operation fails.
    fn put_bucket_policy(
        &self,
        bucket: &str,
        policy: crate::models::policy::BucketPolicyDocument,
    ) -> Result<()>;
    ///
    /// # Errors
    ///
    /// Returns an error when the underlying emulator operation fails.
    fn delete_bucket_policy(&self, bucket: &str) -> Result<()>;
}

/// Provider session/state sidecars for restart-safe emulator workflows.
pub trait ProviderStateStore: Send + Sync {
    ///
    /// # Errors
    ///
    /// Returns an error when the underlying emulator operation fails.
    fn put_provider_state(&self, provider: &str, key: &str, data: Vec<u8>) -> Result<()>;
    ///
    /// # Errors
    ///
    /// Returns an error when the underlying emulator operation fails.
    fn get_provider_state(&self, provider: &str, key: &str) -> Result<Vec<u8>>;
    ///
    /// # Errors
    ///
    /// Returns an error when the underlying emulator operation fails.
    fn delete_provider_state(&self, provider: &str, key: &str) -> Result<()>;
}

/// Durable provider-native upload payloads. State documents contain only
/// metadata; bytes live in independently replaceable files addressed by an
/// opaque session and item key.
pub trait UploadStore: Send + Sync {
    /// Atomically stages all or a selected range of a spooled request body.
    ///
    /// # Errors
    ///
    /// Returns an error when the source range cannot be read or persisted.
    fn stage_upload_payload(
        &self,
        provider: &str,
        session: &str,
        item: &str,
        payload_path: &Path,
        source_offset: u64,
        len: u64,
    ) -> Result<()>;

    /// Streams staged items, in order, into an object. Returns `false` when
    /// an optional object precondition no longer matches.
    ///
    /// # Errors
    ///
    /// Returns an error when an item cannot be read, composed, or committed.
    #[allow(clippy::too_many_arguments)]
    fn compose_upload_payloads(
        &self,
        provider: &str,
        session: &str,
        items: &[String],
        bucket: &str,
        key: String,
        object: Object,
        condition: Option<&ObjectCondition>,
    ) -> Result<bool>;

    /// Removes staged items that are no longer referenced by provider state.
    ///
    /// # Errors
    ///
    /// Returns an error when obsolete staging files cannot be removed.
    fn retain_upload_items(&self, provider: &str, session: &str, items: &[String]) -> Result<()>;

    /// Removes all staged bytes for one provider session.
    ///
    /// # Errors
    ///
    /// Returns an error when the session staging directory cannot be removed.
    fn delete_upload_session(&self, provider: &str, session: &str) -> Result<()>;
}

/// Storage backend aggregate - synchronous operations.
/// HTTP layers handle async/await by calling these operations on request paths.
///
/// Prefer the focused capability traits in new private helpers. Keep this
/// aggregate for public compatibility and entrypoints that need to pass one
/// backend through multiple subsystems.
pub trait Storage: Send + Sync {
    ///
    /// # Errors
    ///
    /// Returns an error when the underlying emulator operation fails.
    fn create_bucket(&self, name: String) -> Result<()>;
    ///
    /// # Errors
    ///
    /// Returns an error when the underlying emulator operation fails.
    fn delete_bucket(&self, name: &str) -> Result<()>;
    ///
    /// # Errors
    ///
    /// Returns an error when the underlying emulator operation fails.
    fn get_bucket(&self, name: &str) -> Result<Bucket>;
    ///
    /// # Errors
    ///
    /// Returns an error when the underlying emulator operation fails.
    fn list_buckets(&self) -> Result<Vec<Bucket>>;
    ///
    /// # Errors
    ///
    /// Returns an error when the underlying emulator operation fails.
    fn bucket_exists(&self, name: &str) -> Result<bool>;
    ///
    /// # Errors
    ///
    /// Returns an error when the underlying emulator operation fails.
    fn update_bucket_metadata(
        &self,
        bucket: &str,
        metadata: HashMap<String, String>,
    ) -> Result<Bucket>;

    ///
    /// # Errors
    ///
    /// Returns an error when the underlying emulator operation fails.
    fn put_object(&self, bucket: &str, key: String, object: Object) -> Result<()>;
    ///
    /// # Errors
    ///
    /// Returns an error when the conditional write cannot be evaluated or persisted.
    fn put_object_if(
        &self,
        bucket: &str,
        key: String,
        object: Object,
        condition: &ObjectCondition,
    ) -> Result<bool>;
    /// Atomically replaces content type, user metadata, and provider metadata
    /// without creating an object version or changing object identity fields.
    ///
    /// # Errors
    ///
    /// Returns an error when the current metadata cannot be read or persisted.
    fn replace_object_metadata_if_unchanged(
        &self,
        bucket: &str,
        key: &str,
        observed: &Object,
        updated: &Object,
    ) -> Result<bool>;
    ///
    /// # Errors
    ///
    /// Returns an error when the underlying emulator operation fails.
    fn get_object(&self, bucket: &str, key: &str) -> Result<Object>;
    /// Reads object attributes without requiring payload bytes when supported.
    ///
    /// # Errors
    ///
    /// Returns an error when the underlying emulator operation fails.
    fn get_object_metadata(&self, bucket: &str, key: &str) -> Result<Object> {
        self.get_object(bucket, key)
    }
    ///
    /// # Errors
    ///
    /// Returns an error when the underlying emulator operation fails.
    fn get_object_range(
        &self,
        bucket: &str,
        key: &str,
        start: u64,
        end: Option<u64>,
    ) -> Result<(Object, Vec<u8>)>;
    ///
    /// # Errors
    ///
    /// Returns an error when the underlying emulator operation fails.
    fn delete_object(&self, bucket: &str, key: &str) -> Result<()>;
    ///
    /// # Errors
    ///
    /// Returns an error when the conditional delete cannot be evaluated or persisted.
    fn delete_object_if(
        &self,
        bucket: &str,
        key: &str,
        condition: &ObjectCondition,
    ) -> Result<bool>;
    ///
    /// # Errors
    ///
    /// Returns an error when the underlying emulator operation fails.
    fn update_object_storage_class(
        &self,
        bucket: &str,
        key: &str,
        storage_class: &str,
    ) -> Result<()>;
    ///
    /// # Errors
    ///
    /// Returns an error when the underlying emulator operation fails.
    fn object_exists(&self, bucket: &str, key: &str) -> Result<bool>;
    /// See [`ObjectStore::put_object_streamed`].
    ///
    /// # Errors
    ///
    /// Returns an error when `payload_path` cannot be read or the write fails.
    fn put_object_streamed(
        &self,
        bucket: &str,
        key: String,
        object: Object,
        payload_path: &Path,
    ) -> Result<()>;
    /// See [`ObjectStore::put_object_streamed_if`].
    ///
    /// # Errors
    ///
    /// Returns an error when `payload_path` cannot be read or the conditional
    /// write fails.
    fn put_object_streamed_if(
        &self,
        bucket: &str,
        key: String,
        object: Object,
        payload_path: &Path,
        condition: &ObjectCondition,
    ) -> Result<bool>;
    ///
    /// # Errors
    ///
    /// Returns an error when the underlying emulator operation fails.
    fn list_objects(
        &self,
        bucket: &str,
        prefix: Option<&str>,
        delimiter: Option<&str>,
        marker: Option<&str>,
        max_keys: Option<usize>,
    ) -> Result<ListObjectsResult>;

    ///
    /// # Errors
    ///
    /// Returns an error when the underlying emulator operation fails.
    fn create_multipart_upload(&self, bucket: &str, key: String) -> Result<MultipartUpload>;
    ///
    /// # Errors
    ///
    /// Returns an error when the underlying emulator operation fails.
    fn create_multipart_upload_with_metadata(
        &self,
        bucket: &str,
        key: String,
        content_type: Option<String>,
        metadata: HashMap<String, String>,
        provider_metadata: HashMap<String, String>,
    ) -> Result<MultipartUpload>;
    ///
    /// # Errors
    ///
    /// Returns an error when the underlying emulator operation fails.
    fn upload_part(
        &self,
        bucket: &str,
        upload_id: &str,
        part_number: u32,
        data: Vec<u8>,
    ) -> Result<String>;
    ///
    /// # Errors
    ///
    /// Returns an error when the underlying emulator operation fails.
    fn list_multipart_uploads(&self, bucket: &str) -> Result<Vec<MultipartUpload>>;
    ///
    /// # Errors
    ///
    /// Returns an error when the underlying emulator operation fails.
    fn list_parts(&self, bucket: &str, upload_id: &str) -> Result<Vec<crate::models::Part>>;
    ///
    /// # Errors
    ///
    /// Returns an error when the underlying emulator operation fails.
    fn get_multipart_upload(&self, bucket: &str, upload_id: &str) -> Result<MultipartUpload>;
    ///
    /// # Errors
    ///
    /// Returns an error when the underlying emulator operation fails.
    fn complete_multipart_upload(&self, bucket: &str, upload_id: &str) -> Result<String>;
    /// See [`MultipartStore::complete_multipart_upload_with_parts`].
    ///
    /// # Errors
    ///
    /// Returns an error when the multipart completion manifest is invalid.
    fn complete_multipart_upload_with_parts(
        &self,
        bucket: &str,
        upload_id: &str,
        parts: &[(u32, String)],
    ) -> Result<String>;
    ///
    /// # Errors
    ///
    /// Returns an error when the underlying emulator operation fails.
    fn abort_multipart_upload(&self, bucket: &str, upload_id: &str) -> Result<()>;
    /// See [`MultipartStore::upload_part_streamed`].
    ///
    /// # Errors
    ///
    /// Returns an error when `payload_path` cannot be read or the write fails.
    fn upload_part_streamed(
        &self,
        bucket: &str,
        upload_id: &str,
        part_number: u32,
        payload_path: &Path,
        len: u64,
        etag: String,
    ) -> Result<String>;

    ///
    /// # Errors
    ///
    /// Returns an error when the underlying emulator operation fails.
    fn enable_versioning(&self, bucket: &str) -> Result<()>;
    ///
    /// # Errors
    ///
    /// Returns an error when the underlying emulator operation fails.
    fn suspend_versioning(&self, bucket: &str) -> Result<()>;
    ///
    /// # Errors
    ///
    /// Returns an error when the underlying emulator operation fails.
    fn get_object_version(&self, bucket: &str, key: &str, version_id: &str) -> Result<Object>;
    ///
    /// # Errors
    ///
    /// Returns an error when the underlying emulator operation fails.
    fn list_object_versions(&self, bucket: &str, prefix: Option<&str>) -> Result<Vec<Object>>;
    ///
    /// # Errors
    ///
    /// Returns an error when the underlying emulator operation fails.
    fn list_object_versions_for_key(&self, bucket: &str, key: &str) -> Result<Vec<Object>>;
    ///
    /// # Errors
    ///
    /// Returns an error when the underlying emulator operation fails.
    fn delete_object_version(&self, bucket: &str, key: &str, version_id: &str) -> Result<()>;

    ///
    /// # Errors
    ///
    /// Returns an error when the underlying emulator operation fails.
    fn get_object_tags(&self, bucket: &str, key: &str) -> Result<HashMap<String, String>>;
    ///
    /// # Errors
    ///
    /// Returns an error when the underlying emulator operation fails.
    fn put_object_tags(&self, bucket: &str, key: &str, tags: HashMap<String, String>)
        -> Result<()>;
    ///
    /// # Errors
    ///
    /// Returns an error when the underlying emulator operation fails.
    fn delete_object_tags(&self, bucket: &str, key: &str) -> Result<()>;

    ///
    /// # Errors
    ///
    /// Returns an error when the underlying emulator operation fails.
    fn get_bucket_acl(&self, bucket: &str) -> Result<crate::models::policy::Acl>;
    ///
    /// # Errors
    ///
    /// Returns an error when the underlying emulator operation fails.
    fn put_bucket_acl(&self, bucket: &str, acl: crate::models::policy::Acl) -> Result<()>;
    ///
    /// # Errors
    ///
    /// Returns an error when the underlying emulator operation fails.
    fn get_object_acl(&self, bucket: &str, key: &str) -> Result<crate::models::policy::Acl>;
    ///
    /// # Errors
    ///
    /// Returns an error when the underlying emulator operation fails.
    fn put_object_acl(
        &self,
        bucket: &str,
        key: &str,
        acl: crate::models::policy::Acl,
    ) -> Result<()>;

    ///
    /// # Errors
    ///
    /// Returns an error when the underlying emulator operation fails.
    fn get_bucket_lifecycle(
        &self,
        bucket: &str,
    ) -> Result<crate::models::lifecycle::LifecycleConfiguration>;
    ///
    /// # Errors
    ///
    /// Returns an error when the underlying emulator operation fails.
    fn put_bucket_lifecycle(
        &self,
        bucket: &str,
        config: crate::models::lifecycle::LifecycleConfiguration,
    ) -> Result<()>;
    ///
    /// # Errors
    ///
    /// Returns an error when the underlying emulator operation fails.
    fn delete_bucket_lifecycle(&self, bucket: &str) -> Result<()>;

    ///
    /// # Errors
    ///
    /// Returns an error when the underlying emulator operation fails.
    fn get_bucket_policy(
        &self,
        bucket: &str,
    ) -> Result<crate::models::policy::BucketPolicyDocument>;
    ///
    /// # Errors
    ///
    /// Returns an error when the underlying emulator operation fails.
    fn put_bucket_policy(
        &self,
        bucket: &str,
        policy: crate::models::policy::BucketPolicyDocument,
    ) -> Result<()>;
    ///
    /// # Errors
    ///
    /// Returns an error when the underlying emulator operation fails.
    fn delete_bucket_policy(&self, bucket: &str) -> Result<()>;

    ///
    /// # Errors
    ///
    /// Returns an error when the underlying emulator operation fails.
    fn put_provider_state(&self, provider: &str, key: &str, data: Vec<u8>) -> Result<()>;
    ///
    /// # Errors
    ///
    /// Returns an error when the underlying emulator operation fails.
    fn get_provider_state(&self, provider: &str, key: &str) -> Result<Vec<u8>>;
    ///
    /// # Errors
    ///
    /// Returns an error when the underlying emulator operation fails.
    fn delete_provider_state(&self, provider: &str, key: &str) -> Result<()>;

    /// See [`UploadStore::stage_upload_payload`].
    ///
    /// # Errors
    ///
    /// Returns an error when the source range cannot be read or persisted.
    fn stage_upload_payload(
        &self,
        provider: &str,
        session: &str,
        item: &str,
        payload_path: &Path,
        source_offset: u64,
        len: u64,
    ) -> Result<()>;
    /// See [`UploadStore::compose_upload_payloads`].
    ///
    /// # Errors
    ///
    /// Returns an error when an item cannot be read, composed, or committed.
    #[allow(clippy::too_many_arguments)]
    fn compose_upload_payloads(
        &self,
        provider: &str,
        session: &str,
        items: &[String],
        bucket: &str,
        key: String,
        object: Object,
        condition: Option<&ObjectCondition>,
    ) -> Result<bool>;
    /// See [`UploadStore::retain_upload_items`].
    ///
    /// # Errors
    ///
    /// Returns an error when obsolete staging files cannot be removed.
    fn retain_upload_items(&self, provider: &str, session: &str, items: &[String]) -> Result<()>;
    /// See [`UploadStore::delete_upload_session`].
    ///
    /// # Errors
    ///
    /// Returns an error when the session staging directory cannot be removed.
    fn delete_upload_session(&self, provider: &str, session: &str) -> Result<()>;
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
        + UploadStore
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

    fn put_object_if(
        &self,
        bucket: &str,
        key: String,
        object: Object,
        condition: &ObjectCondition,
    ) -> Result<bool> {
        ObjectStore::put_object_if(self, bucket, key, object, condition)
    }

    fn replace_object_metadata_if_unchanged(
        &self,
        bucket: &str,
        key: &str,
        observed: &Object,
        updated: &Object,
    ) -> Result<bool> {
        ObjectStore::replace_object_metadata_if_unchanged(self, bucket, key, observed, updated)
    }

    fn get_object(&self, bucket: &str, key: &str) -> Result<Object> {
        ObjectStore::get_object(self, bucket, key)
    }

    fn get_object_metadata(&self, bucket: &str, key: &str) -> Result<Object> {
        ObjectStore::get_object_metadata(self, bucket, key)
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

    fn delete_object_if(
        &self,
        bucket: &str,
        key: &str,
        condition: &ObjectCondition,
    ) -> Result<bool> {
        ObjectStore::delete_object_if(self, bucket, key, condition)
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

    fn put_object_streamed(
        &self,
        bucket: &str,
        key: String,
        object: Object,
        payload_path: &Path,
    ) -> Result<()> {
        ObjectStore::put_object_streamed(self, bucket, key, object, payload_path)
    }

    fn put_object_streamed_if(
        &self,
        bucket: &str,
        key: String,
        object: Object,
        payload_path: &Path,
        condition: &ObjectCondition,
    ) -> Result<bool> {
        ObjectStore::put_object_streamed_if(self, bucket, key, object, payload_path, condition)
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

    fn complete_multipart_upload_with_parts(
        &self,
        bucket: &str,
        upload_id: &str,
        parts: &[(u32, String)],
    ) -> Result<String> {
        MultipartStore::complete_multipart_upload_with_parts(self, bucket, upload_id, parts)
    }

    fn abort_multipart_upload(&self, bucket: &str, upload_id: &str) -> Result<()> {
        MultipartStore::abort_multipart_upload(self, bucket, upload_id)
    }

    fn upload_part_streamed(
        &self,
        bucket: &str,
        upload_id: &str,
        part_number: u32,
        payload_path: &Path,
        len: u64,
        etag: String,
    ) -> Result<String> {
        MultipartStore::upload_part_streamed(
            self,
            bucket,
            upload_id,
            part_number,
            payload_path,
            len,
            etag,
        )
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

    fn stage_upload_payload(
        &self,
        provider: &str,
        session: &str,
        item: &str,
        payload_path: &Path,
        source_offset: u64,
        len: u64,
    ) -> Result<()> {
        UploadStore::stage_upload_payload(
            self,
            provider,
            session,
            item,
            payload_path,
            source_offset,
            len,
        )
    }

    fn compose_upload_payloads(
        &self,
        provider: &str,
        session: &str,
        items: &[String],
        bucket: &str,
        key: String,
        object: Object,
        condition: Option<&ObjectCondition>,
    ) -> Result<bool> {
        UploadStore::compose_upload_payloads(
            self, provider, session, items, bucket, key, object, condition,
        )
    }

    fn delete_upload_session(&self, provider: &str, session: &str) -> Result<()> {
        UploadStore::delete_upload_session(self, provider, session)
    }

    fn retain_upload_items(&self, provider: &str, session: &str, items: &[String]) -> Result<()> {
        UploadStore::retain_upload_items(self, provider, session, items)
    }
}

impl BucketStore for dyn Storage + '_ {
    fn create_bucket(&self, name: String) -> Result<()> {
        Storage::create_bucket(self, name)
    }

    fn delete_bucket(&self, name: &str) -> Result<()> {
        Storage::delete_bucket(self, name)
    }

    fn get_bucket(&self, name: &str) -> Result<Bucket> {
        Storage::get_bucket(self, name)
    }

    fn list_buckets(&self) -> Result<Vec<Bucket>> {
        Storage::list_buckets(self)
    }

    fn bucket_exists(&self, name: &str) -> Result<bool> {
        Storage::bucket_exists(self, name)
    }

    fn update_bucket_metadata(
        &self,
        bucket: &str,
        metadata: HashMap<String, String>,
    ) -> Result<Bucket> {
        Storage::update_bucket_metadata(self, bucket, metadata)
    }
}

impl ObjectStore for dyn Storage + '_ {
    fn put_object(&self, bucket: &str, key: String, object: Object) -> Result<()> {
        Storage::put_object(self, bucket, key, object)
    }

    fn put_object_if(
        &self,
        bucket: &str,
        key: String,
        object: Object,
        condition: &ObjectCondition,
    ) -> Result<bool> {
        Storage::put_object_if(self, bucket, key, object, condition)
    }

    fn replace_object_metadata_if_unchanged(
        &self,
        bucket: &str,
        key: &str,
        observed: &Object,
        updated: &Object,
    ) -> Result<bool> {
        Storage::replace_object_metadata_if_unchanged(self, bucket, key, observed, updated)
    }

    fn get_object(&self, bucket: &str, key: &str) -> Result<Object> {
        Storage::get_object(self, bucket, key)
    }

    fn get_object_metadata(&self, bucket: &str, key: &str) -> Result<Object> {
        Storage::get_object_metadata(self, bucket, key)
    }

    fn get_object_range(
        &self,
        bucket: &str,
        key: &str,
        start: u64,
        end: Option<u64>,
    ) -> Result<(Object, Vec<u8>)> {
        Storage::get_object_range(self, bucket, key, start, end)
    }

    fn delete_object(&self, bucket: &str, key: &str) -> Result<()> {
        Storage::delete_object(self, bucket, key)
    }

    fn delete_object_if(
        &self,
        bucket: &str,
        key: &str,
        condition: &ObjectCondition,
    ) -> Result<bool> {
        Storage::delete_object_if(self, bucket, key, condition)
    }

    fn update_object_storage_class(
        &self,
        bucket: &str,
        key: &str,
        storage_class: &str,
    ) -> Result<()> {
        Storage::update_object_storage_class(self, bucket, key, storage_class)
    }

    fn object_exists(&self, bucket: &str, key: &str) -> Result<bool> {
        Storage::object_exists(self, bucket, key)
    }

    fn put_object_streamed(
        &self,
        bucket: &str,
        key: String,
        object: Object,
        payload_path: &Path,
    ) -> Result<()> {
        Storage::put_object_streamed(self, bucket, key, object, payload_path)
    }

    fn put_object_streamed_if(
        &self,
        bucket: &str,
        key: String,
        object: Object,
        payload_path: &Path,
        condition: &ObjectCondition,
    ) -> Result<bool> {
        Storage::put_object_streamed_if(self, bucket, key, object, payload_path, condition)
    }
}

impl ObjectListingStore for dyn Storage + '_ {
    fn list_objects(
        &self,
        bucket: &str,
        prefix: Option<&str>,
        delimiter: Option<&str>,
        marker: Option<&str>,
        max_keys: Option<usize>,
    ) -> Result<ListObjectsResult> {
        Storage::list_objects(self, bucket, prefix, delimiter, marker, max_keys)
    }
}

impl MultipartStore for dyn Storage + '_ {
    fn create_multipart_upload(&self, bucket: &str, key: String) -> Result<MultipartUpload> {
        Storage::create_multipart_upload(self, bucket, key)
    }

    fn create_multipart_upload_with_metadata(
        &self,
        bucket: &str,
        key: String,
        content_type: Option<String>,
        metadata: HashMap<String, String>,
        provider_metadata: HashMap<String, String>,
    ) -> Result<MultipartUpload> {
        Storage::create_multipart_upload_with_metadata(
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
        Storage::upload_part(self, bucket, upload_id, part_number, data)
    }

    fn list_multipart_uploads(&self, bucket: &str) -> Result<Vec<MultipartUpload>> {
        Storage::list_multipart_uploads(self, bucket)
    }

    fn list_parts(&self, bucket: &str, upload_id: &str) -> Result<Vec<crate::models::Part>> {
        Storage::list_parts(self, bucket, upload_id)
    }

    fn get_multipart_upload(&self, bucket: &str, upload_id: &str) -> Result<MultipartUpload> {
        Storage::get_multipart_upload(self, bucket, upload_id)
    }

    fn complete_multipart_upload(&self, bucket: &str, upload_id: &str) -> Result<String> {
        Storage::complete_multipart_upload(self, bucket, upload_id)
    }

    fn complete_multipart_upload_with_parts(
        &self,
        bucket: &str,
        upload_id: &str,
        parts: &[(u32, String)],
    ) -> Result<String> {
        Storage::complete_multipart_upload_with_parts(self, bucket, upload_id, parts)
    }

    fn abort_multipart_upload(&self, bucket: &str, upload_id: &str) -> Result<()> {
        Storage::abort_multipart_upload(self, bucket, upload_id)
    }

    fn upload_part_streamed(
        &self,
        bucket: &str,
        upload_id: &str,
        part_number: u32,
        payload_path: &Path,
        len: u64,
        etag: String,
    ) -> Result<String> {
        Storage::upload_part_streamed(
            self,
            bucket,
            upload_id,
            part_number,
            payload_path,
            len,
            etag,
        )
    }
}

impl VersionStore for dyn Storage + '_ {
    fn enable_versioning(&self, bucket: &str) -> Result<()> {
        Storage::enable_versioning(self, bucket)
    }

    fn suspend_versioning(&self, bucket: &str) -> Result<()> {
        Storage::suspend_versioning(self, bucket)
    }

    fn get_object_version(&self, bucket: &str, key: &str, version_id: &str) -> Result<Object> {
        Storage::get_object_version(self, bucket, key, version_id)
    }

    fn list_object_versions(&self, bucket: &str, prefix: Option<&str>) -> Result<Vec<Object>> {
        Storage::list_object_versions(self, bucket, prefix)
    }

    fn list_object_versions_for_key(&self, bucket: &str, key: &str) -> Result<Vec<Object>> {
        Storage::list_object_versions_for_key(self, bucket, key)
    }

    fn delete_object_version(&self, bucket: &str, key: &str, version_id: &str) -> Result<()> {
        Storage::delete_object_version(self, bucket, key, version_id)
    }
}

impl TagStore for dyn Storage + '_ {
    fn get_object_tags(&self, bucket: &str, key: &str) -> Result<HashMap<String, String>> {
        Storage::get_object_tags(self, bucket, key)
    }

    fn put_object_tags(
        &self,
        bucket: &str,
        key: &str,
        tags: HashMap<String, String>,
    ) -> Result<()> {
        Storage::put_object_tags(self, bucket, key, tags)
    }

    fn delete_object_tags(&self, bucket: &str, key: &str) -> Result<()> {
        Storage::delete_object_tags(self, bucket, key)
    }
}

impl AclStore for dyn Storage + '_ {
    fn get_bucket_acl(&self, bucket: &str) -> Result<crate::models::policy::Acl> {
        Storage::get_bucket_acl(self, bucket)
    }

    fn put_bucket_acl(&self, bucket: &str, acl: crate::models::policy::Acl) -> Result<()> {
        Storage::put_bucket_acl(self, bucket, acl)
    }

    fn get_object_acl(&self, bucket: &str, key: &str) -> Result<crate::models::policy::Acl> {
        Storage::get_object_acl(self, bucket, key)
    }

    fn put_object_acl(
        &self,
        bucket: &str,
        key: &str,
        acl: crate::models::policy::Acl,
    ) -> Result<()> {
        Storage::put_object_acl(self, bucket, key, acl)
    }
}

impl LifecycleStore for dyn Storage + '_ {
    fn get_bucket_lifecycle(
        &self,
        bucket: &str,
    ) -> Result<crate::models::lifecycle::LifecycleConfiguration> {
        Storage::get_bucket_lifecycle(self, bucket)
    }

    fn put_bucket_lifecycle(
        &self,
        bucket: &str,
        config: crate::models::lifecycle::LifecycleConfiguration,
    ) -> Result<()> {
        Storage::put_bucket_lifecycle(self, bucket, config)
    }

    fn delete_bucket_lifecycle(&self, bucket: &str) -> Result<()> {
        Storage::delete_bucket_lifecycle(self, bucket)
    }
}

impl PolicyStore for dyn Storage + '_ {
    fn get_bucket_policy(
        &self,
        bucket: &str,
    ) -> Result<crate::models::policy::BucketPolicyDocument> {
        Storage::get_bucket_policy(self, bucket)
    }

    fn put_bucket_policy(
        &self,
        bucket: &str,
        policy: crate::models::policy::BucketPolicyDocument,
    ) -> Result<()> {
        Storage::put_bucket_policy(self, bucket, policy)
    }

    fn delete_bucket_policy(&self, bucket: &str) -> Result<()> {
        Storage::delete_bucket_policy(self, bucket)
    }
}

impl ProviderStateStore for dyn Storage + '_ {
    fn put_provider_state(&self, provider: &str, key: &str, data: Vec<u8>) -> Result<()> {
        Storage::put_provider_state(self, provider, key, data)
    }

    fn get_provider_state(&self, provider: &str, key: &str) -> Result<Vec<u8>> {
        Storage::get_provider_state(self, provider, key)
    }

    fn delete_provider_state(&self, provider: &str, key: &str) -> Result<()> {
        Storage::delete_provider_state(self, provider, key)
    }
}

impl UploadStore for dyn Storage + '_ {
    fn stage_upload_payload(
        &self,
        provider: &str,
        session: &str,
        item: &str,
        payload_path: &Path,
        source_offset: u64,
        len: u64,
    ) -> Result<()> {
        Storage::stage_upload_payload(
            self,
            provider,
            session,
            item,
            payload_path,
            source_offset,
            len,
        )
    }

    fn compose_upload_payloads(
        &self,
        provider: &str,
        session: &str,
        items: &[String],
        bucket: &str,
        key: String,
        object: Object,
        condition: Option<&ObjectCondition>,
    ) -> Result<bool> {
        Storage::compose_upload_payloads(
            self, provider, session, items, bucket, key, object, condition,
        )
    }

    fn delete_upload_session(&self, provider: &str, session: &str) -> Result<()> {
        Storage::delete_upload_session(self, provider, session)
    }

    fn retain_upload_items(&self, provider: &str, session: &str, items: &[String]) -> Result<()> {
        Storage::retain_upload_items(self, provider, session, items)
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
    fn should_drive_basic_bucket_object_listing_flow_with_focused_traits() {
        // Arrange
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

        // Act
        let listing: &dyn ObjectListingStore = &storage;
        let result = listing
            .list_objects("capability", Some("docs/"), None, None, Some(10))
            .unwrap();

        // Assert
        assert_eq!(result.objects.len(), 1);
        assert_eq!(result.objects[0].key, "docs/readme.txt");

        let _ = std::fs::remove_dir_all(&base);
    }
}
