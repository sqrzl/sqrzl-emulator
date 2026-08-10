use crate::error::Error;
use crate::models::{
    lifecycle::{NoncurrentVersionExpiration, Rule},
    LifecycleConfiguration, Status, StorageClass, Transition,
};
use crate::storage::Storage;
use chrono::{DateTime, NaiveDate, Utc};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tracing::{debug, error, info};

const GCS_SOFT_DELETE_SECONDS_KEY: &str = "gcs_soft_delete_seconds";
const GCS_RETENTION_SECONDS_KEY: &str = "gcs_retention_seconds";
const AZURE_VERSIONING_KEY: &str = "azure_versioning_enabled";
const AZURE_SOFT_DELETE_DAYS_KEY: &str = "azure_soft_delete_days";
const S3_OBJECT_LOCK_MODE_KEY: &str = "s3_object_lock_mode";
const S3_OBJECT_LOCK_UNTIL_KEY: &str = "s3_object_lock_until";
const S3_OBJECT_LOCK_LEGAL_HOLD_KEY: &str = "s3_object_lock_legal_hold";
const AZURE_IMMUTABILITY_UNTIL_KEY: &str = "azure_immutability_until";
const AZURE_LEGAL_HOLD_KEY: &str = "azure_legal_hold";

fn positive_duration_or_invalid(metadata: &HashMap<String, String>, key: &str) -> bool {
    metadata
        .get(key)
        .is_some_and(|value| value.parse::<u64>().map_or(true, |value| value > 0))
}

fn foreign_provider_data_protection_active(
    storage: &dyn Storage,
    bucket: &str,
) -> Result<bool, Error> {
    let bucket = storage.get_bucket(bucket)?;
    Ok(
        positive_duration_or_invalid(&bucket.metadata, GCS_SOFT_DELETE_SECONDS_KEY)
            || positive_duration_or_invalid(&bucket.metadata, GCS_RETENTION_SECONDS_KEY)
            || bucket
                .metadata
                .get(AZURE_VERSIONING_KEY)
                .is_some_and(|value| !value.eq_ignore_ascii_case("false"))
            || positive_duration_or_invalid(&bucket.metadata, AZURE_SOFT_DELETE_DAYS_KEY),
    )
}

fn parse_lock_timestamp(value: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(value)
        .or_else(|_| DateTime::parse_from_rfc2822(value))
        .ok()
        .map(|value| value.with_timezone(&Utc))
}

fn object_has_active_data_protection(object: &crate::models::Object, now: DateTime<Utc>) -> bool {
    let s3_legal_hold = object
        .provider_metadata
        .get(S3_OBJECT_LOCK_LEGAL_HOLD_KEY)
        .is_some_and(|value| !value.eq_ignore_ascii_case("OFF"));
    let s3_retention = object
        .provider_metadata
        .get(S3_OBJECT_LOCK_UNTIL_KEY)
        .is_some_and(|value| parse_lock_timestamp(value).is_none_or(|until| until > now));
    let incomplete_s3_retention = object
        .provider_metadata
        .contains_key(S3_OBJECT_LOCK_MODE_KEY)
        && !object
            .provider_metadata
            .contains_key(S3_OBJECT_LOCK_UNTIL_KEY);
    let azure_legal_hold = object
        .provider_metadata
        .get(AZURE_LEGAL_HOLD_KEY)
        .is_some_and(|value| !value.eq_ignore_ascii_case("false"));
    let azure_retention = object
        .provider_metadata
        .get(AZURE_IMMUTABILITY_UNTIL_KEY)
        .is_some_and(|value| parse_lock_timestamp(value).is_none_or(|until| until > now));

    s3_legal_hold || s3_retention || incomplete_s3_retention || azure_legal_hold || azure_retention
}

