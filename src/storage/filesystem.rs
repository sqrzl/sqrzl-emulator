use crate::error::{Error, Result};
use crate::models::{policy::Acl, Bucket, MultipartUpload, Object};
use crate::storage::{LockFreeIndex, Storage};
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use uuid::Uuid;

mod io;

pub struct FilesystemStorage {
    base_path: PathBuf,
    index: Arc<LockFreeIndex>,
    uploads_cache: Mutex<HashMap<String, HashMap<String, MultipartUpload>>>,
}

impl Storage for FilesystemStorage {
    fn create_bucket(&self, name: String) -> Result<()> {
        let bucket_dir = self.bucket_dir(&name);

        if bucket_dir.exists() {
            return Err(Error::BucketAlreadyExists);
        }

        fs::create_dir(&bucket_dir)
            .map_err(|e| Error::InternalError(format!("Failed to create bucket: {}", e)))?;

        // Update index
        self.index.get_or_create_bucket(name);

        Ok(())
    }

    fn delete_bucket(&self, name: &str) -> Result<()> {
        let bucket_dir = self.bucket_dir(name);

        if !bucket_dir.exists() {
            return Err(Error::BucketNotFound);
        }

        // Check if bucket is empty
        let entries = fs::read_dir(&bucket_dir)
            .map_err(|e| Error::InternalError(format!("Failed to read bucket: {}", e)))?;

        for entry in entries {
            let entry = entry
                .map_err(|e| Error::InternalError(format!("Failed to read bucket entry: {}", e)))?;
            if !self.is_bucket_control_entry(name, &entry) {
                return Err(Error::BucketNotEmpty);
            }
        }

        fs::remove_dir_all(&bucket_dir)
            .map_err(|e| Error::InternalError(format!("Failed to delete bucket: {}", e)))?;

        // Update index
        self.index.clear_bucket(name);

        Ok(())
    }

    fn get_bucket(&self, name: &str) -> Result<Bucket> {
        let bucket_dir = self.bucket_dir(name);

        if !bucket_dir.exists() {
            return Err(Error::BucketNotFound);
        }

        let mut bucket = Bucket::new(name.to_string());
        bucket.versioning_enabled = self.versioning_enabled(name);
        bucket.metadata = self.read_bucket_metadata(name)?;
        Ok(bucket)
    }

    fn list_buckets(&self) -> Result<Vec<Bucket>> {
        let mut buckets = Vec::new();
        let entries = fs::read_dir(&self.base_path)
            .map_err(|e| Error::InternalError(format!("Failed to read base path: {}", e)))?;

        for entry in entries {
            let entry =
                entry.map_err(|e| Error::InternalError(format!("Failed to read entry: {}", e)))?;

            let metadata = entry
                .metadata()
                .map_err(|e| Error::InternalError(format!("Failed to get metadata: {}", e)))?;

            if metadata.is_dir() {
                let name = entry.file_name();
                if let Some(bucket_name) = name.to_str() {
                    let mut bucket = Bucket::new(bucket_name.to_string());
                    bucket.versioning_enabled = self.versioning_enabled(bucket_name);
                    bucket.metadata = self.read_bucket_metadata(bucket_name)?;
                    buckets.push(bucket);
                }
            }
        }

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
        if !self.bucket_exists(bucket)? {
            return Err(Error::BucketNotFound);
        }

        self.write_bucket_metadata(bucket, &metadata)?;

        let mut bucket_record = self.get_bucket(bucket)?;
        bucket_record.metadata = metadata;
        Ok(bucket_record)
    }

    fn put_object(&self, bucket: &str, key: String, object: Object) -> Result<()> {
        let bucket_dir = self.bucket_dir(bucket);

        if !bucket_dir.exists() {
            return Err(Error::BucketNotFound);
        }

        let object_id = Self::compute_object_id(bucket, &key);

        let mut object = object;

        if self.versioning_enabled(bucket) {
            match self.get_object(bucket, &key) {
                Ok(current_object) => {
                    let snapshot_version_id = current_object
                        .version_id
                        .clone()
                        .unwrap_or_else(|| Uuid::new_v4().to_string());
                    self.write_version_snapshot(
                        bucket,
                        &object_id,
                        &snapshot_version_id,
                        &current_object,
                    )?;
                }
                Err(Error::KeyNotFound) => {}
                Err(e) => return Err(e),
            }

            object.version_id = Some(Uuid::new_v4().to_string());
        } else {
            object.version_id = None;
        }

        self.write_object_files(bucket, &object_id, &object)?;

        // Update index
        self.index.insert(bucket.to_string(), key);

        Ok(())
    }

