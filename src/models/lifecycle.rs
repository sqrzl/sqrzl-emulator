use serde::{Deserialize, Serialize};

/// Lifecycle configuration for automatic object management
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct LifecycleConfiguration {
    pub rules: Vec<Rule>,
}

/// A single lifecycle rule
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Rule {
    pub id: Option<String>,
    pub status: Status,
    pub filter: Option<Filter>,
    pub expiration: Option<Expiration>,
    pub noncurrent_version_expiration: Option<NoncurrentVersionExpiration>,
    pub transitions: Vec<Transition>,
}

/// Rule status: Enabled or Disabled
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum Status {
    Enabled,
    Disabled,
}

/// Filter for selecting objects to apply the rule to
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Filter {
    pub prefix: Option<String>,
    pub tags: Vec<Tag>,
}

/// Tag filter for lifecycle rules
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Tag {
    pub key: String,
    pub value: String,
}

/// Expiration action - when to delete objects
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Expiration {
    pub days: Option<u32>,
    pub date: Option<String>, // ISO 8601 format
    pub expired_object_delete_marker: Option<bool>,
}

/// Noncurrent version expiration action - when to delete stale versions
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NoncurrentVersionExpiration {
    pub noncurrent_days: u32,
}

/// Transition action - when to move objects to different storage class
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Transition {
    pub days: Option<u32>,
    pub date: Option<String>, // ISO 8601 format
    pub storage_class: StorageClass,
}

/// Storage classes supported by Wasabi/S3
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum StorageClass {
    #[serde(rename = "STANDARD")]
    Standard,
    #[serde(rename = "GLACIER")]
    Glacier,
    #[serde(rename = "DEEP_ARCHIVE")]
    DeepArchive,
}

impl Filter {
    #[must_use]
    pub fn matches(&self, key: &str, tags: &std::collections::HashMap<String, String>) -> bool {
        // Check prefix match
        if let Some(prefix) = &self.prefix {
            if !key.starts_with(prefix) {
                return false;
            }
        }

        // Check tag matches (all tags must match)
        for tag in &self.tags {
            match tags.get(&tag.key) {
                Some(value) if value == &tag.value => {}
                _ => return false,
            }
        }

        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn should_match_prefix_given_matching_key() {
        // Arrange
        let filter = Filter {
            prefix: Some("logs/".to_string()),
            tags: vec![],
        };

        // Act
        // Assert
        assert!(filter.matches("logs/2024/01/file.txt", &HashMap::new()));
        assert!(!filter.matches("data/file.txt", &HashMap::new()));
    }

    #[test]
    fn should_match_tags_given_matching_tags() {
        // Arrange
        let filter = Filter {
            prefix: None,
            tags: vec![Tag {
                key: "env".to_string(),
                value: "prod".to_string(),
            }],
        };
        let mut tags = HashMap::new();
        tags.insert("env".to_string(), "prod".to_string());

        // Act
        // Assert
        assert!(filter.matches("any/key", &tags));

        tags.insert("env".to_string(), "dev".to_string());
        assert!(!filter.matches("any/key", &tags));
    }

    #[test]
    fn should_match_prefix_with_tags() {
        // Arrange
        let filter = Filter {
            prefix: Some("logs/".to_string()),
            tags: vec![Tag {
                key: "type".to_string(),
                value: "access".to_string(),
            }],
        };
        let mut tags = HashMap::new();
        tags.insert("type".to_string(), "access".to_string());

        // Act
        // Assert
        assert!(filter.matches("logs/access.log", &tags));
        assert!(!filter.matches("data/access.log", &tags));

        // Wrong tags - should not match even though prefix matches
        let mut wrong_tags = HashMap::new();
        wrong_tags.insert("type".to_string(), "error".to_string());
        assert!(!filter.matches("logs/error.log", &wrong_tags));
    }
}
