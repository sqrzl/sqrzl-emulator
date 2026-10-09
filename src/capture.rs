//! Durable publication of newly captured messages and provider replay records.
//!
//! A capture writes an intent before touching its destinations. A single commit
//! marker publishes every destination. Readers ignore incomplete transactions,
//! and reopening a store removes their files. This covers process termination
//! on a local filesystem; it does not coordinate multiple emulator processes.

use crate::error::{Error, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Component, Path, PathBuf};

pub(crate) const TRANSACTION_METADATA: &str = "__sqrzl_capture_transaction";
const TRANSACTIONS: &str = ".capture-transactions";
const RECORDS: &str = ".repeatability";

#[cfg(test)]
type PublicationHook = Box<dyn Fn(&str, usize) -> Result<()>>;
#[cfg(test)]
std::thread_local! {
    static PUBLICATION_HOOK: std::cell::RefCell<Option<PublicationHook>> = const { std::cell::RefCell::new(None) };
    static LISTING_HOOK: std::cell::RefCell<Option<Box<dyn Fn()>>> = const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
pub(crate) fn test_listing_phase() {
    LISTING_HOOK.with(|hook| {
        if let Some(hook) = hook.borrow().as_ref() {
            hook();
        }
    });
}

/// The durable fingerprint and wire result of one provider-scoped request.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RepeatabilityRecord {
    pub key: String,
    pub request_hash: String,
    pub first_sent: String,
    pub status: u16,
    pub result: Value,
    #[serde(default)]
    pub transaction_id: String,
}

#[derive(Serialize, Deserialize)]
struct Intent {
    paths: Vec<PathBuf>,
}

pub(crate) fn new_transaction_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

pub(crate) fn record_path(root: &Path, key: &str) -> PathBuf {
    use sha2::{Digest, Sha256};
    root.join(RECORDS).join(format!(
        "{}.json",
        hex::encode(Sha256::digest(key.as_bytes()))
    ))
}

pub(crate) fn record_file(root: &Path, record: &RepeatabilityRecord) -> Result<(PathBuf, Vec<u8>)> {
    Ok((
        record_path(root, &record.key),
        serde_json::to_vec(record).map_err(serialization_error)?,
    ))
}

pub(crate) fn load_record(root: &Path, key: &str) -> Result<Option<RepeatabilityRecord>> {
    let data = match fs::read(record_path(root, key)) {
        Ok(data) => data,
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
            ) =>
        {
            return Ok(None)
        }
        Err(error) => return Err(io_error(error)),
    };
    let record: RepeatabilityRecord = serde_json::from_slice(&data).map_err(serialization_error)?;
    if record.key != key {
        return Err(Error::InternalError(
            "repeatability storage key collision".to_string(),
        ));
    }
    Ok(is_committed(root, &record.transaction_id).then_some(record))
}

pub(crate) fn is_committed(root: &Path, transaction_id: &str) -> bool {
    transaction_id.is_empty()
        || (uuid::Uuid::parse_str(transaction_id).is_ok()
            && root
                .join(TRANSACTIONS)
                .join(transaction_id)
                .join("committed")
                .is_file())
}

