use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Bucket {
    pub name: String,
    pub created_at: DateTime<Utc>,
    pub versioning_enabled: bool,
    pub policy: Option<BucketPolicy>,
    pub lifecycle_rules: Vec<LifecycleRule>,
    pub metadata: HashMap<String, String>,
    #[serde(default)]
    pub acl: Option<crate::models::policy::Acl>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BucketPolicy {
    pub version: String,
    pub statements: Vec<PolicyStatement>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PolicyStatement {
    pub effect: String,        // "Allow" or "Deny"
    pub principal: String,     // "*" or specific principal
    pub action: Vec<String>,   // e.g., ["s3:GetObject", "s3:PutObject"]
    pub resource: Vec<String>, // e.g., ["arn:aws:s3:::bucket/*"]
    pub condition: Option<HashMap<String, HashMap<String, Vec<String>>>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LifecycleRule {
    pub id: String,
    pub prefix: String,
    pub status: String, // "Enabled" or "Disabled"
    pub expiration: Option<LifecycleExpiration>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LifecycleExpiration {
    pub days: u32,
    pub date: Option<DateTime<Utc>>,
    pub expired_object_delete_marker: bool,
}

impl Bucket {
    #[must_use]
    pub fn new(name: String) -> Self {
        Self {
            name,
            created_at: Utc::now(),
            versioning_enabled: false,
            policy: None,
            lifecycle_rules: Vec::new(),
            metadata: HashMap::new(),
            acl: None,
        }
    }
}
