use crate::error::{Error, Result};
use crate::sms::model::{
    CallbackAttempt, ListSmsMessagesResult, ListSmsParams, NewSmsMessage, SmsConversation,
    SmsDeliveryState, SmsDirection, SmsMedia, SmsMessage, SmsProvider, TextDestination,
};
use crate::sms::{generate_message_id, generate_provider_message_id, SmsStore};
use chrono::Utc;
use sha2::{Digest, Sha256};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

pub struct FilesystemSmsStore {
    root: PathBuf,
    write_lock: Mutex<()>,
}

impl FilesystemSmsStore {
    /// Opens the independent `_sms` persistence tree beneath `blobs_path`.
    ///
    /// # Errors
    ///
    /// Returns an error when the persistence directories cannot be created.
    pub fn open(blobs_path: impl AsRef<Path>) -> Result<Self> {
        let root = blobs_path.as_ref().join("_sms");
        for child in [
            "messages",
            "conversations",
            "media",
            "destinations",
            "callbacks",
        ] {
            fs::create_dir_all(root.join(child)).map_err(io_err)?;
        }
        Ok(Self {
            root,
            write_lock: Mutex::new(()),
        })
    }

    fn message_path(&self, message_id: &str) -> Result<PathBuf> {
        validate_id(message_id)?;
        Ok(self
            .root
            .join("messages")
            .join(format!("{message_id}.json")))
    }

    fn peer_hash(peer: &str) -> Result<String> {
        if peer.trim().is_empty() {
            return Err(Error::InvalidRequest("peer must not be empty".to_string()));
        }
        Ok(hex::encode(Sha256::digest(peer.as_bytes())))
    }

    fn peer_sidecar(&self, peer: &str) -> Result<PathBuf> {
        Ok(self
            .root
            .join("conversations")
            .join(format!("{}.peer", Self::peer_hash(peer)?)))
    }

    fn peer_index_dir(&self, peer: &str) -> Result<PathBuf> {
        Ok(self.root.join("conversations").join(Self::peer_hash(peer)?))
    }

    fn media_path(&self, message_id: &str, media_id: &str) -> Result<PathBuf> {
        validate_id(message_id)?;
        validate_id(media_id)?;
        Ok(self.root.join("media").join(message_id).join(media_id))
    }

    fn destination_path(&self, provider: SmsProvider, local_number: &str) -> Result<PathBuf> {
        let hash = Self::peer_hash(local_number)?;
        Ok(self
            .root
            .join("destinations")
            .join(provider.as_str())
            .join(format!("{hash}.json")))
    }

    fn callback_path(&self, attempt_id: &str) -> Result<PathBuf> {
        validate_id(attempt_id)?;
        Ok(self
            .root
            .join("callbacks")
            .join(format!("{attempt_id}.json")))
    }

    fn read_message_path(path: &Path) -> Result<SmsMessage> {
        let bytes = fs::read(path).map_err(|err| {
            if err.kind() == std::io::ErrorKind::NotFound {
                Error::MessageNotFound
            } else {
                io_err(err)
            }
        })?;
        serde_json::from_slice(&bytes).map_err(|err| Error::InternalError(err.to_string()))
    }

    fn remove_file_if_present(path: &Path) -> Result<()> {
        match fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(err) => Err(io_err(err)),
        }
    }

    fn remove_dir_if_present(path: &Path) -> Result<()> {
        match fs::remove_dir_all(path) {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(err) => Err(io_err(err)),
        }
    }

    fn message_ids_for_peer(&self, peer: &str) -> Result<Vec<String>> {
        let dir = self.peer_index_dir(peer)?;
        let Ok(entries) = fs::read_dir(dir) else {
            return Ok(Vec::new());
        };
        let mut ids = entries
            .filter_map(std::result::Result::ok)
            .filter_map(|entry| {
                entry
                    .path()
                    .file_stem()
                    .and_then(std::ffi::OsStr::to_str)
                    .map(str::to_string)
            })
            .collect::<Vec<_>>();
        ids.sort();
        Ok(ids)
    }

    fn delete_message_locked(&self, peer: &str, message_id: &str) -> Result<()> {
        let message_path = self.message_path(message_id)?;
        if let Ok(message) = Self::read_message_path(&message_path) {
            if message.peer != peer {
                return Err(Error::MessageNotFound);
            }
        }

        Self::remove_file_if_present(&message_path)?;
        Self::remove_file_if_present(
            &self.peer_index_dir(peer)?.join(format!("{message_id}.idx")),
        )?;
        Self::remove_dir_if_present(&self.root.join("media").join(message_id))?;

        for attempt in self.list_callbacks(message_id)? {
            Self::remove_file_if_present(&self.callback_path(&attempt.attempt_id)?)?;
        }

        if self.message_ids_for_peer(peer)?.is_empty() {
            Self::remove_dir_if_present(&self.peer_index_dir(peer)?)?;
            Self::remove_file_if_present(&self.peer_sidecar(peer)?)?;
        }
        Ok(())
    }
}

