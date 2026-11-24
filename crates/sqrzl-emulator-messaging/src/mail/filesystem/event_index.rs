//! Transactional SES intent ownership and secondary indexes with bounded page reads.
use super::{
    fs, io_err, ses_event_bytes, ses_event_records, validate_segment, Digest, Error,
    FilesystemMailStore, Mutex, Path, PathBuf, Result, SesEventRecord, Sha256, SES_EVENTS_DIR,
};
use crate::mail::ses_events::{EventCursor, SesEventPage};
use redb::{Database, ReadableDatabase, ReadableTable, TableDefinition};
use std::collections::HashMap;
use std::ops::Bound;
use std::sync::{Arc, OnceLock, Weak};

const RECORDS: TableDefinition<&str, &[u8]> = TableDefinition::new("events_v1");
const ORDER: TableDefinition<&str, &str> = TableDefinition::new("event_order_v1");
const MESSAGES: TableDefinition<&str, &str> = TableDefinition::new("message_order_v1");
const SCHEMA: TableDefinition<&str, u64> = TableDefinition::new("schema");
const MAX_RECORD_BYTES: usize = 64 * 1024 * 1024;
const PAGE_BYTES: usize = 8 * 1024 * 1024;
type Databases = Mutex<HashMap<PathBuf, Weak<EventIndex>>>;
static DATABASES: OnceLock<Databases> = OnceLock::new();

pub(super) struct EventIndex {
    database: Database,
}
fn error(error: impl std::fmt::Display) -> Error {
    Error::InternalError(format!("SES event index: {error}"))
}
fn order_key(event: &SesEventRecord) -> String {
    format!(
        "{}/{}",
        event.created_at.format("%Y%m%dT%H%M%S%.9fZ"),
        event.event_id
    )
}
fn message_prefix(message_id: &str) -> String {
    format!("{}/", hex::encode(Sha256::digest(message_id.as_bytes())))
}
fn message_key(event: &SesEventRecord) -> String {
    message_prefix(&event.message_id) + &order_key(event)
}

#[cfg(not(test))]
fn open_database(root: &Path) -> Result<Database> {
    Database::builder()
        .set_cache_size(8 * 1024 * 1024)
        .create(root.join(".ses-events-v1.redb"))
        .map_err(error)
}
#[cfg(test)]
fn open_database(root: &Path) -> Result<Database> {
    let file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(root.join(".ses-events-v1.redb"))
        .map_err(error)?;
    let backend = FailingBackend {
        inner: redb::backends::FileBackend::new(file).map_err(error)?,
        fail_path: root.join(".ses-writes-fail"),
    };
    Database::builder()
        .set_cache_size(8 * 1024 * 1024)
        .create_with_backend(backend)
        .map_err(error)
}
#[cfg(test)]
#[derive(Debug)]
struct FailingBackend {
    inner: redb::backends::FileBackend,
    fail_path: PathBuf,
}
#[cfg(test)]
impl FailingBackend {
    fn write_allowed(&self) -> std::io::Result<()> {
        if self.fail_path.is_file() {
            Err(std::io::Error::new(
                std::io::ErrorKind::StorageFull,
                "injected SES index persistence failure",
            ))
        } else {
            Ok(())
        }
    }
}
#[cfg(test)]
impl redb::StorageBackend for FailingBackend {
    fn len(&self) -> std::io::Result<u64> {
        self.inner.len()
    }
    fn read(&self, offset: u64, out: &mut [u8]) -> std::io::Result<()> {
        self.inner.read(offset, out)
    }
    fn set_len(&self, length: u64) -> std::io::Result<()> {
        self.write_allowed()?;
        self.inner.set_len(length)
    }
    fn sync_data(&self) -> std::io::Result<()> {
        self.write_allowed()?;
        self.inner.sync_data()
    }
    fn write(&self, offset: u64, bytes: &[u8]) -> std::io::Result<()> {
        self.write_allowed()?;
        self.inner.write(offset, bytes)
    }
    fn close(&self) -> std::io::Result<()> {
        self.inner.close()
    }
    fn try_lock_range(
        &self,
        start: Bound<u64>,
        end: Bound<u64>,
    ) -> std::result::Result<bool, redb::BackendError> {
        self.inner.try_lock_range(start, end)
    }
    fn try_lock_shared_range(
        &self,
        start: Bound<u64>,
        end: Bound<u64>,
    ) -> std::result::Result<bool, redb::BackendError> {
        self.inner.try_lock_shared_range(start, end)
    }
    fn lock_range(
        &self,
        start: Bound<u64>,
        end: Bound<u64>,
    ) -> std::result::Result<(), redb::BackendError> {
        self.inner.lock_range(start, end)
    }
    fn lock_shared_range(
        &self,
        start: Bound<u64>,
        end: Bound<u64>,
    ) -> std::result::Result<(), redb::BackendError> {
        self.inner.lock_shared_range(start, end)
    }
    fn unlock_range(
        &self,
        start: Bound<u64>,
        end: Bound<u64>,
    ) -> std::result::Result<(), redb::BackendError> {
        self.inner.unlock_range(start, end)
    }
    fn query_lock_range(
        &self,
        start: Bound<u64>,
        end: Bound<u64>,
    ) -> std::result::Result<bool, redb::BackendError> {
        self.inner.query_lock_range(start, end)
    }
}

