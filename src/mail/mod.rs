//! Email-emulator domain: capture of outbound mail via SMTP and (in later
//! phases) HTTP-shaped provider APIs such as `SendGrid`, SES, and Azure
//! Communication Services.
//!
//! This is a domain independent of the blob-storage `Storage` trait
//! (`crate::storage`) — mailbox/message semantics don't fit bucket/key blob
//! semantics, so mail gets its own store abstraction ([`MailStore`]) rather than
//! reusing `Storage`. See `docs/support-certification.md` for how this domain's
//! compatibility claims are tracked in `compatibility-matrix.json`.

pub mod filesystem;
pub mod model;
pub mod providers;
pub mod smtp;

pub use filesystem::FilesystemMailStore;
pub use model::{
    Address, Attachment, DeliveryState, DeliveryStatus, ListMessagesParams, ListMessagesResult,
    MailboxInfo, Message, SourceProtocol, StoredMessage,
};
pub use smtp::SmtpServer;

use crate::error::{Error, Result};

/// Synthetic mailbox holding one copy of every message regardless of recipient —
/// the account-wide equivalent of `list_buckets` for mail.
pub const ALL_MAILBOX: &str = "_all";

/// Storage abstraction for captured email messages.
///
/// Messages are filed per-mailbox (mailbox key = normalized lowercase recipient
/// address), mirroring the admin UI's per-bucket drill-down. A message with
/// multiple recipients is stored once per mailbox it was sent to — see
/// [`fan_out`], which every capture path (SMTP, and later SendGrid/SES/ACS
/// adapters) should go through rather than calling `store_message` directly.
pub trait MailStore: Send + Sync {
    ///
    /// # Errors
    ///
    /// Returns an error when the underlying emulator operation fails.
    fn store_message(
        &self,
        mailbox: &str,
        message_id: &str,
        message: Message,
    ) -> Result<StoredMessage>;

    ///
    /// # Errors
    ///
    /// Returns [`Error::MessageNotFound`] when no such message exists, or another
    /// error when the underlying emulator operation fails.
    fn get_message(&self, mailbox: &str, message_id: &str) -> Result<StoredMessage>;

    ///
    /// # Errors
    ///
    /// Returns an error when the underlying emulator operation fails.
    fn list_messages(
        &self,
        mailbox: &str,
        params: ListMessagesParams,
    ) -> Result<ListMessagesResult>;

    /// Deletes a message. Idempotent: deleting an already-absent message is not
    /// an error, matching the emulator's existing object-delete semantics.
    ///
    /// # Errors
    ///
    /// Returns an error when the underlying emulator operation fails.
    fn delete_message(&self, mailbox: &str, message_id: &str) -> Result<()>;

    /// Deletes all stored copies for a mailbox.
    ///
    /// # Errors
    ///
    /// Returns an error when the underlying emulator operation fails.
    fn delete_mailbox(&self, mailbox: &str) -> Result<()>;

    ///
    /// # Errors
    ///
    /// Returns an error when the underlying emulator operation fails.
    fn update_delivery_status(
        &self,
        mailbox: &str,
        message_id: &str,
        status: DeliveryStatus,
    ) -> Result<()>;

    /// Lists mailboxes that have received at least one message. Excludes
    /// [`ALL_MAILBOX`], which is not a "real" recipient mailbox.
    ///
    /// # Errors
    ///
    /// Returns an error when the underlying emulator operation fails.
    fn list_mailboxes(&self) -> Result<Vec<MailboxInfo>>;

    ///
    /// # Errors
    ///
    /// Returns an error when the underlying emulator operation fails.
    fn ensure_mailbox(&self, mailbox: &str) -> Result<()>;
}

/// Generates a fresh message id shared by every per-mailbox copy of one message.
#[must_use]
pub fn generate_message_id() -> String {
    format!("msg-{}", uuid::Uuid::new_v4())
}

/// Stores `message` once per recipient mailbox (To + Cc + Bcc, deduplicated) plus
/// once under [`ALL_MAILBOX`], all sharing one generated message id.
///
/// Every capture path (the SMTP server, and later the SendGrid/SES/ACS adapters)
/// should build a [`Message`] and call this rather than `store_message` directly,
/// so recipient fan-out stays consistent across source protocols.
///
/// # Errors
///
/// Returns [`Error::InvalidRequest`] when `message` has no recipients, or
/// propagates the underlying store's error.
pub fn fan_out<S: MailStore + ?Sized>(store: &S, message: &Message) -> Result<Vec<StoredMessage>> {
    fan_out_with_id(store, message, &generate_message_id())
}