impl SmsStore for FilesystemSmsStore {
    fn store_message(&self, new: NewSmsMessage) -> Result<SmsMessage> {
        if new.from.trim().is_empty() || new.to.trim().is_empty() {
            return Err(Error::InvalidRequest(
                "from and to must not be empty".to_string(),
            ));
        }
        let peer = match new.direction {
            SmsDirection::Outbound => new.to.clone(),
            SmsDirection::Inbound => new.from.clone(),
        };
        let message_id = generate_message_id();
        let provider_message_id = new
            .provider_message_id
            .unwrap_or_else(|| generate_provider_message_id(new.provider));
        let now = Utc::now();
        let mut media = Vec::with_capacity(new.media.len());
        let mut media_content = Vec::new();
        for item in new.media {
            let media_id = format!("media-{}", uuid::Uuid::new_v4());
            let size = item.content.as_ref().map(Vec::len);
            if let Some(content) = item.content {
                media_content.push((media_id.clone(), content));
            }
            media.push(SmsMedia {
                media_id,
                filename: item.filename,
                content_type: item.content_type,
                size,
                external_url: item.external_url,
            });
        }
        let message = SmsMessage {
            message_id: message_id.clone(),
            batch_id: new.batch_id,
            provider: new.provider,
            provider_message_id,
            direction: new.direction,
            channel: new.channel,
            from: new.from,
            to: new.to,
            body: new.body,
            media,
            metadata: new.metadata,
            peer: peer.clone(),
            delivery_state: SmsDeliveryState::Accepted,
            created_at: now,
            updated_at: now,
        };

        let _guard = self
            .write_lock
            .lock()
            .map_err(|_| Error::InternalError("SMS store lock poisoned".to_string()))?;
        let peer_dir = self.peer_index_dir(&peer)?;
        fs::create_dir_all(&peer_dir).map_err(io_err)?;
        let sidecar = self.peer_sidecar(&peer)?;
        if sidecar.exists() {
            let existing = fs::read_to_string(&sidecar).map_err(io_err)?;
            if existing != peer {
                return Err(Error::InternalError(
                    "SMS peer storage key collision".to_string(),
                ));
            }
        } else {
            write_atomic(&sidecar, peer.as_bytes())?;
        }
        let message_bytes =
            serde_json::to_vec(&message).map_err(|err| Error::InternalError(err.to_string()))?;
        write_atomic(&self.message_path(&message_id)?, &message_bytes)?;
        write_atomic(&peer_dir.join(format!("{message_id}.idx")), b"")?;
        for (media_id, content) in media_content {
            write_atomic(&self.media_path(&message_id, &media_id)?, &content)?;
        }
        Ok(message)
    }

    fn get_message(&self, message_id: &str) -> Result<SmsMessage> {
        Self::read_message_path(&self.message_path(message_id)?)
    }

    fn get_message_by_provider_id(&self, provider_message_id: &str) -> Result<SmsMessage> {
        for entry in fs::read_dir(self.root.join("messages")).map_err(io_err)? {
            let entry = entry.map_err(io_err)?;
            if entry.path().extension().and_then(std::ffi::OsStr::to_str) != Some("json") {
                continue;
            }
            let message = Self::read_message_path(&entry.path())?;
            if message.provider_message_id == provider_message_id {
                return Ok(message);
            }
        }
        Err(Error::MessageNotFound)
    }

