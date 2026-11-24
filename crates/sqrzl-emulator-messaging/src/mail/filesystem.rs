//! Filesystem-backed [`MailStore`] implementation.
//!
//! Layout mirrors `FilesystemStorage`'s small-metadata-as-JSON convention: each
//! mailbox is a hashed directory under `{blobs_path}/_mail/` with a sidecar that
//! preserves its original address. Each stored message is one `{message_id}.json`
//! file holding a serialized [`StoredMessage`] and, where available, one
//! `{message_id}.raw` file with the full MIME payload.
//! Writes go through a temp-file-then-rename so a crash mid-write can't leave a
//! corrupt file.

use crate::capture::{self, RepeatabilityRecord, TRANSACTION_METADATA};
use crate::error::{Error, Result};
use crate::mail::model::{
    DeliveryStatus, ListMessagesParams, ListMessagesResult, MailboxInfo, Message, StoredMessage,
};
use crate::mail::ses_events::{SesEventDestination, SesEventRecord};
use crate::mail::{MailStore, ALL_MAILBOX};
use chrono::Utc;
use sha2::{Digest, Sha256};
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

mod event_index;
mod summary;

pub struct FilesystemMailStore {
    root: PathBuf,
    capture_lock: Mutex<()>,
    event_index: std::sync::OnceLock<std::sync::Arc<event_index::EventIndex>>,
}

type CaptureFiles = Vec<(PathBuf, Vec<u8>)>;

const RAW_SUFFIX: &str = ".raw";
const MAILBOX_METADATA: &str = ".mailbox";
const MAILBOX_DIRECTORY_PREFIX: &str = "mbx-";
// Dot-prefixed so mailbox listing never treats them as recipient mailboxes.
const SES_EVENTS_DIR: &str = ".ses-events";
const SES_DESTINATIONS_DIR: &str = ".ses-event-destinations";

impl FilesystemMailStore {
    /// Opens (creating if needed) the `_mail` subtree under `blobs_path`.
    ///
    /// # Errors
    ///
    /// Returns an error when the underlying emulator operation fails.
    pub fn open(blobs_path: impl AsRef<Path>) -> Result<Self> {
        let root = blobs_path.as_ref().join("_mail");
        fs::create_dir_all(&root).map_err(|err| io_err(&err))?;
        capture::recover(&root)?;
        let store = Self {
            root,
            capture_lock: Mutex::new(()),
            event_index: std::sync::OnceLock::new(),
        };
        store.upgrade_summary_index()?;
        if store
            .root
            .join(".ses-events-v1.redb")
            .try_exists()
            .map_err(|error| io_err(&error))?
            || store
                .root
                .join(SES_EVENTS_DIR)
                .try_exists()
                .map_err(|error| io_err(&error))?
        {
            store.event_index()?;
        }
        Ok(store)
    }

