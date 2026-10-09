use crate::error::{Error, Result};
use crate::models::{policy::Acl, Bucket, MultipartUpload, Object};
use crate::storage::{
    AclStore, BucketStore, DirectoryEntry, DirectoryEntryKind, LifecycleStore, LockFreeIndex,
    MultipartStore, ObjectCondition, ObjectListingStore, ObjectStore, PolicyStore,
    ProviderStateStore, TagStore, UploadStore, VersionStore, MULTIPART_MAX_OBJECT_SIZE_KEY,
    MULTIPART_MAX_PART_SIZE_KEY, MULTIPART_MIN_NON_FINAL_PART_SIZE_KEY, MULTIPART_TAGS_KEY,
    S3_MAXIMUM_OBJECT_SIZE, S3_MAXIMUM_PART_SIZE, S3_MINIMUM_NON_FINAL_PART_SIZE,
};
use std::collections::HashMap;
use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, Weak};
use uuid::Uuid;

#[cfg(test)]
mod consistency_tests;
#[cfg(test)]
mod crash_tests;
mod io;
mod publication;

#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TestPhase {
    ReadMetadata,
    ReadPayloadMetadata,
    FullPayload,
    BodyPrepared,
    MetadataPrepared,
    VersionBodyPrepared,
    VersionMetadataPrepared,
    BodyPublished,
    DirectoryCreated,
    PublicationStaged,
    PublicationCommitted,
    MetadataPublished,
    PublicationCleaned,
    VersionBodyPublished,
    VersionMetadataPublished,
    VersionPublished,
    VersionDeleted,
    VersionRetiring,
    MarkerBodyPublished,
    MarkerMetadataPublished,
    CurrentBodyRemoved,
    CurrentRemoved,
    UploadCleanup,
    UploadRecordRetired,
    UploadCleanupDone,
}

#[cfg(test)]
type TestHook = Arc<dyn Fn(TestPhase) + Send + Sync>;

