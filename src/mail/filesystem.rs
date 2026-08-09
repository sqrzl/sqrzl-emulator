//! Filesystem-backed [`MailStore`] implementation.
//!
//! Layout mirrors `FilesystemStorage`'s small-metadata-as-JSON convention: each
//! mailbox is a hashed directory under `{blobs_path}/_mail/` with a sidecar that
//! preserves its original address. Each stored message is one `{message_id}.json`
//! file holding a serialized [`StoredMessage`] and, where available, one
//! `{message_id}.raw` file with the full MIME payload.
//! Writes go through a temp-file-then-rename so a crash mid-write can't leave a
//! corrupt file.

use crate::error::{Error, Result};
use crate::mail::model::{
    DeliveryStatus, ListMessagesParams, ListMessagesResult, MailboxInfo, Message, StoredMessage,
};
use crate::mail::{MailStore, ALL_MAILBOX};
use chrono::Utc;
use sha2::{Digest, Sha256};
use std::fs;
use std::path::{Path, PathBuf};

pub struct FilesystemMailStore {
    root: PathBuf,
}

const RAW_SUFFIX: &str = ".raw";
const MAILBOX_METADATA: &str = ".mailbox";
const MAILBOX_DIRECTORY_PREFIX: &str = "mbx-";

impl FilesystemMailStore {
    /// Opens (creating if needed) the `_mail` subtree under `blobs_path`.
    ///
    /// # Errors
    ///
    /// Returns an error when the underlying emulator operation fails.
    pub fn open(blobs_path: impl AsRef<Path>) -> Result<Self> {
        let root = blobs_path.as_ref().join("_mail");
        fs::create_dir_all(&root).map_err(|err| io_err(&err))?;
        Ok(Self { root })
    }

    fn mailbox_dir(&self, mailbox: &str) -> Result<PathBuf> {
        if mailbox.is_empty() {
            return Err(Error::InvalidRequest(
                "mailbox address must not be empty".to_string(),
            ));
        }

        // Continue to recognize safe directories written by builds predating
        // mailbox-key encoding.
        let legacy = self.root.join(mailbox);
        if validate_segment(mailbox).is_ok() && legacy.is_dir() {
            return Ok(legacy);
        }

        Ok(self.root.join(mailbox_storage_key(mailbox)))
    }

    fn ensure_mailbox_dir(&self, mailbox: &str) -> Result<PathBuf> {
        let dir = self.mailbox_dir(mailbox)?;
        fs::create_dir_all(&dir).map_err(|err| io_err(&err))?;
        let metadata_path = dir.join(MAILBOX_METADATA);
        match fs::read_to_string(&metadata_path) {
            Ok(stored_mailbox) if stored_mailbox == mailbox => {}
            Ok(_) => {
                return Err(Error::InternalError(
                    "mailbox storage key collision".to_string(),
                ));
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                write_atomic(&metadata_path, mailbox.as_bytes())?;
            }
            Err(err) => return Err(io_err(&err)),
        }
        Ok(dir)
    }

    fn message_path(&self, mailbox: &str, message_id: &str) -> Result<PathBuf> {
        validate_segment(message_id)?;
        Ok(self
            .mailbox_dir(mailbox)?
            .join(format!("{message_id}.json")))
    }

    fn raw_message_path(&self, mailbox: &str, message_id: &str) -> Result<PathBuf> {
        validate_segment(message_id)?;
        Ok(self
            .mailbox_dir(mailbox)?
            .join(format!("{message_id}{RAW_SUFFIX}")))
    }

    fn read_stored(path: &Path) -> Result<StoredMessage> {
        let data = fs::read(path).map_err(|_| Error::MessageNotFound)?;
        serde_json::from_slice(&data).map_err(|e| Error::InternalError(e.to_string()))
    }

    fn read_raw(path: &Path) -> Option<Vec<u8>> {
        fs::read(path).ok()
    }
}

impl MailStore for FilesystemMailStore {
    fn store_message(
        &self,
        mailbox: &str,
        message_id: &str,
        message: Message,
    ) -> Result<StoredMessage> {
        self.ensure_mailbox_dir(mailbox)?;
        let path = self.message_path(mailbox, message_id)?;
        let received_at = Utc::now();
        let stored = StoredMessage {
            message_id: message_id.to_string(),
            mailbox: mailbox.to_string(),
            message: message.clone(),
            delivery_status: DeliveryStatus::accepted(received_at),
            received_at,
        };
        let data = serde_json::to_vec(&stored).map_err(|e| Error::InternalError(e.to_string()))?;
        write_atomic(&path, &data)?;
        if let Some(raw_mime) = &message.raw_mime {
            let raw_path = self.raw_message_path(mailbox, message_id)?;
            write_atomic(&raw_path, raw_mime)?;
        }
        Ok(stored)
    }