    fn ses_destination_path(&self, configuration_set: &str) -> PathBuf {
        self.root.join(SES_DESTINATIONS_DIR).join(format!(
            "{}.json",
            hex::encode(configuration_set.as_bytes())
        ))
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

    fn prepare_copy(
        &self,
        mailbox: &str,
        message_id: &str,
        mut message: Message,
        transaction_id: &str,
    ) -> Result<(StoredMessage, CaptureFiles)> {
        self.ensure_mailbox_dir(mailbox)?;
        let path = self.message_path(mailbox, message_id)?;
        message.provider_metadata.insert(
            TRANSACTION_METADATA.to_string(),
            serde_json::Value::String(transaction_id.to_string()),
        );
        let received_at = Utc::now();
        let stored = StoredMessage {
            message_id: message_id.to_string(),
            mailbox: mailbox.to_string(),
            message,
            delivery_status: DeliveryStatus::accepted(received_at),
            received_at,
        };
        let mut files = vec![(
            path,
            serde_json::to_vec(&stored).map_err(|error| Error::InternalError(error.to_string()))?,
        )];
        if let Some(raw_mime) = &stored.message.raw_mime {
            files.push((
                self.raw_message_path(mailbox, message_id)?,
                raw_mime.clone(),
            ));
        }
        files.extend(self.summary_files(&stored)?);
        Ok((stored, files))
    }

    fn list_messages_snapshot(
        &self,
        mailbox: &str,
        params: &ListMessagesParams,
        committed: &std::collections::HashSet<String>,
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
            #[cfg(test)]
            capture::test_listing_phase();
            let mut stored = match Self::read_stored(&path) {
                Ok(stored) => stored,
                // Rollback and admin deletion may remove an enumerated entry
                // before it is opened. It contributes no committed message.
                Err(Error::MessageNotFound) => continue,
                Err(error) => return Err(error),
            };
            let id = stored
                .message
                .provider_metadata
                .get(TRANSACTION_METADATA)
                .and_then(serde_json::Value::as_str)
                .unwrap_or("");
            if id.is_empty() || committed.contains(id) {
                self.overlay_summary(&mut stored)?;
                messages.push(stored);
            }
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

    fn visible(&self, message: &StoredMessage) -> bool {
        let id = message
            .message
            .provider_metadata
            .get(TRANSACTION_METADATA)
            .and_then(serde_json::Value::as_str)
            .unwrap_or("");
        capture::is_committed(&self.root, id)
    }

    fn read_stored(path: &Path) -> Result<StoredMessage> {
        let data = fs::read(path).map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                Error::MessageNotFound
            } else {
                io_err(&error)
            }
        })?;
        serde_json::from_slice(&data).map_err(|e| Error::InternalError(e.to_string()))
    }

    fn read_raw(path: &Path) -> Option<Vec<u8>> {
        fs::read(path).ok()
    }
}

impl MailStore for FilesystemMailStore {
    fn check_ready(&self) -> Result<()> {
        std::fs::read_dir(&self.root).map_err(|error| Error::InternalError(error.to_string()))?;
        if let Some(index) = self.event_index.get() {
            if !self.root.join(".ses-events-v1.redb").is_file() {
                return Err(Error::InternalError(
                    "SES event index file is missing".into(),
                ));
            }
            index.check_ready()?;
        }
        Ok(())
    }

    fn capture_batch(
        &self,
        messages: &[(String, Message)],
        records: &[RepeatabilityRecord],
    ) -> Result<Option<Vec<Vec<StoredMessage>>>> {
        super::budget::check_messages(messages.iter().map(|(_, message)| message), records)?;
        let _guard = self
            .capture_lock
            .lock()
            .map_err(|_| Error::InternalError("mail capture lock poisoned".to_string()))?;
        let id = capture::new_transaction_id();
        let mut files = Vec::new();
        let mut result = Vec::with_capacity(messages.len());
        for (message_id, message) in messages {
            let mut targets = message
                .recipients()
                .into_iter()
                .map(crate::mail::Address::mailbox_key)
                .collect::<Vec<_>>();
            targets.sort();
            targets.dedup();
            if targets.is_empty() {
                return Err(Error::InvalidRequest(
                    "message has no recipients".to_string(),
                ));
            }
            targets.insert(0, ALL_MAILBOX.to_string());
            let mut copies = Vec::new();
            for mailbox in targets {
                let (stored, copy_files) =
                    self.prepare_copy(&mailbox, message_id, message.clone(), &id)?;
                files.extend(copy_files);
                if mailbox != ALL_MAILBOX {
                    copies.push(stored);
                }
            }
            result.push(copies);
        }
        for record in records {
            let mut record = record.clone();
            record.transaction_id.clone_from(&id);
            files.push(capture::record_file(&self.root, &record)?);
        }
        capture::commit(&self.root, &id, &files)?;
        Ok(Some(result))
    }

    fn get_repeatability_record(&self, key: &str) -> Result<Option<RepeatabilityRecord>> {
        capture::load_record(&self.root, key)
    }

