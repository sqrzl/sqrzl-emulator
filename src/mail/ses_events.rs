//! Simulated SES v2 event publishing for captured mail.
//!
//! An admin request selects a captured SES message and an event type; Sqrzl
//! renders the documented event-publishing record (`eventType`, `mail`, and the
//! per-type detail object), stores it so tests can read it back, and, when the
//! message's configuration set has a destination, posts it as an SNS
//! `Notification` envelope to an allowlisted HTTP callback. SNS subscription
//! management, signing certificates, `EventBridge` and Firehose are out of scope.

use crate::error::{Error, Result};
use crate::mail::model::{DeliveryState, DeliveryStatus, SourceProtocol, StoredMessage};
use crate::mail::providers::{SES_CONFIGURATION_SET_METADATA, SES_EMAIL_TAGS_METADATA};
use crate::mail::{MailStore, ALL_MAILBOX};
use crate::sms::simulator::{send_http_callback, validate_callback_url_for, CallbackPolicy};
use chrono::{DateTime, SecondsFormat, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

pub(crate) const MAIL_CALLBACK_POLICY: CallbackPolicy = CallbackPolicy {
    allowed_hosts_env: "SQRZL_MAIL_CALLBACK_ALLOWED_HOSTS",
    timeout_env: "SQRZL_MAIL_CALLBACK_TIMEOUT_MS",
};

const ACCOUNT_ID: &str = "000000000000";
const REGION: &str = "us-east-1";

/// A bounded oldest-first page of simulated events.
#[derive(Debug, Serialize)]
pub struct SesEventPage {
    pub items: Vec<SesEventRecord>,
    pub next: Option<String>,
}

#[derive(Serialize, Deserialize)]
pub(crate) struct EventCursor {
    pub(crate) message_id: Option<String>,
    pub(crate) created_at: DateTime<Utc>,
    pub(crate) event_id: String,
}

pub(crate) fn event_page(
    records: impl Iterator<Item = Result<SesEventRecord>>,
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
    let marker = cursor.map(|cursor| (cursor.created_at, cursor.event_id));
    let mut selected = BTreeMap::new();
    for record in records {
        let record = record?;
        if message_id.is_some_and(|id| id != record.message_id) {
            continue;
        }
        let key = (record.created_at, record.event_id.clone());
        if marker.as_ref().is_some_and(|marker| &key <= marker) {
            continue;
        }
        if selected.len() == limit + 1 {
            if selected
                .last_key_value()
                .is_some_and(|(last, _)| &key >= last)
            {
                continue;
            }
            selected.pop_last();
        }
        selected.insert(key, record);
    }
    let more = selected.len() > limit;
    if more {
        selected.pop_last();
    }
    let next = if more {
        selected
            .last_key_value()
            .map(|((created_at, event_id), _)| {
                serde_json::to_string(&EventCursor {
                    message_id: message_id.map(str::to_owned),
                    created_at: *created_at,
                    event_id: event_id.clone(),
                })
            })
            .transpose()
            .map_err(|error| Error::InternalError(error.to_string()))?
    } else {
        None
    };
    Ok(SesEventPage {
        items: selected.into_values().collect(),
        next,
    })
}

#[cfg(test)]
thread_local! { static EVENT_DECODE_COUNT: std::cell::Cell<usize> = const { std::cell::Cell::new(0) }; }
#[cfg(test)]
fn count_event_decode<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<String, D::Error> {
    EVENT_DECODE_COUNT.with(|count| count.set(count.get() + 1));
    String::deserialize(deserializer)
}
#[cfg(test)]
pub(crate) fn take_event_decode_count() -> usize {
    EVENT_DECODE_COUNT.with(|count| count.replace(0))
}

/// SES event-publishing event types.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SesEventType {
    Send,
    Delivery,
    Bounce,
    Complaint,
    Reject,
    DeliveryDelay,
    #[serde(rename = "Rendering Failure")]
    RenderingFailure,
}

impl SesEventType {
    fn name(self) -> &'static str {
        match self {
            Self::Send => "Send",
            Self::Delivery => "Delivery",
            Self::Bounce => "Bounce",
            Self::Complaint => "Complaint",
            Self::Reject => "Reject",
            Self::DeliveryDelay => "DeliveryDelay",
            Self::RenderingFailure => "Rendering Failure",
        }
    }

    /// The record key that holds this event's detail object.
    fn detail_key(self) -> &'static str {
        match self {
            Self::Send => "send",
            Self::Delivery => "delivery",
            Self::Bounce => "bounce",
            Self::Complaint => "complaint",
            Self::Reject => "reject",
            Self::DeliveryDelay => "deliveryDelay",
            Self::RenderingFailure => "failure",
        }
    }
}

