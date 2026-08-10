//! Domain model for captured email messages.
//!
//! This mirrors the *shape* of `crate::models` (plain serde-friendly structs) but
//! is deliberately independent of it: mail has no bucket/key concept.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Which source protocol/provider a captured message arrived through.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceProtocol {
    Smtp,
    SendGrid,
    Ses,
    Acs,
}

/// A single named email address (e.g. `"Alice" <alice@example.com>`, or a bare address).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Address {
    pub email: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

impl Address {
    #[must_use]
    pub fn new(email: impl Into<String>) -> Self {
        Self {
            email: email.into(),
            name: None,
        }
    }

    /// The lowercased, trimmed address used as a mailbox key.
    #[must_use]
    pub fn mailbox_key(&self) -> String {
        self.email.trim().to_ascii_lowercase()
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Attachment {
    pub filename: String,
    pub content_type: String,
    pub content: Vec<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disposition: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_id: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeliveryState {
    Accepted,
    Delivered,
    Bounced,
    Rejected,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DeliveryStatus {
    pub state: DeliveryState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    pub updated_at: DateTime<Utc>,
}

impl DeliveryStatus {
    #[must_use]
    pub fn accepted(now: DateTime<Utc>) -> Self {
        Self {
            state: DeliveryState::Accepted,
            detail: None,
            updated_at: now,
        }
    }
}

/// A captured email message, as submitted by a source protocol/provider, before it
/// is fanned out into per-recipient mailbox copies (see [`crate::mail::fan_out`]).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Message {
    pub source_protocol: SourceProtocol,
    pub from: Address,
    #[serde(default)]
    pub to: Vec<Address>,
    #[serde(default)]
    pub cc: Vec<Address>,
    #[serde(default)]
    pub bcc: Vec<Address>,
    #[serde(default)]
    pub reply_to: Vec<Address>,
    #[serde(default)]
    pub subject: String,
    #[serde(default)]
    pub headers: HashMap<String, String>,
    #[serde(default)]
    pub body_text: Option<String>,
    #[serde(default)]
    pub body_html: Option<String>,
    #[serde(default)]
    pub attachments: Vec<Attachment>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_engagement_tracking_disabled: Option<bool>,
    #[serde(default)]
    pub provider_metadata: HashMap<String, serde_json::Value>,
    /// The full captured payload, where the source protocol makes one available
    /// (SMTP DATA), for raw download/inspection.
    #[serde(default)]
    pub raw_mime: Option<Vec<u8>>,
    #[serde(default)]
    pub thread_id: Option<String>,
}

impl Message {
    /// All recipients (To + Cc + Bcc) — the set of mailboxes this message fans out into.
    #[must_use]
    pub fn recipients(&self) -> Vec<&Address> {
        self.to
            .iter()
            .chain(self.cc.iter())
            .chain(self.bcc.iter())
            .collect()
    }
}

/// A message as filed in one mailbox (a recipient's mailbox, or the synthetic
/// [`crate::mail::ALL_MAILBOX`] outbox-wide view).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StoredMessage {
    pub message_id: String,
    pub mailbox: String,
    pub message: Message,
    pub delivery_status: DeliveryStatus,
    pub received_at: DateTime<Utc>,
}

#[derive(Clone, Debug, Default)]
pub struct ListMessagesParams {
    pub marker: Option<String>,
    pub limit: Option<usize>,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct ListMessagesResult {
    pub messages: Vec<StoredMessage>,
    pub next_marker: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MailboxInfo {
    pub address: String,
    pub message_count: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_received_at: Option<DateTime<Utc>>,
}