    fn store_message(
        &self,
        mailbox: &str,
        message_id: &str,
        message: Message,
    ) -> Result<StoredMessage> {
        super::budget::check_single_copy(&message, mailbox, message_id)?;
        let _guard = self
            .capture_lock
            .lock()
            .map_err(|_| Error::InternalError("mail capture lock poisoned".to_string()))?;
        let id = capture::new_transaction_id();
        let (stored, files) = self.prepare_copy(mailbox, message_id, message, &id)?;
        capture::commit(&self.root, &id, &files)?;
        Ok(stored)
    }

    fn get_message(&self, mailbox: &str, message_id: &str) -> Result<StoredMessage> {
        let path = self.message_path(mailbox, message_id)?;
        let mut stored = Self::read_stored(&path)?;
        if !self.visible(&stored) {
            return Err(Error::MessageNotFound);
        }
        self.overlay_summary(&mut stored)?;
        let raw_path = self.raw_message_path(mailbox, message_id)?;
        if let Some(raw_mime) = Self::read_raw(&raw_path) {
            stored.message.raw_mime = Some(raw_mime);
        }
        Ok(stored)
    }

    fn get_message_case_insensitive(
        &self,
        mailbox: &str,
        message_id: &str,
    ) -> Result<StoredMessage> {
        match self.get_message(mailbox, message_id) {
            Err(Error::MessageNotFound) => {}
            result => return result,
        }
        // Match filenames before loading content so operation lookup does not
        // materialize the mailbox's captured bodies and attachments.
        let entries = fs::read_dir(self.mailbox_dir(mailbox)?).map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                Error::MessageNotFound
            } else {
                io_err(&error)
            }
        })?;
        for entry in entries {
            let name = entry.map_err(|error| io_err(&error))?.file_name();
            let Some(candidate) = name.to_str().and_then(|name| name.strip_suffix(".json")) else {
                continue;
            };
            if candidate.eq_ignore_ascii_case(message_id) {
                match self.get_message(mailbox, candidate) {
                    Err(Error::MessageNotFound) => {}
                    result => return result,
                }
            }
        }
        Err(Error::MessageNotFound)
    }

    fn list_message_summaries(
        &self,
        mailbox: &str,
        params: ListMessagesParams,
    ) -> Result<ListMessagesResult> {
        let _guard = self
            .capture_lock
            .lock()
            .map_err(|_| Error::InternalError("mail capture lock poisoned".into()))?;
        self.summary_page(mailbox, &params)
    }
    fn message_cursor(&self, message: &StoredMessage) -> Result<String> {
        Self::summary_cursor(message)
    }
    fn list_mailbox_page(
        &self,
        after: Option<&str>,
        limit: usize,
        search: Option<&str>,
    ) -> Result<super::model::ListMailboxesResult> {
        let _guard = self
            .capture_lock
            .lock()
            .map_err(|_| Error::InternalError("mail capture lock poisoned".into()))?;
        self.mailbox_page(after, limit, search)
    }

    fn list_messages(
        &self,
        mailbox: &str,
        params: ListMessagesParams,
    ) -> Result<ListMessagesResult> {
        self.list_messages_snapshot(mailbox, &params, &capture::committed_snapshot(&self.root)?)
    }

    fn delete_message(&self, mailbox: &str, message_id: &str) -> Result<()> {
        let _guard = self
            .capture_lock
            .lock()
            .map_err(|_| Error::InternalError("mail capture lock poisoned".to_string()))?;
        let path = self.message_path(mailbox, message_id)?;
        let raw_path = self.raw_message_path(mailbox, message_id)?;
        match fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(err) => Err(io_err(&err)),
        }?;
        self.remove_summary(mailbox, message_id)?;
        match fs::remove_file(&raw_path) {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(err) => Err(io_err(&err)),
        }
    }

    fn delete_mailbox(&self, mailbox: &str) -> Result<()> {
        let _guard = self
            .capture_lock
            .lock()
            .map_err(|_| Error::InternalError("mail capture lock poisoned".to_string()))?;
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
        let _guard = self
            .capture_lock
            .lock()
            .map_err(|_| Error::InternalError("mail capture lock poisoned".to_string()))?;
        self.update_summary_status(mailbox, message_id, status)
    }

    fn list_mailboxes(&self) -> Result<Vec<MailboxInfo>> {
        let mut result = Vec::new();
        let mut marker = None;
        loop {
            let page = self.list_mailbox_page(marker.as_deref(), 500, None)?;
            result.extend(page.mailboxes);
            let Some(next) = page.next_marker else {
                break;
            };
            marker = Some(next);
        }
        Ok(result)
    }

    fn ensure_mailbox(&self, mailbox: &str) -> Result<()> {
        let _guard = self
            .capture_lock
            .lock()
            .map_err(|_| Error::InternalError("mail capture lock poisoned".to_string()))?;
        self.ensure_mailbox_dir(mailbox)?;
        Ok(())
    }

    fn put_ses_event_destination(
        &self,
        destination: &SesEventDestination,
    ) -> Result<SesEventDestination> {
        let _guard = self
            .capture_lock
            .lock()
            .map_err(|_| Error::InternalError("mail capture lock poisoned".to_string()))?;
        let path = self.ses_destination_path(&destination.configuration_set);
        let mut stored = destination.clone();
        if let Ok(previous) = fs::read(&path)
            .map_err(|err| io_err(&err))
            .and_then(|bytes| {
                serde_json::from_slice::<SesEventDestination>(&bytes)
                    .map_err(|err| Error::InternalError(err.to_string()))
            })
        {
            stored.created_at = previous.created_at;
        }
        let bytes =
            serde_json::to_vec(&stored).map_err(|err| Error::InternalError(err.to_string()))?;
        write_atomic(&path, &bytes)?;
        Ok(stored)
    }

    fn get_ses_event_destination(&self, configuration_set: &str) -> Result<SesEventDestination> {
        let bytes = fs::read(self.ses_destination_path(configuration_set)).map_err(|err| {
            if err.kind() == std::io::ErrorKind::NotFound {
                Error::MessageNotFound
            } else {
                io_err(&err)
            }
        })?;
        serde_json::from_slice(&bytes).map_err(|err| Error::InternalError(err.to_string()))
    }

    fn delete_ses_event_destination(&self, configuration_set: &str) -> Result<()> {
        let _guard = self
            .capture_lock
            .lock()
            .map_err(|_| Error::InternalError("mail capture lock poisoned".to_string()))?;
        match fs::remove_file(self.ses_destination_path(configuration_set)) {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(err) => Err(io_err(&err)),
        }
    }

    fn claim_ses_event(&self, event: &SesEventRecord) -> Result<Option<SesEventRecord>> {
        self.event_index()?.put(event, true)
    }
    fn get_ses_event(&self, event_id: &str) -> Result<SesEventRecord> {
        validate_segment(event_id)?;
        self.event_index()?.get(event_id)
    }
    fn record_ses_event(&self, event: &SesEventRecord) -> Result<()> {
        self.event_index()?.put(event, false)?;
        Ok(())
    }
    fn list_ses_event_page(
        &self,
        message_id: Option<&str>,
        after: Option<&str>,
        limit: usize,
    ) -> Result<crate::mail::ses_events::SesEventPage> {
        self.event_index()?.page(message_id, after, limit)
    }
    fn list_ses_events(&self, message_id: Option<&str>) -> Result<Vec<SesEventRecord>> {
        let index = self.event_index()?;
        let mut events = Vec::new();
        let mut cursor = None;
        loop {
            let page = index.page(message_id, cursor.as_deref(), 500)?;
            events.extend(page.items);
            let Some(next) = page.next else {
                break;
            };
            cursor = Some(next);
        }
        Ok(events)
    }
}