/// Atomically captures every message in one provider request. If any fan-out
/// fails, all copies created for earlier messages in the batch are removed.
pub(crate) fn fan_out_batch<S: MailStore + ?Sized>(
    store: &S,
    messages: &[Message],
) -> Result<Vec<Vec<StoredMessage>>> {
    let mut captured: Vec<(String, Vec<String>)> = Vec::with_capacity(messages.len());
    let mut result = Vec::with_capacity(messages.len());
    for message in messages {
        let message_id = generate_message_id();
        match fan_out_with_id(store, message, &message_id) {
            Ok(stored) => {
                let mailboxes = message
                    .recipients()
                    .into_iter()
                    .map(model::Address::mailbox_key)
                    .chain(std::iter::once(ALL_MAILBOX.to_string()))
                    .collect::<Vec<_>>();
                captured.push((message_id, mailboxes));
                result.push(stored);
            }
            Err(error) => {
                let rollback_errors = captured
                    .into_iter()
                    .flat_map(|(id, mailboxes)| {
                        mailboxes
                            .into_iter()
                            .map(move |mailbox| (id.clone(), mailbox))
                    })
                    .filter_map(|(id, mailbox)| {
                        store
                            .delete_message(&mailbox, &id)
                            .err()
                            .map(|rollback| format!("{mailbox}/{id}: {rollback}"))
                    })
                    .collect::<Vec<_>>();
                if rollback_errors.is_empty() {
                    return Err(error);
                }
                return Err(Error::InternalError(format!(
                    "mail batch failed ({error}); rollback failed for {}",
                    rollback_errors.join(", ")
                )));
            }
        }
    }
    Ok(result)
}