/// Where a write's payload bytes come from: already resident in
/// `Object.data`, or already spooled to a file on disk (so the write can
/// move it into place instead of copying it through memory).
#[derive(Clone, Copy)]
enum ObjectPayload<'a> {
    InMemory,
    Spooled(&'a Path),
    /// Existing immutable generation bytes stay in place until the decision commits.
    Stored(&'a Path),
}

pub struct FilesystemStorage {
    base_path: PathBuf,
    index: Arc<LockFreeIndex>,
    uploads_cache: Mutex<HashMap<String, HashMap<String, MultipartUpload>>>,
    object_locks: Mutex<HashMap<String, Weak<Mutex<()>>>>,
    bucket_locks: Mutex<HashMap<String, Weak<Mutex<()>>>>,
    #[cfg(test)]
    test_hook: Mutex<Option<TestHook>>,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct BucketIdentity {
    created_at: chrono::DateTime<chrono::Utc>,
    modified_at: chrono::DateTime<chrono::Utc>,
}

impl BucketStore for FilesystemStorage {
    fn create_bucket(&self, name: String) -> Result<()> {
        let lock = self.bucket_lock(&name)?;
        let _guard = lock
            .lock()
            .map_err(|_| Error::InternalError("Failed to lock bucket for creation".to_string()))?;
        let bucket_dir = self.bucket_dir(&name);

        if bucket_dir.exists() {
            return Err(Error::BucketAlreadyExists);
        }

        Self::create_directory_durable(&bucket_dir)?;
        Self::atomic_write(&bucket_dir.join(".bucket.name"), name.as_bytes())
            .map_err(|e| Error::InternalError(format!("Failed to persist bucket identity: {e}")))?;
        let now = chrono::Utc::now();
        if let Err(error) = self.write_bucket_identity(
            &name,
            &BucketIdentity {
                created_at: now,
                modified_at: now,
            },
        ) {
            fs::remove_dir_all(&bucket_dir).map_err(|rollback| {
                Error::InternalError(format!(
                    "{error}; failed to roll back bucket creation: {rollback}"
                ))
            })?;
            return Err(error);
        }

        // Update index
        self.index.get_or_create_bucket(name);

        Ok(())
    }

    fn delete_bucket(&self, name: &str) -> Result<()> {
        let lock = self.bucket_lock(name)?;
        let _guard = lock
            .lock()
            .map_err(|_| Error::InternalError("Failed to lock bucket for deletion".to_string()))?;
        let bucket_dir = self.bucket_dir(name);

        if !bucket_dir.exists() {
            return Err(Error::BucketNotFound);
        }

        // Check if bucket is empty
        let entries = fs::read_dir(&bucket_dir)
            .map_err(|e| Error::InternalError(format!("Failed to read bucket: {e}")))?;

        for entry in entries {
            let entry = entry
                .map_err(|e| Error::InternalError(format!("Failed to read bucket entry: {e}")))?;
            if !Self::is_bucket_control_entry(&entry) {
                return Err(Error::BucketNotEmpty);
            }
        }

        fs::remove_dir_all(&bucket_dir)
            .map_err(|e| Error::InternalError(format!("Failed to delete bucket: {e}")))?;

        // Update index
        self.index.clear_bucket(name);

        Ok(())
    }

    fn get_bucket(&self, name: &str) -> Result<Bucket> {
        let lock = self.bucket_lock(name)?;
        let _guard = lock
            .lock()
            .map_err(|_| Error::InternalError("Failed to lock bucket for reading".to_string()))?;
        self.get_bucket_locked(name)
    }

    fn list_buckets(&self) -> Result<Vec<Bucket>> {
        let mut buckets = Vec::new();
        let entries = fs::read_dir(&self.base_path)
            .map_err(|e| Error::InternalError(format!("Failed to read base path: {e}")))?;

        for entry in entries {
            let entry =
                entry.map_err(|e| Error::InternalError(format!("Failed to read entry: {e}")))?;

            let metadata = entry
                .metadata()
                .map_err(|e| Error::InternalError(format!("Failed to get metadata: {e}")))?;

            if metadata.is_dir() {
                if let Ok(bucket_name) = fs::read_to_string(entry.path().join(".bucket.name")) {
                    buckets.push(self.get_bucket(&bucket_name)?);
                }
            }
        }

        buckets.sort_by(|left, right| left.name.cmp(&right.name));
        Ok(buckets)
    }

    fn bucket_exists(&self, name: &str) -> Result<bool> {
        Ok(self.bucket_dir(name).exists())
    }

    fn update_bucket_metadata(
        &self,
        bucket: &str,
        metadata: HashMap<String, String>,
    ) -> Result<Bucket> {
        let lock = self.bucket_lock(bucket)?;
        let _guard = lock.lock().map_err(|_| {
            Error::InternalError("Failed to lock bucket for metadata update".to_string())
        })?;
        if !self.bucket_exists(bucket)? {
            return Err(Error::BucketNotFound);
        }

        let mut record = self.get_bucket_locked(bucket)?;
        if record.metadata != metadata {
            self.write_bucket_metadata(bucket, &metadata)?;
            record.metadata = metadata;
            record.modified_at = chrono::Utc::now();
            self.write_bucket_identity(
                bucket,
                &BucketIdentity {
                    created_at: record.created_at,
                    modified_at: record.modified_at,
                },
            )?;
        }
        Ok(record)
    }
}

impl FilesystemStorage {
    fn get_bucket_locked(&self, name: &str) -> Result<Bucket> {
        if !self.bucket_dir(name).exists() {
            return Err(Error::BucketNotFound);
        }
        let identity = self.read_bucket_identity_locked(name)?;
        let mut bucket = Bucket::new(name.to_string());
        bucket.created_at = identity.created_at;
        bucket.modified_at = identity.modified_at;
        bucket.versioning_enabled = self.versioning_enabled(name);
        bucket.metadata = self.read_bucket_metadata(name)?;
        Ok(bucket)
    }

    #[cfg(test)]
    fn test_phase(&self, phase: TestPhase) {
        let hook = self.test_hook.lock().unwrap().clone();
        if let Some(hook) = hook {
            hook(phase);
        }
    }

    fn validate_version_id(version_id: &str) -> Result<()> {
        let mut components = std::path::Path::new(version_id).components();
        let is_single_normal_component =
            matches!(components.next(), Some(std::path::Component::Normal(_)))
                && components.next().is_none();
        let has_cross_platform_separator = version_id
            .chars()
            .any(|character| matches!(character, '/' | '\\' | '\0'));
        if !is_single_normal_component || has_cross_platform_separator {
            return Err(Error::NoSuchVersion);
        }
        Ok(())
    }

    fn object_condition_matches(
        &self,
        bucket: &str,
        key: &str,
        condition: &ObjectCondition,
    ) -> Result<bool> {
        if let ObjectCondition::All(conditions) = condition {
            for condition in conditions {
                if !self.object_condition_matches(bucket, key, condition)? {
                    return Ok(false);
                }
            }
            return Ok(true);
        }
        match (condition, self.read_object_metadata_locked(bucket, key)) {
            (
                ObjectCondition::Missing | ObjectCondition::MissingOrEtagNotIn(_),
                Err(Error::KeyNotFound),
            ) => Ok(true),
            (ObjectCondition::Missing, Ok(_)) | (_, Err(Error::KeyNotFound)) => Ok(false),
            (ObjectCondition::Etag(expected), Ok(object)) => Ok(object.etag == *expected),
            (ObjectCondition::EtagIn(expected), Ok(object)) => {
                Ok(expected.iter().any(|etag| etag == &object.etag))
            }
            (ObjectCondition::EtagNotIn(rejected), Ok(object)) => {
                Ok(!rejected.iter().any(|etag| etag == &object.etag))
            }
            (ObjectCondition::MissingOrEtagNotIn(rejected), Ok(object)) => {
                Ok(!rejected.iter().any(|etag| etag == &object.etag))
            }
            (ObjectCondition::Metadata { key, value }, Ok(object)) => {
                Ok(object.metadata.get(key) == Some(value))
            }
            (ObjectCondition::MetadataNot { key, value }, Ok(object)) => {
                Ok(object.metadata.get(key) != Some(value))
            }
            (ObjectCondition::All(_), _) => unreachable!("handled above"),
            (_, Err(error)) => Err(error),
        }
    }

    fn put_object_locked(&self, bucket: &str, key: &str, object: Object) -> Result<()> {
        self.put_object_locked_with_payload(bucket, key, object, ObjectPayload::InMemory)
    }

    /// Shared by [`Self::put_object_locked`] and the streaming write path:
    /// handles versioning bookkeeping identically regardless of whether the
    /// new payload bytes live in `object.data` or in a file already
    /// spooled to disk.
    fn put_object_locked_with_payload(
        &self,
        bucket: &str,
        key: &str,
        mut object: Object,
        payload: ObjectPayload<'_>,
    ) -> Result<()> {
        if !self.bucket_dir(bucket).exists() {
            return Err(Error::BucketNotFound);
        }
        let object_id = Self::compute_object_id(bucket, key);
        let versioning_enabled = self.versioning_enabled(bucket);
        let versioning_suspended = self.versioning_suspended(bucket);
        if versioning_enabled || versioning_suspended {
            match self.read_object_metadata_locked(bucket, key) {
                Ok(current_object) => {
                    let snapshot_version_id = current_object
                        .version_id
                        .clone()
                        .unwrap_or_else(|| "null".to_string());
                    if versioning_enabled || snapshot_version_id != "null" {
                        self.write_version_snapshot(
                            bucket,
                            &object_id,
                            &snapshot_version_id,
                            &current_object,
                        )?;
                    }
                }
                Err(Error::KeyNotFound) => {}
                Err(error) => return Err(error),
            }
            if versioning_suspended {
                object.version_id = Some("null".to_string());
            } else {
                object.version_id = Some(Uuid::new_v4().to_string());
            }
        } else {
            object.version_id = None;
        }
        let outcome = match payload {
            ObjectPayload::InMemory => self.write_object_files(bucket, &object_id, &object),
            ObjectPayload::Spooled(payload_path) => {
                self.write_object_files_from_path(bucket, &object_id, &object, payload_path)
            }
            ObjectPayload::Stored(path) => self.publish_pair(
                &self.object_id_dir(bucket, &object_id),
                &object,
                ObjectPayload::Stored(path),
            ),
        };
        // An ambiguous commit remains visible to listings, whose locked
        // metadata read recovers or fails closed just like GET and HEAD.
        let directory = self.object_id_dir(bucket, &object_id);
        if outcome.is_ok()
            || directory.join(".publication.json").exists()
            || directory.join("object.blob").exists()
        {
            self.index.insert(bucket, key);
        }
        outcome
    }

    fn delete_object_locked(&self, bucket: &str, key: &str) -> Result<()> {
        let object_id = Self::compute_object_id(bucket, key);
        let object_id_dir = self.object_id_dir(bucket, &object_id);
        let versioning_enabled = self.versioning_enabled(bucket);
        let versioning_suspended = self.versioning_suspended(bucket);
        if versioning_enabled || versioning_suspended {
            if !self.bucket_exists(bucket)? {
                return Err(Error::BucketNotFound);
            }
            match self.read_object_metadata_locked(bucket, key) {
                Ok(current_object) => {
                    let current_version_id = current_object
                        .version_id
                        .clone()
                        .unwrap_or_else(|| "null".to_string());
                    if versioning_enabled || current_version_id != "null" {
                        self.write_version_snapshot(
                            bucket,
                            &object_id,
                            &current_version_id,
                            &current_object,
                        )?;
                    }
                }
                Err(Error::KeyNotFound) => {}
                Err(error) => return Err(error),
            }
            let delete_marker_id = if versioning_suspended {
                "null".to_string()
            } else {
                Uuid::new_v4().to_string()
            };
            let mut delete_marker = Object::new(
                key.to_string(),
                Vec::new(),
                "application/x-sqrzl-delete-marker".to_string(),
            );
            delete_marker.version_id = Some(delete_marker_id.clone());
            delete_marker
                .provider_metadata
                .insert("s3_delete_marker".to_string(), "true".to_string());
            let remove_versions = if versioning_suspended {
                vec!["null".to_string()]
            } else {
                Vec::new()
            };
            self.change_history(
                &object_id_dir,
                None,
                remove_versions,
                Some(delete_marker),
                true,
            )?;
        } else {
            self.read_object_metadata_locked(bucket, key)?;
            self.change_history(&object_id_dir, None, Vec::new(), None, true)?;
            fs::remove_dir_all(&object_id_dir).map_err(|error| {
                Error::InternalError(format!(
                    "Failed to remove retired object directory: {error}"
                ))
            })?;
            Self::sync_directory(&self.bucket_dir(bucket))?;
        }
        self.index.remove(bucket, key);
        Ok(())
    }

    fn assign_null_version_ids_to_unversioned_objects(&self, bucket: &str) -> Result<()> {
        let entries = fs::read_dir(self.bucket_dir(bucket)).map_err(|error| {
            Error::InternalError(format!(
                "Failed to scan bucket while configuring versioning: {error}"
            ))
        })?;
        for entry in entries {
            let entry = entry.map_err(|error| {
                Error::InternalError(format!(
                    "Failed to inspect object while configuring versioning: {error}"
                ))
            })?;
            if !entry.path().is_dir()
                || entry
                    .file_name()
                    .to_str()
                    .is_some_and(|name| name.starts_with('.'))
            {
                continue;
            }
            let metadata_path = entry.path().join("object.meta.json");
            let Ok(object) = Self::read_object_metadata(&metadata_path) else {
                continue;
            };
            let object_lock = self.object_lock(bucket, &object.key)?;
            let _guard = object_lock.lock().map_err(|_| {
                Error::InternalError(
                    "Failed to lock object while configuring versioning".to_string(),
                )
            })?;
            Self::recover_publication(&entry.path())?;
            let Ok(mut current) = Self::read_object_metadata(&metadata_path) else {
                continue;
            };
            if current.version_id.is_none() {
                current.version_id = Some("null".to_string());
                Self::write_object_metadata(&metadata_path, &current)?;
            }
        }
        Ok(())
    }
}

impl ObjectStore for FilesystemStorage {
    fn put_object(&self, bucket: &str, key: String, object: Object) -> Result<()> {
        let object_lock = self.object_lock(bucket, &key)?;
        let _guard = object_lock
            .lock()
            .map_err(|_| Error::InternalError("Failed to lock object for write".to_string()))?;
        self.put_object_locked(bucket, &key, object)
    }

    fn put_object_streamed(
        &self,
        bucket: &str,
        key: String,
        object: Object,
        payload_path: &Path,
    ) -> Result<()> {
        let object_lock = self.object_lock(bucket, &key)?;
        let _guard = object_lock
            .lock()
            .map_err(|_| Error::InternalError("Failed to lock object for write".to_string()))?;
        self.put_object_locked_with_payload(
            bucket,
            &key,
            object,
            ObjectPayload::Spooled(payload_path),
        )
    }

    fn put_object_streamed_if(
        &self,
        bucket: &str,
        key: String,
        object: Object,
        payload_path: &Path,
        condition: &ObjectCondition,
    ) -> Result<bool> {
        let object_lock = self.object_lock(bucket, &key)?;
        let _guard = object_lock.lock().map_err(|_| {
            Error::InternalError("Failed to lock object for conditional streamed write".to_string())
        })?;
        if !self.object_condition_matches(bucket, &key, condition)? {
            return Ok(false);
        }
        self.put_object_locked_with_payload(
            bucket,
            &key,
            object,
            ObjectPayload::Spooled(payload_path),
        )?;
        Ok(true)
    }

    fn put_object_if(
        &self,
        bucket: &str,
        key: String,
        object: Object,
        condition: &ObjectCondition,
    ) -> Result<bool> {
        let object_lock = self.object_lock(bucket, &key)?;
        let _guard = object_lock.lock().map_err(|_| {
            Error::InternalError("Failed to lock object for conditional write".to_string())
        })?;
        if !self.object_condition_matches(bucket, &key, condition)? {
            return Ok(false);
        }
        self.put_object_locked(bucket, &key, object)?;
        Ok(true)
    }

    fn replace_object_metadata_if_unchanged(
        &self,
        bucket: &str,
        key: &str,
        observed: &Object,
        updated: &Object,
    ) -> Result<bool> {
        let object_lock = self.object_lock(bucket, key)?;
        let _guard = object_lock.lock().map_err(|_| {
            Error::InternalError("Failed to lock object for metadata update".to_string())
        })?;
        let object_id = Self::compute_object_id(bucket, key);
        Self::recover_publication(&self.object_id_dir(bucket, &object_id))?;
        let metadata_path = self.object_metadata_path(bucket, &object_id);
        if !metadata_path.exists() {
            return Ok(false);
        }

        let mut current = Self::read_object_metadata(&metadata_path)?;
        if current.etag != observed.etag
            || current.last_modified != observed.last_modified
            || current.version_id != observed.version_id
            || current.content_type != observed.content_type
            || current.metadata != observed.metadata
            || current.provider_metadata != observed.provider_metadata
        {
            return Ok(false);
        }
        current.content_type.clone_from(&updated.content_type);
        current.metadata.clone_from(&updated.metadata);
        current
            .provider_metadata
            .clone_from(&updated.provider_metadata);
        Self::write_object_metadata(&metadata_path, &current)?;
        Ok(true)
    }

    fn get_object(&self, bucket: &str, key: &str) -> Result<Object> {
        let object_lock = self.object_lock(bucket, key)?;
        let _guard = object_lock
            .lock()
            .map_err(|_| Error::InternalError("Failed to lock object for read".to_string()))?;
        self.read_object_locked(bucket, key)
    }

    fn get_object_metadata(&self, bucket: &str, key: &str) -> Result<Object> {
        let object_lock = self.object_lock(bucket, key)?;
        let _guard = object_lock.lock().map_err(|_| {
            Error::InternalError("Failed to lock object for metadata read".to_string())
        })?;
        self.read_object_metadata_locked(bucket, key)
    }

    fn get_object_range(
        &self,
        bucket: &str,
        key: &str,
        start: u64,
        end: Option<u64>,
    ) -> Result<(Object, Vec<u8>)> {
        let object_lock = self.object_lock(bucket, key)?;
        let _guard = object_lock.lock().map_err(|_| {
            Error::InternalError("Failed to lock object for range read".to_string())
        })?;
        self.read_object_range_locked(bucket, key, start, end)
    }

    fn delete_object(&self, bucket: &str, key: &str) -> Result<()> {
        let object_lock = self.object_lock(bucket, key)?;
        let _guard = object_lock
            .lock()
            .map_err(|_| Error::InternalError("Failed to lock object for delete".to_string()))?;
        self.delete_object_locked(bucket, key)
    }

    fn delete_object_if(
        &self,
        bucket: &str,
        key: &str,
        condition: &ObjectCondition,
    ) -> Result<bool> {
        let object_lock = self.object_lock(bucket, key)?;
        let _guard = object_lock.lock().map_err(|_| {
            Error::InternalError("Failed to lock object for conditional delete".to_string())
        })?;
        if !self.object_condition_matches(bucket, key, condition)? {
            return Ok(false);
        }
        self.delete_object_locked(bucket, key)?;
        Ok(true)
    }

    fn update_object_storage_class(
        &self,
        bucket: &str,
        key: &str,
        storage_class: &str,
    ) -> Result<()> {
        let object_lock = self.object_lock(bucket, key)?;
        let _guard = object_lock.lock().map_err(|_| {
            Error::InternalError("Failed to lock object for metadata update".to_string())
        })?;
        let object_id = Self::compute_object_id(bucket, key);
        Self::recover_publication(&self.object_id_dir(bucket, &object_id))?;
        let metadata_path = self.object_metadata_path(bucket, &object_id);

        if !metadata_path.exists() {
            return Err(Error::KeyNotFound);
        }

        let mut object = Self::read_object_metadata(&metadata_path)?;

        object.storage_class = storage_class.to_string();

        Self::write_object_metadata(&metadata_path, &object)
    }

    fn object_exists(&self, bucket: &str, key: &str) -> Result<bool> {
        let lock = self.object_lock(bucket, key)?;
        let _guard = lock.lock().map_err(|_| {
            Error::InternalError("Failed to lock object for existence read".to_string())
        })?;
        let object_id = Self::compute_object_id(bucket, key);
        Self::recover_publication(&self.object_id_dir(bucket, &object_id))?;
        Ok(self.object_data_path(bucket, &object_id).exists())
    }
}

impl FilesystemStorage {
    // Callers must hold the object's mutex. Mutations use these helpers rather
    // than the public readers so the non-reentrant mutex is acquired only once.
    fn read_object_locked(&self, bucket: &str, key: &str) -> Result<Object> {
        let object_id = Self::compute_object_id(bucket, key);
        Self::recover_publication(&self.object_id_dir(bucket, &object_id))?;
        let object_data_path = self.object_data_path(bucket, &object_id);

        if !object_data_path.exists() {
            return Err(Error::KeyNotFound);
        }

        let metadata_path = self.object_metadata_path(bucket, &object_id);
        let mut object = Self::read_object_metadata(&metadata_path)?;
        #[cfg(test)]
        self.test_phase(TestPhase::ReadPayloadMetadata);
        #[cfg(test)]
        self.test_phase(TestPhase::FullPayload);
        object.data = fs::read(&object_data_path)
            .map_err(|e| Error::InternalError(format!("Failed to read object: {e}")))?;
        Ok(object)
    }

    fn read_object_metadata_locked(&self, bucket: &str, key: &str) -> Result<Object> {
        let object_id = Self::compute_object_id(bucket, key);
        Self::recover_publication(&self.object_id_dir(bucket, &object_id))?;
        if !self.object_data_path(bucket, &object_id).exists() {
            return Err(Error::KeyNotFound);
        }
        let object = Self::read_object_metadata(&self.object_metadata_path(bucket, &object_id))?;
        #[cfg(test)]
        self.test_phase(TestPhase::ReadMetadata);
        Ok(object)
    }

    fn read_object_range_locked(
        &self,
        bucket: &str,
        key: &str,
        start: u64,
        end: Option<u64>,
    ) -> Result<(Object, Vec<u8>)> {
        let object_id = Self::compute_object_id(bucket, key);
        Self::recover_publication(&self.object_id_dir(bucket, &object_id))?;
        let object_data_path = self.object_data_path(bucket, &object_id);

        if !object_data_path.exists() {
            return Err(Error::KeyNotFound);
        }

        let metadata_path = self.object_metadata_path(bucket, &object_id);

        let object = Self::read_object_metadata(&metadata_path)?;
        #[cfg(test)]
        self.test_phase(TestPhase::ReadPayloadMetadata);

        // Validate range
        if start >= object.size {
            return Err(Error::InvalidRequest(
                "Range start beyond file size".to_string(),
            ));
        }

        let actual_end = end.map_or(object.size - 1, |e| e.min(object.size - 1));
        if actual_end < start {
            return Err(Error::InvalidRequest(
                "Invalid range: end < start".to_string(),
            ));
        }

        let length = usize::try_from(actual_end - start + 1)
            .map_err(|_| Error::InternalError("Requested range is too large".to_string()))?;

        // Read range from file
        let mut file = fs::File::open(&object_data_path)
            .map_err(|e| Error::InternalError(format!("Failed to open object file: {e}")))?;

        file.seek(SeekFrom::Start(start))
            .map_err(|e| Error::InternalError(format!("Failed to seek: {e}")))?;

        let mut buffer = vec![0u8; length];
        file.read_exact(&mut buffer)
            .map_err(|e| Error::InternalError(format!("Failed to read range: {e}")))?;

        Ok((object, buffer))
    }
}

impl AclStore for FilesystemStorage {
    fn get_bucket_acl(&self, bucket: &str) -> Result<Acl> {
        if !self.bucket_exists(bucket)? {
            return Err(Error::BucketNotFound);
        }

        let path = self.bucket_acl_path(bucket);
        if !path.exists() {
            return Ok(Acl::default());
        }

        let json = fs::read(&path)
            .map_err(|e| Error::InternalError(format!("Failed to read bucket ACL: {e}")))?;
        serde_json::from_slice(&json)
            .map_err(|e| Error::InternalError(format!("Failed to parse bucket ACL: {e}")))
    }

    fn put_bucket_acl(&self, bucket: &str, acl: Acl) -> Result<()> {
        if !self.bucket_exists(bucket)? {
            return Err(Error::BucketNotFound);
        }

        let path = self.bucket_acl_path(bucket);
        let json = serde_json::to_vec(&acl)
            .map_err(|e| Error::InternalError(format!("Failed to serialize bucket ACL: {e}")))?;
        Self::atomic_write(&path, &json)?;
        self.touch_bucket_identity(bucket)
    }

    fn get_object_acl(&self, bucket: &str, key: &str) -> Result<Acl> {
        Ok(self
            .get_object_metadata(bucket, key)?
            .acl
            .unwrap_or_default())
    }

    fn put_object_acl(&self, bucket: &str, key: &str, acl: Acl) -> Result<()> {
        let object_lock = self.object_lock(bucket, key)?;
        let _guard = object_lock.lock().map_err(|_| {
            Error::InternalError("Failed to lock object for ACL update".to_string())
        })?;
        let object_id = Self::compute_object_id(bucket, key);
        Self::recover_publication(&self.object_id_dir(bucket, &object_id))?;
        let metadata_path = self.object_metadata_path(bucket, &object_id);

        if !metadata_path.exists() {
            return Err(Error::KeyNotFound);
        }

        let mut object = Self::read_object_metadata(&metadata_path)?;

        object.acl = Some(acl);

        Self::write_object_metadata(&metadata_path, &object)
    }
}

impl LifecycleStore for FilesystemStorage {
    fn get_bucket_lifecycle(
        &self,
        bucket: &str,
    ) -> Result<crate::models::lifecycle::LifecycleConfiguration> {
        let bucket_path = self.bucket_dir(bucket);
        if !bucket_path.exists() {
            return Err(Error::BucketNotFound);
        }

        let lifecycle_path = bucket_path.join(".lifecycle.json");
        if !lifecycle_path.exists() {
            return Err(Error::KeyNotFound);
        }

        let json = fs::read(&lifecycle_path)
            .map_err(|e| Error::InternalError(format!("Failed to read lifecycle config: {e}")))?;
        serde_json::from_slice(&json)
            .map_err(|e| Error::InternalError(format!("Failed to parse lifecycle config: {e}")))
    }

    fn put_bucket_lifecycle(
        &self,
        bucket: &str,
        config: crate::models::lifecycle::LifecycleConfiguration,
    ) -> Result<()> {
        let bucket_path = self.bucket_dir(bucket);
        if !bucket_path.exists() {
            return Err(Error::BucketNotFound);
        }

        let lifecycle_path = bucket_path.join(".lifecycle.json");
        let json = serde_json::to_vec(&config).map_err(|e| {
            Error::InternalError(format!("Failed to serialize lifecycle config: {e}"))
        })?;
        Self::atomic_write(&lifecycle_path, &json)?;
        self.touch_bucket_identity(bucket)
    }

    fn delete_bucket_lifecycle(&self, bucket: &str) -> Result<()> {
        let bucket_path = self.bucket_dir(bucket);
        if !bucket_path.exists() {
            return Err(Error::BucketNotFound);
        }

        let lifecycle_path = bucket_path.join(".lifecycle.json");
        if lifecycle_path.exists() {
            fs::remove_file(&lifecycle_path).map_err(|e| {
                Error::InternalError(format!("Failed to delete lifecycle config: {e}"))
            })?;
            self.touch_bucket_identity(bucket)?;
        }
        Ok(())
    }
}

impl PolicyStore for FilesystemStorage {
    fn get_bucket_policy(
        &self,
        bucket: &str,
    ) -> Result<crate::models::policy::BucketPolicyDocument> {
        let bucket_path = self.bucket_dir(bucket);
        if !bucket_path.exists() {
            return Err(Error::BucketNotFound);
        }

        let policy_path = bucket_path.join(".policy.json");
        if !policy_path.exists() {
            return Err(Error::KeyNotFound);
        }

        let policy_json = fs::read(&policy_path)
            .map_err(|e| Error::InternalError(format!("Failed to read policy: {e}")))?;

        serde_json::from_slice(&policy_json)
            .map_err(|e| Error::InternalError(format!("Failed to parse policy: {e}")))
    }

    fn put_bucket_policy(
        &self,
        bucket: &str,
        policy: crate::models::policy::BucketPolicyDocument,
    ) -> Result<()> {
        let bucket_path = self.bucket_dir(bucket);
        if !bucket_path.exists() {
            return Err(Error::BucketNotFound);
        }

        let policy_path = bucket_path.join(".policy.json");
        let policy_json = serde_json::to_vec(&policy)
            .map_err(|e| Error::InternalError(format!("Failed to serialize policy: {e}")))?;

        Self::atomic_write(&policy_path, &policy_json)?;
        self.touch_bucket_identity(bucket)
    }

    fn delete_bucket_policy(&self, bucket: &str) -> Result<()> {
        let bucket_path = self.bucket_dir(bucket);
        if !bucket_path.exists() {
            return Err(Error::BucketNotFound);
        }

        let policy_path = bucket_path.join(".policy.json");
        if policy_path.exists() {
            fs::remove_file(&policy_path)
                .map_err(|e| Error::InternalError(format!("Failed to delete policy: {e}")))?;
            self.touch_bucket_identity(bucket)?;
        }
        Ok(())
    }
}

impl ProviderStateStore for FilesystemStorage {
    fn put_provider_state(&self, provider: &str, key: &str, data: Vec<u8>) -> Result<()> {
        let path = self.provider_state_path(provider, key);
        Self::atomic_write(&path, &data)
    }

    fn get_provider_state(&self, provider: &str, key: &str) -> Result<Vec<u8>> {
        let path = self.provider_state_path(provider, key);
        if !path.exists() {
            return Err(Error::KeyNotFound);
        }

        fs::read(&path)
            .map_err(|e| Error::InternalError(format!("Failed to read provider state: {e}")))
    }

    fn delete_provider_state(&self, provider: &str, key: &str) -> Result<()> {
        let path = self.provider_state_path(provider, key);
        if path.exists() {
            fs::remove_file(path).map_err(|e| {
                Error::InternalError(format!("Failed to delete provider state: {e}"))
            })?;
        }
        Ok(())
    }
}

impl UploadStore for FilesystemStorage {
    fn stage_upload_payload(
        &self,
        provider: &str,
        session: &str,
        item: &str,
        payload_path: &Path,
        source_offset: u64,
        len: u64,
    ) -> Result<()> {
        let destination = self.provider_upload_item_path(provider, session, item);
        let source_len = fs::metadata(payload_path)
            .map_err(|error| {
                Error::InternalError(format!("Failed to inspect upload payload: {error}"))
            })?
            .len();
        let end = source_offset
            .checked_add(len)
            .ok_or(Error::EntityTooLarge)?;
        if end > source_len {
            return Err(Error::InvalidRequest(
                "Staged upload range exceeds the request payload".to_string(),
            ));
        }
        if source_offset == 0 && len == source_len {
            return Self::atomic_move(payload_path, &destination);
        }

        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent).map_err(|error| {
                Error::InternalError(format!(
                    "Failed to create upload session directory: {error}"
                ))
            })?;
        }
        let temporary = destination.with_extension(format!("tmp-{}", Uuid::new_v4()));
        let copy_result = (|| -> Result<()> {
            let mut source = fs::File::open(payload_path).map_err(|error| {
                Error::InternalError(format!("Failed to open upload payload: {error}"))
            })?;
            source
                .seek(SeekFrom::Start(source_offset))
                .map_err(|error| {
                    Error::InternalError(format!("Failed to seek upload payload: {error}"))
                })?;
            let mut destination_file = fs::File::create(&temporary).map_err(|error| {
                Error::InternalError(format!("Failed to create staged upload payload: {error}"))
            })?;
            let copied =
                std::io::copy(&mut source.take(len), &mut destination_file).map_err(|error| {
                    Error::InternalError(format!("Failed to stage upload payload: {error}"))
                })?;
            if copied != len {
                return Err(Error::InvalidRequest(
                    "Upload payload ended before the selected range".to_string(),
                ));
            }
            destination_file.sync_all().map_err(|error| {
                Error::InternalError(format!("Failed to sync staged upload payload: {error}"))
            })?;
            fs::rename(&temporary, &destination).map_err(|error| {
                Error::InternalError(format!("Failed to commit staged upload payload: {error}"))
            })?;
            Ok(())
        })();
        if copy_result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        copy_result
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
        let spool_dir = self.base_path.join(".spool");
        fs::create_dir_all(&spool_dir).map_err(|error| {
            Error::InternalError(format!("Failed to create composition directory: {error}"))
        })?;
        let scratch = spool_dir.join(format!(".compose-{}.tmp", Uuid::new_v4()));
        let compose_result = (|| -> Result<()> {
            let mut destination = fs::File::create(&scratch).map_err(|error| {
                Error::InternalError(format!("Failed to create composed payload: {error}"))
            })?;
            for item in items {
                let path = self.provider_upload_item_path(provider, session, item);
                let mut source = fs::File::open(path).map_err(|error| {
                    Error::InternalError(format!("Failed to open staged upload item: {error}"))
                })?;
                std::io::copy(&mut source, &mut destination).map_err(|error| {
                    Error::InternalError(format!("Failed to compose upload item: {error}"))
                })?;
            }
            destination.sync_all().map_err(|error| {
                Error::InternalError(format!("Failed to sync composed upload: {error}"))
            })?;
            Ok(())
        })();
        if let Err(error) = compose_result {
            let _ = fs::remove_file(&scratch);
            return Err(error);
        }

        let written = if let Some(condition) = condition {
            self.put_object_streamed_if(bucket, key, object, &scratch, condition)?
        } else {
            self.put_object_streamed(bucket, key, object, &scratch)?;
            true
        };
        if scratch.exists() {
            let _ = fs::remove_file(&scratch);
        }
        Ok(written)
    }

    fn delete_upload_session(&self, provider: &str, session: &str) -> Result<()> {
        let path = self.provider_upload_session_dir(provider, session);
        if path.exists() {
            fs::remove_dir_all(path).map_err(|error| {
                Error::InternalError(format!("Failed to remove upload session: {error}"))
            })?;
        }
        Ok(())
    }

    fn retain_upload_items(&self, provider: &str, session: &str, items: &[String]) -> Result<()> {
        let session_path = self.provider_upload_session_dir(provider, session);
        if !session_path.exists() {
            return Ok(());
        }
        let retained = items
            .iter()
            .map(|item| self.provider_upload_item_path(provider, session, item))
            .collect::<std::collections::HashSet<_>>();
        for entry in fs::read_dir(&session_path).map_err(|error| {
            Error::InternalError(format!("Failed to inspect upload session: {error}"))
        })? {
            let entry = entry.map_err(|error| {
                Error::InternalError(format!("Failed to inspect upload item: {error}"))
            })?;
            if entry.file_type().is_ok_and(|kind| kind.is_file())
                && !retained.contains(&entry.path())
            {
                fs::remove_file(entry.path()).map_err(|error| {
                    Error::InternalError(format!(
                        "Failed to remove unreferenced upload item: {error}"
                    ))
                })?;
            }
        }
        if session_path
            .read_dir()
            .is_ok_and(|mut entries| entries.next().is_none())
        {
            fs::remove_dir(&session_path).map_err(|error| {
                Error::InternalError(format!("Failed to remove empty upload session: {error}"))
            })?;
        }
        Ok(())
    }
}

