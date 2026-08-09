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

    let message_id = generate_message_id();
    store.ensure_mailbox(ALL_MAILBOX)?;
    store.store_message(ALL_MAILBOX, &message_id, message.clone())?;

    let mut stored = Vec::with_capacity(mailboxes.len());
    for mailbox in mailboxes {
        store.ensure_mailbox(&mailbox)?;
        stored.push(store.store_message(&mailbox, &message_id, message.clone())?);
    }

    Ok(stored)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message_to(addresses: &[&str]) -> Message {
        Message {
            source_protocol: SourceProtocol::Smtp,
            from: Address::new("sender@example.com"),
            to: addresses.iter().map(|a| Address::new(*a)).collect(),
            cc: Vec::new(),
            bcc: Vec::new(),
            subject: "hi".to_string(),
            headers: std::collections::HashMap::new(),
            body_text: Some("hello".to_string()),
            body_html: None,
            attachments: Vec::new(),
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

    fn temp_dir() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("sqrzl-mail-test-{}", uuid::Uuid::new_v4()));
        let _ = std::fs::create_dir_all(&dir);
        dir
    }
}
