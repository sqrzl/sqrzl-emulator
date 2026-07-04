use super::FilesystemStorage;
use crate::error::{Error, Result};
use crate::models::{MultipartUpload, Object};
use crate::storage::LockFreeIndex;
use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::fs;
use std::hash::{Hash, Hasher};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use uuid::Uuid;

impl FilesystemStorage {
    pub fn new(base_path: impl AsRef<Path>) -> Self {
        let base_path = base_path.as_ref().to_path_buf();
        // Ensure base directory exists
        let _ = fs::create_dir_all(&base_path);

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
                    if let Some(bucket_name) = entry
                        .file_name()
                        .to_str()
                        .map(std::string::ToString::to_string)
                    {
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

        Self {
            base_path,
            index,
            uploads_cache: Mutex::new(HashMap::new()),
            object_locks: Mutex::new(HashMap::new()),
        }
    }

    pub(super) fn object_lock(&self, bucket: &str, key: &str) -> Result<Arc<Mutex<()>>> {
        let lock_key = format!("{bucket}/{key}");
        let mut locks = self
            .object_locks
            .lock()
            .map_err(|_| Error::InternalError("Failed to lock object lock registry".to_string()))?;
        Ok(locks
            .entry(lock_key)
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone())
    }

    pub(super) fn bucket_dir(&self, bucket: &str) -> PathBuf {
        self.base_path.join(bucket)
    }

    pub(super) fn provider_state_path(&self, provider: &str, key: &str) -> PathBuf {
        let state_id = Self::compute_object_id(provider, key);
        self.base_path
            .join(".provider-state")
            .join(provider)
            .join(format!("{state_id}.json"))
    }

    pub(super) fn bucket_acl_path(&self, bucket: &str) -> PathBuf {
        self.bucket_dir(bucket).join("bucket.acl.json")
    }

    pub(super) fn is_bucket_control_entry(entry: &fs::DirEntry) -> bool {
        let name = entry.file_name();
        let name = name.to_string_lossy();

        match name.as_ref() {
            ".bucket.meta.json"
            | ".versioning-enabled"
            | ".lifecycle.json"
            | ".policy.json"
            | "bucket.acl.json" => true,
            ".multipart" => entry
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
        self.versioning_marker(bucket).exists()
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
            fs::remove_dir_all(&upload_dir).map_err(|e| {
                Error::InternalError(format!("Failed to remove multipart upload dir: {e}"))
            })?;
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
                if !path.is_dir() {
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

    pub(super) fn ensure_upload_exists(&self, bucket: &str, upload_id: &str) -> Result<()> {
        {
            let cache = self
                .uploads_cache
                .lock()
                .map_err(|_| Error::InternalError("Failed to lock uploads cache".to_string()))?;
            if let Some(uploads) = cache.get(bucket) {
                if uploads.contains_key(upload_id) {
                    return Ok(());
                }

                return Err(Error::NoSuchUpload);
            }
        }

        let uploads = self.load_uploads_from_disk(bucket)?;
        let upload_exists = uploads.contains_key(upload_id);
        self.uploads_cache
            .lock()
            .map_err(|_| Error::InternalError("Failed to lock uploads cache".to_string()))?
            .insert(bucket.to_string(), uploads);

        if upload_exists {
            Ok(())
        } else {
            Err(Error::NoSuchUpload)
        }
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
        let object_id_dir = self.object_id_dir(bucket, object_id);
        fs::create_dir_all(&object_id_dir)
            .map_err(|e| Error::InternalError(format!("Failed to create object directory: {e}")))?;

        let object_data_path = self.object_data_path(bucket, object_id);
        Self::atomic_write(&object_data_path, &object.data)?;

        let metadata_path = self.object_metadata_path(bucket, object_id);
        let metadata_json = serde_json::to_string(object)
            .map_err(|e| Error::InternalError(format!("Failed to serialize metadata: {e}")))?;
        Self::atomic_write(&metadata_path, metadata_json.as_bytes())?;

        Ok(())
    }

    pub(super) fn write_version_snapshot(
        &self,
        bucket: &str,
        object_id: &str,
        version_id: &str,
        object: &Object,
    ) -> Result<()> {
        let version_dir = self.version_dir(bucket, object_id, version_id);
        fs::create_dir_all(&version_dir).map_err(|e| {
            Error::InternalError(format!("Failed to create version directory: {e}"))
        })?;

        let mut version_object = object.clone();
        version_object.version_id = Some(version_id.to_string());

        let version_data_path = self.version_data_path(bucket, object_id, version_id);
        Self::atomic_write(&version_data_path, &version_object.data)?;

        let version_metadata_path = self.version_metadata_path(bucket, object_id, version_id);
        let metadata_json = serde_json::to_string(&version_object).map_err(|e| {
            Error::InternalError(format!("Failed to serialize version metadata: {e}"))
        })?;

        Self::atomic_write(&version_metadata_path, metadata_json.as_bytes())?;

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
        fs::create_dir_all(parent)
            .map_err(|e| Error::InternalError(format!("Failed to create parent directory: {e}")))?;

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
            Ok(())
        })();

        if write_result.is_err() {
            let _ = fs::remove_file(&temp_path);
        }

        write_result
    }
}