/// Stores one message under a caller-supplied provider identifier.
///
/// This is used by provider APIs whose operation identifier is also their
/// durable lookup key (for example, ACS Email long-running operations).
pub(crate) fn fan_out_with_id<S: MailStore + ?Sized>(
    store: &S,
    message: &Message,
    message_id: &str,
) -> Result<Vec<StoredMessage>> {
    let mut mailboxes: Vec<String> = message
        .recipients()
        .into_iter()
        .map(model::Address::mailbox_key)
        .collect();
    mailboxes.sort();
    mailboxes.dedup();

    if mailboxes.is_empty() {
        return Err(Error::InvalidRequest(
            "message has no recipients".to_string(),
        ));
    }

    let mut targets = Vec::with_capacity(mailboxes.len() + 1);
    targets.push(ALL_MAILBOX.to_string());
    targets.extend(mailboxes);

    // Complete every validation and directory-creation step before the first
    // message copy is visible. Generated IDs are unique, and caller-supplied
    // IDs must not overwrite an earlier provider operation.
    for mailbox in &targets {
        store.ensure_mailbox(mailbox)?;
        match store.get_message(mailbox, message_id) {
            Err(Error::MessageNotFound) => {}
            Ok(_) => {
                return Err(Error::InvalidRequest(format!(
                    "message id {message_id} already exists"
                )))
            }
            Err(error) => return Err(error),
        }
    }

    let mut written = Vec::with_capacity(targets.len());
    let mut stored = Vec::with_capacity(targets.len().saturating_sub(1));
    for mailbox in &targets {
        match store.store_message(mailbox, message_id, message.clone()) {
            Ok(copy) => {
                written.push(mailbox.as_str());
                if mailbox != ALL_MAILBOX {
                    stored.push(copy);
                }
            }
            Err(error) => {
                // `store_message` can fail after making its target visible
                // (for example while writing a raw MIME sidecar), so include
                // the current target as well as every completed copy.
                let mut rollback_targets = written;
                rollback_targets.push(mailbox.as_str());
                let rollback_errors = rollback_targets
                    .into_iter()
                    .filter_map(|target| {
                        store
                            .delete_message(target, message_id)
                            .err()
                            .map(|rollback| format!("{target}: {rollback}"))
                    })
                    .collect::<Vec<_>>();
                if rollback_errors.is_empty() {
                    return Err(error);
                }
                return Err(Error::InternalError(format!(
                    "mail fan-out failed ({error}); rollback failed for {}",
                    rollback_errors.join(", ")
                )));
            }
        }
    }

    Ok(stored)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct FailAfterWriteStore {
        inner: FilesystemMailStore,
        fail_on_store: usize,
        store_calls: AtomicUsize,
    }

    impl MailStore for FailAfterWriteStore {
        fn store_message(
            &self,
            mailbox: &str,
            message_id: &str,
            message: Message,
        ) -> Result<StoredMessage> {
            let stored = self.inner.store_message(mailbox, message_id, message)?;
            if self.store_calls.fetch_add(1, Ordering::SeqCst) + 1 == self.fail_on_store {
                return Err(Error::InternalError("injected store failure".to_string()));
            }
            Ok(stored)
        }

        fn get_message(&self, mailbox: &str, message_id: &str) -> Result<StoredMessage> {
            self.inner.get_message(mailbox, message_id)
        }

        fn list_messages(
            &self,
            mailbox: &str,
            params: ListMessagesParams,
        ) -> Result<ListMessagesResult> {
            self.inner.list_messages(mailbox, params)
        }

        fn delete_message(&self, mailbox: &str, message_id: &str) -> Result<()> {
            self.inner.delete_message(mailbox, message_id)
        }

        fn delete_mailbox(&self, mailbox: &str) -> Result<()> {
            self.inner.delete_mailbox(mailbox)
        }

        fn update_delivery_status(
            &self,
            mailbox: &str,
            message_id: &str,
            status: DeliveryStatus,
        ) -> Result<()> {
            self.inner
                .update_delivery_status(mailbox, message_id, status)
        }

        fn list_mailboxes(&self) -> Result<Vec<MailboxInfo>> {
            self.inner.list_mailboxes()
        }

        fn ensure_mailbox(&self, mailbox: &str) -> Result<()> {
            self.inner.ensure_mailbox(mailbox)
        }
    }

    fn message_to(addresses: &[&str]) -> Message {
        Message {
            source_protocol: SourceProtocol::Smtp,
            from: Address::new("sender@example.com"),
            to: addresses.iter().map(|a| Address::new(*a)).collect(),
            cc: Vec::new(),
            bcc: Vec::new(),
            reply_to: Vec::new(),
            subject: "hi".to_string(),
            headers: std::collections::HashMap::new(),
            body_text: Some("hello".to_string()),
            body_html: None,
            attachments: Vec::new(),
            user_engagement_tracking_disabled: None,
            provider_metadata: std::collections::HashMap::new(),
            raw_mime: None,
            thread_id: None,
        }
    }

    #[test]
    fn should_reject_fan_out_when_message_has_no_recipients() {
        // Arrange
        // Act
        // Assert
        let store = FilesystemMailStore::open(temp_dir()).expect("store should open");
        let message = message_to(&[]);

        let err = fan_out(&store, &message).expect_err("fan-out without recipients should fail");

        assert!(matches!(err, Error::InvalidRequest(_)));
    }

    #[test]
    fn should_dedupe_mailboxes_when_recipient_appears_twice() {
        // Arrange
        // Act
        // Assert
        let store = FilesystemMailStore::open(temp_dir()).expect("store should open");
        let message = message_to(&["Bob@Example.com", "bob@example.com"]);

        let stored = fan_out(&store, &message).expect("fan-out should succeed");

        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].mailbox, "bob@example.com");
    }

    #[test]
    fn should_leave_no_captured_copies_when_fan_out_validation_fails() {
        // Arrange
        let store = FilesystemMailStore::open(temp_dir()).expect("store should open");
        let message = message_to(&["alice@example.com", ""]);

        // Act
        fan_out_with_id(&store, &message, "atomic-message")
            .expect_err("an empty recipient mailbox should fail before capture");

        // Assert
        assert!(store
            .list_messages(ALL_MAILBOX, ListMessagesParams::default())
            .expect("all mailbox should remain readable")
            .messages
            .is_empty());
        assert!(store
            .list_messages("alice@example.com", ListMessagesParams::default())
            .expect("recipient mailbox should remain readable")
            .messages
            .is_empty());
    }

    #[test]
    fn should_roll_back_every_personalization_when_a_batch_store_fails_after_writing() {
        // Arrange
        let store = FailAfterWriteStore {
            inner: FilesystemMailStore::open(temp_dir()).expect("store should open"),
            fail_on_store: 3,
            store_calls: AtomicUsize::new(0),
        };
        let messages = [
            message_to(&["alice@example.com"]),
            message_to(&["bob@example.com"]),
        ];

        // Act
        fan_out_batch(&store, &messages)
            .expect_err("the injected second-personalization failure should surface");

        // Assert
        for mailbox in [ALL_MAILBOX, "alice@example.com", "bob@example.com"] {
            assert!(store
                .list_messages(mailbox, ListMessagesParams::default())
                .expect("mailbox should remain readable")
                .messages
                .is_empty());
        }
    }

    fn temp_dir() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("sqrzl-mail-test-{}", uuid::Uuid::new_v4()));
        let _ = std::fs::create_dir_all(&dir);
        dir
    }
}