/// Where a configuration set's events are delivered.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SesEventDestination {
    pub configuration_set: String,
    pub callback_url: String,
    /// Event types delivered to the callback; empty means every type.
    #[serde(default)]
    pub event_types: Vec<SesEventType>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SesCallbackState {
    Pending,
    Succeeded,
    Failed,
}

/// One delivery of an event record to its callback.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SesCallbackAttempt {
    pub url: String,
    pub request_headers: HashMap<String, String>,
    pub request_body: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_status: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_body: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub state: SesCallbackState,
    pub attempted_at: DateTime<Utc>,
}

/// Whether every local update and callback outcome has been recorded.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SesEventState {
    Pending,
    #[default]
    Completed,
}

/// A simulated event-publishing record and its delivery outcome.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SesEventRecord {
    #[serde(default)]
    pub state: SesEventState,
    /// Stable caller-selected identity for safe retries.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    #[serde(default, skip_serializing, rename = "__sqrzl_request_fingerprint")]
    pub(crate) request_fingerprint: Option<String>,
    #[cfg_attr(test, serde(deserialize_with = "count_event_decode"))]
    pub event_id: String,
    pub message_id: String,
    pub event_type: SesEventType,
    /// The SES event-publishing record, as SES would publish it.
    pub record: Value,
    /// The SNS `Notification` envelope carrying `record`.
    pub notification: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub callback: Option<SesCallbackAttempt>,
    pub created_at: DateTime<Utc>,
}

/// Optional per-type detail for a simulated event.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SesEventRequest {
    #[serde(default)]
    pub request_id: Option<String>,
    pub message_id: String,
    pub event_type: SesEventType,
    /// Recipients the event applies to; defaults to every recipient.
    #[serde(default)]
    pub recipients: Vec<String>,
    #[serde(default)]
    pub bounce_type: Option<String>,
    #[serde(default)]
    pub bounce_sub_type: Option<String>,
    #[serde(default)]
    pub complaint_feedback_type: Option<String>,
    #[serde(default)]
    pub delay_type: Option<String>,
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default)]
    pub smtp_response: Option<String>,
}

impl SesEventRequest {
    fn validate(&self) -> Result<()> {
        if self.request_id.as_ref().is_some_and(|id| {
            id.is_empty()
                || id.len() > 128
                || !id
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        }) {
            return Err(Error::InvalidRequest(
                "request_id must contain 1-128 ASCII letters, digits, hyphens or underscores"
                    .to_string(),
            ));
        }
        let field_allowed = |field: &Option<String>, name: &str, applies: bool| {
            if field.is_some() && !applies {
                return Err(Error::InvalidRequest(format!(
                    "{name} does not apply to {} events",
                    self.event_type.name()
                )));
            }
            Ok(())
        };
        let kind = self.event_type;
        field_allowed(
            &self.bounce_type,
            "bounce_type",
            kind == SesEventType::Bounce,
        )?;
        field_allowed(
            &self.bounce_sub_type,
            "bounce_sub_type",
            kind == SesEventType::Bounce,
        )?;
        field_allowed(
            &self.complaint_feedback_type,
            "complaint_feedback_type",
            kind == SesEventType::Complaint,
        )?;
        field_allowed(
            &self.delay_type,
            "delay_type",
            kind == SesEventType::DeliveryDelay,
        )?;
        field_allowed(
            &self.reason,
            "reason",
            matches!(kind, SesEventType::Reject | SesEventType::RenderingFailure),
        )?;
        field_allowed(
            &self.smtp_response,
            "smtp_response",
            kind == SesEventType::Delivery,
        )?;
        if let Some(bounce_type) = &self.bounce_type {
            if !matches!(
                bounce_type.as_str(),
                "Undetermined" | "Permanent" | "Transient"
            ) {
                return Err(Error::InvalidRequest(
                    "bounce_type must be Undetermined, Permanent, or Transient".to_string(),
                ));
            }
        }
        Ok(())
    }
    fn fingerprint(&self) -> Result<String> {
        let mut payload =
            serde_json::to_value(self).map_err(|error| Error::InternalError(error.to_string()))?;
        payload
            .as_object_mut()
            .ok_or_else(|| {
                Error::InternalError("SES request serialization must produce an object".to_string())
            })?
            .remove("request_id");
        Ok(hex::encode(Sha256::digest(payload.to_string().as_bytes())))
    }
}