    fn get_object(&self, bucket: &str, key: &str) -> Result<Object> {
        let object_id = Self::compute_object_id(bucket, key);
        let object_data_path = self.object_data_path(bucket, &object_id);

        if !object_data_path.exists() {
            return Err(Error::KeyNotFound);
        }

        let metadata_path = self.object_metadata_path(bucket, &object_id);
        let mut object = self.read_object_metadata(&metadata_path)?;
        object.data = fs::read(&object_data_path)
            .map_err(|e| Error::InternalError(format!("Failed to read object: {}", e)))?;
        Ok(object)
    }

    fn get_object_range(
        &self,
        bucket: &str,
        key: &str,
        start: u64,
        end: Option<u64>,
    ) -> Result<(Object, Vec<u8>)> {
        let object_id = Self::compute_object_id(bucket, key);
        let object_data_path = self.object_data_path(bucket, &object_id);

        if !object_data_path.exists() {
            return Err(Error::KeyNotFound);
        }

        let metadata_path = self.object_metadata_path(bucket, &object_id);

        let object = self.read_object_metadata(&metadata_path)?;

        // Validate range
        if start >= object.size {
            return Err(Error::InternalError(
                "Range start beyond file size".to_string(),
            ));
        }

        let actual_end = end
            .map(|e| e.min(object.size - 1))
            .unwrap_or(object.size - 1);
        if actual_end < start {
            return Err(Error::InternalError(
                "Invalid range: end < start".to_string(),
            ));
        }

        let length = (actual_end - start + 1) as usize;

        // Read range from file
        use std::io::{Read, Seek, SeekFrom};
        let mut file = fs::File::open(&object_data_path)
            .map_err(|e| Error::InternalError(format!("Failed to open object file: {}", e)))?;

        file.seek(SeekFrom::Start(start))
            .map_err(|e| Error::InternalError(format!("Failed to seek: {}", e)))?;

        let mut buffer = vec![0u8; length];
        file.read_exact(&mut buffer)
            .map_err(|e| Error::InternalError(format!("Failed to read range: {}", e)))?;

        Ok((object, buffer))
    }

    fn delete_object(&self, bucket: &str, key: &str) -> Result<()> {
        let object_id = Self::compute_object_id(bucket, key);
        let object_id_dir = self.object_id_dir(bucket, &object_id);

        self.get_object(bucket, key)?;

        if self.versioning_enabled(bucket) {
            let object_data_path = self.object_data_path(bucket, &object_id);
            let metadata_path = self.object_metadata_path(bucket, &object_id);

            if object_data_path.exists() {
                fs::remove_file(&object_data_path)
                    .map_err(|e| Error::InternalError(format!("Failed to delete object: {}", e)))?;
            }

            if metadata_path.exists() {
                fs::remove_file(&metadata_path)
                    .map_err(|e| Error::InternalError(format!("Failed to delete object: {}", e)))?;
            }

            if !self.version_entries_exist(bucket, &object_id)? {
                let _ = fs::remove_dir_all(&object_id_dir);
            }
        } else {
            // Remove entire object_id directory
            fs::remove_dir_all(&object_id_dir)
                .map_err(|e| Error::InternalError(format!("Failed to delete object: {}", e)))?;
        }

        // Update index
        self.index.remove(bucket, key);

        Ok(())
    }

    fn update_object_storage_class(
        &self,
        bucket: &str,
        key: &str,
        storage_class: &str,
    ) -> Result<()> {
        let object_id = Self::compute_object_id(bucket, key);
        let metadata_path = self.object_metadata_path(bucket, &object_id);

        if !metadata_path.exists() {
            return Err(Error::KeyNotFound);
        }

        let mut object = self.read_object_metadata(&metadata_path)?;

        object.storage_class = storage_class.to_string();

        self.write_object_metadata(&metadata_path, &object)
    }