fn ses_event_records(root: &Path) -> Result<impl Iterator<Item = Result<SesEventRecord>>> {
    let entries = match fs::read_dir(root.join(SES_EVENTS_DIR)) {
        Ok(entries) => Some(entries),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(io_err(&error)),
    };
    Ok(entries.into_iter().flatten().filter_map(|entry| {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => return Some(Err(io_err(&error))),
        };
        if entry.path().extension().and_then(std::ffi::OsStr::to_str) != Some("json") {
            return None;
        }
        let bytes = match fs::read(entry.path()) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return None,
            Err(error) => return Some(Err(io_err(&error))),
        };
        Some(
            serde_json::from_slice(&bytes).map_err(|error| Error::InternalError(error.to_string())),
        )
    }))
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

fn ses_event_bytes(event: &SesEventRecord) -> Result<Vec<u8>> {
    let mut value =
        serde_json::to_value(event).map_err(|error| Error::InternalError(error.to_string()))?;
    if let Some(fingerprint) = &event.request_fingerprint {
        value["__sqrzl_request_fingerprint"] = serde_json::json!(fingerprint);
    }
    serde_json::to_vec(&value).map_err(|error| Error::InternalError(error.to_string()))
}

fn write_atomic(path: &Path, data: &[u8]) -> Result<()> {
    write_atomic_mode(path, data, true).map(|_| ())
}