/// Check if an object should be deleted due to lifecycle rules
/// This is called eagerly when accessing objects to enforce expiration immediately
///
/// # Errors
///
/// Returns an error when the underlying emulator operation fails.
pub fn check_object_expiration(
    storage: &Arc<dyn Storage>,
    bucket: &str,
    key: &str,
) -> Result<bool, Error> {
    let now = Utc::now();

    // Get lifecycle configuration for this bucket
    let config = match storage.get_bucket_lifecycle(bucket) {
        Ok(cfg) => cfg,
        Err(Error::KeyNotFound) => return Ok(false), // No lifecycle config
        Err(e) => {
            error!(
                "Failed to get lifecycle config for bucket {}: {}",
                bucket, e
            );
            return Err(e);
        }
    };

    if foreign_provider_data_protection_active(storage.as_ref(), bucket)? {
        debug!(
            bucket = bucket,
            "Skipping S3 lifecycle expiration for a foreign-provider protected bucket"
        );
        return Ok(false);
    }

    // Get the object to check expiration
    let object = storage.get_object(bucket, key)?;
    if object_has_active_data_protection(&object, now) {
        debug!(
            bucket = bucket,
            key = key,
            "Skipping S3 lifecycle expiration for a protected object"
        );
        return Ok(false);
    }

    // Get object tags
    let tags = storage.get_object_tags(bucket, key).unwrap_or_default();

    for rule in &config.rules {
        // Skip disabled rules
        if rule.status != Status::Enabled {
            continue;
        }

        if !rule_matches_filter(rule, key, &tags) {
            continue;
        }

        // Apply expiration action
        if let Some(expiration) = &rule.expiration {
            if should_expire(object.last_modified, expiration, now) {
                info!(
                    bucket = bucket,
                    key = key,
                    rule_id = rule.id.as_deref().unwrap_or("unnamed"),
                    "Expiring object (eager check)"
                );

                // Delete the object
                storage.delete_object(bucket, key)?;
                return Ok(true);
            }
        }
    }

    Ok(false)
}

fn rule_matches_filter(
    rule: &crate::models::lifecycle::Rule,
    key: &str,
    tags: &HashMap<String, String>,
) -> bool {
    rule.filter
        .as_ref()
        .is_none_or(|filter| filter.matches(key, tags))
}

fn should_expire(
    last_modified: DateTime<Utc>,
    expiration: &crate::models::lifecycle::Expiration,
    now: DateTime<Utc>,
) -> bool {
    let object_date = last_modified;

    // Check days-based expiration
    if let Some(days) = expiration.days {
        let age_days = (now - object_date).num_days();
        if age_days >= i64::from(days) {
            return true;
        }
    }

    // Check date-based expiration (ISO 8601 format: YYYY-MM-DD)
    if let Some(date_str) = &expiration.date {
        if let Ok(expire_date) = NaiveDate::parse_from_str(date_str, "%Y-%m-%d") {
            let expire_datetime = expire_date.and_hms_opt(0, 0, 0).map(|dt| dt.and_utc());

            if let Some(expire_dt) = expire_datetime {
                if now >= expire_dt {
                    return true;
                }
            }
        }
    }

    false
}

fn should_expire_noncurrent_version(
    last_modified: DateTime<Utc>,
    noncurrent_days: u32,
    now: DateTime<Utc>,
) -> bool {
    (now - last_modified).num_days() >= i64::from(noncurrent_days)
}

fn should_transition(
    last_modified: DateTime<Utc>,
    transition: &Transition,
    now: DateTime<Utc>,
) -> bool {
    if let Some(days) = transition.days {
        if (now - last_modified).num_days() >= i64::from(days) {
            return true;
        }
    }

    if let Some(date_str) = &transition.date {
        if let Ok(transition_date) = NaiveDate::parse_from_str(date_str, "%Y-%m-%d") {
            if let Some(transition_datetime) =
                transition_date.and_hms_opt(0, 0, 0).map(|dt| dt.and_utc())
            {
                if now >= transition_datetime {
                    return true;
                }
            }
        }
    }

    false
}

fn storage_class_to_str(storage_class: &StorageClass) -> &'static str {
    match storage_class {
        StorageClass::Standard => "STANDARD",
        StorageClass::Glacier => "GLACIER",
        StorageClass::DeepArchive => "DEEP_ARCHIVE",
    }
}

/// Background job that executes lifecycle rules periodically
pub struct LifecycleExecutor {
    storage: Arc<dyn Storage>,
    interval: Duration,
}

struct NoncurrentVersionGroup {
    key: String,
    current_version_id: Option<String>,
    versions: Vec<crate::models::Object>,
}

impl LifecycleExecutor {
    /// Create a new lifecycle executor with the specified interval.
    pub fn new(storage: Arc<dyn Storage>, interval: Duration) -> Self {
        Self { storage, interval }
    }

    /// Start the lifecycle executor as a background task
    #[must_use]
    pub fn start(self) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            info!(
                "Lifecycle executor started with interval: {:?}",
                self.interval
            );

