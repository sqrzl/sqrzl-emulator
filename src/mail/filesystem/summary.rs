//! Transaction-published payload-free mailbox metadata and bounded page selection.
use super::{
    capture, fs, io_err, validate_segment, write_atomic, CaptureFiles, DeliveryStatus, Digest,
    Error, FilesystemMailStore, ListMessagesParams, ListMessagesResult, MailboxInfo, Path, PathBuf,
    Result, Sha256, StoredMessage, Utc, ALL_MAILBOX, MAILBOX_METADATA, TRANSACTION_METADATA,
};
use crate::mail::model::ListMailboxesResult;
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io::Read;

const SUMMARIES: &str = ".summaries";
const INDEX: &str = ".summary-index";
const INDEX_VERSION: &str = ".summary-index-v1";
const MAX_SUMMARY_BYTES: usize = 64 * 1024 * 1024;
const PAGE_BYTES: usize = 8 * 1024 * 1024;
#[derive(Serialize, Deserialize)]
struct Entry {
    message_id: String,
    received_at: chrono::DateTime<Utc>,
    transaction_id: String,
}
#[derive(Serialize, Deserialize)]
struct Cursor {
    mailbox: String,
    received_at: chrono::DateTime<Utc>,
    message_id: String,
}

fn read_json<T: DeserializeOwned>(path: &Path, limit: usize) -> Result<T> {
    let mut bytes = Vec::new();
    fs::File::open(path)
        .map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                Error::MessageNotFound
            } else {
                io_err(&error)
            }
        })?
        .take((limit + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|error| io_err(&error))?;
    if bytes.len() > limit {
        return Err(Error::InternalError(
            "mail metadata exceeds its read bound".into(),
        ));
    }
    serde_json::from_slice(&bytes).map_err(|error| Error::InternalError(error.to_string()))
}
fn json(value: &impl Serialize) -> Result<Vec<u8>> {
    serde_json::to_vec(value).map_err(|error| Error::InternalError(error.to_string()))
}
fn index_path(dir: &Path, stored: &StoredMessage) -> PathBuf {
    dir.join(INDEX).join(format!(
        "{}-{}.json",
        stored.received_at.format("%Y%m%dT%H%M%S%.9fZ"),
        hex::encode(Sha256::digest(stored.message_id.as_bytes()))
    ))
}
fn entry(stored: &StoredMessage) -> Entry {
    Entry {
        message_id: stored.message_id.clone(),
        received_at: stored.received_at,
        transaction_id: stored
            .message
            .provider_metadata
            .get(TRANSACTION_METADATA)
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .to_owned(),
    }
}
fn entries(dir: &Path) -> Result<impl Iterator<Item = Result<Entry>>> {
    let entries = match fs::read_dir(dir.join(INDEX)) {
        Ok(entries) => Some(entries),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(io_err(&error)),
    };
    Ok(entries.into_iter().flatten().filter_map(|entry| {
        let path = match entry {
            Ok(entry) => entry.path(),
            Err(error) => return Some(Err(io_err(&error))),
        };
        if path.extension().and_then(std::ffi::OsStr::to_str) != Some("json") {
            return None;
        }
        match read_json(&path, 4096) {
            Err(Error::MessageNotFound) => None,
            value => Some(value),
        }
    }))
}