impl TagStore for FilesystemStorage {
    fn get_object_tags(&self, bucket: &str, key: &str) -> Result<HashMap<String, String>> {
        Ok(self.get_object_metadata(bucket, key)?.tags)
    }

    fn put_object_tags(
        &self,
        bucket: &str,
        key: &str,
        tags: HashMap<String, String>,
    ) -> Result<()> {
        let object_lock = self.object_lock(bucket, key)?;
        let _guard = object_lock.lock().map_err(|_| {
            Error::InternalError("Failed to lock object for tag update".to_string())
        })?;
        let object_id = Self::compute_object_id(bucket, key);
        Self::recover_publication(&self.object_id_dir(bucket, &object_id))?;
        let metadata_path = self.object_metadata_path(bucket, &object_id);

        if !metadata_path.exists() {
            return Err(Error::KeyNotFound);
        }

        let mut object = Self::read_object_metadata(&metadata_path)?;

        object.tags = tags;

        Self::write_object_metadata(&metadata_path, &object)?;

        Ok(())
    }

    fn delete_object_tags(&self, bucket: &str, key: &str) -> Result<()> {
        let object_lock = self.object_lock(bucket, key)?;
        let _guard = object_lock.lock().map_err(|_| {
            Error::InternalError("Failed to lock object for tag update".to_string())
        })?;
        let object_id = Self::compute_object_id(bucket, key);
        Self::recover_publication(&self.object_id_dir(bucket, &object_id))?;
        let metadata_path = self.object_metadata_path(bucket, &object_id);

        if !metadata_path.exists() {
            return Err(Error::KeyNotFound);
        }

        let mut object = Self::read_object_metadata(&metadata_path)?;

        // Clear all tags
        object.tags.clear();

        Self::write_object_metadata(&metadata_path, &object)?;

        Ok(())
    }
}

impl FilesystemStorage {
    fn scan_delimited_entries(
        &self,
        bucket: &str,
        prefix: &str,
        delimiter: &str,
        marker: Option<&str>,
        desired_entry_count: usize,
    ) -> Vec<DirectoryEntry> {
        let scan_batch_size = desired_entry_count.clamp(1, 1_024);
        let mut scan_marker = marker.map(str::to_string);
        let mut entries = Vec::with_capacity(desired_entry_count.min(1_024));

        loop {
            let keys = self.index.list_prefix_marker(
                bucket,
                Some(prefix),
                scan_marker.as_deref(),
                Some(scan_batch_size),
            );
            if keys.is_empty() {
                break;
            }
            let reached_end = keys.len() < scan_batch_size;

            for key in keys {
                scan_marker = Some(key.clone());
                let remainder = &key[prefix.len()..];
                let entry = remainder.find(delimiter).map_or_else(
                    || DirectoryEntry {
                        path: key.clone(),
                        kind: DirectoryEntryKind::Object,
                    },
                    |index| DirectoryEntry {
                        path: key[..prefix.len() + index + delimiter.len()].to_string(),
                        kind: DirectoryEntryKind::CommonPrefix,
                    },
                );
                if marker.is_some_and(|value| entry.path.as_str() <= value)
                    || entries
                        .last()
                        .is_some_and(|previous: &DirectoryEntry| previous.path == entry.path)
                {
                    continue;
                }
                if entry.kind == DirectoryEntryKind::Object {
                    let object_id = Self::compute_object_id(bucket, &entry.path);
                    if !self.object_metadata_path(bucket, &object_id).exists() {
                        continue;
                    }
                }
                entries.push(entry);
                if entries.len() >= desired_entry_count {
                    break;
                }
            }

            if entries.len() >= desired_entry_count || reached_end {
                break;
            }
        }
        entries
    }

    fn list_objects_with_delimiter(
        &self,
        bucket: &str,
        prefix: &str,
        delimiter: &str,
        marker: Option<&str>,
        max_keys: usize,
    ) -> Result<crate::models::ListObjectsResult> {
        let entry_limit = max_keys.saturating_add(1);
        let entries = if delimiter == "/" && (prefix.is_empty() || prefix.ends_with('/')) {
            self.index
                .list_child_entries(bucket, prefix, marker, Some(entry_limit))
        } else {
            self.scan_delimited_entries(bucket, prefix, delimiter, marker, entry_limit)
        };
        let is_truncated = entries.len() > max_keys;
        let page_entries = entries.iter().take(max_keys).collect::<Vec<_>>();
        let next_marker = if is_truncated {
            if max_keys == 0 {
                entries.first().map(|entry| entry.path.clone())
            } else {
                page_entries.last().map(|entry| entry.path.clone())
            }
        } else {
            None
        };

        let mut common_prefixes = Vec::new();
        let mut objects = Vec::with_capacity(page_entries.len());
        for entry in page_entries {
            match entry.kind {
                DirectoryEntryKind::CommonPrefix => common_prefixes.push(entry.path.clone()),
                DirectoryEntryKind::Object => match self.get_object_metadata(bucket, &entry.path) {
                    Ok(object) => objects.push(object),
                    Err(Error::KeyNotFound) => {}
                    Err(error) => return Err(error),
                },
            }
        }
        Ok(crate::models::ListObjectsResult {
            common_prefixes,
            objects,
            is_truncated,
            next_marker,
        })
    }
}

