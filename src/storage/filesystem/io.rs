use super::{BucketIdentity, FilesystemStorage};
use crate::error::{Error, Result};
use crate::models::{MultipartUpload, Object};
use crate::storage::upload_cancellation::{
    UploadCancellation, GCS_ACTIVE_SESSION_STATE, GCS_XML_CANCELLATION_STATE,
};
use crate::storage::{BucketStore, LockFreeIndex, ProviderStateStore, UploadStore};
use sha2::{Digest, Sha256};
use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::fs;
use std::hash::{Hash, Hasher};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use uuid::Uuid;

impl FilesystemStorage {
    pub(crate) const FORMAT_MARKER: &'static str = ".sqrzl-storage-format-v2";

    /// Opens a process storage root, initializing an empty root as format v2.
    ///
    /// Legacy nonempty roots are never modified automatically.
    ///
    /// # Errors
    ///
    /// Returns an actionable error when the root is nonempty and unmarked, or
    /// when the storage root or format marker cannot be read or written.
    pub fn open(base_path: impl AsRef<Path>) -> Result<Self> {
        let base_path = base_path.as_ref();
        Self::create_directory_durable(base_path)?;
        let marker = base_path.join(Self::FORMAT_MARKER);
        if !marker.exists() {
            let nonempty = fs::read_dir(base_path)
                .map_err(|err| {
                    Error::InternalError(format!("Failed to inspect storage root: {err}"))
                })?
                .any(|entry| {
                    entry.map_or(true, |entry| {
                        entry.file_name() != crate::storage::ownership::WRITER_LOCK_FILE
                    })
                });
            if nonempty {
                return Err(Error::InvalidRequest(format!(
                    "Legacy nonempty storage detected at '{}'. Sqrzl storage format v2 is \
                     intentionally incompatible. Archive or clear SQRZL_BLOBS_PATH, then restart; \
                     no data was deleted.",
                    base_path.display()
                )));
            }
            Self::atomic_write(&marker, b"2\n").map_err(|err| {
                Error::InternalError(format!("Failed to write storage format marker: {err}"))
            })?;
        }
        Self::purge_obsolete_vendor_upload_state(base_path)?;
        Self::try_new(base_path)
    }

    fn purge_obsolete_vendor_upload_state(base_path: &Path) -> Result<()> {
        for provider in [
            "azure-block-session",
            "azure-committed-blocks",
            "gcs-resumable-session",
        ] {
            let path = base_path.join(".provider-state").join(provider);
            if path.exists() {
                fs::remove_dir_all(&path).map_err(|error| {
                    Error::InternalError(format!(
                        "Failed to remove obsolete vendor upload state '{}': {error}",
                        path.display()
                    ))
                })?;
            }
        }
        Ok(())
    }

    /// Compatibility constructor for callers that cannot handle initialization errors.
    ///
    /// # Panics
    /// Panics if initialization or journal recovery fails. Use [`Self::open`]
    /// or [`Self::try_new`] to propagate these failures instead.
    pub fn new(base_path: impl AsRef<Path>) -> Self {
        Self::try_new(base_path).expect("Filesystem storage initialization or crash recovery failed; use FilesystemStorage::open to handle errors")
    }