    fn object_exists(&self, bucket: &str, key: &str) -> Result<bool> {
        // Fast path: check lock-free index first
        Ok(self.index.contains(bucket, key))
    }

    fn get_bucket_acl(&self, bucket: &str) -> Result<Acl> {
        if !self.bucket_exists(bucket)? {
            return Err(Error::BucketNotFound);
        }

        let path = self.bucket_acl_path(bucket);
        if !path.exists() {
            return Ok(Acl::default());
        }

        let json = fs::read(&path)
            .map_err(|e| Error::InternalError(format!("Failed to read bucket ACL: {}", e)))?;
        serde_json::from_slice(&json)
            .map_err(|e| Error::InternalError(format!("Failed to parse bucket ACL: {}", e)))
    }

    fn put_bucket_acl(&self, bucket: &str, acl: Acl) -> Result<()> {
        if !self.bucket_exists(bucket)? {
            return Err(Error::BucketNotFound);
        }

        let path = self.bucket_acl_path(bucket);
        let json = serde_json::to_vec(&acl)
            .map_err(|e| Error::InternalError(format!("Failed to serialize bucket ACL: {}", e)))?;
        fs::write(&path, json)
            .map_err(|e| Error::InternalError(format!("Failed to write bucket ACL: {}", e)))
    }

    fn get_object_acl(&self, bucket: &str, key: &str) -> Result<Acl> {
        let object_id = Self::compute_object_id(bucket, key);
        let metadata_path = self.object_metadata_path(bucket, &object_id);

        if !metadata_path.exists() {
            return Err(Error::KeyNotFound);
        }

        let object = self.read_object_metadata(&metadata_path)?;

        Ok(object.acl.unwrap_or_default())
    }

    fn put_object_acl(&self, bucket: &str, key: &str, acl: Acl) -> Result<()> {
        let object_id = Self::compute_object_id(bucket, key);
        let metadata_path = self.object_metadata_path(bucket, &object_id);

        if !metadata_path.exists() {
            return Err(Error::KeyNotFound);
        }

        let mut object = self.read_object_metadata(&metadata_path)?;

        object.acl = Some(acl);

        self.write_object_metadata(&metadata_path, &object)
    }

    fn get_bucket_lifecycle(
        &self,
        bucket: &str,
    ) -> Result<crate::models::lifecycle::LifecycleConfiguration> {
        let bucket_path = self.base_path.join(bucket);
        if !bucket_path.exists() {
            return Err(Error::BucketNotFound);
        }

        let lifecycle_path = bucket_path.join(".lifecycle.json");
        if !lifecycle_path.exists() {
            return Err(Error::KeyNotFound);
        }

        let json = fs::read(&lifecycle_path)
            .map_err(|e| Error::InternalError(format!("Failed to read lifecycle config: {}", e)))?;
        serde_json::from_slice(&json)
            .map_err(|e| Error::InternalError(format!("Failed to parse lifecycle config: {}", e)))
    }

    fn put_bucket_lifecycle(
        &self,
        bucket: &str,
        config: crate::models::lifecycle::LifecycleConfiguration,
    ) -> Result<()> {
        let bucket_path = self.base_path.join(bucket);
        if !bucket_path.exists() {
            return Err(Error::BucketNotFound);
        }

        let lifecycle_path = bucket_path.join(".lifecycle.json");
        let json = serde_json::to_vec(&config).map_err(|e| {
            Error::InternalError(format!("Failed to serialize lifecycle config: {}", e))
        })?;
        fs::write(&lifecycle_path, json)
            .map_err(|e| Error::InternalError(format!("Failed to write lifecycle config: {}", e)))
    }

    fn delete_bucket_lifecycle(&self, bucket: &str) -> Result<()> {
        let bucket_path = self.base_path.join(bucket);
        if !bucket_path.exists() {
            return Err(Error::BucketNotFound);
        }

        let lifecycle_path = bucket_path.join(".lifecycle.json");
        if lifecycle_path.exists() {
            fs::remove_file(&lifecycle_path).map_err(|e| {
                Error::InternalError(format!("Failed to delete lifecycle config: {}", e))
            })?;
        }
        Ok(())
    }

