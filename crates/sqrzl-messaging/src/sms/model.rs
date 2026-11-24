use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SmsProvider {
    Twilio,
    Sns,
    AwsSmsVoiceV2,
    Acs,
}

impl SmsProvider {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Twilio => "twilio",
            Self::Sns => "sns",
            Self::AwsSmsVoiceV2 => "aws-sms-voice-v2",
            Self::Acs => "acs",
        }
    }
}

impl std::str::FromStr for SmsProvider {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.trim().to_ascii_lowercase().as_str() {
            "twilio" => Ok(Self::Twilio),
            "sns" => Ok(Self::Sns),
            "aws-sms-voice-v2" | "sms-voice" | "pinpoint-sms-voice-v2" => Ok(Self::AwsSmsVoiceV2),
            "acs" | "azure" => Ok(Self::Acs),
            _ => Err(format!("unsupported SMS provider: {value}")),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SmsDirection {
    Inbound,
    Outbound,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SmsChannel {
    Sms,
    Mms,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SmsDeliveryState {
    Accepted,
    Delivered,
    Failed,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SmsMedia {
    pub media_id: String,
    pub filename: String,
    pub content_type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub external_url: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SmsMessage {
    pub message_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub batch_id: Option<String>,
    pub provider: SmsProvider,
    pub provider_message_id: String,
    pub direction: SmsDirection,
    pub channel: SmsChannel,
    pub from: String,
    pub to: String,
    #[serde(default)]
    pub body: String,
    #[serde(default)]
    pub media: Vec<SmsMedia>,
    #[serde(default)]
    pub metadata: HashMap<String, Value>,
    pub peer: String,
    pub delivery_state: SmsDeliveryState,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Clone, Debug)]
pub struct NewSmsMessage {
    pub batch_id: Option<String>,
    pub provider: SmsProvider,
    pub provider_message_id: Option<String>,
    pub direction: SmsDirection,
    pub channel: SmsChannel,
    pub from: String,
    pub to: String,
    pub body: String,
    pub media: Vec<NewSmsMedia>,
    pub metadata: HashMap<String, Value>,
}

#[derive(Clone, Debug)]
pub struct NewSmsMedia {
    pub filename: String,
    pub content_type: String,
    pub content: Option<Vec<u8>>,
    pub external_url: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SmsConversation {
    pub peer: String,
    pub message_count: usize,
    pub last_message_at: DateTime<Utc>,
    pub last_message_body: String,
    pub last_direction: SmsDirection,
    pub provider: SmsProvider,
}

#[derive(Clone, Debug, Default)]
pub struct ListSmsParams {
    pub marker: Option<String>,
    pub limit: Option<usize>,
}

#[derive(Clone, Debug, Default)]
pub struct ListSmsMessagesResult {
    pub messages: Vec<SmsMessage>,
    pub next_marker: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TextDestination {
    pub provider: SmsProvider,
    pub local_number: String,
    pub callback_url: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CallbackKind {
    Inbound,
    Delivery,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CallbackAttemptState {
    Succeeded,
    Failed,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CallbackAttempt {
    pub attempt_id: String,
    pub message_id: String,
    pub kind: CallbackKind,
    pub provider: SmsProvider,
    pub url: String,
    pub request_headers: HashMap<String, String>,
    pub request_body: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_status: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_body: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub state: CallbackAttemptState,
    pub attempted_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_of: Option<String>,
}

#[must_use]
pub fn is_e164(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.len() >= 8
        && bytes.len() <= 16
        && bytes.first() == Some(&b'+')
        && bytes[1..].iter().all(u8::is_ascii_digit)
}

#[must_use]
pub fn valid_sender(value: &str) -> bool {
    !value.trim().is_empty()
        && value.len() <= 64
        && !value.contains(['\r', '\n'])
        && (is_e164(value)
            || value
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, ' ' | '-' | '_')))
}