impl ObjectListingStore for FilesystemStorage {
    fn list_objects(
        &self,
        bucket: &str,
        prefix: Option<&str>,
        delimiter: Option<&str>,
        marker: Option<&str>,
        max_keys: Option<usize>,
    ) -> Result<crate::models::ListObjectsResult> {
        let bucket_dir = self.bucket_dir(bucket);
        if !bucket_dir.exists() {
            return Err(Error::BucketNotFound);
        }

        let max_keys = max_keys.unwrap_or(1000);

        if let Some(delimiter) = delimiter.filter(|value| !value.is_empty()) {
            return self.list_objects_with_delimiter(
                bucket,
                prefix.unwrap_or(""),
                delimiter,
                marker,
                max_keys,
            );
        }

        let keys =
            self.index
                .list_prefix_marker(bucket, prefix, marker, Some(max_keys.saturating_add(1)));

        let page_keys = keys.iter().take(max_keys).collect::<Vec<_>>();
        let mut objects = Vec::with_capacity(page_keys.len());
        for obj_key in &page_keys {
            match self.get_object_metadata(bucket, obj_key) {
                Ok(object) => objects.push(object),
                Err(Error::KeyNotFound) => {}
                Err(error) => return Err(error),
            }
        }

        let is_truncated = keys.len() > max_keys;
        let next_marker = if is_truncated {
            if max_keys == 0 {
                keys.first().cloned()
            } else {
                page_keys.last().map(|key| (*key).clone())
            }
        } else {
            None
        };

        Ok(crate::models::ListObjectsResult {
            common_prefixes: Vec::new(),
            objects,
            is_truncated,
            next_marker,
        })
    }
}

impl MultipartStore for FilesystemStorage {
    fn create_multipart_upload(&self, bucket: &str, key: String) -> Result<MultipartUpload> {
        self.create_multipart_upload_with_metadata(
            bucket,
            key,
            None,
            HashMap::new(),
            HashMap::from([
                (
                    MULTIPART_MIN_NON_FINAL_PART_SIZE_KEY.to_string(),
                    S3_MINIMUM_NON_FINAL_PART_SIZE.to_string(),
                ),
                (
                    MULTIPART_MAX_PART_SIZE_KEY.to_string(),
                    S3_MAXIMUM_PART_SIZE.to_string(),
                ),
                (
                    MULTIPART_MAX_OBJECT_SIZE_KEY.to_string(),
                    S3_MAXIMUM_OBJECT_SIZE.to_string(),
                ),
            ]),
        )
    }

    fn create_multipart_upload_with_metadata(
        &self,
        bucket: &str,
        key: String,
        content_type: Option<String>,
        metadata: HashMap<String, String>,
        provider_metadata: HashMap<String, String>,
    ) -> Result<MultipartUpload> {
        if !self.bucket_exists(bucket)? {
            return Err(Error::BucketNotFound);
        }

        let upload = MultipartUpload::new(key, content_type, metadata, provider_metadata);
        let upload_dir = self.multipart_dir(bucket, &upload.upload_id);
        fs::create_dir_all(&upload_dir)
            .map_err(|e| Error::InternalError(format!("Failed to create multipart dir: {e}")))?;
        self.ensure_uploads_cache_loaded(bucket)?;
        self.write_upload_record(bucket, &upload)?;
        let mut cache = self
            .uploads_cache
            .lock()
            .map_err(|_| Error::InternalError("Failed to lock uploads cache".to_string()))?;
        let uploads = cache
            .get_mut(bucket)
            .ok_or_else(|| Error::InternalError("Missing uploads cache entry".to_string()))?;
        uploads.insert(upload.upload_id.clone(), upload.clone());

        Ok(upload)
    }

    fn upload_part(
        &self,
        bucket: &str,
        upload_id: &str,
        part_number: u32,
        data: Vec<u8>,
    ) -> Result<String> {
        if !self.bucket_exists(bucket)? {
            return Err(Error::BucketNotFound);
        }
        if !(1..=10000).contains(&part_number) {
            return Err(Error::InvalidPartNumber);
        }
        let upload = self.get_multipart_upload(bucket, upload_id)?;
        if upload
            .provider_metadata
            .get(MULTIPART_MAX_PART_SIZE_KEY)
            .and_then(|value| value.parse::<u64>().ok())
            .is_some_and(|maximum| data.len() as u64 > maximum)
        {
            return Err(Error::EntityTooLarge);
        }

        let etag = md5_hash(&data);
        let size = data.len() as u64;
        let part_path = self.part_path(bucket, upload_id, part_number);
        Self::atomic_write(&part_path, &data)?;

        self.record_uploaded_part(bucket, upload_id, part_number, etag, size)
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
        if !self.bucket_exists(bucket)? {
            return Err(Error::BucketNotFound);
        }
        if !(1..=10000).contains(&part_number) {
            return Err(Error::InvalidPartNumber);
        }
        let upload = self.get_multipart_upload(bucket, upload_id)?;
        if upload
            .provider_metadata
            .get(MULTIPART_MAX_PART_SIZE_KEY)
            .and_then(|value| value.parse::<u64>().ok())
            .is_some_and(|maximum| len > maximum)
        {
            return Err(Error::EntityTooLarge);
        }

        let part_path = self.part_path(bucket, upload_id, part_number);
        Self::atomic_move(payload_path, &part_path)?;

        self.record_uploaded_part(bucket, upload_id, part_number, etag, len)
    }

    fn list_multipart_uploads(&self, bucket: &str) -> Result<Vec<MultipartUpload>> {
        if !self.bucket_exists(bucket)? {
            return Err(Error::BucketNotFound);
        }

        let uploads = self.load_uploads(bucket)?;
        Ok(uploads.into_values().collect())
    }

    fn list_parts(&self, bucket: &str, upload_id: &str) -> Result<Vec<crate::models::Part>> {
        if !self.bucket_exists(bucket)? {
            return Err(Error::BucketNotFound);
        }

        let uploads = self.load_uploads(bucket)?;
        let upload = uploads.get(upload_id).ok_or(Error::NoSuchUpload)?;

        Ok(upload.parts.clone())
    }

    fn get_multipart_upload(&self, bucket: &str, upload_id: &str) -> Result<MultipartUpload> {
        if !self.bucket_exists(bucket)? {
            return Err(Error::BucketNotFound);
        }

        let uploads = self.load_uploads(bucket)?;
        uploads.get(upload_id).cloned().ok_or(Error::NoSuchUpload)
    }

    fn complete_multipart_upload(&self, bucket: &str, upload_id: &str) -> Result<String> {
        let upload = self.get_multipart_upload(bucket, upload_id)?;
        let parts = upload
            .parts
            .iter()
            .map(|part| (part.part_number, part.etag.clone()))
            .collect::<Vec<_>>();
        self.complete_multipart_upload_with_parts(bucket, upload_id, &parts)
    }

    #[allow(clippy::too_many_lines)]
    fn complete_multipart_upload_with_parts(
        &self,
        bucket: &str,
        upload_id: &str,
        manifest: &[(u32, String)],
    ) -> Result<String> {
        if !self.bucket_exists(bucket)? {
            return Err(Error::BucketNotFound);
        }

        self.ensure_uploads_cache_loaded(bucket)?;
        let upload = {
            let cache = self
                .uploads_cache
                .lock()
                .map_err(|_| Error::InternalError("Failed to lock uploads cache".to_string()))?;
            let uploads = cache
                .get(bucket)
                .ok_or_else(|| Error::InternalError("Missing uploads cache entry".to_string()))?;
            uploads.get(upload_id).cloned().ok_or(Error::NoSuchUpload)?
        };
        let crate::models::MultipartUpload {
            key,
            content_type,
            metadata,
            provider_metadata,
            parts: uploaded_parts,
            ..
        } = upload;

        if manifest.is_empty() {
            return Err(Error::InvalidPartOrder);
        }
        if manifest.windows(2).any(|pair| pair[0].0 >= pair[1].0) {
            return Err(Error::InvalidPartOrder);
        }
        let parts = manifest
            .iter()
            .map(|(part_number, etag)| {
                uploaded_parts
                    .iter()
                    .find(|part| part.part_number == *part_number && part.etag == *etag)
                    .cloned()
                    .ok_or(Error::IncompleteMultipartUpload)
            })
            .collect::<Result<Vec<_>>>()?;
        let minimum_non_final_part_size = provider_metadata
            .get(MULTIPART_MIN_NON_FINAL_PART_SIZE_KEY)
            .and_then(|value| value.parse::<u64>().ok())
            .unwrap_or(0);
        if parts
            .iter()
            .take(parts.len().saturating_sub(1))
            .any(|part| part.size < minimum_non_final_part_size)
        {
            return Err(Error::EntityTooSmall);
        }

        let total_size = parts.iter().try_fold(0u64, |acc, part| {
            acc.checked_add(part.size).ok_or(Error::EntityTooLarge)
        })?;
        if provider_metadata
            .get(MULTIPART_MAX_OBJECT_SIZE_KEY)
            .and_then(|value| value.parse::<u64>().ok())
            .is_some_and(|maximum| total_size > maximum)
        {
            return Err(Error::EntityTooLarge);
        }

        // Compute final ETag: MD5(concat(part_etags)) + "-" + part_count
        let mut etag_hash = md5::Context::new();
        for part in &parts {
            let raw_digest = hex::decode(part.etag.trim_matches('"')).map_err(|error| {
                Error::InternalError(format!("Invalid multipart part ETag: {error}"))
            })?;
            etag_hash.consume(raw_digest);
        }
        let final_etag = format!("{:x}-{}", etag_hash.finalize(), parts.len());

        // Assemble the completed object by streaming each part file
        // straight into a scratch file, never holding more than one part's
        // buffered copy in memory at a time — a multi-gigabyte object built
        // from many parts must not be concatenated into a single in-memory
        // buffer first.
        let spool_path = self.spool_scratch_path(bucket);
        if let Some(parent) = spool_path.parent() {
            fs::create_dir_all(parent)
                .map_err(|e| Error::InternalError(format!("Failed to create spool dir: {e}")))?;
        }
        let assemble_result = (|| -> Result<()> {
            let mut dest = fs::File::create(&spool_path)
                .map_err(|e| Error::InternalError(format!("Failed to create spool file: {e}")))?;
            for part in &parts {
                let part_path = self.part_path(bucket, upload_id, part.part_number);
                let mut src = fs::File::open(&part_path)
                    .map_err(|e| Error::InternalError(format!("Failed to open part: {e}")))?;
                std::io::copy(&mut src, &mut dest)
                    .map_err(|e| Error::InternalError(format!("Failed to append part: {e}")))?;
            }
            dest.sync_all()
                .map_err(|e| Error::InternalError(format!("Failed to sync spool file: {e}")))?;
            Ok(())
        })();
        if assemble_result.is_err() {
            let _ = fs::remove_file(&spool_path);
            if let Some(parent) = spool_path.parent() {
                let _ = fs::remove_dir(parent);
            }
        }
        assemble_result?;

        // Save completed object
        let mut obj = Object::new_with_metadata_and_etag(
            key.clone(),
            Vec::new(),
            content_type.unwrap_or_else(|| "application/octet-stream".to_string()),
            metadata,
            final_etag.clone(),
        );
        obj.size = total_size;
        for (name, value) in &provider_metadata {
            if !matches!(
                name.as_str(),
                MULTIPART_MIN_NON_FINAL_PART_SIZE_KEY
                    | MULTIPART_MAX_PART_SIZE_KEY
                    | MULTIPART_MAX_OBJECT_SIZE_KEY
                    | MULTIPART_TAGS_KEY
                    | "storage_class"
            ) {
                obj.provider_metadata.insert(name.clone(), value.clone());
            }
        }
        if let Some(storage_class) = provider_metadata.get("storage_class") {
            obj.storage_class.clone_from(storage_class);
        }
        if let Some(tags) = provider_metadata
            .get(MULTIPART_TAGS_KEY)
            .and_then(|value| serde_json::from_str::<HashMap<String, String>>(value).ok())
        {
            obj.tags = tags;
        }
        let put_result = self.put_object_streamed(bucket, key, obj, &spool_path);
        if spool_path.exists() {
            let _ = fs::remove_file(&spool_path);
        }
        if let Some(parent) = spool_path.parent() {
            let _ = fs::remove_dir(parent);
        }
        put_result?;
        #[cfg(test)]
        self.test_phase(TestPhase::UploadCleanup);
        {
            let mut cache = self
                .uploads_cache
                .lock()
                .map_err(|_| Error::InternalError("Failed to lock uploads cache".to_string()))?;
            let uploads = cache
                .get_mut(bucket)
                .ok_or_else(|| Error::InternalError("Missing uploads cache entry".to_string()))?;
            uploads.remove(upload_id);
        }
        self.remove_upload_record(bucket, upload_id)?;
        #[cfg(test)]
        self.test_phase(TestPhase::UploadCleanupDone);

        Ok(final_etag)
    }

    fn abort_multipart_upload(&self, bucket: &str, upload_id: &str) -> Result<()> {
        if !self.bucket_exists(bucket)? {
            return Err(Error::BucketNotFound);
        }

        self.ensure_uploads_cache_loaded(bucket)?;
        {
            let mut cache = self
                .uploads_cache
                .lock()
                .map_err(|_| Error::InternalError("Failed to lock uploads cache".to_string()))?;
            let uploads = cache
                .get_mut(bucket)
                .ok_or_else(|| Error::InternalError("Missing uploads cache entry".to_string()))?;
            uploads.remove(upload_id).ok_or(Error::NoSuchUpload)?;
        }
        self.remove_upload_record(bucket, upload_id)?;

        Ok(())
    }
}