    fn get_bucket_policy(
        &self,
        bucket: &str,
    ) -> Result<crate::models::policy::BucketPolicyDocument> {
        let bucket_path = self.base_path.join(bucket);
        if !bucket_path.exists() {
            return Err(Error::BucketNotFound);
        }

        let policy_path = bucket_path.join(".policy.json");
        if !policy_path.exists() {
            return Err(Error::KeyNotFound);
        }

        let policy_json = fs::read(&policy_path)
            .map_err(|e| Error::InternalError(format!("Failed to read policy: {}", e)))?;

        serde_json::from_slice(&policy_json)
            .map_err(|e| Error::InternalError(format!("Failed to parse policy: {}", e)))
    }

    fn put_bucket_policy(
        &self,
        bucket: &str,
        policy: crate::models::policy::BucketPolicyDocument,
    ) -> Result<()> {
        let bucket_path = self.base_path.join(bucket);
        if !bucket_path.exists() {
            return Err(Error::BucketNotFound);
        }

        let policy_path = bucket_path.join(".policy.json");
        let policy_json = serde_json::to_vec(&policy)
            .map_err(|e| Error::InternalError(format!("Failed to serialize policy: {}", e)))?;

        fs::write(&policy_path, policy_json)
            .map_err(|e| Error::InternalError(format!("Failed to write policy: {}", e)))
    }

    fn delete_bucket_policy(&self, bucket: &str) -> Result<()> {
        let bucket_path = self.base_path.join(bucket);
        if !bucket_path.exists() {
            return Err(Error::BucketNotFound);
        }

        let policy_path = bucket_path.join(".policy.json");
        if policy_path.exists() {
            fs::remove_file(&policy_path)
                .map_err(|e| Error::InternalError(format!("Failed to delete policy: {}", e)))?;
        }
        Ok(())
    }

    fn get_object_tags(&self, bucket: &str, key: &str) -> Result<HashMap<String, String>> {
        let object_id = Self::compute_object_id(bucket, key);
        let metadata_path = self.object_metadata_path(bucket, &object_id);

        if !metadata_path.exists() {
            return Err(Error::KeyNotFound);
        }

        Ok(self.read_object_metadata(&metadata_path)?.tags)
    }

    fn put_object_tags(
        &self,
        bucket: &str,
        key: &str,
        tags: HashMap<String, String>,
    ) -> Result<()> {
        let object_id = Self::compute_object_id(bucket, key);
        let metadata_path = self.object_metadata_path(bucket, &object_id);

        if !metadata_path.exists() {
            return Err(Error::KeyNotFound);
        }

        let mut object = self.read_object_metadata(&metadata_path)?;

        object.tags = tags;

        self.write_object_metadata(&metadata_path, &object)?;

        Ok(())
    }

    fn delete_object_tags(&self, bucket: &str, key: &str) -> Result<()> {
        let object_id = Self::compute_object_id(bucket, key);
        let metadata_path = self.object_metadata_path(bucket, &object_id);

        if !metadata_path.exists() {
            return Err(Error::KeyNotFound);
        }

        let mut object = self.read_object_metadata(&metadata_path)?;

        // Clear all tags
        object.tags.clear();

        self.write_object_metadata(&metadata_path, &object)?;

        Ok(())
    }

    fn list_objects(
        &self,
        bucket: &str,
        prefix: Option<&str>,
        _delimiter: Option<&str>,
        marker: Option<&str>,
        max_keys: Option<usize>,
    ) -> Result<crate::models::ListObjectsResult> {
        let bucket_dir = self.bucket_dir(bucket);
        if !bucket_dir.exists() {
            return Err(Error::BucketNotFound);
        }

        let max_keys = max_keys.unwrap_or(1000);
        let keys = self
            .index
            .list_prefix_marker(bucket, prefix, marker, Some(max_keys + 1));

        let mut objects = Vec::with_capacity(keys.len().min(max_keys));
        for obj_key in keys.iter().take(max_keys) {
            let object_id = Self::compute_object_id(bucket, obj_key);
            let metadata_path = self.object_metadata_path(bucket, &object_id);
            if let Ok(obj) = self.read_object_metadata(&metadata_path) {
                objects.push(obj);
            }
        }

        let is_truncated = keys.len() > max_keys;
        let next_marker = if is_truncated {
            keys.get(max_keys).cloned()
        } else {
            None
        };

        Ok(crate::models::ListObjectsResult {
            objects,
            is_truncated,
            next_marker,
        })
    }

