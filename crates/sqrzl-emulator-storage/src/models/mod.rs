use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

pub mod bucket;
pub mod lifecycle;
pub mod multipart;
pub mod object;
pub mod policy;

pub use bucket::Bucket;
pub use lifecycle::{
    Expiration, Filter, LifecycleConfiguration, NoncurrentVersionExpiration, Rule, Status,
    StorageClass, Transition,
};
pub use multipart::{MultipartUpload, Part};
pub use object::Object;
pub use policy::{Acl, BucketPolicy, CannedAcl, Owner};

/// Paginated list results
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ListObjectsResult {
    #[serde(default)]
    pub common_prefixes: Vec<String>,
    pub objects: Vec<Object>,
    pub is_truncated: bool,
    pub next_marker: Option<String>,
}

/// S3 Object metadata
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ObjectMetadata {
    pub key: String,
    pub size: u64,
    pub etag: String,
    pub content_type: String,
    pub last_modified: DateTime<Utc>,
    pub version_id: Option<String>,
    pub storage_class: String,
    pub metadata: HashMap<String, String>,
}

/// Versioning status for a bucket
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
pub enum VersioningStatus {
    Enabled,
    Suspended,
    NotSet,
}

impl std::fmt::Display for VersioningStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            VersioningStatus::Enabled => write!(f, "Enabled"),
            VersioningStatus::Suspended => write!(f, "Suspended"),
            VersioningStatus::NotSet => write!(f, ""),
        }
    }
}
