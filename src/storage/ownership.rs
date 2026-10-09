use crate::error::{Error, Result};
use std::fs::{self, File};
use std::path::Path;

pub(crate) const WRITER_LOCK_FILE: &str = ".sqrzl-writer.lock";

/// Exclusive ownership of one emulator storage root.
///
/// Acquire this before opening any store or running recovery, and retain it
/// until every listener and background writer stops. All writers must cooperate
/// with this rule; modifying files directly while the emulator runs is unsupported.
/// The lock file remains after release so another process cannot lock a new inode.
pub struct StorageRootWriter {
    _file: File,
}

impl StorageRootWriter {
    /// Claim a root without waiting for an active writer.
    ///
    /// # Errors
    ///
    /// Returns an error when the root cannot be opened or already has a writer.
    pub fn acquire(root: impl AsRef<Path>) -> Result<Self> {
        let root = root.as_ref();
        fs::create_dir_all(root).map_err(|error| {
            Error::InternalError(format!("Failed to create storage root: {error}"))
        })?;
        if !root.join(super::FilesystemStorage::FORMAT_MARKER).exists()
            && fs::read_dir(root)
                .map_err(|error| {
                    Error::InternalError(format!("Failed to inspect storage root: {error}"))
                })?
                .any(|entry| entry.map_or(true, |entry| entry.file_name() != WRITER_LOCK_FILE))
        {
            return Err(Error::InvalidRequest(format!(
                "Legacy nonempty storage detected at '{}'. Archive or clear \
                 SQRZL_BLOBS_PATH before starting the emulator; no data was modified.",
                root.display()
            )));
        }
        let file = File::options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(root.join(WRITER_LOCK_FILE))
            .map_err(|error| {
                Error::InternalError(format!("Failed to open storage writer lock: {error}"))
            })?;
        file.try_lock().map_err(|error| match error {
            std::fs::TryLockError::WouldBlock => Error::InvalidRequest(format!(
                "Storage root '{}' already has an active writer. Use a distinct \
                 SQRZL_BLOBS_PATH or stop the other emulator.",
                root.display()
            )),
            std::fs::TryLockError::Error(error) => {
                Error::InternalError(format!("Failed to lock storage root: {error}"))
            }
        })?;
        Ok(Self { _file: file })
    }
}