/// Validate a callback destination against the mail callback allowlist.
///
/// # Errors
/// Returns an error when the URL is not an allowlisted HTTP callback.
pub fn validate_destination_url(url: &str) -> Result<()> {
    validate_callback_url_for(MAIL_CALLBACK_POLICY, url)
}

pub struct SesEventSimulator {
    store: Arc<dyn MailStore>,
}

impl SesEventSimulator {
    #[must_use]
    pub fn new(store: Arc<dyn MailStore>) -> Self {
        Self { store }
    }

    /// Render, store, and (when configured) deliver one event record.
    ///
    /// # Errors
    /// Returns an error for unknown or non-SES messages, invalid detail, or
    /// persistence failures. Callback failures are recorded, not returned.
    pub async fn simulate(&self, request: SesEventRequest) -> Result<SesEventRecord> {
        request.validate()?;
        let request_id = request
            .request_id
            .clone()
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        let event_id = format!(
            "ses-event-{}",
            hex::encode(Sha256::digest(request_id.as_bytes()))
        );
        let fingerprint = request.fingerprint()?;
        match self.store.get_ses_event(&event_id) {
            Ok(event) => return replay(event, &fingerprint),
            Err(Error::MessageNotFound) => {}
            Err(error) => return Err(error),
        }
        let stored = self
            .store
            .get_message_case_insensitive(ALL_MAILBOX, &request.message_id)?;
        if stored.message.source_protocol != SourceProtocol::Ses {
            return Err(Error::InvalidRequest(
                "SES events can be simulated only for messages captured through SES SendEmail"
                    .to_string(),
            ));
        }
        let all_recipients = stored
            .message
            .recipients()
            .into_iter()
            .map(|address| address.email.clone())
            .collect::<Vec<_>>();
        let recipients = if request.recipients.is_empty() {
            all_recipients.clone()
        } else {
            for recipient in &request.recipients {
                if !all_recipients
                    .iter()
                    .any(|candidate| candidate.eq_ignore_ascii_case(recipient))
                {
                    return Err(Error::InvalidRequest(format!(
                        "{recipient} is not a recipient of message {}",
                        stored.message_id
                    )));
                }
            }
            request.recipients.clone()
        };

        let now = Utc::now();
        let record = render_record(&stored, &request, &recipients, now);
        let configuration_set = stored
            .message
            .provider_metadata
            .get(SES_CONFIGURATION_SET_METADATA)
            .and_then(Value::as_str)
            .map(str::to_string);
        let topic = configuration_set.as_deref().unwrap_or("sqrzl-ses-events");
        let notification = sns_notification(&record, topic, now);

        let destination = match configuration_set.as_deref() {
            Some(name) => match self.store.get_ses_event_destination(name) {
                Ok(destination) => Some(destination),
                Err(Error::MessageNotFound) => None,
                Err(error) => return Err(error),
            },
            None => None,
        };
        let callback = destination
            .filter(|destination| {
                destination.event_types.is_empty()
                    || destination.event_types.contains(&request.event_type)
            })
            .map(|destination| pending_callback(&destination.callback_url, &notification));
        let mut event = SesEventRecord {
            state: SesEventState::Pending,
            event_id,
            request_id: Some(request_id),
            request_fingerprint: Some(fingerprint.clone()),
            message_id: stored.message_id.clone(),
            event_type: request.event_type,
            record,
            notification,
            callback,
            created_at: now,
        };
        if let Some(existing) = self.store.claim_ses_event(&event)? {
            return replay(existing, &fingerprint);
        }
        // Persist intent and finish local delivery state before external effects.
        // Replaying pending intent never repeats an uncertain callback.
        self.apply_delivery_state(&stored, request.event_type, &recipients, now)?;
        if let Some(attempt) = event.callback.take() {
            event.callback = Some(deliver(attempt).await);
        }
        event.state = SesEventState::Completed;
        self.store.record_ses_event(&event)?;
        Ok(event)
    }

