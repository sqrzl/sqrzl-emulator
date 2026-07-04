use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MultipartUpload {
    pub upload_id: String,
    pub key: String,
    pub initiated: DateTime<Utc>,
    #[serde(default)]
    pub content_type: Option<String>,
    #[serde(default)]
    pub metadata: HashMap<String, String>,
    #[serde(default)]
    pub provider_metadata: HashMap<String, String>,
    pub parts: Vec<Part>,
    #[serde(default, skip_serializing)]
    pub part_data: HashMap<u32, Vec<u8>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Part {
    pub part_number: u32,
    pub etag: String,
    pub size: u64,
    pub last_modified: DateTime<Utc>,
}

impl MultipartUpload {
    #[must_use]
    pub fn new(
        key: String,
        content_type: Option<String>,
        metadata: HashMap<String, String>,
        provider_metadata: HashMap<String, String>,
    ) -> Self {
        Self {
            upload_id: Uuid::new_v4().to_string(),
            key,
            initiated: Utc::now(),
            content_type,
            metadata,
            provider_metadata,
            parts: Vec::new(),
            part_data: HashMap::new(),
        }
    }
}