impl FilesystemMailStore {
    pub(super) fn summary_files(&self, stored: &StoredMessage) -> Result<CaptureFiles> {
        validate_segment(&stored.message_id)?;
        let dir = self.mailbox_dir(&stored.mailbox)?;
        Ok(vec![
            (
                dir.join(SUMMARIES)
                    .join(format!("{}.json", stored.message_id)),
                json(&stored.metadata_only())?,
            ),
            (index_path(&dir, stored), json(&entry(stored))?),
        ])
    }
    pub(super) fn upgrade_summary_index(&self) -> Result<()> {
        if self.root.join(INDEX_VERSION).is_file() {
            return Ok(());
        }
        let mut populated = false;
        for mailbox in fs::read_dir(&self.root).map_err(|error| io_err(&error))? {
            let mailbox = mailbox.map_err(|error| io_err(&error))?;
            if !mailbox.path().is_dir() || mailbox.file_name().to_string_lossy().starts_with('.') {
                continue;
            }
            for file in fs::read_dir(mailbox.path()).map_err(|error| io_err(&error))? {
                let path = file.map_err(|error| io_err(&error))?.path();
                if path.extension().and_then(std::ffi::OsStr::to_str) != Some("json") {
                    continue;
                }
                populated = true;
                let summary_path =
                    mailbox
                        .path()
                        .join(SUMMARIES)
                        .join(path.file_name().ok_or_else(|| {
                            Error::InternalError("mail payload has no filename".into())
                        })?);
                let stored = match read_json(&summary_path, MAX_SUMMARY_BYTES) {
                    Ok(summary) => summary,
                    Err(Error::MessageNotFound) => Self::read_stored(&path)?,
                    Err(error) => return Err(error),
                };
                if !self.visible(&stored) {
                    continue;
                }
                for (path, bytes) in self.summary_files(&stored)? {
                    if let Some(parent) = path.parent() {
                        fs::create_dir_all(parent).map_err(|error| io_err(&error))?;
                    }
                    if !path.exists() {
                        write_atomic(&path, &bytes)?;
                    }
                }
            }
        }
        if populated {
            write_atomic(&self.root.join(INDEX_VERSION), b"1")?;
        }
        Ok(())
    }
    fn summary(&self, mailbox: &str, id: &str) -> Result<StoredMessage> {
        validate_segment(id)?;
        let dir = self.mailbox_dir(mailbox)?;
        let stored: StoredMessage = read_json(
            &dir.join(SUMMARIES).join(format!("{id}.json")),
            MAX_SUMMARY_BYTES,
        )?;
        if stored.message_id != id || stored.mailbox != mailbox {
            return Err(Error::InternalError(
                "mail summary identity mismatch".into(),
            ));
        }
        Ok(stored)
    }
    pub(super) fn overlay_summary(&self, stored: &mut StoredMessage) -> Result<()> {
        match self.summary(&stored.mailbox, &stored.message_id) {
            Ok(summary) => stored.delivery_status = summary.delivery_status,
            Err(Error::MessageNotFound)
                if !stored
                    .message
                    .provider_metadata
                    .contains_key(TRANSACTION_METADATA) => {}
            Err(error) => return Err(error),
        }
        Ok(())
    }
    fn visible_entry(&self, dir: &Path, entry: &Entry) -> Result<bool> {
        validate_segment(&entry.message_id)?;
        Ok(capture::is_committed(&self.root, &entry.transaction_id)
            && dir
                .join(format!("{}.json", entry.message_id))
                .try_exists()
                .map_err(|error| io_err(&error))?)
    }
    pub(super) fn summary_cursor(stored: &StoredMessage) -> Result<String> {
        serde_json::to_string(&Cursor {
            mailbox: stored.mailbox.clone(),
            received_at: stored.received_at,
            message_id: stored.message_id.clone(),
        })
        .map_err(|error| Error::InternalError(error.to_string()))
    }
    pub(super) fn summary_page(
        &self,
        mailbox: &str,
        params: &ListMessagesParams,
    ) -> Result<ListMessagesResult> {
        let limit = params.limit.unwrap_or(50);
        if !(1..=500).contains(&limit) {
            return Err(Error::InvalidRequest(
                "summary page limit must be between 1 and 500".into(),
            ));
        }
        let cursor = params
            .marker
            .as_deref()
            .map(serde_json::from_str::<Cursor>)
            .transpose()
            .map_err(|_| Error::InvalidRequest("invalid message cursor".into()))?;
        if cursor
            .as_ref()
            .is_some_and(|cursor| cursor.mailbox != mailbox)
        {
            return Err(Error::InvalidRequest(
                "message cursor does not match mailbox".into(),
            ));
        }
        let marker = cursor.map(|cursor| (cursor.received_at, cursor.message_id));
        let dir = self.mailbox_dir(mailbox)?;
        let mut candidates = BTreeMap::new();
        for record in entries(&dir)? {
            let record = record?;
            let key = (record.received_at, record.message_id.clone());
            if marker.as_ref().is_some_and(|marker| &key <= marker)
                || !self.visible_entry(&dir, &record)?
            {
                continue;
            }
            if candidates.len() == limit + 1 {
                if candidates
                    .last_key_value()
                    .is_some_and(|(last, _)| &key >= last)
                {
                    continue;
                }
                candidates.pop_last();
            }
            candidates.insert(key, record);
        }
        let mut messages = Vec::new();
        let mut bytes = 0;
        let mut more = false;
        for record in candidates.into_values() {
            let summary = self.summary(mailbox, &record.message_id)?;
            if summary.received_at != record.received_at
                || entry(&summary).transaction_id != record.transaction_id
            {
                return Err(Error::InternalError("mail summary/index mismatch".into()));
            }
            let size = json(&summary)?.len();
            if messages.len() >= limit || (!messages.is_empty() && bytes + size > PAGE_BYTES) {
                more = true;
                break;
            }
            bytes += size;
            messages.push(summary);
        }
        let next_marker = if more {
            messages.last().map(Self::summary_cursor).transpose()?
        } else {
            None
        };
        Ok(ListMessagesResult {
            messages,
            next_marker,
        })
    }
    fn mailbox_summary(&self, address: String, dir: &Path) -> Result<Option<MailboxInfo>> {
        let mut count = 0;
        let mut last = None;
        for record in entries(dir)? {
            let record = record?;
            if !self.visible_entry(dir, &record)? {
                continue;
            }
            count += 1;
            last = Some(
                last.map_or(record.received_at, |last: chrono::DateTime<Utc>| {
                    last.max(record.received_at)
                }),
            );
        }
        Ok((count > 0).then_some(MailboxInfo {
            address,
            message_count: count,
            last_received_at: last,
        }))
    }
    pub(super) fn mailbox_page(
        &self,
        after: Option<&str>,
        limit: usize,
        search: Option<&str>,
    ) -> Result<ListMailboxesResult> {
        if !(1..=500).contains(&limit) {
            return Err(Error::InvalidRequest(
                "mailbox page limit must be between 1 and 500".into(),
            ));
        }
        let mut selected = BTreeMap::new();
        for directory in fs::read_dir(&self.root).map_err(|error| io_err(&error))? {
            let directory = directory.map_err(|error| io_err(&error))?;
            if !directory.path().is_dir()
                || directory.file_name().to_string_lossy().starts_with('.')
            {
                continue;
            }
            let metadata = directory.path().join(MAILBOX_METADATA);
            let address = match fs::read_to_string(metadata) {
                Ok(address) => address,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    directory.file_name().to_string_lossy().into_owned()
                }
                Err(error) => return Err(io_err(&error)),
            };
            if address == ALL_MAILBOX
                || after.is_some_and(|marker| address.as_str() <= marker)
                || search.is_some_and(|search| !address.to_ascii_lowercase().contains(search))
            {
                continue;
            }
            let Some(summary) = self.mailbox_summary(address.clone(), &directory.path())? else {
                continue;
            };
            if selected.len() == limit + 1 {
                if selected
                    .last_key_value()
                    .is_some_and(|(last, _)| &address >= last)
                {
                    continue;
                }
                selected.pop_last();
            }
            selected.insert(address, summary);
        }
        let more = selected.len() > limit;
        if more {
            selected.pop_last();
        }
        let next_marker = more
            .then(|| {
                selected
                    .last_key_value()
                    .map(|(address, _)| address.clone())
            })
            .flatten();
        Ok(ListMailboxesResult {
            mailboxes: selected.into_values().collect(),
            next_marker,
        })
    }
    pub(super) fn update_summary_status(
        &self,
        mailbox: &str,
        id: &str,
        status: DeliveryStatus,
    ) -> Result<()> {
        let mut summary = self.summary(mailbox, id)?;
        if !self.visible(&summary) || !self.message_path(mailbox, id)?.is_file() {
            return Err(Error::MessageNotFound);
        }
        summary.delivery_status = status;
        write_atomic(
            &self
                .mailbox_dir(mailbox)?
                .join(SUMMARIES)
                .join(format!("{id}.json")),
            &json(&summary)?,
        )
    }
    pub(super) fn remove_summary(&self, mailbox: &str, id: &str) -> Result<()> {
        let summary = match self.summary(mailbox, id) {
            Ok(summary) => summary,
            Err(Error::MessageNotFound) => return Ok(()),
            Err(error) => return Err(error),
        };
        for path in [
            index_path(&self.mailbox_dir(mailbox)?, &summary),
            self.mailbox_dir(mailbox)?
                .join(SUMMARIES)
                .join(format!("{id}.json")),
        ] {
            match fs::remove_file(path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(io_err(&error)),
            }
        }
        Ok(())
    }
}
