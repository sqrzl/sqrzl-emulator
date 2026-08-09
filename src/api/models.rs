use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize)]
pub struct BucketInfo {
    pub name: String,
    pub created_at: String,
    pub versioning_enabled: bool,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct BucketDetails {
    pub name: String,
    pub created_at: String,
    pub versioning_enabled: bool,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ListBucketsResponse {
    pub items: Vec<BucketInfo>,
    pub next: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct MailboxInfo {
    pub address: String,
    pub message_count: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_received_at: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ListMailboxesResponse {
    pub items: Vec<MailboxInfo>,
    pub next: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct MailAddress {
    pub email: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct MessageAttachmentSummary {
    pub filename: String,
    pub content_type: String,
    #[serde(default)]
    pub size: usize,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ListMessagesResponse {
    pub items: Vec<MessageSummary>,
    pub next: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct MessageSummary {
    pub message_id: String,
    pub from: MailAddress,
    pub subject: String,
    pub received_at: String,
    pub delivery_state: crate::mail::model::DeliveryState,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub to: Vec<MailAddress>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct MessageDetail {
    pub message_id: String,
    pub mailbox: String,
    pub from: MailAddress,
    pub to: Vec<MailAddress>,
    pub cc: Vec<MailAddress>,
    pub bcc: Vec<MailAddress>,
    pub subject: String,
    pub received_at: String,
    pub source_protocol: crate::mail::model::SourceProtocol,
    pub delivery_state: crate::mail::model::DeliveryState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delivery_detail: Option<String>,
    #[serde(default)]
    pub headers: std::collections::HashMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body_text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body_html: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<String>,
    #[serde(default)]
    pub attachments: Vec<MessageAttachmentSummary>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SuccessResponse {
    pub success: bool,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct AdminSessionResponse {
    pub mode: String,
    pub username: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct VersioningStatus {
    pub enabled: bool,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ObjectInfo {
    pub key: String,
    pub size: u64,
    pub last_modified: String,
    pub etag: String,
    pub content_type: Option<String>,
    pub storage_class: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ObjectFolderInfo {
    pub name: String,
    pub prefix: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ObjectMetadata {
    pub key: String,
    pub size: u64,
    pub last_modified: String,
    pub etag: String,
    pub content_type: Option<String>,
    pub metadata: std::collections::HashMap<String, String>,
    pub version_id: Option<String>,
    pub storage_class: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ListObjectsResponse {
    pub folders: Vec<ObjectFolderInfo>,
    pub items: Vec<ObjectInfo>,
    pub next: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ObjectVersionInfo {
    pub key: String,
    pub version_id: String,
    pub size: u64,
    pub last_modified: String,
    pub etag: String,
    pub is_latest: bool,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ListVersionsResponse {
    pub items: Vec<ObjectVersionInfo>,
    pub next: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ListMultipartUploadsResponse {
    pub items: Vec<crate::models::MultipartUpload>,
    pub next: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct TagsResponse {
    pub tags: std::collections::HashMap<String, String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct TagsRequest {
    pub tags: std::collections::HashMap<String, String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ErrorResponse {
    pub error: String,
    pub code: String,
    pub details: Option<String>,
}