    /// Constructs a store after recovering committed publications.
    ///
    /// Callers must hold the storage-root writer guard before opening a shared
    /// root; recovery mutates on-disk state before the index is reconstructed.
    ///
    /// # Errors
    /// Returns an error when root initialization or recovery fails.
    pub fn try_new(base_path: impl AsRef<Path>) -> Result<Self> {
        let base_path = base_path.as_ref().to_path_buf();
        // Ensure base directory exists
        Self::create_directory_durable(&base_path)?;
        Self::recover_publications(&base_path)?;

        let index = Arc::new(LockFreeIndex::new());

        // Rebuild index from filesystem
        if let Ok(entries) = fs::read_dir(&base_path) {
            for entry in entries.flatten() {
                let Ok(metadata) = entry.metadata() else {
                    continue;
                };

                if metadata.is_dir() {
                    if entry
                        .file_name()
                        .to_str()
                        .is_some_and(|name| name.starts_with('.'))
                    {
                        continue;
                    }
                    if let Ok(bucket_name) = fs::read_to_string(entry.path().join(".bucket.name")) {
                        index.get_or_create_bucket(bucket_name.clone());

                        // Scan bucket for object_id directories
                        if let Ok(objects) = fs::read_dir(entry.path()) {
                            for obj_entry in objects.flatten() {
                                let path = obj_entry.path();
                                if path.is_dir() {
                                    // Each directory is an object_id, read metadata to get key
                                    let metadata_path = path.join("object.meta.json");
                                    if let Ok(metadata_json) = fs::read(&metadata_path) {
                                        if let Ok(obj) =
                                            serde_json::from_slice::<Object>(&metadata_json)
                                        {
                                            index.insert(&bucket_name, &obj.key);
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }

        let storage = Self {
            base_path,
            index,
            uploads_cache: Mutex::new(HashMap::new()),
            object_locks: Mutex::new(HashMap::new()),
            bucket_locks: Mutex::new(HashMap::new()),
            #[cfg(test)]
            test_hook: Mutex::new(None),
        };
        storage.recover_gcs_cancellations()?;
        Ok(storage)
    }

    fn recover_gcs_cancellations(&self) -> Result<()> {
        let directory = self
            .base_path
            .join(".provider-state")
            .join(GCS_XML_CANCELLATION_STATE);
        let entries = match fs::read_dir(&directory) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(Error::InternalError(error.to_string())),
        };
        for entry in entries {
            let path = entry
                .map_err(|error| Error::InternalError(error.to_string()))?
                .path();
            if path.extension().is_none_or(|extension| extension != "json") {
                continue;
            }
            let decision: UploadCancellation = serde_json::from_slice(
                &fs::read(&path).map_err(|error| Error::InternalError(error.to_string()))?,
            )
            .map_err(|error| {
                Error::InternalError(format!("Invalid upload cancellation decision: {error}"))
            })?;
            if path != self.provider_state_path(GCS_XML_CANCELLATION_STATE, &decision.session_id) {
                return Err(Error::InternalError(
                    "Upload cancellation decision has an invalid session identity".to_string(),
                ));
            }
            // A durable decision must retire active payloads before any reader
            // can observe the reopened store, even while its tombstone is live.
            self.delete_provider_state(GCS_ACTIVE_SESSION_STATE, &decision.session_id)?;
            self.delete_upload_session("gcs", &decision.session_id)?;
            let current_bucket = match self.get_bucket(&decision.bucket) {
                Ok(bucket) => Some(bucket),
                Err(Error::BucketNotFound) => None,
                Err(error) => return Err(error),
            };
            if decision.expires_at <= chrono::Utc::now()
                || current_bucket
                    .is_none_or(|bucket| bucket.created_at != decision.bucket_created_at)
            {
                self.delete_provider_state(GCS_XML_CANCELLATION_STATE, &decision.session_id)?;
            }
        }
        Ok(())
    }

    pub(super) fn object_lock(&self, bucket: &str, key: &str) -> Result<Arc<Mutex<()>>> {
        self.object_id_lock(bucket, &Self::compute_object_id(bucket, key))
    }

    pub(super) fn object_id_lock(&self, bucket: &str, object_id: &str) -> Result<Arc<Mutex<()>>> {
        let lock_key = format!("{bucket}/{object_id}");
        let mut locks = self
            .object_locks
            .lock()
            .map_err(|_| Error::InternalError("Failed to lock object lock registry".to_string()))?;
        locks.retain(|_, lock| lock.strong_count() > 0);
        if let Some(lock) = locks.get(&lock_key).and_then(std::sync::Weak::upgrade) {
            return Ok(lock);
        }
        let lock = Arc::new(Mutex::new(()));
        locks.insert(lock_key, Arc::downgrade(&lock));
        Ok(lock)
    }

    pub(super) fn bucket_lock(&self, bucket: &str) -> Result<Arc<Mutex<()>>> {
        let mut locks = self
            .bucket_locks
            .lock()
            .map_err(|_| Error::InternalError("Failed to lock bucket lock registry".to_string()))?;
        locks.retain(|_, lock| lock.strong_count() > 0);
        if let Some(lock) = locks.get(bucket).and_then(std::sync::Weak::upgrade) {
            return Ok(lock);
        }
        let lock = Arc::new(Mutex::new(()));
        locks.insert(bucket.to_string(), Arc::downgrade(&lock));
        Ok(lock)
    }

    pub(super) fn read_bucket_identity_locked(&self, bucket: &str) -> Result<BucketIdentity> {
        let path = self.bucket_dir(bucket).join(".bucket.identity.json");
        match fs::read(&path) {
            Ok(json) => serde_json::from_slice(&json)
                .map_err(|error| Error::InternalError(format!("Invalid bucket identity: {error}"))),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                // Existing v2 buckets predate the identity sidecar. The name
                // marker belongs to bucket creation; object writes do not alter
                // it. Derive once and persist, never substitute the read clock.
                let metadata = fs::metadata(self.bucket_dir(bucket).join(".bucket.name")).map_err(
                    |error| {
                        Error::InternalError(format!(
                            "Failed to inspect legacy bucket identity: {error}"
                        ))
                    },
                )?;
                let created_at = metadata
                    .created()
                    .or_else(|_| metadata.modified())
                    .map_err(|error| {
                        Error::InternalError(format!(
                            "Failed to derive legacy bucket creation: {error}"
                        ))
                    })?
                    .into();
                let identity = BucketIdentity {
                    created_at,
                    modified_at: created_at,
                };
                self.write_bucket_identity(bucket, &identity)?;
                Ok(identity)
            }
            Err(error) => Err(Error::InternalError(format!(
                "Failed to read bucket identity: {error}"
            ))),
        }
    }

    pub(super) fn write_bucket_identity(
        &self,
        bucket: &str,
        identity: &BucketIdentity,
    ) -> Result<()> {
        let json = serde_json::to_vec(identity).map_err(|error| {
            Error::InternalError(format!("Failed to serialize bucket identity: {error}"))
        })?;
        Self::atomic_write(
            &self.bucket_dir(bucket).join(".bucket.identity.json"),
            &json,
        )
    }

    pub(super) fn touch_bucket_identity(&self, bucket: &str) -> Result<()> {
        let lock = self.bucket_lock(bucket)?;
        let _guard = lock.lock().map_err(|_| {
            Error::InternalError("Failed to lock bucket identity for modification".to_string())
        })?;
        let mut identity = self.read_bucket_identity_locked(bucket)?;
        identity.modified_at = chrono::Utc::now();
        self.write_bucket_identity(bucket, &identity)
    }

    pub(super) fn bucket_dir(&self, bucket: &str) -> PathBuf {
        let digest = Sha256::digest(bucket.as_bytes());
        self.base_path.join(format!("b-{}", hex::encode(digest)))
    }

    pub(super) fn provider_state_path(&self, provider: &str, key: &str) -> PathBuf {
        let state_id = Self::compute_object_id(provider, key);
        self.base_path
            .join(".provider-state")
            .join(provider)
            .join(format!("{state_id}.json"))
    }

    pub(super) fn provider_upload_session_dir(&self, provider: &str, session: &str) -> PathBuf {
        let provider_id = hex::encode(Sha256::digest(provider.as_bytes()));
        let session_id = hex::encode(Sha256::digest(session.as_bytes()));
        self.base_path
            .join(".provider-uploads")
            .join(provider_id)
            .join(session_id)
    }

    pub(super) fn provider_upload_item_path(
        &self,
        provider: &str,
        session: &str,
        item: &str,
    ) -> PathBuf {
        let item_id = hex::encode(Sha256::digest(item.as_bytes()));
        self.provider_upload_session_dir(provider, session)
            .join(format!("{item_id}.blob"))
    }

    pub(super) fn bucket_acl_path(&self, bucket: &str) -> PathBuf {
        self.bucket_dir(bucket).join("bucket.acl.json")
    }

    pub(super) fn is_bucket_control_entry(entry: &fs::DirEntry) -> bool {
        let name = entry.file_name();
        let name = name.to_string_lossy();

        match name.as_ref() {
            ".bucket.meta.json"
            | ".bucket.identity.json"
            | ".bucket.name"
            | ".versioning-enabled"
            | ".lifecycle.json"
            | ".policy.json"
            | "bucket.acl.json" => true,
            ".multipart" | ".spool" => entry
                .path()
                .read_dir()
                .is_ok_and(|entries| entries.flatten().next().is_none()),
            _ => false,
        }
    }

    pub(super) fn bucket_metadata_path(&self, bucket: &str) -> PathBuf {
        self.bucket_dir(bucket).join(".bucket.meta.json")
    }

    pub(super) fn versioning_marker(&self, bucket: &str) -> PathBuf {
        self.bucket_dir(bucket).join(".versioning-enabled")
    }

    pub(super) fn versioning_enabled(&self, bucket: &str) -> bool {
        let marker = self.versioning_marker(bucket);
        marker.exists()
            && match fs::read(&marker) {
                Ok(state) => state.as_slice() != b"suspended",
                Err(_) => true,
            }
    }

    pub(super) fn versioning_suspended(&self, bucket: &str) -> bool {
        fs::read(self.versioning_marker(bucket)).is_ok_and(|state| state.as_slice() == b"suspended")
    }

    pub(super) fn compute_object_id(bucket: &str, key: &str) -> String {
        let mut hasher = DefaultHasher::new();
        (bucket, key).hash(&mut hasher);
        format!("{:x}", hasher.finish())
    }

    pub(super) fn object_id_dir(&self, bucket: &str, object_id: &str) -> PathBuf {
        self.bucket_dir(bucket).join(object_id)
    }

    pub(super) fn object_data_path(&self, bucket: &str, object_id: &str) -> PathBuf {
        self.object_id_dir(bucket, object_id).join("object.blob")
    }

    pub(super) fn object_metadata_path(&self, bucket: &str, object_id: &str) -> PathBuf {
        self.object_id_dir(bucket, object_id)
            .join("object.meta.json")
    }

    pub(super) fn versions_dir(&self, bucket: &str, object_id: &str) -> PathBuf {
        self.object_id_dir(bucket, object_id).join("versions")
    }

    pub(super) fn version_dir(&self, bucket: &str, object_id: &str, version_id: &str) -> PathBuf {
        self.versions_dir(bucket, object_id).join(version_id)
    }

    pub(super) fn version_data_path(
        &self,
        bucket: &str,
        object_id: &str,
        version_id: &str,
    ) -> PathBuf {
        self.version_dir(bucket, object_id, version_id)
            .join("object.blob")
    }

    pub(super) fn version_metadata_path(
        &self,
        bucket: &str,
        object_id: &str,
        version_id: &str,
    ) -> PathBuf {
        self.version_dir(bucket, object_id, version_id)
            .join("object.meta.json")
    }

    pub(super) fn multipart_dir(&self, bucket: &str, upload_id: &str) -> PathBuf {
        self.bucket_dir(bucket).join(".multipart").join(upload_id)
    }

    pub(super) fn part_path(&self, bucket: &str, upload_id: &str, part_number: u32) -> PathBuf {
        self.multipart_dir(bucket, upload_id)
            .join(format!("part-{part_number:05}"))
    }

    pub(super) fn multipart_root(&self, bucket: &str) -> PathBuf {
        self.bucket_dir(bucket).join(".multipart")
    }

    pub(super) fn upload_record_dir(&self, bucket: &str, upload_id: &str) -> PathBuf {
        self.multipart_dir(bucket, upload_id)
    }

    pub(super) fn upload_record_path(&self, bucket: &str, upload_id: &str) -> PathBuf {
        self.upload_record_dir(bucket, upload_id)
            .join("upload.json")
    }

    pub(super) fn read_upload_record(upload_path: &Path) -> Result<MultipartUpload> {
        let json_bytes = fs::read(upload_path).map_err(|e| {
            Error::InternalError(format!("Failed to read multipart upload record: {e}"))
        })?;

        let mut upload: MultipartUpload = serde_json::from_slice(&json_bytes).map_err(|e| {
            Error::InternalError(format!("Failed to parse multipart upload record: {e}"))
        })?;
        Self::normalize_upload_parts(&mut upload);
        Ok(upload)
    }

    pub(super) fn write_upload_record(&self, bucket: &str, upload: &MultipartUpload) -> Result<()> {
        let upload_path = self.upload_record_path(bucket, &upload.upload_id);
        Self::write_upload_record_at_path(&upload_path, upload)
    }

    pub(super) fn write_upload_record_at_path(
        upload_path: &Path,
        upload: &MultipartUpload,
    ) -> Result<()> {
        let _upload_dir = upload_path
            .parent()
            .ok_or_else(|| Error::InternalError("Invalid multipart upload path".to_string()))?;

        let mut buffer = Vec::new();
        {
            let mut writer = std::io::BufWriter::new(&mut buffer);
            serde_json::to_writer(&mut writer, upload).map_err(|e| {
                Error::InternalError(format!("Failed to serialize multipart upload record: {e}"))
            })?;
            writer.flush().map_err(|e| {
                Error::InternalError(format!("Failed to write multipart upload record: {e}"))
            })?;
        }
        Self::atomic_write(upload_path, &buffer)?;

        Ok(())
    }

    pub(super) fn remove_upload_record(&self, bucket: &str, upload_id: &str) -> Result<()> {
        let upload_dir = self.upload_record_dir(bucket, upload_id);
        if upload_dir.exists() {
            let root = self.multipart_root(bucket);
            let retired = root.join(format!(".retired-upload-{}", Uuid::new_v4()));
            fs::rename(&upload_dir, &retired).map_err(|error| {
                Error::InternalError(format!("Failed to retire multipart upload: {error}"))
            })?;
            Self::sync_directory(&root)?;
            #[cfg(test)]
            self.test_phase(super::TestPhase::UploadRecordRetired);
            fs::remove_dir_all(&retired).map_err(|e| {
                Error::InternalError(format!("Failed to remove multipart upload dir: {e}"))
            })?;
            Self::sync_directory(&root)?;
        }
        Ok(())
    }

    pub(super) fn load_uploads_from_disk(
        &self,
        bucket: &str,
    ) -> Result<std::collections::HashMap<String, MultipartUpload>> {
        let multipart_root = self.multipart_root(bucket);
        let mut uploads = std::collections::HashMap::new();

        if multipart_root.exists() {
            let entries = fs::read_dir(&multipart_root)
                .map_err(|e| Error::InternalError(format!("Failed to read multipart dir: {e}")))?;

            for entry in entries.flatten() {
                let path = entry.path();
                if !path.is_dir() || entry.file_name().to_string_lossy().starts_with('.') {
                    continue;
                }
                let upload_path = path.join("upload.json");
                if let Ok(upload) = Self::read_upload_record(&upload_path) {
                    uploads.insert(upload.upload_id.clone(), upload);
                }
            }
        }

        if uploads.is_empty() {
            let legacy_uploads_path = multipart_root.join("uploads.json");
            if legacy_uploads_path.exists() {
                let json_bytes = fs::read(&legacy_uploads_path).map_err(|e| {
                    Error::InternalError(format!("Failed to read legacy uploads index: {e}"))
                })?;

                uploads = serde_json::from_slice(&json_bytes).map_err(|e| {
                    Error::InternalError(format!("Failed to parse legacy uploads index: {e}"))
                })?;
                for upload in uploads.values_mut() {
                    Self::normalize_upload_parts(upload);
                }
            }
        }

        Ok(uploads)
    }

    pub(super) fn load_uploads(
        &self,
        bucket: &str,
    ) -> Result<std::collections::HashMap<String, MultipartUpload>> {
        {
            let cache = self
                .uploads_cache
                .lock()
                .map_err(|_| Error::InternalError("Failed to lock uploads cache".to_string()))?;
            if let Some(cached) = cache.get(bucket).cloned() {
                return Ok(cached);
            }
        }

        let uploads = self.load_uploads_from_disk(bucket)?;
        self.uploads_cache
            .lock()
            .map_err(|_| Error::InternalError("Failed to lock uploads cache".to_string()))?
            .insert(bucket.to_string(), uploads.clone());
        Ok(uploads)
    }

    pub(super) fn ensure_uploads_cache_loaded(&self, bucket: &str) -> Result<()> {
        let needs_load = {
            let cache = self
                .uploads_cache
                .lock()
                .map_err(|_| Error::InternalError("Failed to lock uploads cache".to_string()))?;
            !cache.contains_key(bucket)
        };

        if needs_load {
            let uploads = self.load_uploads_from_disk(bucket)?;
            self.uploads_cache
                .lock()
                .map_err(|_| Error::InternalError("Failed to lock uploads cache".to_string()))?
                .insert(bucket.to_string(), uploads);
        }

        Ok(())
    }

    fn normalize_upload_parts(upload: &mut MultipartUpload) {
        upload.parts.sort_unstable_by_key(|part| part.part_number);
    }

    pub(super) fn write_object_files(
        &self,
        bucket: &str,
        object_id: &str,
        object: &Object,
    ) -> Result<()> {
        self.publish_pair(
            &self.object_id_dir(bucket, object_id),
            object,
            super::ObjectPayload::InMemory,
        )
    }

    pub(super) fn write_version_snapshot(
        &self,
        bucket: &str,
        object_id: &str,
        version_id: &str,
        object: &Object,
    ) -> Result<()> {
        let version_dir = self.version_dir(bucket, object_id, version_id);
        let mut version_object = object.clone();
        version_object.version_id = Some(version_id.to_string());
        self.publish_pair(
            &version_dir,
            &version_object,
            super::ObjectPayload::Stored(&self.object_data_path(bucket, object_id)),
        )?;
        #[cfg(test)]
        self.test_phase(super::TestPhase::VersionPublished);
        Ok(())
    }

    pub(super) fn read_bucket_metadata(&self, bucket: &str) -> Result<HashMap<String, String>> {
        let path = self.bucket_metadata_path(bucket);
        if !path.exists() {
            return Ok(HashMap::new());
        }

        let json = fs::read(&path)
            .map_err(|e| Error::InternalError(format!("Failed to read bucket metadata: {e}")))?;
        serde_json::from_slice(&json)
            .map_err(|e| Error::InternalError(format!("Failed to parse bucket metadata: {e}")))
    }

    pub(super) fn write_bucket_metadata(
        &self,
        bucket: &str,
        metadata: &HashMap<String, String>,
    ) -> Result<()> {
        let path = self.bucket_metadata_path(bucket);
        let json = serde_json::to_vec(metadata).map_err(|e| {
            Error::InternalError(format!("Failed to serialize bucket metadata: {e}"))
        })?;
        Self::atomic_write(&path, &json)
    }

    pub(super) fn read_object_metadata(metadata_path: &Path) -> Result<Object> {
        let json = fs::read_to_string(metadata_path)
            .map_err(|e| Error::InternalError(format!("Failed to read metadata: {e}")))?;
        serde_json::from_str(&json)
            .map_err(|e| Error::InternalError(format!("Failed to parse metadata: {e}")))
    }

    pub(super) fn write_object_metadata(metadata_path: &Path, object: &Object) -> Result<()> {
        let json = serde_json::to_string(object)
            .map_err(|e| Error::InternalError(format!("Failed to serialize metadata: {e}")))?;
        Self::atomic_write(metadata_path, json.as_bytes())
    }

    pub(super) fn version_entries_exist(&self, bucket: &str, object_id: &str) -> Result<bool> {
        let versions_dir = self.versions_dir(bucket, object_id);
        if !versions_dir.exists() {
            return Ok(false);
        }

        let entries = fs::read_dir(&versions_dir)
            .map_err(|e| Error::InternalError(format!("Failed to read versions dir: {e}")))?;

        Ok(entries.flatten().next().is_some())
    }

    pub(super) fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
        let parent = path
            .parent()
            .ok_or_else(|| Error::InternalError("Invalid file path".to_string()))?;
        Self::create_directory_durable(parent)?;

        let file_name = path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| Error::InternalError("Invalid file name".to_string()))?;
        let temp_path = parent.join(format!(".{file_name}.{}.tmp", Uuid::new_v4()));

        let write_result = (|| -> Result<()> {
            let mut file = fs::File::create(&temp_path)
                .map_err(|e| Error::InternalError(format!("Failed to create temp file: {e}")))?;
            file.write_all(bytes)
                .map_err(|e| Error::InternalError(format!("Failed to write temp file: {e}")))?;
            file.sync_all()
                .map_err(|e| Error::InternalError(format!("Failed to sync temp file: {e}")))?;
            fs::rename(&temp_path, path)
                .map_err(|e| Error::InternalError(format!("Failed to commit temp file: {e}")))?;
            Self::sync_directory(parent)
        })();

        if write_result.is_err() {
            let _ = fs::remove_file(&temp_path);
        }

        write_result
    }

    /// Moves an already-written file into place as `dest`, without reading
    /// its contents into memory. Prefers a same-filesystem rename; falls
    /// back to copy-then-remove when `src` and `dest` live on different
    /// filesystems (rename cannot cross a mount boundary).
    pub(super) fn atomic_move(src: &Path, dest: &Path) -> Result<()> {
        let parent = dest
            .parent()
            .ok_or_else(|| Error::InternalError("Invalid file path".to_string()))?;
        Self::create_directory_durable(parent)?;
        // Sync spooled bytes before either a same-device rename or the copy
        // fallback. Never copy over a visible destination in place.
        fs::File::open(src)
            .and_then(|file| file.sync_all())
            .map_err(|error| {
                Error::InternalError(format!("Failed to sync spooled payload: {error}"))
            })?;
        if fs::rename(src, dest).is_err() {
            Self::atomic_copy(src, dest)?;
            fs::remove_file(src).map_err(|error| {
                Error::InternalError(format!("Failed to remove spooled payload: {error}"))
            })?;
        }
        Self::sync_directory(parent)?;
        if let Some(source_parent) = src.parent().filter(|path| *path != parent) {
            Self::sync_directory(source_parent)?;
        }
        Ok(())
    }

    pub(super) fn atomic_copy(src: &Path, dest: &Path) -> Result<()> {
        let parent = dest
            .parent()
            .ok_or_else(|| Error::InternalError("Invalid copy destination".to_string()))?;
        Self::create_directory_durable(parent)?;
        let temp = parent.join(format!(".copy-{}.tmp", Uuid::new_v4()));
        let outcome = (|| {
            fs::copy(src, &temp).map_err(|error| {
                Error::InternalError(format!("Failed to copy staged payload: {error}"))
            })?;
            fs::File::open(&temp)
                .and_then(|file| file.sync_all())
                .map_err(|error| {
                    Error::InternalError(format!("Failed to sync copied payload: {error}"))
                })?;
            fs::rename(&temp, dest).map_err(|error| {
                Error::InternalError(format!("Failed to publish copied payload: {error}"))
            })?;
            Self::sync_directory(parent)
        })();
        if outcome.is_err() {
            let _ = fs::remove_file(temp);
        }
        outcome
    }

    /// A scratch path, on the same filesystem as this bucket's blob storage,
    /// for a file that will be moved into place with [`Self::atomic_move`].
    pub(super) fn spool_scratch_path(&self, bucket: &str) -> PathBuf {
        self.bucket_dir(bucket)
            .join(".spool")
            .join(format!("{}.tmp", Uuid::new_v4()))
    }

    /// Same as [`Self::write_object_files`], but the object payload is moved
    /// in from `payload_path` (already fully written to disk) instead of
    /// being copied out of `object.data`, so completing a write never
    /// requires the whole payload to be resident in memory at once.
    pub(super) fn write_object_files_from_path(
        &self,
        bucket: &str,
        object_id: &str,
        object: &Object,
        payload_path: &Path,
    ) -> Result<()> {
        self.publish_pair(
            &self.object_id_dir(bucket, object_id),
            object,
            super::ObjectPayload::Spooled(payload_path),
        )
    }

    /// Records a part's `etag`/`size` in the upload's part list. Shared by
    /// [`super::FilesystemStorage`]'s in-memory and streamed `upload_part`
    /// implementations, both of which write the part's bytes to disk
    /// themselves before calling this.
    pub(super) fn record_uploaded_part(
        &self,
        bucket: &str,
        upload_id: &str,
        part_number: u32,
        etag: String,
        size: u64,
    ) -> Result<String> {
        let upload_path = self.upload_record_path(bucket, upload_id);
        let mut cache = self
            .uploads_cache
            .lock()
            .map_err(|_| Error::InternalError("Failed to lock uploads cache".to_string()))?;
        let uploads = cache
            .get_mut(bucket)
            .ok_or_else(|| Error::InternalError("Missing uploads cache entry".to_string()))?;
        let upload = uploads.get_mut(upload_id).ok_or(Error::NoSuchUpload)?;
        let part = crate::models::Part {
            part_number,
            etag: etag.clone(),
            size,
            last_modified: chrono::Utc::now(),
        };
        match upload
            .parts
            .binary_search_by_key(&part_number, |existing| existing.part_number)
        {
            Ok(index) => upload.parts[index] = part,
            Err(index) => upload.parts.insert(index, part),
        }
        Self::write_upload_record_at_path(&upload_path, upload)?;
        Ok(etag)
    }
}