    fn list_messages(&self, peer: &str, params: ListSmsParams) -> Result<ListSmsMessagesResult> {
        let mut messages = self
            .message_ids_for_peer(peer)?
            .into_iter()
            .map(|message_id| self.get_message(&message_id))
            .collect::<Result<Vec<_>>>()?;
        messages.sort_by(|a, b| {
            a.created_at
                .cmp(&b.created_at)
                .then_with(|| a.message_id.cmp(&b.message_id))
        });
        let start = params.marker.as_ref().map_or(0, |marker| {
            messages
                .iter()
                .position(|message| message.message_id == *marker)
                .map_or(0, |index| index + 1)
        });
        let limit = params.limit.unwrap_or(usize::MAX);
        let mut page = messages.into_iter().skip(start).collect::<Vec<_>>();
        let next_marker = if page.len() > limit {
            page.truncate(limit);
            page.last().map(|message| message.message_id.clone())
        } else {
            None
        };
        Ok(ListSmsMessagesResult {
            messages: page,
            next_marker,
        })
    }

    fn list_conversations(&self) -> Result<Vec<SmsConversation>> {
        let mut conversations = Vec::new();
        for entry in fs::read_dir(self.root.join("conversations")).map_err(io_err)? {
            let entry = entry.map_err(io_err)?;
            if entry.path().extension().and_then(std::ffi::OsStr::to_str) != Some("peer") {
                continue;
            }
            let peer = fs::read_to_string(entry.path()).map_err(io_err)?;
            let messages = self
                .list_messages(&peer, ListSmsParams::default())?
                .messages;
            let Some(last) = messages.last() else {
                continue;
            };
            conversations.push(SmsConversation {
                peer,
                message_count: messages.len(),
                last_message_at: last.created_at,
                last_message_body: last.body.clone(),
                last_direction: last.direction,
                provider: last.provider,
            });
        }
        conversations.sort_by(|a, b| {
            b.last_message_at
                .cmp(&a.last_message_at)
                .then_with(|| a.peer.cmp(&b.peer))
        });
        Ok(conversations)
    }

    fn delete_message(&self, peer: &str, message_id: &str) -> Result<()> {
        let _guard = self
            .write_lock
            .lock()
            .map_err(|_| Error::InternalError("SMS store lock poisoned".to_string()))?;
        self.delete_message_locked(peer, message_id)
    }

    fn delete_conversation(&self, peer: &str) -> Result<()> {
        let _guard = self
            .write_lock
            .lock()
            .map_err(|_| Error::InternalError("SMS store lock poisoned".to_string()))?;
        for message_id in self.message_ids_for_peer(peer)? {
            self.delete_message_locked(peer, &message_id)?;
        }
        Ok(())
    }

    fn transition_delivery(&self, message_id: &str, state: SmsDeliveryState) -> Result<SmsMessage> {
        if state == SmsDeliveryState::Accepted {
            return Err(Error::InvalidRequest(
                "delivery transition must be delivered or failed".to_string(),
            ));
        }
        let _guard = self
            .write_lock
            .lock()
            .map_err(|_| Error::InternalError("SMS store lock poisoned".to_string()))?;
        let path = self.message_path(message_id)?;
        let mut message = Self::read_message_path(&path)?;
        match message.delivery_state {
            SmsDeliveryState::Accepted => {
                message.delivery_state = state;
                message.updated_at = Utc::now();
                let bytes = serde_json::to_vec(&message)
                    .map_err(|err| Error::InternalError(err.to_string()))?;
                write_atomic(&path, &bytes)?;
                Ok(message)
            }
            current if current == state => Ok(message),
            _ => Err(Error::InvalidRequest(
                "terminal delivery states are immutable".to_string(),
            )),
        }
    }

    fn read_media(&self, message_id: &str, media_id: &str) -> Result<(SmsMedia, Vec<u8>)> {
        let message = self.get_message(message_id)?;
        let media = message
            .media
            .into_iter()
            .find(|media| media.media_id == media_id)
            .ok_or(Error::MessageNotFound)?;
        if media.external_url.is_some() {
            return Err(Error::InvalidRequest(
                "external media is referenced, not stored locally".to_string(),
            ));
        }
        let content = fs::read(self.media_path(message_id, media_id)?).map_err(|err| {
            if err.kind() == std::io::ErrorKind::NotFound {
                Error::MessageNotFound
            } else {
                io_err(err)
            }
        })?;
        Ok((media, content))
    }