            loop {
                tokio::time::sleep(self.interval).await;

                if let Err(e) = self.execute_lifecycle_rules() {
                    error!("Failed to execute lifecycle rules: {}", e);
                }
            }
        })
    }

    fn execute_lifecycle_rules(&self) -> Result<(), Error> {
        debug!("Executing lifecycle rules...");
        let now = Utc::now();

        // Get all buckets
        let buckets = tokio::task::block_in_place(|| self.storage.list_buckets())?;

        for bucket in buckets {
            // Get lifecycle configuration for this bucket
            let config = match tokio::task::block_in_place(|| {
                self.storage.get_bucket_lifecycle(&bucket.name)
            }) {
                Ok(cfg) => cfg,
                Err(Error::KeyNotFound) => continue, // No lifecycle config
                Err(e) => {
                    error!(
                        "Failed to get lifecycle config for bucket {}: {}",
                        bucket.name, e
                    );
                    continue;
                }
            };

            self.apply_lifecycle_rules(&bucket.name, &config, now)?;
        }

        debug!("Lifecycle rules execution completed");
        Ok(())
    }

    fn apply_lifecycle_rules(
        &self,
        bucket_name: &str,
        config: &LifecycleConfiguration,
        now: DateTime<Utc>,
    ) -> Result<(), Error> {
        if foreign_provider_data_protection_active(self.storage.as_ref(), bucket_name)? {
            debug!(
                bucket = bucket_name,
                "Skipping S3 lifecycle rules for a foreign-provider protected bucket"
            );
            return Ok(());
        }

        for rule in &config.rules {
            // Skip disabled rules
            if rule.status != Status::Enabled {
                continue;
            }

            debug!(
                bucket = bucket_name,
                rule_id = rule.id.as_deref().unwrap_or("unnamed"),
                "Applying lifecycle rule"
            );

            self.apply_current_object_rule(bucket_name, rule, now)?;
            self.apply_noncurrent_version_rule(bucket_name, rule, now)?;
        }

        Ok(())
    }

    fn apply_current_object_rule(
        &self,
        bucket_name: &str,
        rule: &Rule,
        now: DateTime<Utc>,
    ) -> Result<(), Error> {
        let result = tokio::task::block_in_place(|| {
            self.storage
                .list_objects(bucket_name, None, None, None, None)
        })?;

        for object in result.objects {
            let tags = self.object_tags(bucket_name, &object.key);
            if !rule_matches_filter(rule, &object.key, &tags) {
                continue;
            }

            if let Some(expiration) = &rule.expiration {
                if should_expire(object.last_modified, expiration, now) {
                    if object_has_active_data_protection(&object, now) {
                        debug!(
                            bucket = bucket_name,
                            key = object.key,
                            "Skipping lifecycle expiration for a protected object"
                        );
                        continue;
                    }
                    info!(
                        bucket = bucket_name,
                        key = object.key,
                        rule_id = rule.id.as_deref().unwrap_or("unnamed"),
                        "Expiring object"
                    );

                    let _ = tokio::task::block_in_place(|| {
                        self.storage.delete_object(bucket_name, &object.key)
                    });
                    continue;
                }
            }

            self.apply_transition(bucket_name, rule, &object, now);
        }

        Ok(())
    }

    fn apply_transition(
        &self,
        bucket_name: &str,
        rule: &Rule,
        object: &crate::models::Object,
        now: DateTime<Utc>,
    ) {
        let Some(transition) = rule
            .transitions
            .iter()
            .find(|transition| should_transition(object.last_modified, transition, now))
        else {
            return;
        };

        let storage_class = storage_class_to_str(&transition.storage_class);
        if object.storage_class == storage_class {
            return;
        }

        info!(
            bucket = bucket_name,
            key = object.key,
            rule_id = rule.id.as_deref().unwrap_or("unnamed"),
            storage_class = storage_class,
            "Transitioning object storage class"
        );

        if let Err(e) = tokio::task::block_in_place(|| {
            self.storage
                .update_object_storage_class(bucket_name, &object.key, storage_class)
        }) {
            error!(
                bucket = bucket_name,
                key = object.key,
                rule_id = rule.id.as_deref().unwrap_or("unnamed"),
                storage_class = storage_class,
                error = %e,
                "Failed to transition object storage class"
            );
        }
    }

    fn apply_noncurrent_version_rule(
        &self,
        bucket_name: &str,
        rule: &Rule,
        now: DateTime<Utc>,
    ) -> Result<(), Error> {
        let Some(noncurrent_expiration) = &rule.noncurrent_version_expiration else {
            return Ok(());
        };

        for (key, mut versions) in self.filtered_versions_by_key(bucket_name, rule)? {
            versions.sort_by(|left, right| {
                right
                    .last_modified
                    .cmp(&left.last_modified)
                    .then_with(|| left.version_id.cmp(&right.version_id))
            });

            let current_version_id = self.current_version_id(bucket_name, &key);
            self.expire_noncurrent_versions(
                bucket_name,
                rule,
                noncurrent_expiration,
                now,
                NoncurrentVersionGroup {
                    key,
                    current_version_id,
                    versions,
                },
            );
        }

        Ok(())
    }

    fn filtered_versions_by_key(
        &self,
        bucket_name: &str,
        rule: &Rule,
    ) -> Result<HashMap<String, Vec<crate::models::Object>>, Error> {
        let versions =
            tokio::task::block_in_place(|| self.storage.list_object_versions(bucket_name, None))?;
        let mut versions_by_key: HashMap<String, Vec<crate::models::Object>> = HashMap::new();

        for version in versions {
            let tags = self.object_tags(bucket_name, &version.key);
            if rule_matches_filter(rule, &version.key, &tags) {
                versions_by_key
                    .entry(version.key.clone())
                    .or_default()
                    .push(version);
            }
        }

        Ok(versions_by_key)
    }

    fn expire_noncurrent_versions(
        &self,
        bucket_name: &str,
        rule: &Rule,
        expiration: &NoncurrentVersionExpiration,
        now: DateTime<Utc>,
        version_group: NoncurrentVersionGroup,
    ) {
        for version in version_group.versions {
            if version_group.current_version_id.as_deref() == version.version_id.as_deref() {
                continue;
            }

            if !should_expire_noncurrent_version(
                version.last_modified,
                expiration.noncurrent_days,
                now,
            ) {
                continue;
            }

            if object_has_active_data_protection(&version, now) {
                debug!(
                    bucket = bucket_name,
                    key = version_group.key,
                    version_id = version.version_id.as_deref().unwrap_or("unknown"),
                    "Skipping lifecycle expiration for a protected noncurrent version"
                );
                continue;
            }

            if let Some(version_id) = version.version_id.as_deref() {
                info!(
                    bucket = bucket_name,
                    key = version_group.key,
                    version_id = version_id,
                    rule_id = rule.id.as_deref().unwrap_or("unnamed"),
                    "Expiring noncurrent object version"
                );

                let _ = tokio::task::block_in_place(|| {
                    self.storage
                        .delete_object_version(bucket_name, &version_group.key, version_id)
                });
            }
        }
    }

    fn current_version_id(&self, bucket_name: &str, key: &str) -> Option<String> {
        tokio::task::block_in_place(|| self.storage.get_object(bucket_name, key))
            .ok()
            .and_then(|object| object.version_id)
    }

    fn object_tags(&self, bucket_name: &str, key: &str) -> HashMap<String, String> {
        tokio::task::block_in_place(|| self.storage.get_object_tags(bucket_name, key))
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::lifecycle::{
        Expiration, Filter, LifecycleConfiguration, NoncurrentVersionExpiration, Rule, Status,
        StorageClass, Transition,
    };
    use crate::models::Object;
    use crate::storage::FilesystemStorage;
    use chrono::{TimeZone, Utc};
    use std::fs;
    use std::sync::Arc;

    fn temp_storage() -> Arc<dyn Storage> {
        let dir =
            std::env::temp_dir().join(format!("sqrzl-lifecycle-test-{}", uuid::Uuid::new_v4()));
        let _ = fs::create_dir_all(&dir);
        Arc::new(FilesystemStorage::new(dir))
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn should_expire_noncurrent_versions_when_rule_is_configured() {
        // Arrange
        let storage = temp_storage();
        storage.create_bucket("bucket".to_string()).unwrap();
        storage.enable_versioning("bucket").unwrap();

        let now = Utc.with_ymd_and_hms(2024, 4, 10, 12, 0, 0).unwrap();

        let mut first_version = Object::new(
            "doc.txt".to_string(),
            b"v1".to_vec(),
            "text/plain".to_string(),
        );
        first_version.last_modified = now - chrono::Duration::days(40);
        storage
            .put_object("bucket", "doc.txt".to_string(), first_version)
            .unwrap();

        let first_version_id = storage
            .get_object("bucket", "doc.txt")
            .unwrap()
            .version_id
            .clone()
            .expect("first version id should exist");

        let mut second_version = Object::new(
            "doc.txt".to_string(),
            b"v2".to_vec(),
            "text/plain".to_string(),
        );
        second_version.last_modified = now - chrono::Duration::days(1);
        storage
            .put_object("bucket", "doc.txt".to_string(), second_version)
            .unwrap();

        let current_version_id = storage
            .get_object("bucket", "doc.txt")
            .unwrap()
            .version_id
            .clone()
            .expect("current version id should exist");

        let config = LifecycleConfiguration {
            rules: vec![Rule {
                id: Some("cleanup-noncurrent".to_string()),
                status: Status::Enabled,
                filter: Some(Filter {
                    prefix: Some("doc".to_string()),
                    tags: vec![],
                }),
                expiration: None,
                noncurrent_version_expiration: Some(NoncurrentVersionExpiration {
                    noncurrent_days: 30,
                }),
                transitions: vec![],
            }],
        };

        let executor = LifecycleExecutor::new(storage.clone(), Duration::from_hours(1));

        // Act
        executor
            .apply_lifecycle_rules("bucket", &config, now)
            .expect("lifecycle rules should apply");

        // Assert
        let versions = storage
            .list_object_versions("bucket", Some("doc.txt"))
            .unwrap();
        let version_ids: Vec<_> = versions
            .into_iter()
            .filter_map(|obj| obj.version_id)
            .collect();

        assert!(version_ids.contains(&current_version_id));
        assert!(!version_ids.contains(&first_version_id));
        assert_eq!(
            storage.get_object("bucket", "doc.txt").unwrap().data,
            b"v2".to_vec()
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn should_transition_current_objects_when_rule_is_configured() {
        // Arrange
        let storage = temp_storage();
        storage.create_bucket("bucket".to_string()).unwrap();

        let now = Utc.with_ymd_and_hms(2024, 4, 10, 12, 0, 0).unwrap();

        let mut object = Object::new(
            "archive/report.txt".to_string(),
            b"payload".to_vec(),
            "text/plain".to_string(),
        );
        object.last_modified = now - chrono::Duration::days(45);
        storage
            .put_object("bucket", "archive/report.txt".to_string(), object)
            .unwrap();

        let config = LifecycleConfiguration {
            rules: vec![Rule {
                id: Some("transition-archive".to_string()),
                status: Status::Enabled,
                filter: Some(Filter {
                    prefix: Some("archive/".to_string()),
                    tags: vec![],
                }),
                expiration: None,
                noncurrent_version_expiration: None,
                transitions: vec![Transition {
                    days: Some(30),
                    date: None,
                    storage_class: StorageClass::Glacier,
                }],
            }],
        };

        let executor = LifecycleExecutor::new(storage.clone(), Duration::from_hours(1));

        // Act
        executor
            .apply_lifecycle_rules("bucket", &config, now)
            .expect("lifecycle rules should apply");

        // Assert
        let transitioned = storage.get_object("bucket", "archive/report.txt").unwrap();
        assert_eq!(transitioned.storage_class, "GLACIER");
        assert_eq!(transitioned.data, b"payload".to_vec());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn should_not_eagerly_expire_objects_from_gcs_retained_buckets() {
        // Arrange
        let storage = temp_storage();
        storage.create_bucket("retained".to_string()).unwrap();
        let mut bucket = storage.get_bucket("retained").unwrap();
        bucket
            .metadata
            .insert(GCS_RETENTION_SECONDS_KEY.to_string(), "3600".to_string());
        storage
            .update_bucket_metadata("retained", bucket.metadata)
            .unwrap();

        let mut object = Object::new(
            "protected.txt".to_string(),
            b"must remain".to_vec(),
            "text/plain".to_string(),
        );
        object.last_modified = Utc::now() - chrono::Duration::days(10);
        storage
            .put_object("retained", "protected.txt".to_string(), object)
            .unwrap();
        storage
            .put_bucket_lifecycle(
                "retained",
                LifecycleConfiguration {
                    rules: vec![Rule {
                        id: Some("expire-current".to_string()),
                        status: Status::Enabled,
                        filter: None,
                        expiration: Some(Expiration {
                            days: Some(1),
                            date: None,
                            expired_object_delete_marker: None,
                        }),
                        noncurrent_version_expiration: None,
                        transitions: vec![],
                    }],
                },
            )
            .unwrap();

        // Act
        let expired = check_object_expiration(&storage, "retained", "protected.txt").unwrap();

        // Assert
        assert!(!expired);
        assert_eq!(
            storage
                .get_object("retained", "protected.txt")
                .unwrap()
                .data,
            b"must remain".to_vec()
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn should_not_expire_gcs_soft_delete_history_through_s3_lifecycle() {
        // Arrange
        let storage = temp_storage();
        storage.create_bucket("soft-delete".to_string()).unwrap();
        let mut bucket = storage.get_bucket("soft-delete").unwrap();
        bucket.metadata.insert(
            GCS_SOFT_DELETE_SECONDS_KEY.to_string(),
            "604800".to_string(),
        );
        storage
            .update_bucket_metadata("soft-delete", bucket.metadata)
            .unwrap();
        storage.enable_versioning("soft-delete").unwrap();

        let now = Utc.with_ymd_and_hms(2024, 4, 10, 12, 0, 0).unwrap();
        let mut first = Object::new(
            "object".to_string(),
            b"first".to_vec(),
            "application/octet-stream".to_string(),
        );
        first.last_modified = now - chrono::Duration::days(40);
        storage
            .put_object("soft-delete", "object".to_string(), first)
            .unwrap();
        let first_version_id = storage
            .get_object("soft-delete", "object")
            .unwrap()
            .version_id
            .expect("first version should have an id");

        let mut second = Object::new(
            "object".to_string(),
            b"second".to_vec(),
            "application/octet-stream".to_string(),
        );
        second.last_modified = now - chrono::Duration::days(1);
        storage
            .put_object("soft-delete", "object".to_string(), second)
            .unwrap();

        let config = LifecycleConfiguration {
            rules: vec![Rule {
                id: Some("purge-history".to_string()),
                status: Status::Enabled,
                filter: None,
                expiration: None,
                noncurrent_version_expiration: Some(NoncurrentVersionExpiration {
                    noncurrent_days: 30,
                }),
                transitions: vec![],
            }],
        };

        // Act
        LifecycleExecutor::new(storage.clone(), Duration::from_hours(1))
            .apply_lifecycle_rules("soft-delete", &config, now)
            .unwrap();

        // Assert
        assert_eq!(
            storage
                .get_object_version("soft-delete", "object", &first_version_id)
                .unwrap()
                .data,
            b"first".to_vec()
        );
        assert_eq!(
            storage.get_object("soft-delete", "object").unwrap().data,
            b"second".to_vec()
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn should_not_expire_locked_s3_noncurrent_versions() {
        // Arrange
        let storage = temp_storage();
        storage.create_bucket("object-lock".to_string()).unwrap();
        storage.enable_versioning("object-lock").unwrap();

        let now = Utc.with_ymd_and_hms(2024, 4, 10, 12, 0, 0).unwrap();
        let mut first = Object::new(
            "object".to_string(),
            b"locked".to_vec(),
            "application/octet-stream".to_string(),
        );
        first.last_modified = now - chrono::Duration::days(40);
        first.provider_metadata.insert(
            S3_OBJECT_LOCK_MODE_KEY.to_string(),
            "COMPLIANCE".to_string(),
        );
        first.provider_metadata.insert(
            S3_OBJECT_LOCK_UNTIL_KEY.to_string(),
            (now + chrono::Duration::days(10)).to_rfc3339(),
        );
        storage
            .put_object("object-lock", "object".to_string(), first)
            .unwrap();
        let first_version_id = storage
            .get_object("object-lock", "object")
            .unwrap()
            .version_id
            .expect("first version should have an id");

        let mut second = Object::new(
            "object".to_string(),
            b"current".to_vec(),
            "application/octet-stream".to_string(),
        );
        second.last_modified = now - chrono::Duration::days(1);
        storage
            .put_object("object-lock", "object".to_string(), second)
            .unwrap();

        let config = LifecycleConfiguration {
            rules: vec![Rule {
                id: Some("purge-history".to_string()),
                status: Status::Enabled,
                filter: None,
                expiration: None,
                noncurrent_version_expiration: Some(NoncurrentVersionExpiration {
                    noncurrent_days: 30,
                }),
                transitions: vec![],
            }],
        };

        // Act
        LifecycleExecutor::new(storage.clone(), Duration::from_hours(1))
            .apply_lifecycle_rules("object-lock", &config, now)
            .unwrap();

        // Assert
        assert_eq!(
            storage
                .get_object_version("object-lock", "object", &first_version_id)
                .unwrap()
                .data,
            b"locked".to_vec()
        );
        assert_eq!(
            storage.get_object("object-lock", "object").unwrap().data,
            b"current".to_vec()
        );
    }
}