    fn get_message(&self, mailbox: &str, message_id: &str) -> Result<StoredMessage> {
        let path = self.message_path(mailbox, message_id)?;
        let mut stored = Self::read_stored(&path)?;
        let raw_path = self.raw_message_path(mailbox, message_id)?;
        if let Some(raw_mime) = Self::read_raw(&raw_path) {
            stored.message.raw_mime = Some(raw_mime);
        }
        Ok(stored)
    }

    fn list_messages(
        &self,
        mailbox: &str,
        params: ListMessagesParams,
    ) -> Result<ListMessagesResult> {
        let dir = self.mailbox_dir(mailbox)?;
        let Ok(entries) = fs::read_dir(&dir) else {
            return Ok(ListMessagesResult::default());
        };

        let mut messages = Vec::new();
        for entry in entries.filter_map(std::result::Result::ok) {
            let path = entry.path();
            if path.extension().and_then(std::ffi::OsStr::to_str) != Some("json") {
                continue;
            }
            messages.push(Self::read_stored(&path)?);
        }
        messages.sort_by(|a, b| {
            a.received_at
                .cmp(&b.received_at)
                .then_with(|| a.message_id.cmp(&b.message_id))
        });

        let start = match &params.marker {
            Some(marker) => messages
                .iter()
                .position(|message| message.message_id == *marker)
                .map_or(0, |index| index + 1),
            None => 0,
        };
        let limit = params.limit.unwrap_or(usize::MAX);
        let mut page: Vec<StoredMessage> = messages.into_iter().skip(start).collect();
        let next_marker = if page.len() > limit {
            page.truncate(limit);
            page.last().map(|message| message.message_id.clone())
        } else {
            None
        };

        Ok(ListMessagesResult {
            messages: page,
            next_marker,
        })
    }

    fn delete_message(&self, mailbox: &str, message_id: &str) -> Result<()> {
        let path = self.message_path(mailbox, message_id)?;
        let raw_path = self.raw_message_path(mailbox, message_id)?;
        match fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(err) => Err(io_err(&err)),
        }?;
        match fs::remove_file(&raw_path) {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(err) => Err(io_err(&err)),
        }
    }

    fn delete_mailbox(&self, mailbox: &str) -> Result<()> {
        let dir = self.mailbox_dir(mailbox)?;
        match fs::remove_dir_all(&dir) {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(err) => Err(io_err(&err)),
        }
    }

    fn update_delivery_status(
        &self,
        mailbox: &str,
        message_id: &str,
        status: DeliveryStatus,
    ) -> Result<()> {
        let path = self.message_path(mailbox, message_id)?;
        let mut stored = Self::read_stored(&path)?;
        stored.delivery_status = status;
        let data = serde_json::to_vec(&stored).map_err(|e| Error::InternalError(e.to_string()))?;
        write_atomic(&path, &data)
    }

    fn list_mailboxes(&self) -> Result<Vec<MailboxInfo>> {
        let Ok(entries) = fs::read_dir(&self.root) else {
            return Ok(Vec::new());
        };

        let mut mailboxes = Vec::new();
        for entry in entries.filter_map(std::result::Result::ok) {
            if !entry.path().is_dir() {
                continue;
            }
            let metadata_path = entry.path().join(MAILBOX_METADATA);
            let address = match fs::read_to_string(metadata_path) {
                Ok(address) => address,
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                    let Some(address) = entry.file_name().to_str().map(str::to_string) else {
                        continue;
                    };
                    address
                }
                Err(err) => return Err(io_err(&err)),
            };
            if address == ALL_MAILBOX {
                continue;
            }

            let result = self.list_messages(&address, ListMessagesParams::default())?;
            mailboxes.push(MailboxInfo {
                address,
                message_count: result.messages.len(),
                last_received_at: result.messages.last().map(|message| message.received_at),
            });
        }
        mailboxes.sort_by(|a, b| a.address.cmp(&b.address));
        Ok(mailboxes)
    }

    fn ensure_mailbox(&self, mailbox: &str) -> Result<()> {
        self.ensure_mailbox_dir(mailbox)?;
        Ok(())
    }
}