    fn put_destination(
        &self,
        provider: SmsProvider,
        local_number: &str,
        callback_url: &str,
    ) -> Result<TextDestination> {
        if local_number.trim().is_empty() || callback_url.trim().is_empty() {
            return Err(Error::InvalidRequest(
                "local_number and callback_url must not be empty".to_string(),
            ));
        }
        let path = self.destination_path(provider, local_number)?;
        let now = Utc::now();
        let created_at = fs::read(&path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<TextDestination>(&bytes).ok())
            .map_or(now, |destination| destination.created_at);
        let destination = TextDestination {
            provider,
            local_number: local_number.to_string(),
            callback_url: callback_url.to_string(),
            created_at,
            updated_at: now,
        };
        let bytes = serde_json::to_vec(&destination)
            .map_err(|err| Error::InternalError(err.to_string()))?;
        let _guard = self
            .write_lock
            .lock()
            .map_err(|_| Error::InternalError("SMS store lock poisoned".to_string()))?;
        write_atomic(&path, &bytes)?;
        Ok(destination)
    }

    fn get_destination(
        &self,
        provider: SmsProvider,
        local_number: &str,
    ) -> Result<TextDestination> {
        let bytes = fs::read(self.destination_path(provider, local_number)?).map_err(|err| {
            if err.kind() == std::io::ErrorKind::NotFound {
                Error::MessageNotFound
            } else {
                io_err(err)
            }
        })?;
        serde_json::from_slice(&bytes).map_err(|err| Error::InternalError(err.to_string()))
    }

    fn delete_destination(&self, provider: SmsProvider, local_number: &str) -> Result<()> {
        let _guard = self
            .write_lock
            .lock()
            .map_err(|_| Error::InternalError("SMS store lock poisoned".to_string()))?;
        Self::remove_file_if_present(&self.destination_path(provider, local_number)?)
    }

    fn record_callback(&self, attempt: CallbackAttempt) -> Result<()> {
        let bytes =
            serde_json::to_vec(&attempt).map_err(|err| Error::InternalError(err.to_string()))?;
        let _guard = self
            .write_lock
            .lock()
            .map_err(|_| Error::InternalError("SMS store lock poisoned".to_string()))?;
        let path = self.callback_path(&attempt.attempt_id)?;
        if path.exists() {
            return Err(Error::InvalidRequest(
                "callback attempt already exists".to_string(),
            ));
        }
        write_atomic(&path, &bytes)
    }

    fn get_callback(&self, attempt_id: &str) -> Result<CallbackAttempt> {
        let bytes = fs::read(self.callback_path(attempt_id)?).map_err(|err| {
            if err.kind() == std::io::ErrorKind::NotFound {
                Error::MessageNotFound
            } else {
                io_err(err)
            }
        })?;
        serde_json::from_slice(&bytes).map_err(|err| Error::InternalError(err.to_string()))
    }

    fn list_callbacks(&self, message_id: &str) -> Result<Vec<CallbackAttempt>> {
        let mut attempts = Vec::new();
        for entry in fs::read_dir(self.root.join("callbacks")).map_err(io_err)? {
            let entry = entry.map_err(io_err)?;
            if entry.path().extension().and_then(std::ffi::OsStr::to_str) != Some("json") {
                continue;
            }
            let bytes = fs::read(entry.path()).map_err(io_err)?;
            let attempt = serde_json::from_slice::<CallbackAttempt>(&bytes)
                .map_err(|err| Error::InternalError(err.to_string()))?;
            if attempt.message_id == message_id {
                attempts.push(attempt);
            }
        }
        attempts.sort_by(|a, b| {
            a.attempted_at
                .cmp(&b.attempted_at)
                .then_with(|| a.attempt_id.cmp(&b.attempt_id))
        });
        Ok(attempts)
    }
}

fn validate_id(value: &str) -> Result<()> {
    if value.is_empty() || value.contains('/') || value.contains('\\') || value.contains("..") {
        return Err(Error::InvalidRequest(format!(
            "invalid SMS storage identifier: {value}"
        )));
    }
    Ok(())
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(io_err)?;
    }
    let filename = path
        .file_name()
        .ok_or_else(|| Error::InternalError("SMS path has no filename".to_string()))?;
    let mut temporary = filename.to_os_string();
    temporary.push(format!(".{}.tmp", uuid::Uuid::new_v4()));
    let temporary = path.with_file_name(temporary);
    fs::write(&temporary, bytes).map_err(io_err)?;
    fs::rename(temporary, path).map_err(io_err)
}