    /// Terminal outcomes update the captured copies' delivery state.
    fn apply_delivery_state(
        &self,
        stored: &StoredMessage,
        event_type: SesEventType,
        recipients: &[String],
        now: DateTime<Utc>,
    ) -> Result<()> {
        let state = match event_type {
            SesEventType::Delivery => DeliveryState::Delivered,
            SesEventType::Bounce => DeliveryState::Bounced,
            SesEventType::Reject | SesEventType::RenderingFailure => DeliveryState::Rejected,
            _ => return Ok(()),
        };
        let status = DeliveryStatus {
            state,
            detail: Some(format!("SES {} event", event_type.name())),
            updated_at: now,
        };
        let mailboxes = recipients
            .iter()
            .map(|recipient| recipient.trim().to_ascii_lowercase())
            .chain(std::iter::once(ALL_MAILBOX.to_string()));
        for mailbox in mailboxes {
            match self
                .store
                .update_delivery_status(&mailbox, &stored.message_id, status.clone())
            {
                Ok(()) | Err(Error::MessageNotFound) => {}
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }
}

fn timestamp(value: DateTime<Utc>) -> String {
    value.to_rfc3339_opts(SecondsFormat::Millis, true)
}

fn render_record(
    stored: &StoredMessage,
    request: &SesEventRequest,
    recipients: &[String],
    now: DateTime<Utc>,
) -> Value {
    let mail = render_mail(stored);
    let detail = render_detail(stored, request, recipients, now);
    let mut record = Map::new();
    record.insert(
        "eventType".to_string(),
        Value::String(request.event_type.name().to_string()),
    );
    record.insert("mail".to_string(), mail);
    record.insert(request.event_type.detail_key().to_string(), detail);
    Value::Object(record)
}

/// The `mail` object shared by every event type.
fn render_mail(stored: &StoredMessage) -> Value {
    let message = &stored.message;
    let source = message.from.email.clone();
    let from_domain = source.rsplit_once('@').map_or("", |(_, domain)| domain);
    let mut tags = BTreeMap::<String, Vec<String>>::new();
    tags.insert("ses:caller-identity".to_string(), vec!["sqrzl".to_string()]);
    tags.insert("ses:from-domain".to_string(), vec![from_domain.to_string()]);
    tags.insert("ses:source-ip".to_string(), vec!["127.0.0.1".to_string()]);
    if let Some(set) = message
        .provider_metadata
        .get(SES_CONFIGURATION_SET_METADATA)
        .and_then(Value::as_str)
    {
        tags.insert("ses:configuration-set".to_string(), vec![set.to_string()]);
    }
    if let Some(caller_tags) = message
        .provider_metadata
        .get(SES_EMAIL_TAGS_METADATA)
        .and_then(Value::as_array)
    {
        for tag in caller_tags {
            if let (Some(name), Some(value)) = (
                tag.get("Name").and_then(Value::as_str),
                tag.get("Value").and_then(Value::as_str),
            ) {
                tags.insert(name.to_string(), vec![value.to_string()]);
            }
        }
    }
    let destination = message
        .recipients()
        .into_iter()
        .map(|address| address.email.clone())
        .collect::<Vec<_>>();
    json!({
        "timestamp": timestamp(stored.received_at),
        "source": source,
        "sourceArn": format!("arn:aws:ses:{REGION}:{ACCOUNT_ID}:identity/{source}"),
        "sendingAccountId": ACCOUNT_ID,
        "messageId": stored.message_id,
        "destination": destination,
        "headersTruncated": false,
        "commonHeaders": {
            "from": [source],
            "to": message.to.iter().map(|address| address.email.clone()).collect::<Vec<_>>(),
            "messageId": stored.message_id,
            "subject": message.subject,
        },
        "tags": tags,
    })
}

/// The per-type detail object.
fn render_detail(
    stored: &StoredMessage,
    request: &SesEventRequest,
    recipients: &[String],
    now: DateTime<Utc>,
) -> Value {
    let at = timestamp(now);
    let feedback_id = format!("{}-sqrzl", uuid::Uuid::new_v4().simple());
    match request.event_type {
        SesEventType::Send => json!({}),
        SesEventType::Delivery => json!({
            "timestamp": at,
            "processingTimeMillis": (now - stored.received_at).num_milliseconds().max(0),
            "recipients": recipients,
            "smtpResponse": request.smtp_response.clone()
                .unwrap_or_else(|| "250 2.6.0 Message received".to_string()),
            "reportingMTA": "sqrzl.local",
        }),
        SesEventType::Bounce => json!({
            "bounceType": request.bounce_type.clone().unwrap_or_else(|| "Permanent".to_string()),
            "bounceSubType": request.bounce_sub_type.clone().unwrap_or_else(|| "General".to_string()),
            "bouncedRecipients": recipients.iter().map(|recipient| json!({
                "emailAddress": recipient,
                "action": "failed",
                "status": "5.1.1",
                "diagnosticCode": "smtp; 550 5.1.1 user unknown",
            })).collect::<Vec<_>>(),
            "timestamp": at,
            "feedbackId": feedback_id,
            "reportingMTA": "dsn; sqrzl.local",
        }),
        SesEventType::Complaint => json!({
            "complainedRecipients": recipients.iter()
                .map(|recipient| json!({"emailAddress": recipient}))
                .collect::<Vec<_>>(),
            "timestamp": at,
            "feedbackId": feedback_id,
            "userAgent": "sqrzl-emulator",
            "complaintFeedbackType": request.complaint_feedback_type.clone()
                .unwrap_or_else(|| "abuse".to_string()),
            "arrivalDate": at,
        }),
        SesEventType::Reject => json!({
            "reason": request.reason.clone().unwrap_or_else(|| "Bad content".to_string()),
        }),
        SesEventType::DeliveryDelay => json!({
            "timestamp": at,
            "delayType": request.delay_type.clone()
                .unwrap_or_else(|| "TransientCommunicationFailure".to_string()),
            "expirationTime": timestamp(stored.received_at + chrono::Duration::hours(14)),
            "delayedRecipients": recipients.iter().map(|recipient| json!({
                "emailAddress": recipient,
                "status": "4.4.1",
                "diagnosticCode": "smtp; 421 4.4.1 Unable to connect",
            })).collect::<Vec<_>>(),
        }),
        SesEventType::RenderingFailure => json!({
            "errorMessage": request.reason.clone()
                .unwrap_or_else(|| "Attribute 'name' is not present in the rendering data.".to_string()),
            "templateName": "sqrzl-template",
        }),
    }
}

fn sns_notification(record: &Value, topic: &str, now: DateTime<Utc>) -> Value {
    json!({
        "Type": "Notification",
        "MessageId": uuid::Uuid::new_v4().to_string(),
        "TopicArn": format!("arn:aws:sns:{REGION}:{ACCOUNT_ID}:{topic}"),
        "Message": record.to_string(),
        "Timestamp": timestamp(now),
        "SignatureVersion": "1",
        "Signature": "UNSIGNED-EMULATOR-NOTIFICATION",
        "SigningCertURL": "http://localhost/sqrzl-emulator-no-production-certificate",
        "UnsubscribeURL": "http://localhost/sqrzl-emulator-no-subscription-management",
    })
}

fn pending_callback(url: &str, notification: &Value) -> SesCallbackAttempt {
    let body = notification.to_string();
    let headers = HashMap::from([
        (
            "content-type".to_string(),
            "text/plain; charset=UTF-8".to_string(),
        ),
        (
            "x-amz-sns-message-type".to_string(),
            "Notification".to_string(),
        ),
        (
            "x-amz-sns-message-id".to_string(),
            notification["MessageId"]
                .as_str()
                .unwrap_or_default()
                .to_string(),
        ),
        (
            "x-amz-sns-topic-arn".to_string(),
            notification["TopicArn"]
                .as_str()
                .unwrap_or_default()
                .to_string(),
        ),
    ]);
    SesCallbackAttempt {
        url: url.to_string(),
        request_headers: headers,
        request_body: body,
        response_status: None,
        response_body: None,
        error: None,
        state: SesCallbackState::Pending,
        attempted_at: Utc::now(),
    }
}

async fn deliver(mut attempt: SesCallbackAttempt) -> SesCallbackAttempt {
    let url = &attempt.url;
    let body = &attempt.request_body;
    let mut headers = attempt.request_headers.clone();
    let response = send_http_callback(MAIL_CALLBACK_POLICY, url, &headers, body.as_bytes()).await;
    headers.insert("content-length".to_string(), body.len().to_string());
    let (response_status, response_body, error, state) = match response {
        Ok((status, response_body)) if (200..300).contains(&status) => (
            Some(status),
            Some(response_body),
            None,
            SesCallbackState::Succeeded,
        ),
        Ok((status, response_body)) => (
            Some(status),
            Some(response_body),
            Some(format!("callback returned HTTP {status}")),
            SesCallbackState::Failed,
        ),
        Err(error) => (None, None, Some(error), SesCallbackState::Failed),
    };
    attempt.request_headers = headers;
    attempt.response_status = response_status;
    attempt.response_body = response_body;
    attempt.error = error;
    attempt.state = state;
    attempt
}

fn replay(event: SesEventRecord, fingerprint: &str) -> Result<SesEventRecord> {
    if event.request_fingerprint.as_deref() == Some(fingerprint) {
        Ok(event)
    } else {
        Err(Error::InvalidRequest(
            "request_id was already used with a different SES event payload".to_string(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mail::model::{Address, Message};
    use crate::mail::{fan_out, FilesystemMailStore};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn store() -> Arc<dyn MailStore> {
        Arc::new(
            FilesystemMailStore::open(
                std::env::temp_dir().join(format!("sqrzl-ses-events-{}", uuid::Uuid::new_v4())),
            )
            .unwrap(),
        )
    }

    fn ses_message(configuration_set: Option<&str>) -> Message {
        let mut provider_metadata = HashMap::new();
        provider_metadata.insert(
            SES_EMAIL_TAGS_METADATA.to_string(),
            json!([{"Name": "order", "Value": "o-42"}]),
        );
        if let Some(set) = configuration_set {
            provider_metadata.insert(SES_CONFIGURATION_SET_METADATA.to_string(), json!(set));
        }
        Message {
            source_protocol: SourceProtocol::Ses,
            from: Address::new("sender@example.com"),
            to: vec![
                Address::new("one@example.com"),
                Address::new("two@example.com"),
            ],
            cc: Vec::new(),
            bcc: Vec::new(),
            reply_to: Vec::new(),
            subject: "events".to_string(),
            headers: HashMap::new(),
            body_text: Some("hello".to_string()),
            body_html: None,
            attachments: Vec::new(),
            user_engagement_tracking_disabled: None,
            provider_metadata,
            raw_mime: None,
            thread_id: None,
        }
    }

    fn request(message_id: &str, event_type: SesEventType) -> SesEventRequest {
        SesEventRequest {
            request_id: None,
            message_id: message_id.to_string(),
            event_type,
            recipients: Vec::new(),
            bounce_type: None,
            bounce_sub_type: None,
            complaint_feedback_type: None,
            delay_type: None,
            reason: None,
            smtp_response: None,
        }
    }

    async fn callback_receiver() -> (String, tokio::task::JoinHandle<String>) {
        callback_receiver_with(|| {}).await
    }

    async fn callback_receiver_with(
        before_response: impl FnOnce() + Send + 'static,
    ) -> (String, tokio::task::JoinHandle<String>) {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let port = listener.local_addr().unwrap().port();
        let receiver = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut bytes = Vec::new();
            let mut buffer = [0_u8; 4096];
            loop {
                let read = stream.read(&mut buffer).await.unwrap();
                if read == 0 {
                    break;
                }
                bytes.extend_from_slice(&buffer[..read]);
                let Some(header_end) = bytes.windows(4).position(|part| part == b"\r\n\r\n") else {
                    continue;
                };
                let head = String::from_utf8_lossy(&bytes[..header_end]).to_ascii_lowercase();
                let length = head
                    .lines()
                    .find_map(|line| line.strip_prefix("content-length: "))
                    .and_then(|value| value.parse::<usize>().ok())
                    .unwrap_or(0);
                if bytes.len() >= header_end + 4 + length {
                    break;
                }
            }
            before_response();
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok")
                .await
                .unwrap();
            String::from_utf8(bytes).unwrap()
        });
        (format!("http://127.0.0.1:{port}/ses"), receiver)
    }

    #[tokio::test]
    async fn should_post_sns_wrapped_bounce_record_with_caller_ses_tags() {
        // Arrange
        let store = store();
        let stored = fan_out(store.as_ref(), &ses_message(Some("events"))).unwrap();
        let message_id = stored[0].message_id.clone();
        let (url, receiver) = callback_receiver().await;
        let now = Utc::now();
        store
            .put_ses_event_destination(&SesEventDestination {
                configuration_set: "events".to_string(),
                callback_url: url,
                event_types: Vec::new(),
                created_at: now,
                updated_at: now,
            })
            .unwrap();
        let mut bounce = request(&message_id, SesEventType::Bounce);
        bounce.recipients = vec!["two@example.com".to_string()];
        bounce.bounce_type = Some("Transient".to_string());

        // Act
        let event = SesEventSimulator::new(store.clone())
            .simulate(bounce)
            .await
            .unwrap();

        // Assert
        let posted = receiver.await.unwrap();
        assert!(posted
            .to_ascii_lowercase()
            .contains("x-amz-sns-message-type: notification"));
        let body = &posted[posted.find("\r\n\r\n").unwrap() + 4..];
        let envelope: Value = serde_json::from_str(body).unwrap();
        assert_eq!(envelope["Type"], "Notification");
        assert_eq!(
            envelope["TopicArn"],
            "arn:aws:sns:us-east-1:000000000000:events"
        );
        let record: Value = serde_json::from_str(envelope["Message"].as_str().unwrap()).unwrap();
        assert_eq!(record["eventType"], "Bounce");
        assert_eq!(record["mail"]["messageId"], message_id.as_str());
        assert_eq!(record["mail"]["tags"]["order"], json!(["o-42"]));
        assert_eq!(
            record["mail"]["tags"]["ses:configuration-set"],
            json!(["events"])
        );
        assert_eq!(record["bounce"]["bounceType"], "Transient");
        assert_eq!(
            record["bounce"]["bouncedRecipients"][0]["emailAddress"],
            "two@example.com"
        );
        assert!(record["bounce"]["timestamp"].is_string());
        assert_eq!(
            event.callback.as_ref().unwrap().state,
            SesCallbackState::Succeeded
        );
        assert_eq!(store.list_ses_events(Some(&message_id)).unwrap().len(), 1);
        assert_eq!(
            store
                .get_message("two@example.com", &message_id)
                .unwrap()
                .delivery_status
                .state,
            DeliveryState::Bounced
        );
        assert_eq!(
            store
                .get_message("one@example.com", &message_id)
                .unwrap()
                .delivery_status
                .state,
            DeliveryState::Accepted
        );
    }

    #[tokio::test]
    async fn should_record_every_event_type_without_a_destination() {
        // Arrange
        let store = store();
        let message_id = fan_out(store.as_ref(), &ses_message(None)).unwrap()[0]
            .message_id
            .clone();
        let simulator = SesEventSimulator::new(store.clone());
        let types = [
            (SesEventType::Send, "send"),
            (SesEventType::Delivery, "delivery"),
            (SesEventType::Complaint, "complaint"),
            (SesEventType::Reject, "reject"),
            (SesEventType::DeliveryDelay, "deliveryDelay"),
            (SesEventType::RenderingFailure, "failure"),
        ];

        // Act
        let mut records = Vec::new();
        for (event_type, _) in types {
            records.push(
                simulator
                    .simulate(request(&message_id, event_type))
                    .await
                    .unwrap(),
            );
        }
        let mut mismatched = request(&message_id, SesEventType::Delivery);
        mismatched.bounce_type = Some("Permanent".to_string());
        let mut stranger = request(&message_id, SesEventType::Delivery);
        stranger.recipients = vec!["stranger@example.com".to_string()];

        // Assert
        for ((event_type, key), event) in types.iter().zip(&records) {
            assert_eq!(event.record["eventType"], event_type.name());
            assert!(event.record[*key].is_object(), "{key}");
            assert!(event.callback.is_none());
        }
        assert_eq!(records[5].record["eventType"], "Rendering Failure");
        assert!(simulator.simulate(mismatched).await.is_err());
        assert!(simulator.simulate(stranger).await.is_err());
        assert_eq!(store.list_ses_events(None).unwrap().len(), types.len());
    }
    #[tokio::test]
    async fn should_not_deliver_an_event_before_persistence_succeeds() {
        // Arrange
        let root = std::env::temp_dir().join(format!("sqrzl-ses-failure-{}", uuid::Uuid::new_v4()));
        let store: Arc<dyn MailStore> = Arc::new(FilesystemMailStore::open(&root).unwrap());
        let message_id = fan_out(store.as_ref(), &ses_message(Some("events"))).unwrap()[0]
            .message_id
            .clone();
        let (url, receiver) = callback_receiver().await;
        let now = Utc::now();
        store
            .put_ses_event_destination(&SesEventDestination {
                configuration_set: "events".to_string(),
                callback_url: url,
                event_types: Vec::new(),
                created_at: now,
                updated_at: now,
            })
            .unwrap();
        std::fs::write(root.join("_mail/.ses-events"), b"injected failure").unwrap();

        // Act
        let result = SesEventSimulator::new(store)
            .simulate(request(&message_id, SesEventType::Delivery))
            .await;
        let delivered = receiver.is_finished();
        receiver.abort();

        // Assert
        assert!(result.is_err());
        assert!(
            !delivered,
            "callback escaped before its event could be persisted"
        );
    }

    #[tokio::test]
    async fn should_replay_a_stable_ses_request_without_repeating_its_callback() {
        // Arrange
        let store = store();
        let message_id = fan_out(store.as_ref(), &ses_message(Some("events"))).unwrap()[0]
            .message_id
            .clone();
        let (url, receiver) = callback_receiver().await;
        let now = Utc::now();
        store
            .put_ses_event_destination(&SesEventDestination {
                configuration_set: "events".to_string(),
                callback_url: url,
                event_types: Vec::new(),
                created_at: now,
                updated_at: now,
            })
            .unwrap();
        let payload = json!({"request_id": "stable-request", "message_id": message_id, "event_type": "Delivery"});
        let simulator = SesEventSimulator::new(store.clone());

        // Act
        let first = simulator
            .simulate(serde_json::from_value(payload.clone()).unwrap())
            .await
            .unwrap();
        receiver.await.unwrap();
        let replay = simulator
            .simulate(serde_json::from_value(payload.clone()).unwrap())
            .await
            .unwrap();
        let mut conflicting = payload;
        conflicting["event_type"] = json!("Bounce");
        let conflict = simulator
            .simulate(serde_json::from_value(conflicting).unwrap())
            .await;

        // Assert
        assert_eq!(first.event_id, replay.event_id);
        assert_eq!(store.list_ses_events(Some(&message_id)).unwrap().len(), 1);
        assert_eq!(replay.callback.unwrap().state, SesCallbackState::Succeeded);
        assert!(conflict.is_err());
    }
    #[tokio::test]
    async fn should_preserve_pending_callback_identity_after_outcome_persistence_fails() {
        // Arrange
        let root = std::env::temp_dir().join(format!("sqrzl-ses-outcome-{}", uuid::Uuid::new_v4()));
        let store: Arc<dyn MailStore> = Arc::new(FilesystemMailStore::open(&root).unwrap());
        let message_id = fan_out(store.as_ref(), &ses_message(Some("events"))).unwrap()[0]
            .message_id
            .clone();
        let fail_root = root.clone();
        let (url, receiver) = callback_receiver_with(move || {
            std::fs::write(
                fail_root.join("_mail/.ses-writes-fail"),
                b"inject durable commit failure",
            )
            .unwrap();
        })
        .await;
        let now = Utc::now();
        store
            .put_ses_event_destination(&SesEventDestination {
                configuration_set: "events".to_string(),
                callback_url: url,
                event_types: Vec::new(),
                created_at: now,
                updated_at: now,
            })
            .unwrap();
        let mut event_request = request(&message_id, SesEventType::Delivery);
        event_request.request_id = Some("uncertain-outcome".to_string());

        // Act
        let first = SesEventSimulator::new(store)
            .simulate(event_request.clone())
            .await;
        receiver.await.unwrap();
        std::fs::remove_file(root.join("_mail/.ses-writes-fail")).unwrap();
        let reopened: Arc<dyn MailStore> = Arc::new(FilesystemMailStore::open(&root).unwrap());
        let replay = SesEventSimulator::new(reopened.clone())
            .simulate(event_request)
            .await
            .unwrap();

        // Assert
        assert!(first.is_err());
        assert_eq!(
            replay.callback.as_ref().unwrap().state,
            SesCallbackState::Pending
        );
        assert_eq!(serde_json::to_value(&replay).unwrap()["state"], "pending");
        assert_eq!(
            reopened.list_ses_events(Some(&message_id)).unwrap().len(),
            1
        );
    }

    #[tokio::test]
    async fn should_claim_one_callback_for_concurrent_stable_ses_requests() {
        // Arrange
        let store = store();
        let message_id = fan_out(store.as_ref(), &ses_message(Some("events"))).unwrap()[0]
            .message_id
            .clone();
        let (url, receiver) = callback_receiver().await;
        let now = Utc::now();
        store
            .put_ses_event_destination(&SesEventDestination {
                configuration_set: "events".to_string(),
                callback_url: url,
                event_types: Vec::new(),
                created_at: now,
                updated_at: now,
            })
            .unwrap();
        let mut event_request = request(&message_id, SesEventType::Delivery);
        event_request.request_id = Some("concurrent-request".to_string());
        let simulator = SesEventSimulator::new(store.clone());

        // Act
        let (first, second) = tokio::join!(
            simulator.simulate(event_request.clone()),
            simulator.simulate(event_request)
        );
        let first = first.unwrap();
        let second = second.unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(2), receiver)
            .await
            .unwrap()
            .unwrap();

        // Assert
        assert_eq!(first.event_id, second.event_id);
        assert_ne!(first.callback.unwrap().state, SesCallbackState::Failed);
        assert_ne!(second.callback.unwrap().state, SesCallbackState::Failed);
        assert_eq!(store.list_ses_events(Some(&message_id)).unwrap().len(), 1);
    }
    #[tokio::test]
    async fn should_keep_case_distinct_request_ids_independent_of_filesystem_case_rules() {
        // Arrange
        let store = store();
        let message_id = fan_out(store.as_ref(), &ses_message(None)).unwrap()[0]
            .message_id
            .clone();
        let mut first = request(&message_id, SesEventType::Send);
        first.request_id = Some("CaseSensitive".to_string());
        let mut second = first.clone();
        second.request_id = Some("casesensitive".to_string());
        let simulator = SesEventSimulator::new(store.clone());

        // Act
        let first = simulator.simulate(first).await.unwrap();
        let second = simulator.simulate(second).await.unwrap();

        // Assert
        assert_ne!(first.event_id, second.event_id);
        assert_eq!(store.list_ses_events(Some(&message_id)).unwrap().len(), 2);
    }
}