pub(crate) fn committed_snapshot(root: &Path) -> Result<HashSet<String>> {
    let mut ids = HashSet::new();
    match fs::read_dir(root.join(TRANSACTIONS)) {
        Ok(entries) => {
            for entry in entries {
                let entry = entry.map_err(io_error)?;
                if entry.path().join("committed").is_file() {
                    ids.insert(entry.file_name().to_string_lossy().into_owned());
                }
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(io_error(error)),
    }
    Ok(ids)
}

pub(crate) fn recover(root: &Path) -> Result<()> {
    fs::create_dir_all(root.join(TRANSACTIONS)).map_err(io_error)?;
    for entry in fs::read_dir(root.join(TRANSACTIONS)).map_err(io_error)? {
        let entry = entry.map_err(io_error)?;
        if !entry.path().is_dir() || entry.path().join("committed").is_file() {
            continue;
        }
        let intent_path = entry.path().join("intent.json");
        if intent_path.is_file() {
            let intent: Intent = serde_json::from_slice(&fs::read(intent_path).map_err(io_error)?)
                .map_err(serialization_error)?;
            for path in intent.paths {
                validate_relative(&path)?;
                remove_if_present(&root.join(path))?;
            }
        }
        fs::remove_dir_all(entry.path()).map_err(io_error)?;
    }
    sync_directory(&root.join(TRANSACTIONS))
}

pub(crate) fn commit(root: &Path, id: &str, files: &[(PathBuf, Vec<u8>)]) -> Result<()> {
    commit_with_hook(root, id, files, |phase, index| {
        #[cfg(test)]
        PUBLICATION_HOOK.with(|hook| match hook.borrow().as_ref() {
            Some(hook) => hook(phase, index),
            None => Ok(()),
        })?;
        #[cfg(test)]
        if std::env::var("SQRZL_CAPTURE_TEST_PHASE").ok().as_deref() == Some(phase)
            && std::env::var("SQRZL_CAPTURE_TEST_INDEX")
                .ok()
                .and_then(|value| value.parse::<usize>().ok())
                == Some(index)
        {
            std::process::exit(73);
        }
        let _ = (phase, index);
        Ok(())
    })
}

/// The hook is used by subprocess tests to exit at actual publication steps.
pub(crate) fn commit_with_hook(
    root: &Path,
    id: &str,
    files: &[(PathBuf, Vec<u8>)],
    mut hook: impl FnMut(&str, usize) -> Result<()>,
) -> Result<()> {
    recover(root)?;
    let mut paths = Vec::with_capacity(files.len() * 2);
    let mut unique = HashSet::new();
    for (path, _) in files {
        let relative = path
            .strip_prefix(root)
            .map_err(|_| Error::InvalidRequest("capture path escapes store".to_string()))?;
        validate_relative(relative)?;
        if path.exists() || !unique.insert(path.clone()) {
            return Err(Error::InvalidRequest(
                "capture destination already exists".to_string(),
            ));
        }
        paths.push(relative.to_path_buf());
        paths.push(pending_path(relative, id));
    }
    let transaction_dir = root.join(TRANSACTIONS).join(id);
    fs::create_dir_all(&transaction_dir).map_err(io_error)?;
    hook("prepare", 0)?;
    let intent = serde_json::to_vec(&Intent { paths }).map_err(serialization_error)?;
    write_new_synced(&transaction_dir.join("intent.json"), &intent)?;
    sync_directory(&transaction_dir)?;
    sync_directory(&root.join(TRANSACTIONS))?;
    let result = (|| {
        hook("intent", 0)?;
        for (index, (path, data)) in files.iter().enumerate() {
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).map_err(io_error)?;
            }
            let pending = pending_path(path, id);
            write_new_synced(&pending, data)?;
            hook("stage", index)?;
            fs::rename(&pending, path).map_err(io_error)?;
            hook("publish", index)?;
            if let Some(parent) = path.parent() {
                sync_directory(parent)?;
            }
            hook("file", index)?;
        }
        write_new_synced(&transaction_dir.join("committed"), b"")?;
        hook("commit_marker", files.len())?;
        sync_directory(&transaction_dir)?;
        hook("commit", files.len())?;
        Ok(())
    })();
    if result.is_err() && !transaction_dir.join("committed").is_file() {
        // Keep the intent if removal fails so reopening can retry recovery.
        for (path, _) in files {
            remove_if_present(path)?;
            remove_if_present(&pending_path(path, id))?;
        }
        fs::remove_dir_all(&transaction_dir).map_err(io_error)?;
        sync_directory(&root.join(TRANSACTIONS))?;
    }
    result
}

fn pending_path(path: &Path, id: &str) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".pending-{id}"));
    path.with_file_name(name)
}

fn validate_relative(path: &Path) -> Result<()> {
    if path.as_os_str().is_empty()
        || path
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(Error::InvalidRequest(
            "invalid capture journal path".to_string(),
        ));
    }
    Ok(())
}

fn write_new_synced(path: &Path, data: &[u8]) -> Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(io_error)?;
    file.write_all(data).map_err(io_error)?;
    file.sync_all().map_err(io_error)
}