fn io_err(err: impl std::fmt::Display) -> Error {
    Error::InternalError(err.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sms::model::{NewSmsMedia, SmsChannel, SmsProvider};
    use std::collections::HashMap;

    fn temp_path() -> PathBuf {
        std::env::temp_dir().join(format!("sqrzl-sms-test-{}", uuid::Uuid::new_v4()))
    }

    fn message(direction: SmsDirection, from: &str, to: &str) -> NewSmsMessage {
        NewSmsMessage {
            batch_id: None,
            provider: SmsProvider::Twilio,
            provider_message_id: None,
            direction,
            channel: SmsChannel::Sms,
            from: from.to_string(),
            to: to.to_string(),
            body: "hello".to_string(),
            media: Vec::new(),
            metadata: HashMap::new(),
        }
    }

    #[test]
    fn should_preserve_remote_peer_conversation_across_restart() {
        // Arrange
        // Act
        // Assert
        let path = temp_path();
        let store = FilesystemSmsStore::open(&path).unwrap();
        store
            .store_message(message(
                SmsDirection::Outbound,
                "+15550000001",
                "+15550000002",
            ))
            .unwrap();
        store
            .store_message(message(
                SmsDirection::Inbound,
                "+15550000002",
                "+15550000001",
            ))
            .unwrap();
        drop(store);

        let reopened = FilesystemSmsStore::open(path).unwrap();
        let messages = reopened
            .list_messages("+15550000002", ListSmsParams::default())
            .unwrap();
        assert_eq!(messages.messages.len(), 2);
        assert_eq!(reopened.list_conversations().unwrap().len(), 1);
    }

    #[test]
    fn should_persist_special_peers_in_canonical_message_namespace() {
        // Arrange
        // Act
        // Assert
        let path = temp_path();
        let store = FilesystemSmsStore::open(&path).unwrap();
        let stored = store
            .store_message(message(SmsDirection::Outbound, "sender", "+1/%25"))
            .unwrap();

        assert_eq!(
            store.get_message(&stored.message_id).unwrap().peer,
            "+1/%25"
        );
        assert!(store
            .list_messages("+1/%25", ListSmsParams::default())
            .unwrap()
            .messages
            .iter()
            .any(|message| message.message_id == stored.message_id));
        let index = store.peer_index_dir("+1/%25").unwrap();
        assert!(fs::read_dir(index).unwrap().all(|entry| entry
            .unwrap()
            .path()
            .extension()
            .unwrap()
            == "idx"));
    }

    #[test]
    fn should_support_message_lifecycle_with_media() {
        // Arrange
        // Act
        // Assert
        let store = FilesystemSmsStore::open(temp_path()).unwrap();
        let mut first = message(SmsDirection::Inbound, "+15550000002", "+15550000001");
        first.channel = SmsChannel::Mms;
        first.media.push(NewSmsMedia {
            filename: "photo.jpg".to_string(),
            content_type: "image/jpeg".to_string(),
            content: Some(vec![1, 2, 3]),
            external_url: None,
        });
        let first = store.store_message(first).unwrap();
        let second = store
            .store_message(message(
                SmsDirection::Outbound,
                "+15550000001",
                "+15550000002",
            ))
            .unwrap();

        let page = store
            .list_messages(
                "+15550000002",
                ListSmsParams {
                    marker: None,
                    limit: Some(1),
                },
            )
            .unwrap();
        assert_eq!(page.messages.len(), 1);
        assert!(page.next_marker.is_some());
        assert_eq!(
            store
                .read_media(&first.message_id, &first.media[0].media_id)
                .unwrap()
                .1,
            vec![1, 2, 3]
        );

        let delivered = store
            .transition_delivery(&second.message_id, SmsDeliveryState::Delivered)
            .unwrap();
        assert_eq!(delivered.delivery_state, SmsDeliveryState::Delivered);
        assert_eq!(
            store
                .transition_delivery(&second.message_id, SmsDeliveryState::Delivered)
                .unwrap()
                .delivery_state,
            SmsDeliveryState::Delivered
        );
        assert!(store
            .transition_delivery(&second.message_id, SmsDeliveryState::Failed)
            .is_err());

        store
            .delete_message("+15550000002", &first.message_id)
            .unwrap();
        assert!(store.get_message(&first.message_id).is_err());
        store.delete_conversation("+15550000002").unwrap();
        assert!(store.list_conversations().unwrap().is_empty());
    }
}
