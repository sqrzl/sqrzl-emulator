use super::{FilesystemStorage, Object, Path, PathBuf, Result, TestHook};

impl FilesystemStorage {
    /// # Panics
    /// Panics if the test-hook mutex has been poisoned.
    pub fn set_test_hook(&self, hook: Option<TestHook>) {
        *self.test_hook.lock().unwrap() = hook;
    }

    #[must_use]
    pub fn test_base_path(&self) -> &Path {
        &self.base_path
    }

    #[must_use]
    pub fn test_compute_object_id(bucket: &str, key: &str) -> String {
        Self::compute_object_id(bucket, key)
    }

    #[must_use]
    pub fn test_object_id_dir(&self, bucket: &str, id: &str) -> PathBuf {
        self.object_id_dir(bucket, id)
    }

    #[must_use]
    pub fn test_object_data_path(&self, bucket: &str, id: &str) -> PathBuf {
        self.object_data_path(bucket, id)
    }

    #[must_use]
    pub fn test_object_metadata_path(&self, bucket: &str, id: &str) -> PathBuf {
        self.object_metadata_path(bucket, id)
    }

    /// # Errors
    /// Returns an error for unreadable or invalid metadata.
    pub fn test_read_object_metadata(path: &Path) -> Result<Object> {
        Self::read_object_metadata(path)
    }

    /// # Errors
    /// Returns an error when serialization or publication fails.
    pub fn test_write_object_metadata(path: &Path, object: &Object) -> Result<()> {
        Self::write_object_metadata(path, object)
    }

    /// # Errors
    /// Returns an error when the file cannot be written or published.
    pub fn test_atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
        Self::atomic_write(path, bytes)
    }
}