impl FilesystemMailStore {
    pub(super) fn event_index(&self) -> Result<Arc<EventIndex>> {
        if let Some(index) = self.event_index.get() {
            return Ok(index.clone());
        }
        // A malformed legacy directory remains a visible persistence failure.
        match fs::read_dir(self.root.join(SES_EVENTS_DIR)) {
            Ok(_) => {}
            Err(failure) if failure.kind() == std::io::ErrorKind::NotFound => {}
            Err(failure) => return Err(io_err(&failure)),
        }
        let root = fs::canonicalize(&self.root).map_err(|failure| io_err(&failure))?;
        let mut databases = DATABASES
            .get_or_init(Mutex::default)
            .lock()
            .map_err(error)?;
        databases.retain(|_, index| index.strong_count() != 0);
        let index = if let Some(index) = databases.get(&root).and_then(Weak::upgrade) {
            index
        } else {
            let database = open_database(&root)?;
            let index = Arc::new(EventIndex { database });
            index.initialize(&root)?;
            fs::File::open(&root)
                .and_then(|file| file.sync_all())
                .map_err(|failure| io_err(&failure))?;
            databases.insert(root, Arc::downgrade(&index));
            index
        };
        let _ = self.event_index.set(index.clone());
        Ok(index)
    }
}

impl EventIndex {
    fn initialize(&self, root: &Path) -> Result<()> {
        let write = self.database.begin_write().map_err(error)?;
        let imported = {
            write.open_table(RECORDS).map_err(error)?;
            write.open_table(ORDER).map_err(error)?;
            write.open_table(MESSAGES).map_err(error)?;
            let schema = write.open_table(SCHEMA).map_err(error)?;
            let imported = schema
                .get("legacy_import_complete")
                .map_err(error)?
                .is_some();
            imported
        };
        write.commit().map_err(error)?;
        if imported {
            return Ok(());
        }
        // Legacy import is resumable and streams one payload at a time. Existing
        // database records win after an interrupted import, including completed callbacks.
        for event in ses_event_records(root)? {
            self.put(&event?, true)?;
        }
        let write = self.database.begin_write().map_err(error)?;
        {
            write
                .open_table(SCHEMA)
                .map_err(error)?
                .insert("legacy_import_complete", 1)
                .map_err(error)?;
        }
        write.commit().map_err(error)
    }
    pub(super) fn check_ready(&self) -> Result<()> {
        let read = self.database.begin_read().map_err(error)?;
        read.open_table(RECORDS).map_err(error)?;
        read.open_table(ORDER).map_err(error)?;
        read.open_table(MESSAGES).map_err(error)?;
        Ok(())
    }
    pub(super) fn get(&self, id: &str) -> Result<SesEventRecord> {
        let read = self.database.begin_read().map_err(error)?;
        let table = read.open_table(RECORDS).map_err(error)?;
        let bytes = table
            .get(id)
            .map_err(error)?
            .ok_or(Error::MessageNotFound)?;
        serde_json::from_slice(bytes.value()).map_err(error)
    }
    pub(super) fn put(
        &self,
        event: &SesEventRecord,
        create_only: bool,
    ) -> Result<Option<SesEventRecord>> {
        validate_segment(&event.event_id)?;
        let bytes = ses_event_bytes(event)?;
        if bytes.len() > MAX_RECORD_BYTES {
            return Err(Error::CaptureTooLarge);
        }
        let mut write = self.database.begin_write().map_err(error)?;
        write
            .set_durability(redb::Durability::Immediate)
            .map_err(error)?;
        let prior: Option<SesEventRecord> = {
            let table = write.open_table(RECORDS).map_err(error)?;
            let prior = table
                .get(event.event_id.as_str())
                .map_err(error)?
                .map(|bytes| serde_json::from_slice(bytes.value()).map_err(error))
                .transpose()?;
            prior
        };
        if create_only && prior.is_some() {
            return Ok(prior);
        }
        {
            let mut records = write.open_table(RECORDS).map_err(error)?;
            records
                .insert(event.event_id.as_str(), bytes.as_slice())
                .map_err(error)?;
            let mut order = write.open_table(ORDER).map_err(error)?;
            let mut messages = write.open_table(MESSAGES).map_err(error)?;
            if let Some(prior) = prior {
                order.remove(order_key(&prior).as_str()).map_err(error)?;
                messages
                    .remove(message_key(&prior).as_str())
                    .map_err(error)?;
            }
            order
                .insert(order_key(event).as_str(), event.event_id.as_str())
                .map_err(error)?;
            messages
                .insert(message_key(event).as_str(), event.event_id.as_str())
                .map_err(error)?;
        }
        #[cfg(test)]
        crash_point("before_commit");
        write.commit().map_err(error)?;
        #[cfg(test)]
        crash_point("after_commit");
        Ok(None)
    }
    pub(super) fn page(
        &self,
        message_id: Option<&str>,
        after: Option<&str>,
        limit: usize,
    ) -> Result<SesEventPage> {
        if !(1..=500).contains(&limit) {
            return Err(Error::InvalidRequest(
                "limit must be between 1 and 500".into(),
            ));
        }
        let cursor = after
            .map(serde_json::from_str::<EventCursor>)
            .transpose()
            .map_err(|_| Error::InvalidRequest("invalid SES event cursor".into()))?;
        if cursor
            .as_ref()
            .is_some_and(|cursor| cursor.message_id.as_deref() != message_id)
        {
            return Err(Error::InvalidRequest(
                "SES event cursor does not match message filter".into(),
            ));
        }
        let prefix = message_id.map(message_prefix).unwrap_or_default();
        let start = cursor.as_ref().map_or_else(
            || prefix.clone(),
            |cursor| {
                format!(
                    "{}{}/{}",
                    prefix,
                    cursor.created_at.format("%Y%m%dT%H%M%S%.9fZ"),
                    cursor.event_id
                )
            },
        );
        let end = format!("{prefix}~");
        let lower = if cursor.is_some() {
            Bound::Excluded(start.as_str())
        } else {
            Bound::Included(start.as_str())
        };
        let upper = if message_id.is_some() {
            Bound::Excluded(end.as_str())
        } else {
            Bound::Unbounded
        };
        let read = self.database.begin_read().map_err(error)?;
        let index = read
            .open_table(if message_id.is_some() {
                MESSAGES
            } else {
                ORDER
            })
            .map_err(error)?;
        let records = read.open_table(RECORDS).map_err(error)?;
        let mut items = Vec::new();
        let mut bytes = 0;
        let mut more = false;
        for row in index
            .range::<&str>((lower, upper))
            .map_err(error)?
            .take(limit + 1)
        {
            let (_, id) = row.map_err(error)?;
            if items.len() == limit {
                more = true;
                break;
            }
            let payload = records.get(id.value()).map_err(error)?.ok_or_else(|| {
                Error::InternalError("SES index references a missing event".into())
            })?;
            if !items.is_empty() && bytes + payload.value().len() > PAGE_BYTES {
                more = true;
                break;
            }
            bytes += payload.value().len();
            let event: SesEventRecord = serde_json::from_slice(payload.value()).map_err(error)?;
            if message_id.is_some_and(|id| id != event.message_id) {
                return Err(Error::InternalError(
                    "SES message index identity mismatch".into(),
                ));
            }
            items.push(event);
        }
        let next = if more {
            items
                .last()
                .map(|event| {
                    serde_json::to_string(&EventCursor {
                        message_id: message_id.map(str::to_owned),
                        created_at: event.created_at,
                        event_id: event.event_id.clone(),
                    })
                    .map_err(error)
                })
                .transpose()?
        } else {
            None
        };
        Ok(SesEventPage { items, next })
    }
    #[cfg(test)]
    pub(super) fn remove_for_test(&self, id: &str) -> Result<()> {
        let event = self.get(id)?;
        let write = self.database.begin_write().map_err(error)?;
        {
            write
                .open_table(RECORDS)
                .map_err(error)?
                .remove(id)
                .map_err(error)?;
            write
                .open_table(ORDER)
                .map_err(error)?
                .remove(order_key(&event).as_str())
                .map_err(error)?;
            write
                .open_table(MESSAGES)
                .map_err(error)?
                .remove(message_key(&event).as_str())
                .map_err(error)?;
        }
        write.commit().map_err(error)
    }
}
#[cfg(test)]
fn crash_point(phase: &str) {
    if std::env::var("SQRZL_TEST_SES_INDEX_PHASE").as_deref() == Ok(phase) {
        std::process::exit(73);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mail::MailStore;
    use chrono::Utc;
    fn sample(id: &str) -> SesEventRecord {
        serde_json::from_value(serde_json::json!({"event_id":id,"message_id":"message","event_type":"Delivery","created_at":Utc::now(),"state":"pending","record":{},"notification":{}})).unwrap()
    }
    #[test]
    #[ignore = "subprocess worker invoked by event index crash qualification"]
    fn should_exit_at_event_index_commit_boundary() {
        // Arrange
        let root = std::env::var("SQRZL_TEST_SES_INDEX_ROOT").unwrap();
        let store = FilesystemMailStore::open(root).unwrap();
        // Act
        store.claim_ses_event(&sample("new-event")).unwrap();
        // Assert
        panic!("crash boundary was not reached");
    }
    #[test]
    fn should_publish_event_payload_with_its_indexes_at_every_commit_boundary() {
        // Arrange
        for phase in ["before_commit", "after_commit"] {
            let root =
                std::env::temp_dir().join(format!("sqrzl-ses-crash-{}", uuid::Uuid::new_v4()));
            let store = FilesystemMailStore::open(&root).unwrap();
            store.record_ses_event(&sample("previous-event")).unwrap();
            drop(store);
            // Act
            let status = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "mail::filesystem::event_index::tests::should_exit_at_event_index_commit_boundary", "--ignored"])
                .env("SQRZL_TEST_SES_INDEX_ROOT", &root).env("SQRZL_TEST_SES_INDEX_PHASE", phase)
                .stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).status().unwrap();
            let recovered = FilesystemMailStore::open(&root).unwrap();
            let page = recovered
                .list_ses_event_page(Some("message"), None, 10)
                .unwrap();
            // Assert
            assert_eq!(status.code(), Some(73));
            assert!(recovered.get_ses_event("previous-event").is_ok());
            assert_eq!(
                recovered.get_ses_event("new-event").is_ok(),
                phase == "after_commit"
            );
            assert_eq!(
                page.items.len(),
                if phase == "after_commit" { 2 } else { 1 }
            );
            drop(recovered);
            fs::remove_dir_all(root).unwrap();
        }
    }
    #[test]
    fn should_import_legacy_events_without_replacing_a_later_completed_record() {
        // Arrange
        let root = std::env::temp_dir().join(format!("sqrzl-ses-import-{}", uuid::Uuid::new_v4()));
        let directory = root.join("_mail").join(SES_EVENTS_DIR);
        fs::create_dir_all(&directory).unwrap();
        let event = sample("legacy-event");
        fs::write(
            directory.join("legacy-event.json"),
            ses_event_bytes(&event).unwrap(),
        )
        .unwrap();
        // Act
        let store = FilesystemMailStore::open(&root).unwrap();
        let mut completed = store.get_ses_event("legacy-event").unwrap();
        completed.state = crate::mail::ses_events::SesEventState::Completed;
        store.record_ses_event(&completed).unwrap();
        drop(store);
        let reopened = FilesystemMailStore::open(&root).unwrap();
        let stored = reopened.get_ses_event("legacy-event").unwrap();
        // Assert
        assert!(matches!(
            stored.state,
            crate::mail::ses_events::SesEventState::Completed
        ));
        assert_eq!(
            reopened
                .list_ses_event_page(Some("message"), None, 1)
                .unwrap()
                .items
                .len(),
            1
        );
        drop(reopened);
        fs::remove_dir_all(root).unwrap();
    }
}