    fn create_multipart_upload(&self, bucket: &str, key: String) -> Result<MultipartUpload> {
        self.create_multipart_upload_with_metadata(
            bucket,
            key,
            None,
            HashMap::new(),
            HashMap::new(),
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
            .map_err(|e| Error::InternalError(format!("Failed to create multipart dir: {}", e)))?;
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

        // Validate part number
        if !(1..=10000).contains(&part_number) {
            return Err(Error::InvalidPartNumber);
        }

        self.ensure_upload_exists(bucket, upload_id)?;

        // Compute ETag
        let etag = md5_hash(&data);
        let size = data.len() as u64;
        let upload_path = self.upload_record_path(bucket, upload_id);

        // Write part data
        let part_path = self.part_path(bucket, upload_id, part_number);
        fs::write(&part_path, &data)
            .map_err(|e| Error::InternalError(format!("Failed to write part: {}", e)))?;

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
        upload.part_data.insert(part_number, data);
        Self::write_upload_record_at_path(&upload_path, upload)?;

        Ok(etag)
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
        if !self.bucket_exists(bucket)? {
            return Err(Error::BucketNotFound);
        }

        self.ensure_uploads_cache_loaded(bucket)?;
        let upload = {
            let mut cache = self
                .uploads_cache
                .lock()
                .map_err(|_| Error::InternalError("Failed to lock uploads cache".to_string()))?;
            let uploads = cache
                .get_mut(bucket)
                .ok_or_else(|| Error::InternalError("Missing uploads cache entry".to_string()))?;
            uploads.remove(upload_id).ok_or(Error::NoSuchUpload)?
        };
        let crate::models::MultipartUpload {
            key,
            content_type,
            metadata,
            provider_metadata,
            parts,
            part_data,
            ..
        } = upload;

        if parts.is_empty() {
            return Err(Error::InvalidPartOrder);
        }

        // Validate parts are sequential starting from 1
        for (i, part) in parts.iter().enumerate() {
            if part.part_number != (i as u32 + 1) {
                return Err(Error::InvalidPartOrder);
            }
        }

        // Read all parts and concatenate
        let total_size = parts
            .iter()
            .fold(0usize, |acc, part| acc.saturating_add(part.size as usize));
        let mut object_data = Vec::with_capacity(total_size);
        for part in &parts {
            if let Some(part_data) = part_data.get(&part.part_number) {
                object_data.extend_from_slice(part_data);
            } else {
                let part_path = self.part_path(bucket, upload_id, part.part_number);
                let part_data = fs::read(&part_path)
                    .map_err(|e| Error::InternalError(format!("Failed to read part: {}", e)))?;
                object_data.extend_from_slice(&part_data);
            }
        }

        // Compute final ETag: MD5(concat(part_etags)) + "-" + part_count
        let mut etag_hash = md5::Context::new();
        for part in &parts {
            etag_hash.consume(part.etag.as_bytes());
        }
        let final_etag = format!("{:x}-{}", etag_hash.finalize(), parts.len());

        // Save completed object
        let mut obj = Object::new_with_metadata_and_etag(
            key.clone(),
            object_data,
            content_type.unwrap_or_else(|| "application/octet-stream".to_string()),
            metadata,
            final_etag.clone(),
        );
        if let Some(storage_class) = provider_metadata.get("storage_class") {
            obj.storage_class = storage_class.clone();
        }
        self.put_object(bucket, key, obj)?;
        self.remove_upload_record(bucket, upload_id)?;

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

    fn enable_versioning(&self, bucket: &str) -> Result<()> {
        if !self.bucket_exists(bucket)? {
            return Err(Error::BucketNotFound);
        }

        // Mark bucket as versioning-enabled by creating a marker file
        let versioning_marker = self.versioning_marker(bucket);
        fs::write(&versioning_marker, "")
            .map_err(|e| Error::InternalError(format!("Failed to enable versioning: {}", e)))?;
        Ok(())
    }

    fn suspend_versioning(&self, bucket: &str) -> Result<()> {
        if !self.bucket_exists(bucket)? {
            return Err(Error::BucketNotFound);
        }

        // Remove the versioning marker
        let versioning_marker = self.versioning_marker(bucket);
        let _ = fs::remove_file(versioning_marker);
        Ok(())
    }

    fn get_object_version(
        &self,
        bucket: &str,
        key: &str,
        version_id: &str,
    ) -> Result<crate::models::Object> {
        if !self.bucket_exists(bucket)? {
            return Err(Error::BucketNotFound);
        }

        let object_id = Self::compute_object_id(bucket, key);
        let version_data_path = self.version_data_path(bucket, &object_id, version_id);
        if !version_data_path.exists() {
            let current_object = self.get_object(bucket, key).map_err(|err| match err {
                Error::KeyNotFound => Error::NoSuchVersion,
                other => other,
            })?;

            if current_object.version_id.as_deref() == Some(version_id) {
                return Ok(current_object);
            }

            return Err(Error::NoSuchVersion);
        }

        let metadata_path = self.version_metadata_path(bucket, &object_id, version_id);
        let mut object = self.read_object_metadata(&metadata_path)?;
        object.data = fs::read(&version_data_path)
            .map_err(|e| Error::InternalError(format!("Failed to read version: {}", e)))?;

        object.version_id = Some(version_id.to_string());

        Ok(object)
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
                        if name.starts_with(".") {
                            continue;
                        }
                    }

                    let metadata_path = path.join("object.meta.json");
                    if let Ok(obj) = self.read_object_metadata(&metadata_path) {
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
                                        // Read version metadata to get the key and check prefix
                                        let metadata_path = version_path.join("object.meta.json");
                                        if let Ok(obj) = self.read_object_metadata(&metadata_path) {
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

        Ok(versions)
    }

    fn delete_object_version(&self, bucket: &str, key: &str, version_id: &str) -> Result<()> {
        if !self.bucket_exists(bucket)? {
            return Err(Error::BucketNotFound);
        }

        let object_id = Self::compute_object_id(bucket, key);
        let version_data_path = self.version_data_path(bucket, &object_id, version_id);
        if !version_data_path.exists() {
            let current_object = self.get_object(bucket, key).map_err(|err| match err {
                Error::KeyNotFound => Error::NoSuchVersion,
                other => other,
            })?;

            if current_object.version_id.as_deref() != Some(version_id) {
                return Err(Error::NoSuchVersion);
            }

            let object_data_path = self.object_data_path(bucket, &object_id);
            let metadata_path = self.object_metadata_path(bucket, &object_id);

            if object_data_path.exists() {
                fs::remove_file(&object_data_path).map_err(|e| {
                    Error::InternalError(format!("Failed to delete version: {}", e))
                })?;
            }

            if metadata_path.exists() {
                fs::remove_file(&metadata_path).map_err(|e| {
                    Error::InternalError(format!("Failed to delete version: {}", e))
                })?;
            }

            self.index.remove(bucket, key);

            if !self.version_entries_exist(bucket, &object_id)? {
                let version_dir = self.object_id_dir(bucket, &object_id);
                let _ = fs::remove_dir_all(&version_dir);
            }

            return Ok(());
        }

        let version_dir = self.version_dir(bucket, &object_id, version_id);
        fs::remove_dir_all(&version_dir)
            .map_err(|e| Error::InternalError(format!("Failed to delete version: {}", e)))?;

        if !self.object_data_path(bucket, &object_id).exists()
            && !self.version_entries_exist(bucket, &object_id)?
        {
            let object_id_dir = self.object_id_dir(bucket, &object_id);
            let _ = fs::remove_dir_all(&object_id_dir);
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
        std::env::temp_dir().join(format!("peas_fs_test_{}", Uuid::new_v4()))
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
        assert!(matches!(
            storage.get_object_version(bucket, key, &current_version_id),
            Err(Error::NoSuchVersion)
        ));
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
        assert_eq!(version_ids.len(), 1);
        assert!(version_ids.contains(&first_version_id));

        let _ = std::fs::remove_dir_all(&base);
    }
}