fn write_atomic_mode(path: &Path, data: &[u8], replace: bool) -> Result<bool> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|err| io_err(&err))?;
    }
    let mut tmp_name = path
        .file_name()
        .ok_or_else(|| Error::InternalError("message path has no file name".to_string()))?
        .to_os_string();
    tmp_name.push(format!(".{}.tmp", uuid::Uuid::new_v4()));
    let tmp_path = path.with_file_name(tmp_name);
    let result = (|| {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp_path)
            .map_err(|error| io_err(&error))?;
        file.write_all(data).map_err(|error| io_err(&error))?;
        file.sync_all().map_err(|error| io_err(&error))?;
        if replace {
            fs::rename(&tmp_path, path).map_err(|error| io_err(&error))?;
        } else {
            // Link a completely written inode without replacing an existing claim.
            // This arbitrates ownership across independent store handles as well.
            match fs::hard_link(&tmp_path, path) {
                Ok(()) => {
                    fs::remove_file(&tmp_path).map_err(|error| io_err(&error))?;
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    fs::remove_file(&tmp_path).map_err(|error| io_err(&error))?;
                    return Ok(false);
                }
                Err(error) => return Err(io_err(&error)),
            }
        }
        if let Some(parent) = path.parent() {
            fs::File::open(parent)
                .and_then(|directory| directory.sync_all())
                .map_err(|error| io_err(&error))?;
        }
        Ok(true)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp_path);
    }
    result
}