impl FilesystemStorage {
    fn read_object_version_locked(
        &self,
        bucket: &str,
        key: &str,
        version_id: &str,
    ) -> Result<Object> {
        if !self.bucket_exists(bucket)? {
            return Err(Error::BucketNotFound);
        }
        Self::validate_version_id(version_id)?;

        let object_id = Self::compute_object_id(bucket, key);
        Self::recover_publication(&self.object_id_dir(bucket, &object_id))?;
        Self::recover_publication(&self.version_dir(bucket, &object_id, version_id))?;
        let version_data_path = self.version_data_path(bucket, &object_id, version_id);
        if !version_data_path.exists() {
            let current_object = self
                .read_object_locked(bucket, key)
                .map_err(|err| match err {
                    Error::KeyNotFound => Error::NoSuchVersion,
                    other => other,
                })?;

            if current_object.version_id.as_deref() == Some(version_id) {
                return Ok(current_object);
            }

            return Err(Error::NoSuchVersion);
        }

        let metadata_path = self.version_metadata_path(bucket, &object_id, version_id);
        let mut object = Self::read_object_metadata(&metadata_path)?;
        #[cfg(test)]
        self.test_phase(TestPhase::FullPayload);
        object.data = fs::read(&version_data_path)
            .map_err(|e| Error::InternalError(format!("Failed to read version: {e}")))?;

        object.version_id = Some(version_id.to_string());

        Ok(object)
    }
}

impl FilesystemStorage {
    fn list_object_versions_for_key_locked(&self, bucket: &str, key: &str) -> Result<Vec<Object>> {
        if !self.bucket_exists(bucket)? {
            return Err(Error::BucketNotFound);
        }

        let object_id = Self::compute_object_id(bucket, key);
        let object_id_dir = self.object_id_dir(bucket, &object_id);
        Self::recover_publication(&object_id_dir)?;
        if !object_id_dir.exists() {
            return Ok(Vec::new());
        }

        let mut versions = Vec::new();
        let metadata_path = self.object_metadata_path(bucket, &object_id);
        if let Ok(obj) = Self::read_object_metadata(&metadata_path) {
            if obj.key == key && obj.version_id.is_some() {
                versions.push(obj);
            }
        }

        let versions_dir = self.versions_dir(bucket, &object_id);
        if let Ok(version_entries) = fs::read_dir(&versions_dir) {
            for version_entry in version_entries.flatten() {
                let version_path = version_entry.path();
                if !version_path.is_dir() {
                    continue;
                }

                Self::recover_publication(&version_path)?;

                let metadata_path = version_path.join("object.meta.json");
                if let Ok(obj) = Self::read_object_metadata(&metadata_path) {
                    if obj.key == key {
                        versions.push(obj);
                    }
                }
            }
        }

        versions.sort_unstable_by(|a, b| a.version_id.cmp(&b.version_id));
        // A crash after committing the old snapshot but before replacing the
        // current object can leave the same identity in both locations.
        versions.dedup_by(|left, right| left.version_id == right.version_id);
        Ok(versions)
    }
}

impl VersionStore for FilesystemStorage {
    fn enable_versioning(&self, bucket: &str) -> Result<()> {
        if !self.bucket_exists(bucket)? {
            return Err(Error::BucketNotFound);
        }

        // Mark bucket as versioning-enabled by creating a marker file
        let versioning_marker = self.versioning_marker(bucket);
        let changed = !fs::read(&versioning_marker).is_ok_and(|value| value == b"enabled");
        Self::atomic_write(&versioning_marker, b"enabled")?;
        self.assign_null_version_ids_to_unversioned_objects(bucket)?;
        if changed {
            self.touch_bucket_identity(bucket)?;
        }
        Ok(())
    }

    fn suspend_versioning(&self, bucket: &str) -> Result<()> {
        if !self.bucket_exists(bucket)? {
            return Err(Error::BucketNotFound);
        }

        // Preserve the distinction between a never-versioned bucket and one whose
        // versioning is suspended. Suspended buckets retain non-null history and
        // replace a single null version on subsequent writes and deletes.
        let versioning_marker = self.versioning_marker(bucket);
        let changed = !fs::read(&versioning_marker).is_ok_and(|value| value == b"suspended");
        Self::atomic_write(&versioning_marker, b"suspended")?;
        self.assign_null_version_ids_to_unversioned_objects(bucket)?;
        if changed {
            self.touch_bucket_identity(bucket)?;
        }
        Ok(())
    }

    fn get_object_version(
        &self,
        bucket: &str,
        key: &str,
        version_id: &str,
    ) -> Result<crate::models::Object> {
        let object_lock = self.object_lock(bucket, key)?;
        let _guard = object_lock.lock().map_err(|_| {
            Error::InternalError("Failed to lock object for version read".to_string())
        })?;
        self.read_object_version_locked(bucket, key, version_id)
    }