fn mailbox_storage_key(mailbox: &str) -> String {
    format!(
        "{MAILBOX_DIRECTORY_PREFIX}{}",
        hex::encode(Sha256::digest(mailbox.as_bytes()))
    )
}

/// Rejects legacy mailbox or current message-id segments that could escape the
/// mail root. Current mailbox names are mapped through [`mailbox_storage_key`].
fn validate_segment(value: &str) -> Result<()> {
    if value.is_empty() || value.contains('/') || value.contains('\\') || value.contains("..") {
        return Err(Error::InvalidRequest(format!(
            "invalid mail path segment: {value}"
        )));
    }
    Ok(())
}

fn write_atomic(path: &Path, data: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|err| io_err(&err))?;
    }
    let mut tmp_name = path
        .file_name()
        .ok_or_else(|| Error::InternalError("message path has no file name".to_string()))?
        .to_os_string();
    tmp_name.push(".tmp");
    let tmp_path = path.with_file_name(tmp_name);
    fs::write(&tmp_path, data).map_err(|err| io_err(&err))?;
    fs::rename(&tmp_path, path).map_err(|err| io_err(&err))?;
    Ok(())
}

fn io_err(err: &std::io::Error) -> Error {
    Error::InternalError(err.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mail::model::{Address, SourceProtocol};
    use crate::mail::{fan_out, generate_message_id};

    fn temp_store() -> FilesystemMailStore {
        let dir = std::env::temp_dir().join(format!("sqrzl-mailstore-{}", uuid::Uuid::new_v4()));
        FilesystemMailStore::open(dir).expect("store should open")
    }

    fn sample_message(to: &str) -> Message {
        Message {
            source_protocol: SourceProtocol::Smtp,
            from: Address::new("sender@example.com"),
            to: vec![Address::new(to)],
            cc: Vec::new(),
            bcc: Vec::new(),
            subject: "hello".to_string(),
            headers: std::collections::HashMap::new(),
            body_text: Some("hi there".to_string()),
            body_html: None,
            attachments: Vec::new(),
            raw_mime: None,
            thread_id: None,
        }
    }

    #[test]
    fn should_round_trip_message_when_storing_and_getting() {
        // Arrange
        // Act
        // Assert
        let store = temp_store();
        let message_id = generate_message_id();

        let stored = store
            .store_message(
                "alice@example.com",
                &message_id,
                sample_message("alice@example.com"),
            )
            .expect("store should succeed");

        let fetched = store
            .get_message("alice@example.com", &message_id)
            .expect("get should succeed");

        assert_eq!(fetched.message_id, stored.message_id);
        assert_eq!(fetched.message.subject, "hello");
    }

    #[test]
    fn should_round_trip_mailboxes_with_filesystem_path_characters() {
        // Arrange
        // Act
        // Assert
        let store = temp_store();
        let addresses = ["user/tag@example.com", "team%ops@example.com"];

        for address in addresses {
            fan_out(&store, &sample_message(address)).expect("fan-out should succeed");

            let messages = store
                .list_messages(address, ListMessagesParams::default())
                .expect("mailbox should be readable by its original address");
            assert_eq!(messages.messages.len(), 1);
            assert_eq!(messages.messages[0].mailbox, address);
        }

        let listed = store.list_mailboxes().expect("mailboxes should list");
        assert_eq!(
            listed
                .iter()
                .map(|mailbox| mailbox.address.as_str())
                .collect::<Vec<_>>(),
            vec!["team%ops@example.com", "user/tag@example.com"]
        );
    }

    #[test]
    fn should_return_message_not_found_when_message_is_missing() {
        // Arrange
        // Act
        // Assert
        let store = temp_store();

        let err = store
            .get_message("alice@example.com", "does-not-exist")
            .expect_err("missing message should error");

        assert!(matches!(err, Error::MessageNotFound));
    }

    #[test]
    fn should_paginate_mailbox_with_next_marker() {
        // Arrange
        // Act
        // Assert
        let store = temp_store();
        for _ in 0..3 {
            fan_out(&store, &sample_message("alice@example.com")).expect("fan-out should succeed");
        }

        let page = store
            .list_messages(
                "alice@example.com",
                ListMessagesParams {
                    marker: None,
                    limit: Some(2),
                },
            )
            .expect("list should succeed");

        assert_eq!(page.messages.len(), 2);
        assert!(page.next_marker.is_some());

        let next_page = store
            .list_messages(
                "alice@example.com",
                ListMessagesParams {
                    marker: page.next_marker,
                    limit: Some(2),
                },
            )
            .expect("list should succeed");

        assert_eq!(next_page.messages.len(), 1);
        assert!(next_page.next_marker.is_none());
    }

    #[test]
    fn should_report_empty_result_when_mailbox_has_never_received_mail() {
        // Arrange
        // Act
        // Assert
        let store = temp_store();

        let result = store
            .list_messages("nobody@example.com", ListMessagesParams::default())
            .expect("listing an unknown mailbox should not error");

        assert!(result.messages.is_empty());
    }

    #[test]
    fn should_list_mailboxes_with_counts_without_all_pseudo_mailbox() {
        // Arrange
        // Act
        // Assert
        let store = temp_store();
        fan_out(&store, &sample_message("alice@example.com")).expect("fan-out should succeed");
        fan_out(&store, &sample_message("bob@example.com")).expect("fan-out should succeed");

        let mailboxes = store.list_mailboxes().expect("list should succeed");

        assert_eq!(mailboxes.len(), 2);
        assert!(mailboxes
            .iter()
            .all(|mailbox| mailbox.address != ALL_MAILBOX));
        assert!(mailboxes.iter().all(|mailbox| mailbox.message_count == 1));

        let all = store
            .list_messages(ALL_MAILBOX, ListMessagesParams::default())
            .expect("listing the _all mailbox should succeed");
        assert_eq!(all.messages.len(), 2);
    }

    #[test]
    fn should_update_delivery_status() {
        // Arrange
        // Act
        // Assert
        let store = temp_store();
        let message_id = generate_message_id();
        store
            .store_message(
                "alice@example.com",
                &message_id,
                sample_message("alice@example.com"),
            )
            .expect("store should succeed");

        store
            .update_delivery_status(
                "alice@example.com",
                &message_id,
                DeliveryStatus {
                    state: crate::mail::DeliveryState::Delivered,
                    detail: Some("simulated delivery".to_string()),
                    updated_at: Utc::now(),
                },
            )
            .expect("update should succeed");

        let fetched = store
            .get_message("alice@example.com", &message_id)
            .expect("get should succeed");
        assert_eq!(
            fetched.delivery_status.state,
            crate::mail::DeliveryState::Delivered
        );
    }

    #[test]
    fn should_delete_message_idempotently() {
        // Arrange
        // Act
        // Assert
        let store = temp_store();
        let message_id = generate_message_id();
        store
            .store_message(
                "alice@example.com",
                &message_id,
                sample_message("alice@example.com"),
            )
            .expect("store should succeed");

        store
            .delete_message("alice@example.com", &message_id)
            .expect("delete should succeed");
        store
            .delete_message("alice@example.com", &message_id)
            .expect("deleting an already-deleted message should still succeed");

        let err = store
            .get_message("alice@example.com", &message_id)
            .expect_err("message should be gone");
        assert!(matches!(err, Error::MessageNotFound));
    }

    #[test]
    fn should_delete_mailbox_recursively() {
        // Arrange
        // Act
        // Assert
        let store = temp_store();
        fan_out(&store, &sample_message("alice@example.com")).expect("fan-out should succeed");
        fan_out(&store, &sample_message("alice@example.com")).expect("fan-out should succeed");
        assert_eq!(
            store
                .list_messages("alice@example.com", ListMessagesParams::default())
                .expect("list should succeed")
                .messages
                .len(),
            2
        );

        store
            .delete_mailbox("alice@example.com")
            .expect("mailbox delete should succeed");

        let messages = store
            .list_messages("alice@example.com", ListMessagesParams::default())
            .expect("missing mailbox should return empty result");
        assert!(messages.messages.is_empty());

        let all = store
            .list_messages(ALL_MAILBOX, ListMessagesParams::default())
            .expect("list should succeed");
        assert_eq!(all.messages.len(), 2);
    }

    #[test]
    fn should_contain_untrusted_mail_paths() {
        // Arrange
        // Act
        // Assert
        let store = temp_store();

        store
            .store_message("../escape", "id", sample_message("alice@example.com"))
            .expect("mailbox text should be encoded into a contained directory");
        assert!(!store
            .root
            .parent()
            .expect("mail root should have a parent")
            .join("escape")
            .exists());
        assert_eq!(
            store
                .get_message("../escape", "id")
                .expect("encoded mailbox should remain addressable")
                .mailbox,
            "../escape"
        );

        let err = store
            .store_message(
                "alice@example.com",
                "../escape",
                sample_message("alice@example.com"),
            )
            .expect_err("path traversal in message id should be rejected");
        assert!(matches!(err, Error::InvalidRequest(_)));
    }
}