fn sync_directory(path: &Path) -> Result<()> {
    File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(io_error)
}

fn remove_if_present(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
            ) =>
        {
            Ok(())
        }
        Err(error) => Err(io_error(error)),
    }
}

#[allow(clippy::needless_pass_by_value)] // map_err callback signature
fn io_error(error: std::io::Error) -> Error {
    Error::InternalError(error.to_string())
}
#[allow(clippy::needless_pass_by_value)] // map_err callback signature
fn serialization_error(error: serde_json::Error) -> Error {
    Error::InternalError(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mail::{
        Address, FilesystemMailStore, ListMessagesParams, MailStore, Message, SourceProtocol,
    };
    use crate::sms::{
        FilesystemSmsStore, ListSmsParams, NewSmsMedia, NewSmsMessage, SmsChannel, SmsDirection,
        SmsProvider, SmsStore,
    };
    use std::collections::HashMap;

    fn mail_message(to: &[&str]) -> Message {
        Message {
            source_protocol: SourceProtocol::Acs,
            from: Address::new("sender@example.com"),
            to: to.iter().map(|address| Address::new(*address)).collect(),
            cc: Vec::new(),
            bcc: Vec::new(),
            reply_to: Vec::new(),
            subject: "batch".to_string(),
            headers: HashMap::new(),
            body_text: Some("hello".to_string()),
            body_html: None,
            attachments: Vec::new(),
            user_engagement_tracking_disabled: None,
            provider_metadata: HashMap::new(),
            raw_mime: Some(b"Subject: batch\r\n\r\nhello".to_vec()),
            thread_id: None,
        }
    }

    fn sms_message(id: &str) -> NewSmsMessage {
        NewSmsMessage {
            batch_id: Some("batch-qualification".to_string()),
            provider: SmsProvider::Acs,
            provider_message_id: Some(id.to_string()),
            direction: SmsDirection::Outbound,
            channel: SmsChannel::Sms,
            from: "+15550000001".to_string(),
            to: "+15550000002".to_string(),
            body: "hello".to_string(),
            media: vec![NewSmsMedia {
                filename: "capture.txt".to_string(),
                content_type: "text/plain".to_string(),
                content: Some(b"media bytes".to_vec()),
                external_url: None,
            }],
            metadata: HashMap::new(),
        }
    }

    fn record(id: &str) -> RepeatabilityRecord {
        RepeatabilityRecord {
            key: format!("qualification/{id}"),
            request_hash: "fingerprint".to_string(),
            first_sent: "first-sent".to_string(),
            status: 202,
            result: serde_json::json!({"id":id}),
            transaction_id: String::new(),
        }
    }

    /// Only runs in the explicitly selected child process. Exit bypasses Drop,
    /// rollback and normal store cleanup at the requested filesystem boundary.
    #[test]
    #[ignore = "subprocess worker invoked by the capture crash campaigns"]
    fn should_exit_at_capture_crash_boundary() {
        // Arrange
        let Ok(root) = std::env::var("SQRZL_CAPTURE_TEST_ROOT") else {
            return;
        };
        // Act
        if std::env::var("SQRZL_CAPTURE_TEST_DOMAIN").as_deref() == Ok("mail") {
            let store = FilesystemMailStore::open(root).unwrap();
            store
                .capture_batch(
                    &[
                        (
                            "first".to_string(),
                            mail_message(&["alice@example.com", "bob@example.com"]),
                        ),
                        (
                            "second".to_string(),
                            mail_message(&["alice@example.com", "carol@example.com"]),
                        ),
                    ],
                    &[record("first"), record("second")],
                )
                .unwrap();
        } else {
            let store = FilesystemSmsStore::open(root).unwrap();
            store
                .capture_batch(
                    vec![sms_message("first"), sms_message("second")],
                    &[record("first"), record("second")],
                )
                .unwrap();
        }
        // Assert
        panic!("configured crash boundary was not reached");
    }

    #[test]
    fn should_publish_complete_mail_captures_at_every_crash_boundary() {
        // Arrange
        for (phase, index) in crash_boundaries(14) {
            let root = temp_root();
            let reader = FilesystemMailStore::open(&root).unwrap();
            // Act
            crash_child(&root, "mail", phase, index);
            // Assert
            let committed = matches!(phase, "commit" | "commit_marker");
            assert_mail_state(&reader, committed);
            drop(reader);
            let recovered = FilesystemMailStore::open(&root).unwrap();
            assert_mail_state(&recovered, committed);
            assert_no_pending_files(&root);
            fs::remove_dir_all(root).unwrap();
        }
    }

    #[test]
    fn should_publish_complete_sms_captures_at_every_crash_boundary() {
        // Arrange
        for (phase, index) in crash_boundaries(8) {
            let root = temp_root();
            let reader = FilesystemSmsStore::open(&root).unwrap();
            // Act
            crash_child(&root, "sms", phase, index);
            // Assert
            let committed = matches!(phase, "commit" | "commit_marker");
            assert_sms_state(&reader, committed);
            drop(reader);
            let recovered = FilesystemSmsStore::open(&root).unwrap();
            assert_sms_state(&recovered, committed);
            assert_no_pending_files(&root);
            fs::remove_dir_all(root).unwrap();
        }
    }

    fn assert_mail_state(store: &FilesystemMailStore, committed: bool) {
        let mailboxes = store.list_mailboxes().unwrap();
        assert_eq!(mailboxes.len(), if committed { 3 } else { 0 });
        assert!(mailboxes.iter().all(|mailbox| mailbox.message_count > 0));
        for (mailbox, count) in [
            ("_all", 2),
            ("alice@example.com", 2),
            ("bob@example.com", 1),
            ("carol@example.com", 1),
        ] {
            let page = store
                .list_messages(mailbox, ListMessagesParams::default())
                .unwrap();
            assert_eq!(page.messages.len(), if committed { count } else { 0 });
            for message in page.messages {
                assert_eq!(
                    message.message.raw_mime.as_deref(),
                    Some(b"Subject: batch\r\n\r\nhello".as_slice())
                );
            }
        }
        for id in ["first", "second"] {
            assert_eq!(store.get_message("_all", id).is_ok(), committed);
            assert_eq!(
                store
                    .get_repeatability_record(&format!("qualification/{id}"))
                    .unwrap()
                    .is_some(),
                committed
            );
        }
    }

    fn assert_sms_state(store: &FilesystemSmsStore, committed: bool) {
        let page = store
            .list_messages("+15550000002", ListSmsParams::default())
            .unwrap();
        assert_eq!(page.messages.len(), if committed { 2 } else { 0 });
        assert_eq!(
            store.list_conversations().unwrap().len(),
            usize::from(committed)
        );
        for id in ["first", "second"] {
            let result = store.get_message_by_provider_id(id);
            assert_eq!(result.is_ok(), committed);
            assert_eq!(
                store
                    .get_repeatability_record(&format!("qualification/{id}"))
                    .unwrap()
                    .is_some(),
                committed
            );
            if let Ok(message) = result {
                assert_eq!(
                    store
                        .read_media(&message.message_id, &message.media[0].media_id)
                        .unwrap()
                        .1,
                    b"media bytes"
                );
            }
        }
    }

    fn crash_boundaries(files: usize) -> Vec<(&'static str, usize)> {
        let mut phases = vec![
            ("prepare", 0),
            ("intent", 0),
            ("commit_marker", files),
            ("commit", files),
        ];
        for index in 0..files {
            phases.extend([("stage", index), ("publish", index), ("file", index)]);
        }
        phases
    }

    fn temp_root() -> PathBuf {
        std::env::temp_dir().join(format!("sqrzl-capture-crash-{}", uuid::Uuid::new_v4()))
    }

    fn crash_child(root: &Path, domain: &str, phase: &str, index: usize) {
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "capture::tests::should_exit_at_capture_crash_boundary",
                "--ignored",
            ])
            .env("SQRZL_CAPTURE_TEST_ROOT", root)
            .env("SQRZL_CAPTURE_TEST_DOMAIN", domain)
            .env("SQRZL_CAPTURE_TEST_PHASE", phase)
            .env("SQRZL_CAPTURE_TEST_INDEX", index.to_string())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap();
        assert_eq!(status.code(), Some(73), "{domain} {phase} {index}");
    }

    fn assert_no_pending_files(path: &Path) {
        for entry in fs::read_dir(path).unwrap() {
            let entry = entry.unwrap();
            if entry.path().is_dir() {
                assert_no_pending_files(&entry.path());
            } else {
                assert!(!entry.file_name().to_string_lossy().contains(".pending-"));
            }
        }
    }

    #[test]
    fn should_list_only_committed_mail_when_live_capture_rolls_back() {
        overlap_rollback_with_listing("mail");
    }

    #[test]
    fn should_list_only_committed_sms_when_live_capture_rolls_back() {
        overlap_rollback_with_listing("sms");
    }

    fn overlap_rollback_with_listing(domain: &str) {
        use std::sync::{Arc, Barrier};
        let root = temp_root();
        let published = Arc::new(Barrier::new(2));
        let selected = Arc::new(Barrier::new(2));
        let rolled_back = Arc::new(Barrier::new(2));
        let mail = Arc::new(FilesystemMailStore::open(&root).unwrap());
        let sms = Arc::new(FilesystemSmsStore::open(&root).unwrap());
        let (writer_mail, writer_sms) = (mail.clone(), sms.clone());
        let (writer_published, writer_selected, writer_rolled_back) =
            (published.clone(), selected.clone(), rolled_back.clone());
        let mail_domain = domain == "mail";
        let writer = std::thread::spawn(move || {
            PUBLICATION_HOOK.with(|hook| {
                *hook.borrow_mut() = Some(Box::new(move |phase, index| {
                    if phase == "file" && index == if mail_domain { 2 } else { 1 } {
                        writer_published.wait();
                        writer_selected.wait();
                        return Err(Error::InternalError(
                            "injected capture rollback".to_string(),
                        ));
                    }
                    Ok(())
                }));
            });
            let result = if mail_domain {
                writer_mail
                    .capture_batch(
                        &[("pending".to_string(), mail_message(&["alice@example.com"]))],
                        &[],
                    )
                    .map(|_| ())
            } else {
                writer_sms
                    .capture_batch(vec![sms_message("pending")], &[])
                    .map(|_| ())
            };
            PUBLICATION_HOOK.with(|hook| *hook.borrow_mut() = None);
            writer_rolled_back.wait();
            result
        });
        published.wait();
        LISTING_HOOK.with(|hook| {
            *hook.borrow_mut() = Some(Box::new(move || {
                selected.wait();
                rolled_back.wait();
            }));
        });
        let listed = if mail_domain {
            mail.list_messages("alice@example.com", ListMessagesParams::default())
                .map(|page| page.messages.len())
        } else {
            sms.list_messages("+15550000002", ListSmsParams::default())
                .map(|page| page.messages.len())
        };
        LISTING_HOOK.with(|hook| *hook.borrow_mut() = None);
        assert!(writer.join().unwrap().is_err());
        fs::remove_dir_all(root).unwrap();
        assert_eq!(listed.unwrap(), 0, "{domain} exposed an aborted capture");
    }

    #[test]
    fn should_roll_back_every_file_when_persistence_returns_an_error() {
        // Arrange
        let root = temp_root();
        fs::create_dir_all(&root).unwrap();
        for phase in ["intent", "stage", "publish", "file"] {
            let id = new_transaction_id();
            let files = vec![
                (root.join("first.json"), b"first".to_vec()),
                (root.join("second.json"), b"second".to_vec()),
            ];
            // Act
            let result = commit_with_hook(&root, &id, &files, |step, index| {
                if step == phase && index == 0 {
                    Err(Error::InternalError(
                        "injected persistence failure".to_string(),
                    ))
                } else {
                    Ok(())
                }
            });
            // Assert
            assert!(result.is_err());
            assert!(!root.join("first.json").exists());
            assert!(!root.join("second.json").exists());
            recover(&root).unwrap();
            assert_no_pending_files(&root);
        }
        fs::remove_dir_all(root).unwrap();
    }
}