    fn list_object_versions(
        &self,
        bucket: &str,
        prefix: Option<&str>,
    ) -> Result<Vec<crate::models::Object>> {
        if !self.bucket_exists(bucket)? {
            return Err(Error::BucketNotFound);
        }

        let mut versions = Vec::new();
        let prefix = prefix.unwrap_or("");
        let bucket_dir = self.bucket_dir(bucket);

        // Scan all object directories in bucket
        if let Ok(entries) = fs::read_dir(&bucket_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    // Skip special directories
                    if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                        if name.starts_with('.') {
                            continue;
                        }
                    }

                    let object_id =
                        path.file_name()
                            .and_then(|name| name.to_str())
                            .ok_or_else(|| {
                                Error::InternalError(
                                    "Invalid object directory identity".to_string(),
                                )
                            })?;
                    let lock = self.object_id_lock(bucket, object_id)?;
                    let _guard = lock.lock().map_err(|_| {
                        Error::InternalError(
                            "Failed to lock object for version listing".to_string(),
                        )
                    })?;
                    Self::recover_publication(&path)?;
                    let metadata_path = path.join("object.meta.json");
                    if let Ok(obj) = Self::read_object_metadata(&metadata_path) {
                        if obj.key.starts_with(prefix) && obj.version_id.is_some() {
                            versions.push(obj);
                        }
                    }

                    // Check for versions subdirectory
                    let versions_dir = path.join("versions");
                    if versions_dir.exists() {
                        // Scan version directories
                        if let Ok(version_entries) = fs::read_dir(&versions_dir) {
                            for version_entry in version_entries.flatten() {
                                let version_path = version_entry.path();
                                if version_path.is_dir() {
                                    if let Some(_version_id) =
                                        version_path.file_name().and_then(|n| n.to_str())
                                    {
                                        Self::recover_publication(&version_path)?;
                                        // Read version metadata to get the key and check prefix
                                        let metadata_path = version_path.join("object.meta.json");
                                        if let Ok(obj) = Self::read_object_metadata(&metadata_path)
                                        {
                                            if obj.key.starts_with(prefix) {
                                                versions.push(obj);
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }

        versions.sort_unstable_by(|a, b| {
            if a.key == b.key {
                a.version_id.cmp(&b.version_id)
            } else {
                a.key.cmp(&b.key)
            }
        });

        versions
            .dedup_by(|left, right| left.key == right.key && left.version_id == right.version_id);
        Ok(versions)
    }

    fn list_object_versions_for_key(
        &self,
        bucket: &str,
        key: &str,
    ) -> Result<Vec<crate::models::Object>> {
        let object_lock = self.object_lock(bucket, key)?;
        let _guard = object_lock.lock().map_err(|_| {
            Error::InternalError("Failed to lock object for version listing".to_string())
        })?;
        self.list_object_versions_for_key_locked(bucket, key)
    }
    fn delete_object_version(&self, bucket: &str, key: &str, version_id: &str) -> Result<()> {
        let lock = self.object_lock(bucket, key)?;
        let _guard = lock.lock().map_err(|_| {
            Error::InternalError("Failed to lock object for version delete".to_string())
        })?;
        if !self.bucket_exists(bucket)? {
            return Err(Error::BucketNotFound);
        }
        Self::validate_version_id(version_id)?;
        let object_id = Self::compute_object_id(bucket, key);
        let directory = self.object_id_dir(bucket, &object_id);
        Self::recover_publication(&directory)?;
        let current = match self.read_object_metadata_locked(bucket, key) {
            Ok(current) => Some(current),
            Err(Error::KeyNotFound) => None,
            Err(error) => return Err(error),
        };
        let historical_exists = self
            .version_data_path(bucket, &object_id, version_id)
            .exists();
        let deleting_current = current
            .as_ref()
            .is_some_and(|object| object.version_id.as_deref() == Some(version_id));
        if !historical_exists && !deleting_current {
            return Err(Error::NoSuchVersion);
        }
        let mut remove_versions = vec![version_id.to_string()];
        let latest = if deleting_current || current.is_none() {
            self.list_object_versions_for_key_locked(bucket, key)?
                .into_iter()
                .filter(|object| object.version_id.as_deref() != Some(version_id))
                .max_by(|left, right| {
                    left.last_modified
                        .cmp(&right.last_modified)
                        .then_with(|| left.version_id.cmp(&right.version_id))
                })
        } else {
            None
        };
        let promoted = if let Some(latest) = latest.filter(|object| {
            object
                .provider_metadata
                .get("s3_delete_marker")
                .is_none_or(|value| value != "true")
        }) {
            let id = latest.version_id.as_deref().ok_or_else(|| {
                Error::InternalError("Historical object is missing version identity".to_string())
            })?;
            let path = self.version_data_path(bucket, &object_id, id);
            remove_versions.push(id.to_string());
            Some((latest, path))
        } else {
            None
        };
        let clear_current = promoted.is_none() && (deleting_current || current.is_none());
        self.change_history(
            &directory,
            promoted
                .as_ref()
                .map(|(object, path)| (object, path.as_path())),
            remove_versions,
            None,
            clear_current,
        )?;
        if self.object_data_path(bucket, &object_id).exists() {
            self.index.insert(bucket, key);
        } else {
            self.index.remove(bucket, key);
            if !self.version_entries_exist(bucket, &object_id)? {
                fs::remove_dir_all(&directory)
                    .map_err(|error| Error::InternalError(error.to_string()))?;
                Self::sync_directory(&self.bucket_dir(bucket))?;
            }
        }
        Ok(())
    }
}

fn md5_hash(data: &[u8]) -> String {
    use md5;
    format!("{:x}", md5::compute(data))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use uuid::Uuid;

    fn temp_path() -> PathBuf {
        std::env::temp_dir().join(format!("sqrzl_fs_test_{}", Uuid::new_v4()))
    }

    #[test]
    fn should_roundtrip_metadata_on_put_then_get() {
        // Arrange
        let base = temp_path();
        let storage = FilesystemStorage::new(&base);

        let bucket = "meta-bucket";
        storage.create_bucket(bucket.to_string()).unwrap();

        let mut metadata = HashMap::new();
        metadata.insert("owner".to_string(), "alice".to_string());
        metadata.insert("purpose".to_string(), "test".to_string());

        let data = b"hello metadata".to_vec();
        let key = "note.txt".to_string();
        let obj = Object::new_with_metadata(
            key.clone(),
            data.clone(),
            "text/plain".to_string(),
            metadata.clone(),
        );

        // Act
        storage.put_object(bucket, key.clone(), obj).unwrap();

        let fetched = storage.get_object(bucket, &key).unwrap();

        // Assert
        assert_eq!(fetched.data, data, "Object data should round-trip");
        assert_eq!(
            fetched.metadata.len(),
            metadata.len(),
            "Metadata count should match"
        );
        assert_eq!(fetched.metadata.get("owner"), Some(&"alice".to_string()));
        assert_eq!(fetched.metadata.get("purpose"), Some(&"test".to_string()));

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn should_reject_atomic_condition_set_when_any_metadata_predicate_fails() {
        // Arrange
        let base = temp_path();
        let storage = FilesystemStorage::new(&base);
        let bucket = "condition-all-bucket";
        let key = "object.txt";
        storage.create_bucket(bucket.to_string()).unwrap();
        let current = Object::new_with_metadata(
            key.to_string(),
            b"current".to_vec(),
            "text/plain".to_string(),
            HashMap::from([
                ("generation".to_string(), "7".to_string()),
                ("metageneration".to_string(), "2".to_string()),
            ]),
        );
        storage
            .put_object(bucket, key.to_string(), current)
            .unwrap();
        let replacement = Object::new(
            key.to_string(),
            b"replacement".to_vec(),
            "text/plain".to_string(),
        );
        let condition = ObjectCondition::All(vec![
            ObjectCondition::Metadata {
                key: "generation".to_string(),
                value: "7".to_string(),
            },
            ObjectCondition::Metadata {
                key: "metageneration".to_string(),
                value: "999".to_string(),
            },
        ]);

        // Act
        let written = storage
            .put_object_if(bucket, key.to_string(), replacement, &condition)
            .unwrap();

        // Assert
        assert!(!written);
        assert_eq!(storage.get_object(bucket, key).unwrap().data, b"current");

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn should_rebuild_index_with_metadata_present() {
        // Arrange
        let base = temp_path();
        let bucket = "meta-rebuild";
        let key = "file.bin";

        {
            let storage = FilesystemStorage::new(&base);
            storage.create_bucket(bucket.to_string()).unwrap();

            let mut metadata = HashMap::new();
            metadata.insert("role".to_string(), "cache".to_string());

            let data = b"persisted".to_vec();
            let obj = Object::new_with_metadata(
                key.to_string(),
                data,
                "application/octet-stream".to_string(),
                metadata,
            );

            storage.put_object(bucket, key.to_string(), obj).unwrap();
        }

        // Act
        // Recreate storage to force index rebuild from disk
        let storage = FilesystemStorage::new(&base);

        // Assert
        assert!(
            storage.object_exists(bucket, key).unwrap(),
            "Index should include existing object"
        );

        let fetched = storage.get_object(bucket, key).unwrap();
        assert_eq!(fetched.metadata.get("role"), Some(&"cache".to_string()));

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn should_rebuild_directory_index_with_metadata_present() {
        // Arrange
        let base = temp_path();
        let bucket = "dir-rebuild";

        {
            let storage = FilesystemStorage::new(&base);
            storage.create_bucket(bucket.to_string()).unwrap();
            for key in ["docs/api/openapi.json", "docs/readme.txt", "image.png"] {
                storage
                    .put_object(
                        bucket,
                        key.to_string(),
                        Object::new(key.to_string(), b"payload".to_vec(), "text/plain".into()),
                    )
                    .unwrap();
            }
        }

        // Act
        let storage = FilesystemStorage::new(&base);
        let root = storage
            .list_objects(bucket, Some(""), Some("/"), None, Some(10))
            .unwrap();
        let docs = storage
            .list_objects(bucket, Some("docs/"), Some("/"), None, Some(10))
            .unwrap();

        // Assert
        assert_eq!(root.common_prefixes, vec!["docs/".to_string()]);
        assert_eq!(root.objects.len(), 1);
        assert_eq!(root.objects[0].key, "image.png");
        assert_eq!(docs.common_prefixes, vec!["docs/api/".to_string()]);
        assert_eq!(docs.objects.len(), 1);
        assert_eq!(docs.objects[0].key, "docs/readme.txt");

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn should_preserve_common_prefixes_across_generic_delimiter_shapes() {
        // Arrange
        let base = temp_path();
        let storage = FilesystemStorage::new(&base);
        let bucket = "generic-delimiter";
        storage.create_bucket(bucket.to_string()).unwrap();
        for key in ["docs/a.txt", "docs-b.txt"] {
            storage
                .put_object(
                    bucket,
                    key.to_string(),
                    Object::new(key.to_string(), b"payload".to_vec(), "text/plain".into()),
                )
                .unwrap();
        }

        // Act
        let slash = storage
            .list_objects(bucket, Some("doc"), Some("/"), None, Some(10))
            .unwrap();
        let dash = storage
            .list_objects(bucket, Some("docs"), Some("-"), None, Some(10))
            .unwrap();

        // Assert
        assert_eq!(slash.common_prefixes, vec!["docs/".to_string()]);
        assert_eq!(dash.common_prefixes, vec!["docs-".to_string()]);

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn should_resume_generic_delimiter_listing_after_common_prefix_marker() {
        // Arrange
        let base = temp_path();
        let storage = FilesystemStorage::new(&base);
        let bucket = "generic-delimiter-page";
        storage.create_bucket(bucket.to_string()).unwrap();
        for key in ["docs/a.txt", "docs/b.txt", "document.txt"] {
            storage
                .put_object(
                    bucket,
                    key.to_string(),
                    Object::new(key.to_string(), b"payload".to_vec(), "text/plain".into()),
                )
                .unwrap();
        }

        // Act
        let first = storage
            .list_objects(bucket, Some("doc"), Some("/"), None, Some(1))
            .unwrap();
        let second = storage
            .list_objects(
                bucket,
                Some("doc"),
                Some("/"),
                first.next_marker.as_deref(),
                Some(1),
            )
            .unwrap();

        // Assert
        assert_eq!(first.common_prefixes, vec!["docs/".to_string()]);
        assert!(first.objects.is_empty());
        assert!(first.is_truncated);
        assert_eq!(first.next_marker.as_deref(), Some("docs/"));
        assert_eq!(second.common_prefixes.len(), 0);
        assert_eq!(second.objects.len(), 1);
        assert_eq!(second.objects[0].key, "document.txt");
        assert!(!second.is_truncated);
        assert!(second.next_marker.is_none());

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn should_page_flat_object_listing_without_skipping_marker_boundary() {
        // Arrange
        let base = temp_path();
        let storage = FilesystemStorage::new(&base);
        let bucket = "flat-page";
        storage.create_bucket(bucket.to_string()).unwrap();

        for key in ["a.txt", "b.txt", "c.txt"] {
            storage
                .put_object(
                    bucket,
                    key.to_string(),
                    Object::new(key.to_string(), b"payload".to_vec(), "text/plain".into()),
                )
                .unwrap();
        }

        // Act
        let first = storage
            .list_objects(bucket, None, None, None, Some(1))
            .unwrap();
        let second = storage
            .list_objects(bucket, None, None, first.next_marker.as_deref(), Some(1))
            .unwrap();
        let third = storage
            .list_objects(bucket, None, None, second.next_marker.as_deref(), Some(1))
            .unwrap();

        // Assert
        assert_eq!(first.objects.len(), 1);
        assert_eq!(first.objects[0].key, "a.txt");
        assert_eq!(first.next_marker.as_deref(), Some("a.txt"));
        assert_eq!(second.objects.len(), 1);
        assert_eq!(second.objects[0].key, "b.txt");
        assert_eq!(second.next_marker.as_deref(), Some("b.txt"));
        assert_eq!(third.objects.len(), 1);
        assert_eq!(third.objects[0].key, "c.txt");
        assert!(third.next_marker.is_none());

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn should_prune_directory_prefix_after_last_child_delete() {
        // Arrange
        let base = temp_path();
        let storage = FilesystemStorage::new(&base);
        let bucket = "dir-prune";
        storage.create_bucket(bucket.to_string()).unwrap();

        for key in ["docs/api/openapi.json", "docs/readme.txt"] {
            storage
                .put_object(
                    bucket,
                    key.to_string(),
                    Object::new(key.to_string(), b"payload".to_vec(), "text/plain".into()),
                )
                .unwrap();
        }

        // Act
        storage
            .delete_object(bucket, "docs/api/openapi.json")
            .unwrap();
        let docs_after_nested_delete = storage
            .list_objects(bucket, Some("docs/"), Some("/"), None, Some(10))
            .unwrap();
        storage.delete_object(bucket, "docs/readme.txt").unwrap();
        let root_after_all_deletes = storage
            .list_objects(bucket, Some(""), Some("/"), None, Some(10))
            .unwrap();

        // Assert
        assert_eq!(docs_after_nested_delete.common_prefixes.len(), 0);
        assert_eq!(docs_after_nested_delete.objects.len(), 1);
        assert_eq!(docs_after_nested_delete.objects[0].key, "docs/readme.txt");
        assert_eq!(root_after_all_deletes.common_prefixes.len(), 0);
        assert!(root_after_all_deletes.objects.is_empty());

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn should_assemble_multipart_object_from_disk_without_leftover_spool_files() {
        // Arrange: several parts, each large enough that concatenating them
        // in memory (the old behavior) would be a meaningfully sized
        // allocation — completion must instead stream part files straight
        // to the final blob.
        let base = temp_path();
        let storage = FilesystemStorage::new(&base);
        let bucket = "multipart-stream-assemble-bucket";
        storage.create_bucket(bucket.to_string()).unwrap();
        let upload = storage
            .create_multipart_upload(bucket, "combined.bin".to_string())
            .unwrap();

        let part_payloads: Vec<Vec<u8>> = vec![
            vec![1u8; 6 * 1024 * 1024],
            vec![2u8; 6 * 1024 * 1024],
            vec![3u8; 2 * 1024 * 1024],
        ];
        for (index, payload) in part_payloads.iter().enumerate() {
            storage
                .upload_part(
                    bucket,
                    &upload.upload_id,
                    u32::try_from(index).expect("part index should fit in u32") + 1,
                    payload.clone(),
                )
                .unwrap();
        }

        // Act
        let etag = storage
            .complete_multipart_upload(bucket, &upload.upload_id)
            .unwrap();

        // Assert: assembled content is the exact concatenation of the parts.
        let stored = storage.get_object(bucket, "combined.bin").unwrap();
        let expected: Vec<u8> = part_payloads.into_iter().flatten().collect();
        assert_eq!(stored.data, expected);
        assert_eq!(stored.size, expected.len() as u64);
        assert!(etag.ends_with("-3"));

        // No scratch directory should be left behind after completion, and it
        // must not make an otherwise empty bucket undeletable.
        let spool_dir = base.join(bucket).join(".spool");
        assert!(!spool_dir.exists());
        storage.delete_object(bucket, "combined.bin").unwrap();
        storage.delete_bucket(bucket).unwrap();

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn should_compute_s3_multipart_etag_from_raw_part_md5_digests() {
        // Arrange
        let base = temp_path();
        let storage = FilesystemStorage::new(&base);
        let bucket = "multipart-etag-bucket";
        storage.create_bucket(bucket.to_string()).unwrap();
        let upload = storage
            .create_multipart_upload(bucket, "combined.bin".to_string())
            .unwrap();
        storage
            .upload_part(bucket, &upload.upload_id, 1, b"hello".to_vec())
            .unwrap();

        // Act
        let etag = storage
            .complete_multipart_upload(bucket, &upload.upload_id)
            .unwrap();

        // Assert
        assert_eq!(etag, "62109206880d38a4010a98e11243924a-1");
        assert_eq!(
            storage.get_object(bucket, "combined.bin").unwrap().etag,
            etag
        );

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn should_complete_only_parts_selected_by_s3_manifest() {
        // Arrange
        let base = temp_path();
        let storage = FilesystemStorage::new(&base);
        let bucket = "multipart-selected-parts-bucket";
        storage.create_bucket(bucket.to_string()).unwrap();
        let upload = storage
            .create_multipart_upload(bucket, "selected.bin".to_string())
            .unwrap();
        let first = vec![b'a'; 5 * 1024 * 1024];
        let omitted = b"not-selected".to_vec();
        let final_part = b"selected-final".to_vec();
        let first_etag = storage
            .upload_part(bucket, &upload.upload_id, 1, first.clone())
            .unwrap();
        storage
            .upload_part(bucket, &upload.upload_id, 2, omitted)
            .unwrap();
        let final_etag = storage
            .upload_part(bucket, &upload.upload_id, 3, final_part.clone())
            .unwrap();
        let manifest = vec![(1, first_etag), (3, final_etag)];

        // Act
        storage
            .complete_multipart_upload_with_parts(bucket, &upload.upload_id, &manifest)
            .unwrap();

        // Assert
        let stored = storage.get_object(bucket, "selected.bin").unwrap();
        let expected = [first, final_part].concat();
        assert_eq!(stored.data, expected);
        assert!(matches!(
            storage.get_multipart_upload(bucket, &upload.upload_id),
            Err(Error::NoSuchUpload)
        ));
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn should_reject_streamed_s3_part_larger_than_five_gibibytes_before_moving_file() {
        // Arrange
        let base = temp_path();
        let storage = FilesystemStorage::new(&base);
        let bucket = "multipart-max-part-bucket";
        storage.create_bucket(bucket.to_string()).unwrap();
        let upload = storage
            .create_multipart_upload(bucket, "large.bin".to_string())
            .unwrap();
        let payload_path = base.join("declared-oversized-part.tmp");
        fs::write(&payload_path, b"small fixture").unwrap();

        // Act
        let result = storage.upload_part_streamed(
            bucket,
            &upload.upload_id,
            1,
            &payload_path,
            S3_MAXIMUM_PART_SIZE + 1,
            "etag".to_string(),
        );

        // Assert
        assert!(matches!(result, Err(Error::EntityTooLarge)));
        assert!(payload_path.exists());
        assert!(storage
            .list_parts(bucket, &upload.upload_id)
            .unwrap()
            .is_empty());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn should_complete_single_noninitial_s3_multipart_part() {
        // Arrange
        let base = temp_path();
        let storage = FilesystemStorage::new(&base);
        let bucket = "multipart-noninitial-part-bucket";
        storage.create_bucket(bucket.to_string()).unwrap();
        let upload = storage
            .create_multipart_upload(bucket, "combined.bin".to_string())
            .unwrap();
        storage
            .upload_part(bucket, &upload.upload_id, 7, b"payload".to_vec())
            .unwrap();

        // Act
        let etag = storage
            .complete_multipart_upload(bucket, &upload.upload_id)
            .unwrap();

        // Assert
        assert!(etag.ends_with("-1"));
        assert_eq!(
            storage.get_object(bucket, "combined.bin").unwrap().data,
            b"payload"
        );

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn should_reject_small_non_final_multipart_part_without_consuming_upload() {
        // Arrange
        let base = temp_path();
        let storage = FilesystemStorage::new(&base);
        let bucket = "multipart-minimum-bucket";
        storage.create_bucket(bucket.to_string()).unwrap();
        let upload = storage
            .create_multipart_upload(bucket, "combined.bin".to_string())
            .unwrap();
        storage
            .upload_part(bucket, &upload.upload_id, 1, b"too-small".to_vec())
            .unwrap();
        storage
            .upload_part(bucket, &upload.upload_id, 2, b"final".to_vec())
            .unwrap();

        // Act
        let result = storage.complete_multipart_upload(bucket, &upload.upload_id);

        // Assert
        assert!(matches!(result, Err(Error::EntityTooSmall)));
        assert!(storage
            .get_multipart_upload(bucket, &upload.upload_id)
            .is_ok());
        assert!(matches!(
            storage.get_object(bucket, "combined.bin"),
            Err(Error::KeyNotFound)
        ));

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn should_store_tags_then_return_them() {
        // Arrange
        let base = temp_path();
        let storage = FilesystemStorage::new(&base);

        let bucket = "tag-bucket";
        let key = "tag.txt";
        storage.create_bucket(bucket.to_string()).unwrap();

        let data = b"tag-data".to_vec();
        let mut obj = Object::new_with_metadata(
            key.to_string(),
            data.clone(),
            "text/plain".to_string(),
            HashMap::new(),
        );
        obj.tags.insert("env".to_string(), "test".to_string());

        // Act
        storage.put_object(bucket, key.to_string(), obj).unwrap();

        let tags = storage.get_object_tags(bucket, key).unwrap();

        // Assert
        assert_eq!(tags.get("env"), Some(&"test".to_string()));

        let mut new_tags = HashMap::new();
        new_tags.insert("owner".to_string(), "alice".to_string());
        storage
            .put_object_tags(bucket, key, new_tags.clone())
            .unwrap();

        let updated = storage.get_object_tags(bucket, key).unwrap();
        assert_eq!(updated, new_tags);

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn should_store_lifecycle_configuration_then_retrieve_it() {
        use crate::models::lifecycle::*;

        // Arrange
        let base = temp_path();
        let storage = FilesystemStorage::new(&base);
        let bucket = "lifecycle-bucket";
        storage.create_bucket(bucket.to_string()).unwrap();

        let mut config = LifecycleConfiguration::default();
        config.rules.push(Rule {
            id: Some("delete-old-logs".to_string()),
            status: Status::Enabled,
            filter: Some(Filter {
                prefix: Some("logs/".to_string()),
                tags: vec![],
            }),
            expiration: Some(Expiration {
                days: Some(30),
                date: None,
                expired_object_delete_marker: None,
            }),
            noncurrent_version_expiration: None,
            transitions: vec![],
        });

        // Act
        storage
            .put_bucket_lifecycle(bucket, config.clone())
            .unwrap();
        let retrieved = storage.get_bucket_lifecycle(bucket).unwrap();

        // Assert
        assert_eq!(retrieved.rules.len(), 1);
        assert_eq!(retrieved.rules[0].id, Some("delete-old-logs".to_string()));

        storage.delete_bucket_lifecycle(bucket).unwrap();
        assert!(storage.get_bucket_lifecycle(bucket).is_err());

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn should_preserve_bucket_creation_identity_across_restarts() {
        // Arrange
        let base = temp_path();
        let storage = FilesystemStorage::new(&base);
        storage
            .create_bucket("stable-identity".to_string())
            .unwrap();
        // Act
        let first = storage.get_bucket("stable-identity").unwrap().created_at;
        // Assert
        assert_eq!(
            storage.get_bucket("stable-identity").unwrap().created_at,
            first
        );
        assert_eq!(storage.list_buckets().unwrap()[0].created_at, first);
        drop(storage);
        let storage = FilesystemStorage::new(&base);
        assert_eq!(
            storage.get_bucket("stable-identity").unwrap().created_at,
            first
        );
        assert_eq!(storage.list_buckets().unwrap()[0].created_at, first);
        drop(storage);
        fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn should_migrate_legacy_bucket_identity_once_from_filesystem_creation() {
        // Arrange
        let base = temp_path();
        let storage = FilesystemStorage::new(&base);
        let bucket_dir = storage.bucket_dir("legacy-identity");
        fs::create_dir_all(&bucket_dir).unwrap();
        fs::write(bucket_dir.join(".bucket.name"), b"legacy-identity").unwrap();
        let metadata = fs::metadata(bucket_dir.join(".bucket.name")).unwrap();
        let expected: chrono::DateTime<chrono::Utc> = metadata
            .created()
            .or_else(|_| metadata.modified())
            .unwrap()
            .into();
        // Act
        let first = storage.get_bucket("legacy-identity").unwrap().created_at;
        // Assert
        assert_eq!(first, expected);
        assert!(bucket_dir.join(".bucket.identity.json").exists());
        fs::write(bucket_dir.join(".bucket.name"), b"legacy-identity").unwrap();
        drop(storage);
        let storage = FilesystemStorage::new(&base);
        assert_eq!(
            storage.get_bucket("legacy-identity").unwrap().created_at,
            first
        );
        drop(storage);
        fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn should_persist_bucket_metadata_sidecar() {
        // Arrange
        let base = temp_path();
        let storage = FilesystemStorage::new(&base);
        let bucket = "bucket-meta";
        storage.create_bucket(bucket.to_string()).unwrap();

        let metadata = HashMap::from([
            ("s3_requester_pays".to_string(), "true".to_string()),
            ("s3_website_index".to_string(), "index.html".to_string()),
        ]);

        storage
            .update_bucket_metadata(bucket, metadata.clone())
            .unwrap();

        // Act
        let fetched = storage.get_bucket(bucket).unwrap();
        assert_eq!(fetched.metadata, metadata);

        let reopened = FilesystemStorage::new(&base);
        let reopened_bucket = reopened.get_bucket(bucket).unwrap();
        assert_eq!(reopened_bucket.metadata, metadata);

        // Assert
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn should_update_bucket_modification_identity_only_for_bucket_changes() {
        // Arrange
        let base = temp_path();
        let storage = FilesystemStorage::new(&base);
        storage.create_bucket("bucket-changes".to_string()).unwrap();
        // Act
        let initial = storage.get_bucket("bucket-changes").unwrap();
        // Assert
        assert_eq!(initial.created_at, initial.modified_at);
        storage
            .put_object(
                "bucket-changes",
                "data".to_string(),
                Object::new(
                    "data".to_string(),
                    b"data".to_vec(),
                    "text/plain".to_string(),
                ),
            )
            .unwrap();
        let after_object = storage.get_bucket("bucket-changes").unwrap();
        assert_eq!(after_object.modified_at, initial.modified_at);
        let metadata = HashMap::from([("owner".to_string(), "sdk".to_string())]);
        let updated = storage
            .update_bucket_metadata("bucket-changes", metadata.clone())
            .unwrap();
        assert_eq!(updated.created_at, initial.created_at);
        assert!(updated.modified_at > initial.modified_at);
        assert_eq!(
            storage
                .update_bucket_metadata("bucket-changes", metadata)
                .unwrap()
                .modified_at,
            updated.modified_at
        );
        assert_eq!(
            storage.list_buckets().unwrap()[0].modified_at,
            updated.modified_at
        );
        drop(storage);
        let storage = FilesystemStorage::new(&base);
        let reopened = storage.get_bucket("bucket-changes").unwrap();
        assert_eq!(reopened.created_at, initial.created_at);
        assert_eq!(reopened.modified_at, updated.modified_at);
        let before_acl = reopened.modified_at;
        storage
            .put_bucket_acl("bucket-changes", Acl::default())
            .unwrap();
        let after_acl = storage.get_bucket("bucket-changes").unwrap();
        assert_eq!(after_acl.created_at, initial.created_at);
        assert!(after_acl.modified_at > before_acl);
        drop(storage);
        fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn should_create_versions_on_overwrite_when_versioning_enabled() {
        // Arrange
        let base = temp_path();
        let storage = FilesystemStorage::new(&base);

        let bucket = "version-bucket";
        let key = "doc.txt";
        storage.create_bucket(bucket.to_string()).unwrap();
        storage.enable_versioning(bucket).unwrap();

        storage
            .put_object(
                bucket,
                key.to_string(),
                Object::new(key.to_string(), b"v1".to_vec(), "text/plain".to_string()),
            )
            .unwrap();

        // Act
        let first = storage.get_object(bucket, key).unwrap();
        let first_version_id = first.version_id.clone().expect("version id should exist");
        assert_eq!(first.data, b"v1".to_vec());
        assert_eq!(
            storage
                .get_object_version(bucket, key, &first_version_id)
                .unwrap()
                .data,
            b"v1".to_vec()
        );

        storage
            .put_object(
                bucket,
                key.to_string(),
                Object::new(key.to_string(), b"v2".to_vec(), "text/plain".to_string()),
            )
            .unwrap();

        let current = storage.get_object(bucket, key).unwrap();
        let current_version_id = current.version_id.clone().expect("version id should exist");
        assert_ne!(first_version_id, current_version_id);
        assert_eq!(current.data, b"v2".to_vec());
        assert_eq!(
            storage
                .get_object_version(bucket, key, &current_version_id)
                .unwrap()
                .data,
            b"v2".to_vec()
        );

        let versions = storage.list_object_versions(bucket, Some(key)).unwrap();
        let version_ids: Vec<_> = versions
            .into_iter()
            .filter_map(|obj| obj.version_id)
            .collect();

        // Assert
        assert_eq!(version_ids.len(), 2);
        assert!(version_ids.contains(&first_version_id));
        assert!(version_ids.contains(&current_version_id));

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn should_reject_unsafe_version_ids_without_aliasing_current_or_historical_data() {
        // Arrange
        let base = temp_path();
        let storage = FilesystemStorage::new(&base);
        let bucket = "safe-version-path-bucket";
        let key = "doc.txt";
        storage.create_bucket(bucket.to_string()).unwrap();
        storage.enable_versioning(bucket).unwrap();
        storage
            .put_object(
                bucket,
                key.to_string(),
                Object::new(key.to_string(), b"v1".to_vec(), "text/plain".to_string()),
            )
            .unwrap();
        let first_version_id = storage
            .get_object(bucket, key)
            .unwrap()
            .version_id
            .expect("first version id should exist");
        storage
            .put_object(
                bucket,
                key.to_string(),
                Object::new(key.to_string(), b"v2".to_vec(), "text/plain".to_string()),
            )
            .unwrap();
        let versions_before = storage.list_object_versions_for_key(bucket, key).unwrap();

        // Act
        for invalid_version_id in ["", ".", "..", "../other", "other/version", "other\\version"] {
            assert!(matches!(
                storage.get_object_version(bucket, key, invalid_version_id),
                Err(Error::NoSuchVersion)
            ));
            assert!(matches!(
                storage.delete_object_version(bucket, key, invalid_version_id),
                Err(Error::NoSuchVersion)
            ));
        }

        // Assert
        assert_eq!(storage.get_object(bucket, key).unwrap().data, b"v2");
        assert_eq!(
            storage
                .get_object_version(bucket, key, &first_version_id)
                .unwrap()
                .data,
            b"v1"
        );
        assert_eq!(
            storage
                .list_object_versions_for_key(bucket, key)
                .unwrap()
                .len(),
            versions_before.len()
        );

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn should_replace_provider_metadata_without_changing_version_identity_or_history() {
        // Arrange
        let base = temp_path();
        let storage = FilesystemStorage::new(&base);
        let bucket = "provider-metadata-cas-bucket";
        let key = "doc.txt";
        storage.create_bucket(bucket.to_string()).unwrap();
        storage.enable_versioning(bucket).unwrap();
        storage
            .put_object(
                bucket,
                key.to_string(),
                Object::new(key.to_string(), b"v1".to_vec(), "text/plain".to_string()),
            )
            .unwrap();
        storage
            .put_object(
                bucket,
                key.to_string(),
                Object::new(key.to_string(), b"v2".to_vec(), "text/plain".to_string()),
            )
            .unwrap();
        let observed = storage.get_object(bucket, key).unwrap();
        let versions_before = storage.list_object_versions_for_key(bucket, key).unwrap();
        let replacement = HashMap::from([("lease-state".to_string(), "leased".to_string())]);
        let replacement_user_metadata = HashMap::from([("owner".to_string(), "sdk".to_string())]);
        let mut updated = observed.clone();
        updated.content_type = "application/json".to_string();
        updated.metadata.clone_from(&replacement_user_metadata);
        updated.provider_metadata.clone_from(&replacement);

        // Act
        let replaced = storage
            .replace_object_metadata_if_unchanged(bucket, key, &observed, &updated)
            .unwrap();
        let mut stale_updated = observed.clone();
        stale_updated.provider_metadata =
            HashMap::from([("lease-state".to_string(), "released".to_string())]);
        let stale_replacement = storage
            .replace_object_metadata_if_unchanged(bucket, key, &observed, &stale_updated)
            .unwrap();

        // Assert
        assert!(replaced);
        assert!(!stale_replacement);
        let stored = storage.get_object(bucket, key).unwrap();
        assert_eq!(stored.data, observed.data);
        assert_eq!(stored.etag, observed.etag);
        assert_eq!(stored.last_modified, observed.last_modified);
        assert_eq!(stored.version_id, observed.version_id);
        assert_eq!(stored.content_type, "application/json");
        assert_eq!(stored.metadata, replacement_user_metadata);
        assert_eq!(stored.provider_metadata, replacement);
        assert_eq!(
            storage
                .list_object_versions_for_key(bucket, key)
                .unwrap()
                .len(),
            versions_before.len()
        );

        let _ = std::fs::remove_dir_all(&base);
    }

    fn suspended_version_fixture() -> (
        std::path::PathBuf,
        FilesystemStorage,
        &'static str,
        &'static str,
        String,
    ) {
        let base = temp_path();
        let storage = FilesystemStorage::new(&base);
        let bucket = "suspended-version-bucket";
        let key = "doc.txt";
        storage.create_bucket(bucket.to_string()).unwrap();
        storage
            .put_object(
                bucket,
                key.to_string(),
                Object::new(
                    key.to_string(),
                    b"pre-versioning".to_vec(),
                    "text/plain".to_string(),
                ),
            )
            .unwrap();
        assert!(storage
            .get_object(bucket, key)
            .unwrap()
            .version_id
            .is_none());
        storage.enable_versioning(bucket).unwrap();
        assert_eq!(
            storage
                .get_object(bucket, key)
                .unwrap()
                .version_id
                .as_deref(),
            Some("null")
        );
        storage
            .put_object(
                bucket,
                key.to_string(),
                Object::new(
                    key.to_string(),
                    b"versioned".to_vec(),
                    "text/plain".to_string(),
                ),
            )
            .unwrap();
        let versioned_id = storage
            .get_object(bucket, key)
            .unwrap()
            .version_id
            .expect("enabled write should have a version id");
        assert_ne!(versioned_id, "null");
        storage.suspend_versioning(bucket).unwrap();
        let reopened = FilesystemStorage::new(&base);
        (base, reopened, bucket, key, versioned_id)
    }

    #[test]
    fn should_replace_one_null_version_while_preserving_history_when_versioning_is_suspended() {
        // Arrange
        let (base, reopened, bucket, key, versioned_id) = suspended_version_fixture();

        // Act
        reopened
            .put_object(
                bucket,
                key.to_string(),
                Object::new(
                    key.to_string(),
                    b"first-null".to_vec(),
                    "text/plain".to_string(),
                ),
            )
            .unwrap();
        reopened
            .put_object(
                bucket,
                key.to_string(),
                Object::new(
                    key.to_string(),
                    b"replacement-null".to_vec(),
                    "text/plain".to_string(),
                ),
            )
            .unwrap();

        // Assert
        let current = reopened.get_object(bucket, key).unwrap();
        assert_eq!(current.version_id.as_deref(), Some("null"));
        assert_eq!(current.data, b"replacement-null");
        assert!(!reopened.get_bucket(bucket).unwrap().versioning_enabled);
        let versions = reopened.list_object_versions_for_key(bucket, key).unwrap();
        assert_eq!(versions.len(), 2);
        assert_eq!(
            versions
                .iter()
                .filter(|version| version.version_id.as_deref() == Some("null"))
                .count(),
            1
        );
        assert!(versions
            .iter()
            .any(|version| version.version_id.as_deref() == Some(versioned_id.as_str())));

        reopened.delete_object(bucket, key).unwrap();
        reopened.delete_object(bucket, key).unwrap();
        let deleted_versions = reopened.list_object_versions_for_key(bucket, key).unwrap();
        assert_eq!(deleted_versions.len(), 2);
        assert_eq!(
            deleted_versions
                .iter()
                .filter(|version| version.version_id.as_deref() == Some("null"))
                .count(),
            1
        );
        assert!(deleted_versions.iter().any(|version| {
            version.version_id.as_deref() == Some("null")
                && version.provider_metadata.get("s3_delete_marker") == Some(&"true".to_string())
        }));

        reopened.delete_object_version(bucket, key, "null").unwrap();
        let restored = reopened.get_object(bucket, key).unwrap();
        assert_eq!(restored.version_id.as_deref(), Some(versioned_id.as_str()));
        assert_eq!(restored.data, b"versioned");

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn should_preserve_history_when_deleting_current_object() {
        // Arrange
        let base = temp_path();
        let storage = FilesystemStorage::new(&base);

        let bucket = "version-delete-bucket";
        let key = "doc.txt";
        storage.create_bucket(bucket.to_string()).unwrap();
        storage.enable_versioning(bucket).unwrap();

        storage
            .put_object(
                bucket,
                key.to_string(),
                Object::new(key.to_string(), b"v1".to_vec(), "text/plain".to_string()),
            )
            .unwrap();
        let first_version_id = storage
            .get_object(bucket, key)
            .unwrap()
            .version_id
            .clone()
            .expect("version id should exist");

        storage
            .put_object(
                bucket,
                key.to_string(),
                Object::new(key.to_string(), b"v2".to_vec(), "text/plain".to_string()),
            )
            .unwrap();
        let current_version_id = storage
            .get_object(bucket, key)
            .unwrap()
            .version_id
            .clone()
            .expect("version id should exist");

        // Act
        storage.delete_object(bucket, key).unwrap();

        // Assert
        assert!(matches!(
            storage.get_object(bucket, key),
            Err(Error::KeyNotFound)
        ));
        assert_eq!(
            storage
                .get_object_version(bucket, key, &current_version_id)
                .unwrap()
                .data,
            b"v2".to_vec()
        );
        assert_eq!(
            storage
                .get_object_version(bucket, key, &first_version_id)
                .unwrap()
                .data,
            b"v1".to_vec()
        );

        let versions = storage.list_object_versions(bucket, Some(key)).unwrap();
        let version_ids: Vec<_> = versions
            .into_iter()
            .filter_map(|obj| obj.version_id)
            .collect();
        assert_eq!(version_ids.len(), 3);
        assert!(version_ids.contains(&first_version_id));
        assert!(version_ids.contains(&current_version_id));

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn should_create_another_delete_marker_when_versioned_object_is_deleted_repeatedly() {
        // Arrange
        let base = temp_path();
        let storage = FilesystemStorage::new(&base);
        let bucket = "repeated-delete-marker-bucket";
        let key = "object.txt";
        storage.create_bucket(bucket.to_string()).unwrap();
        storage.enable_versioning(bucket).unwrap();
        storage
            .put_object(
                bucket,
                key.to_string(),
                Object::new(key.to_string(), b"value".to_vec(), "text/plain".to_string()),
            )
            .unwrap();

        // Act
        storage.delete_object(bucket, key).unwrap();
        storage.delete_object(bucket, key).unwrap();

        // Assert
        let versions = storage.list_object_versions_for_key(bucket, key).unwrap();
        let delete_marker_ids = versions
            .iter()
            .filter(|version| {
                version.provider_metadata.get("s3_delete_marker") == Some(&"true".to_string())
            })
            .filter_map(|version| version.version_id.as_deref())
            .collect::<Vec<_>>();
        assert_eq!(delete_marker_ids.len(), 2);
        assert_ne!(delete_marker_ids[0], delete_marker_ids[1]);
        assert!(matches!(
            storage.get_object(bucket, key),
            Err(Error::KeyNotFound)
        ));

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn should_reveal_previous_data_when_latest_delete_marker_is_removed() {
        // Arrange
        let base = temp_path();
        let storage = FilesystemStorage::new(&base);
        let bucket = "restore-delete-marker-bucket";
        let key = "object.txt";
        storage.create_bucket(bucket.to_string()).unwrap();
        storage.enable_versioning(bucket).unwrap();
        storage
            .put_object(
                bucket,
                key.to_string(),
                Object::new(
                    key.to_string(),
                    b"recoverable".to_vec(),
                    "text/plain".to_string(),
                ),
            )
            .unwrap();
        storage.delete_object(bucket, key).unwrap();
        let marker_version_id = storage
            .list_object_versions_for_key(bucket, key)
            .unwrap()
            .into_iter()
            .find(|version| {
                version.provider_metadata.get("s3_delete_marker") == Some(&"true".to_string())
            })
            .and_then(|version| version.version_id)
            .expect("delete marker should have a version id");

        // Act
        storage
            .delete_object_version(bucket, key, &marker_version_id)
            .unwrap();

        // Assert
        let restored = storage.get_object(bucket, key).unwrap();
        assert_eq!(restored.data, b"recoverable");
        assert!(!restored.provider_metadata.contains_key("s3_delete_marker"));

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn should_list_versions_for_exact_key_without_unrelated_prefix_matches() {
        // Arrange
        let base = temp_path();
        let storage = FilesystemStorage::new(&base);

        let bucket = "version-exact-bucket";
        let key = "doc.txt";
        storage.create_bucket(bucket.to_string()).unwrap();
        storage.enable_versioning(bucket).unwrap();

        for body in ["v1", "v2", "v3"] {
            storage
                .put_object(
                    bucket,
                    key.to_string(),
                    Object::new(
                        key.to_string(),
                        body.as_bytes().to_vec(),
                        "text/plain".into(),
                    ),
                )
                .unwrap();
        }

        for body in ["other-v1", "other-v2"] {
            storage
                .put_object(
                    bucket,
                    "doc.txt-extra".to_string(),
                    Object::new(
                        "doc.txt-extra".to_string(),
                        body.as_bytes().to_vec(),
                        "text/plain".into(),
                    ),
                )
                .unwrap();
        }

        // Act
        let versions = storage.list_object_versions_for_key(bucket, key).unwrap();

        // Assert
        assert_eq!(versions.len(), 3);
        assert!(versions.iter().all(|version| version.key == key));

        storage.delete_object(bucket, key).unwrap();
        let historical_versions = storage.list_object_versions_for_key(bucket, key).unwrap();
        assert_eq!(historical_versions.len(), 4);
        assert_eq!(
            historical_versions
                .iter()
                .filter(|version| version.provider_metadata.get("s3_delete_marker")
                    == Some(&"true".to_string()))
                .count(),
            1
        );
        assert!(historical_versions.iter().all(|version| version.key == key));

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn should_persist_provider_state_without_listing_it_as_a_bucket() {
        // Arrange
        let base = temp_path();
        let storage = FilesystemStorage::new(&base);

        // Act
        storage
            .put_provider_state("gcs", "session-1", b"state".to_vec())
            .unwrap();
        let restored = FilesystemStorage::new(&base);

        // Assert
        assert_eq!(
            restored.get_provider_state("gcs", "session-1").unwrap(),
            b"state".to_vec()
        );
        assert!(restored.list_buckets().unwrap().is_empty());

        restored.delete_provider_state("gcs", "session-1").unwrap();
        assert!(matches!(
            restored.get_provider_state("gcs", "session-1"),
            Err(Error::KeyNotFound)
        ));

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn should_coordinate_concurrent_writes_to_same_object() {
        // Arrange
        let base = temp_path();
        let storage = Arc::new(FilesystemStorage::new(&base));
        storage.create_bucket("concurrent".to_string()).unwrap();
        let barrier = Arc::new(std::sync::Barrier::new(8));

        // Act
        let handles: Vec<_> = (0..8)
            .map(|index| {
                let storage = storage.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    let payload = format!("payload-{index}");
                    storage
                        .put_object(
                            "concurrent",
                            "same.txt".to_string(),
                            Object::new(
                                "same.txt".to_string(),
                                payload.into_bytes(),
                                "text/plain".to_string(),
                            ),
                        )
                        .unwrap();
                })
            })
            .collect();

        for handle in handles {
            handle.join().unwrap();
        }

        // Assert
        let stored = storage.get_object("concurrent", "same.txt").unwrap();
        let payload = String::from_utf8(stored.data).unwrap();
        assert!(payload.starts_with("payload-"));
        assert_eq!(stored.size, payload.len() as u64);

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn should_serialize_conditional_write_races() {
        // Arrange
        // Act
        // Assert
        use std::sync::Barrier;
        use std::thread;

        let base = temp_path();
        let storage = Arc::new(FilesystemStorage::new(&base));
        storage.create_bucket("conditional".to_string()).unwrap();

        let barrier = Arc::new(Barrier::new(8));
        let handles = (0..8)
            .map(|index| {
                let storage = storage.clone();
                let barrier = barrier.clone();
                thread::spawn(move || {
                    barrier.wait();
                    storage
                        .put_object_if(
                            "conditional",
                            "lease".to_string(),
                            Object::new(
                                "lease".to_string(),
                                format!("create-{index}").into_bytes(),
                                "text/plain".to_string(),
                            ),
                            &ObjectCondition::Missing,
                        )
                        .unwrap()
                })
            })
            .collect::<Vec<_>>();
        assert_eq!(
            handles
                .into_iter()
                .map(|handle| handle.join().unwrap())
                .filter(|won| *won)
                .count(),
            1
        );

        let observed = storage.get_object("conditional", "lease").unwrap().etag;
        let barrier = Arc::new(Barrier::new(8));
        let handles = (0..8)
            .map(|index| {
                let storage = storage.clone();
                let barrier = barrier.clone();
                let observed = observed.clone();
                thread::spawn(move || {
                    barrier.wait();
                    storage
                        .put_object_if(
                            "conditional",
                            "lease".to_string(),
                            Object::new(
                                "lease".to_string(),
                                format!("update-{index}").into_bytes(),
                                "text/plain".to_string(),
                            ),
                            &ObjectCondition::Etag(observed),
                        )
                        .unwrap()
                })
            })
            .collect::<Vec<_>>();
        assert_eq!(
            handles
                .into_iter()
                .map(|handle| handle.join().unwrap())
                .filter(|won| *won)
                .count(),
            1
        );

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn should_keep_bucket_names_outside_filesystem_paths() {
        // Arrange
        let base = temp_path();
        let storage = FilesystemStorage::new(&base);
        let escaped = base
            .parent()
            .unwrap()
            .join(format!("escaped-{}", Uuid::new_v4()));
        let bucket = format!("../{}", escaped.file_name().unwrap().to_string_lossy());

        // Act
        storage.create_bucket(bucket.clone()).unwrap();

        // Assert
        assert!(storage.bucket_exists(&bucket).unwrap());
        assert!(!escaped.exists());
        assert_eq!(storage.list_buckets().unwrap()[0].name, bucket);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn should_list_buckets_in_logical_name_order_after_restart() {
        // Arrange
        let base = temp_path();
        let storage = FilesystemStorage::new(&base);
        for name in ["zebra-bucket", "alpha-bucket", "middle-bucket"] {
            storage.create_bucket(name.to_string()).unwrap();
        }

        // Act
        let reopened = FilesystemStorage::new(&base);
        let names: Vec<_> = reopened
            .list_buckets()
            .unwrap()
            .into_iter()
            .map(|bucket| bucket.name)
            .collect();

        // Assert
        assert_eq!(names, ["alpha-bucket", "middle-bucket", "zebra-bucket"]);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn should_preserve_staged_upload_range_composition_across_storage_restart() {
        // Arrange
        let base = temp_path();
        let storage = FilesystemStorage::new(&base);
        storage.create_bucket("uploads".to_string()).unwrap();
        let first = base.join("first.request");
        let second = base.join("second.request");
        std::fs::write(&first, b"hello ").unwrap();
        std::fs::write(&second, b"xxworldyy").unwrap();
        storage
            .stage_upload_payload("vendor", "session", "one", &first, 0, 6)
            .unwrap();
        storage
            .stage_upload_payload("vendor", "session", "two", &second, 2, 5)
            .unwrap();
        drop(storage);
        let reopened = FilesystemStorage::new(&base);
        let mut object = Object::new(
            "joined.bin".to_string(),
            Vec::new(),
            "application/octet-stream".to_string(),
        );
        object.size = 11;

        // Act
        let written = reopened
            .compose_upload_payloads(
                "vendor",
                "session",
                &["one".to_string(), "two".to_string()],
                "uploads",
                "joined.bin".to_string(),
                object,
                None,
            )
            .unwrap();
        let session_path = reopened.provider_upload_session_dir("vendor", "session");
        reopened.delete_upload_session("vendor", "session").unwrap();

        // Assert
        assert!(written);
        assert_eq!(
            reopened.get_object("uploads", "joined.bin").unwrap().data,
            b"hello world"
        );
        assert!(!session_path.exists());
        assert!(!base.join(".spool").read_dir().unwrap().any(|_| true));
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn should_refuse_nonempty_legacy_storage_without_deleting_it() {
        // Arrange
        let base = temp_path();
        std::fs::create_dir_all(&base).unwrap();
        let legacy = base.join("legacy-data");
        std::fs::write(&legacy, b"keep me").unwrap();

        // Act
        let error = FilesystemStorage::open(&base)
            .err()
            .expect("legacy storage should be rejected");

        // Assert
        assert!(matches!(
            error,
            Error::InvalidRequest(message) if message.contains("Legacy nonempty storage")
        ));
        assert_eq!(std::fs::read(&legacy).unwrap(), b"keep me");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn should_remove_only_obsolete_payload_bearing_vendor_upload_state() {
        // Arrange
        let base = temp_path();
        let _ = FilesystemStorage::open(&base).unwrap();
        let provider_state = base.join(".provider-state");
        for provider in [
            "azure-block-session",
            "azure-committed-blocks",
            "gcs-resumable-session",
        ] {
            let directory = provider_state.join(provider);
            std::fs::create_dir_all(&directory).unwrap();
            std::fs::write(directory.join("legacy.json"), b"payload-bearing state").unwrap();
        }
        let retained = provider_state.join("retained-provider");
        std::fs::create_dir_all(&retained).unwrap();
        std::fs::write(retained.join("state.json"), b"keep me").unwrap();

        // Act
        let reopened = FilesystemStorage::open(&base);

        // Assert
        assert!(reopened.is_ok());
        assert!(!provider_state.join("azure-block-session").exists());
        assert!(!provider_state.join("azure-committed-blocks").exists());
        assert!(!provider_state.join("gcs-resumable-session").exists());
        assert_eq!(
            std::fs::read(retained.join("state.json")).unwrap(),
            b"keep me"
        );
        let _ = std::fs::remove_dir_all(&base);
    }
}