fn io_err(err: &std::io::Error) -> Error {
    Error::InternalError(err.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mail::model::{Address, SourceProtocol};
    use crate::mail::{fan_out, generate_message_id, DeliveryState};

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
            reply_to: Vec::new(),
            subject: "hello".to_string(),
            headers: std::collections::HashMap::new(),
            body_text: Some("hi there".to_string()),
            body_html: None,
            attachments: Vec::new(),
            user_engagement_tracking_disabled: None,
            provider_metadata: std::collections::HashMap::new(),
            raw_mime: None,
            thread_id: None,
        }
    }

    #[test]
    fn should_reject_projected_mail_buffers_before_creating_mailboxes_or_journals() {
        // Arrange a valid message whose recipient copies exceed the aggregate budget.
        let store = temp_store();
        let mut message = sample_message("alice@example.com");
        message.body_text = Some("x".repeat(1536 * 1024));
        message.to = (0..50)
            .map(|n| crate::mail::Address {
                email: format!("recipient{n}@example.com"),
                name: None,
            })
            .collect();
        // Act through the filesystem batch entry point.
        let result = store.capture_batch(&[("oversized".to_string(), message)], &[]);

        // Assert rejection precedes any mailbox or transaction entry.
        assert!(matches!(result, Err(Error::CaptureTooLarge)));
        assert_eq!(fs::read_dir(&store.root).unwrap().count(), 1);
        assert_eq!(
            fs::read_dir(store.root.join(".capture-transactions"))
                .unwrap()
                .count(),
            0
        );
        let mut raw = sample_message("raw@example.com");
        raw.raw_mime = Some(vec![0; 12 * 1024 * 1024]);
        assert!(matches!(
            store.store_message("raw@example.com", "raw", raw),
            Err(Error::CaptureTooLarge)
        ));
        assert_eq!(fs::read_dir(&store.root).unwrap().count(), 1);
        assert_eq!(
            fs::read_dir(store.root.join(".capture-transactions"))
                .unwrap()
                .count(),
            0
        );
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
    fn should_resolve_message_id_case_without_loading_unrelated_capture_content() {
        // Arrange a legacy GUID spelling and unrelated unreadable JSON content.
        let store = temp_store();
        let message_id = "F9168C5E-CEB2-4FAA-B6BF-329BF39FA1E4";
        store
            .store_message(ALL_MAILBOX, message_id, sample_message("alice@example.com"))
            .unwrap();
        fs::write(
            store.message_path(ALL_MAILBOX, "unrelated").unwrap(),
            b"malformed unrelated capture",
        )
        .unwrap();
        // Act
        let fetched = store
            .get_message_case_insensitive(ALL_MAILBOX, &message_id.to_ascii_lowercase())
            .unwrap();
        // Assert
        assert_eq!(fetched.message_id, message_id);
        assert!(matches!(
            store.get_message_case_insensitive(ALL_MAILBOX, "missing"),
            Err(Error::MessageNotFound)
        ));
        assert!(matches!(
            store.get_message_case_insensitive(ALL_MAILBOX, "../escape"),
            Err(Error::InvalidRequest(_))
        ));
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
        assert!(store.list_mailboxes().unwrap().is_empty());
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
    #[test]
    fn should_serialize_concurrent_ses_destination_updates() {
        // Arrange
        let store = std::sync::Arc::new(temp_store());
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(16));
        let mut workers = Vec::new();

        // Act
        for worker in 0..16 {
            let store = store.clone();
            let barrier = barrier.clone();
            workers.push(std::thread::spawn(move || {
                barrier.wait();
                (0..16)
                    .map(|index| {
                        let now = Utc::now();
                        store
                            .put_ses_event_destination(&SesEventDestination {
                                configuration_set: "concurrent".to_string(),
                                callback_url: format!("http://localhost/{worker}/{index}"),
                                event_types: Vec::new(),
                                created_at: now,
                                updated_at: now,
                            })
                            .is_ok()
                    })
                    .collect::<Vec<_>>()
            }));
        }
        let outcomes = workers
            .into_iter()
            .flat_map(|worker| worker.join().unwrap())
            .collect::<Vec<_>>();

        // Assert
        assert!(outcomes.into_iter().all(|succeeded| succeeded));
        assert!(store.get_ses_event_destination("concurrent").is_ok());
    }

    #[test]
    fn should_surface_ses_event_directory_failures() {
        // Arrange
        let store = temp_store();
        fs::write(store.root.join(SES_EVENTS_DIR), b"not a directory").unwrap();

        // Act
        let result = store.list_ses_events(None);

        // Assert
        assert!(result.is_err());
    }
    #[test]
    fn should_claim_one_ses_event_across_independent_store_handles() {
        // Arrange
        let root = std::env::temp_dir().join(format!("sqrzl-ses-claims-{}", uuid::Uuid::new_v4()));
        let stores = (0..16)
            .map(|_| FilesystemMailStore::open(&root).unwrap())
            .collect::<Vec<_>>();
        let event: SesEventRecord = serde_json::from_value(serde_json::json!({
            "event_id": "ses-event-shared", "request_id": "shared", "state": "pending",
            "message_id": "message", "event_type": "Delivery", "record": {}, "notification": {}, "created_at": Utc::now(),
        })).unwrap();
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(stores.len()));

        // Act
        let workers = stores
            .into_iter()
            .map(|store| {
                let barrier = barrier.clone();
                let event = event.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    store.claim_ses_event(&event).unwrap().is_none()
                })
            })
            .collect::<Vec<_>>();
        let owners = workers
            .into_iter()
            .map(|worker| usize::from(worker.join().unwrap()))
            .sum::<usize>();

        // Assert
        assert_eq!(owners, 1, "more than one store claimed callback delivery");
    }
    #[test]
    fn should_continue_ses_event_pages_given_deleted_boundary_record() {
        // Arrange
        let root = std::env::temp_dir().join(format!("sqrzl-ses-page-{}", uuid::Uuid::new_v4()));
        let store = FilesystemMailStore::open(&root).unwrap();
        let created = Utc::now();
        for index in 0..8 {
            let event: SesEventRecord = serde_json::from_value(serde_json::json!({
                "event_id": format!("event-{index}"), "message_id": "message", "event_type": "Delivery",
                "record": {}, "notification": {}, "created_at": created,
            })).unwrap();
            store.record_ses_event(&event).unwrap();
        }
        // Act
        let first = store.list_ses_event_page(Some("message"), None, 3).unwrap();
        store
            .event_index()
            .unwrap()
            .remove_for_test("event-2")
            .unwrap();
        let second = store
            .list_ses_event_page(Some("message"), first.next.as_deref(), 3)
            .unwrap();
        let last = store
            .list_ses_event_page(Some("message"), second.next.as_deref(), 3)
            .unwrap();
        // Assert
        assert_eq!(first.items.len(), 3);
        assert_eq!(second.items[0].event_id, "event-3");
        assert_eq!(last.items.len(), 2);
        assert!(last.next.is_none());
        assert!(store
            .list_ses_event_page(Some("other-message"), first.next.as_deref(), 3)
            .is_err());
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn should_reject_invalid_ses_event_page_parameters_before_reading_records() {
        // Arrange
        let records =
            || std::iter::once_with(|| panic!("records read before parameter validation"));
        // Act
        let invalid_cursor =
            crate::mail::ses_events::event_page(records(), None, Some("garbage"), 3);
        let invalid_limit = crate::mail::ses_events::event_page(records(), None, None, 501);
        // Assert
        assert!(matches!(
            invalid_cursor,
            Err(crate::Error::InvalidRequest(_))
        ));
        assert!(matches!(
            invalid_limit,
            Err(crate::Error::InvalidRequest(_))
        ));
    }
    #[test]
    fn should_list_message_summaries_without_reading_payload_files() {
        // Arrange
        let store = temp_store();
        let mailbox = "summary@example.com";
        let mut message = sample_message(mailbox);
        message.body_text = Some("large body".repeat(32_768));
        let stored = store
            .store_message(mailbox, "message-one", message)
            .unwrap();
        fs::write(
            store.message_path(mailbox, &stored.message_id).unwrap(),
            b"payload must not be read for metadata",
        )
        .unwrap();
        // Act
        let page = store.list_message_summaries(
            mailbox,
            ListMessagesParams {
                limit: Some(1),
                marker: None,
            },
        );
        let mailboxes = store.list_mailbox_page(None, 1, None);
        // Assert
        let page = page.unwrap();
        assert_eq!(page.messages[0].message.subject, "hello");
        assert!(page.messages[0].message.body_text.is_none());
        assert!(page.messages[0].message.attachments.is_empty());
        assert_eq!(mailboxes.unwrap().mailboxes[0].message_count, 1);
        assert!(store.get_message(mailbox, &stored.message_id).is_err());
    }

    #[test]
    fn should_continue_summary_pagination_after_the_boundary_message_is_deleted() {
        // Arrange
        let store = temp_store();
        let mailbox = "summary@example.com";
        for index in 0..4 {
            store
                .store_message(
                    mailbox,
                    &format!("message-{index}"),
                    sample_message(mailbox),
                )
                .unwrap();
        }
        let first = store
            .list_message_summaries(
                mailbox,
                ListMessagesParams {
                    limit: Some(2),
                    marker: None,
                },
            )
            .unwrap();
        // Act
        store
            .delete_message(mailbox, &first.messages.last().unwrap().message_id)
            .unwrap();
        let second = store
            .list_message_summaries(
                mailbox,
                ListMessagesParams {
                    limit: Some(2),
                    marker: first.next_marker,
                },
            )
            .unwrap();
        // Assert
        assert_eq!(second.messages[0].message_id, "message-2");
        assert_eq!(second.messages.len(), 2);
        assert!(second.next_marker.is_none());
    }

    #[test]
    fn should_persist_delivery_metadata_without_rewriting_captured_payloads() {
        // Arrange
        let store = temp_store();
        let mailbox = "summary@example.com";
        let root = store.root.parent().unwrap().to_path_buf();
        store
            .store_message(mailbox, "immutable-body", sample_message(mailbox))
            .unwrap();
        let path = store.message_path(mailbox, "immutable-body").unwrap();
        let original = fs::read(&path).unwrap();
        // Act
        store
            .update_delivery_status(
                mailbox,
                "immutable-body",
                DeliveryStatus {
                    state: DeliveryState::Delivered,
                    detail: Some("accepted".into()),
                    updated_at: Utc::now(),
                },
            )
            .unwrap();
        drop(store);
        let reopened = FilesystemMailStore::open(root).unwrap();
        let detail = reopened.get_message(mailbox, "immutable-body").unwrap();
        let page = reopened
            .list_message_summaries(mailbox, ListMessagesParams::default())
            .unwrap();
        // Assert
        assert_eq!(original, fs::read(path).unwrap());
        assert!(matches!(
            detail.delivery_status.state,
            DeliveryState::Delivered
        ));
        assert!(matches!(
            page.messages[0].delivery_status.state,
            DeliveryState::Delivered
        ));
    }

    #[test]
    fn should_upgrade_legacy_captures_into_payload_free_summary_pages() {
        // Arrange
        let store = temp_store();
        let mailbox = "legacy@example.com";
        let root = store.root.parent().unwrap().to_path_buf();
        store
            .store_message(mailbox, "legacy", sample_message(mailbox))
            .unwrap();
        let mailbox_dir = store.mailbox_dir(mailbox).unwrap();
        fs::remove_dir_all(mailbox_dir.join(".summaries")).unwrap();
        fs::remove_dir_all(mailbox_dir.join(".summary-index")).unwrap();
        drop(store);
        // Act
        let reopened = FilesystemMailStore::open(root).unwrap();
        fs::write(
            reopened.message_path(mailbox, "legacy").unwrap(),
            b"large legacy body is no longer read by listings",
        )
        .unwrap();
        let page = reopened
            .list_message_summaries(mailbox, ListMessagesParams::default())
            .unwrap();
        // Assert
        assert_eq!(page.messages[0].message_id, "legacy");
        assert!(page.messages[0].message.body_text.is_none());
        assert_eq!(
            reopened
                .list_mailbox_page(None, 50, None)
                .unwrap()
                .mailboxes[0]
                .message_count,
            1
        );
    }

    #[test]
    fn should_read_only_requested_ses_event_payloads_given_an_unrelated_archive() {
        // Arrange
        let store = temp_store();
        let created = Utc::now();
        for index in 0..100 {
            let event: SesEventRecord = serde_json::from_value(serde_json::json!({
                "event_id": format!("event-{index:04}"), "message_id": if index == 42 { "target" } else { "unrelated" },
                "event_type":"Delivery", "created_at":created, "record":{"padding":"x".repeat(4096)}, "notification":{},
            })).unwrap();
            store.record_ses_event(&event).unwrap();
        }
        crate::mail::ses_events::take_event_decode_count();
        // Act
        let result = store.list_ses_event_page(Some("target"), None, 1).unwrap();
        let reads = crate::mail::ses_events::take_event_decode_count();
        // Assert
        assert_eq!(result.items.len(), 1);
        assert!(
            reads <= 2,
            "a one-record page deserialized {reads} event payloads"
        );
    }
}
