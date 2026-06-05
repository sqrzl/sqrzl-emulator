use crossbeam_skiplist::SkipMap;
use std::ops::Bound;

/// Lock-free concurrent index for object keys by bucket
/// Uses crossbeam's SkipMap for atomic, wait-free reads
pub struct LockFreeIndex {
    // Map from bucket_name to SkipMap of keys
    buckets: SkipMap<String, SkipMap<String, ()>>,
}

impl LockFreeIndex {
    pub fn new() -> Self {
        Self {
            buckets: SkipMap::new(),
        }
    }

    /// Insert an object key into a bucket's index
    pub fn insert(&self, bucket: String, key: String) {
        // Get or create bucket map
        if let Some(entry) = self.buckets.get(&bucket) {
            entry.value().insert(key, ());
        } else {
            let bucket_map = SkipMap::new();
            bucket_map.insert(key, ());
            self.buckets.insert(bucket, bucket_map);
        }
    }

    /// Remove an object key from a bucket's index
    pub fn remove(&self, bucket: &str, key: &str) -> bool {
        if let Some(entry) = self.buckets.get(bucket) {
            entry.value().remove(key).is_some()
        } else {
            false
        }
    }

    /// Check if an object exists in a bucket
    pub fn contains(&self, bucket: &str, key: &str) -> bool {
        if let Some(entry) = self.buckets.get(bucket) {
            entry.value().get(key).is_some()
        } else {
            false
        }
    }

    /// Get all keys in a bucket matching optional prefix
    pub fn list(&self, bucket: &str, prefix: Option<&str>) -> Vec<String> {
        if let Some(entry) = self.buckets.get(bucket) {
            if let Some(prefix) = prefix {
                let mut result = Vec::new();
                let mut current = entry.value().lower_bound(Bound::Included(prefix));
                while let Some(entry) = current {
                    let key = entry.key();
                    if key.starts_with(prefix) {
                        result.push(key.clone());
                        current = entry.next();
                    } else {
                        break;
                    }
                }
                result
            } else {
                entry
                    .value()
                    .iter()
                    .map(|node| node.key().clone())
                    .collect()
            }
        } else {
            Vec::new()
        }
    }

    /// Get keys in a bucket by prefix, marker, and optional max count.
    ///
    /// This avoids loading the full bucket index for large buckets.
    pub fn list_prefix_marker(
        &self,
        bucket: &str,
        prefix: Option<&str>,
        marker: Option<&str>,
        max_keys: Option<usize>,
    ) -> Vec<String> {
        if let Some(entry) = self.buckets.get(bucket) {
            let mut current = match (prefix, marker) {
                (Some(prefix), Some(marker)) if marker >= prefix => {
                    entry.value().lower_bound(Bound::Excluded(marker))
                }
                (Some(prefix), _) => entry.value().lower_bound(Bound::Included(prefix)),
                (None, Some(marker)) => entry.value().lower_bound(Bound::Excluded(marker)),
                (None, None) => None,
            };

            let mut result = Vec::new();
            if current.is_none() && prefix.is_none() && marker.is_none() {
                current = entry.value().iter().next();
            }

            while let Some(entry) = current {
                let key = entry.key();
                if let Some(prefix) = prefix {
                    if !key.starts_with(prefix) {
                        break;
                    }
                }

                result.push(key.clone());
                if let Some(max) = max_keys {
                    if result.len() >= max {
                        break;
                    }
                }
                current = entry.next();
            }
            result
        } else {
            Vec::new()
        }
    }

    /// Clear all keys from a bucket
    pub fn clear_bucket(&self, bucket: &str) {
        self.buckets.remove(bucket);
    }

    /// Get or create a bucket entry
    pub fn get_or_create_bucket(&self, bucket: String) {
        if self.buckets.get(&bucket).is_none() {
            self.buckets.insert(bucket, SkipMap::new());
        }
    }

    /// Populate index from iterator of (bucket, keys) pairs
    pub fn rebuild<I>(&self, entries: I)
    where
        I: IntoIterator<Item = (String, Vec<String>)>,
    {
        for (bucket, keys) in entries {
            let bucket_map = SkipMap::new();
            for key in keys {
                bucket_map.insert(key, ());
            }
            self.buckets.insert(bucket, bucket_map);
        }
    }

    /// Check if bucket exists
    pub fn bucket_exists(&self, bucket: &str) -> bool {
        self.buckets.contains_key(bucket)
    }
}

#[cfg(test)]
mod tests {
    use super::LockFreeIndex;

    #[test]
    fn list_prefix_marker_uses_start_bounds() {
        let index = LockFreeIndex::new();
        index.insert("bucket".to_string(), "apple".to_string());
        index.insert("bucket".to_string(), "banana".to_string());
        index.insert("bucket".to_string(), "cherry".to_string());

        assert_eq!(
            index.list_prefix_marker("bucket", None, Some("banana"), Some(10)),
            vec!["cherry".to_string()]
        );
        assert_eq!(
            index.list_prefix_marker("bucket", Some("b"), None, Some(10)),
            vec!["banana".to_string()]
        );
        assert_eq!(
            index.list_prefix_marker("bucket", Some("b"), Some("apple"), Some(10)),
            vec!["banana".to_string()]
        );
        assert_eq!(
            index.list_prefix_marker("bucket", Some("c"), Some("a"), Some(10)),
            vec!["cherry".to_string()]
        );
    }

    #[test]
    fn list_prefix_limits_to_prefix_range() {
        let index = LockFreeIndex::new();
        index.insert("bucket".to_string(), "alpha".to_string());
        index.insert("bucket".to_string(), "beta".to_string());
        index.insert("bucket".to_string(), "beta2".to_string());
        index.insert("bucket".to_string(), "gamma".to_string());

        assert_eq!(
            index.list("bucket", Some("beta")),
            vec!["beta".to_string(), "beta2".to_string()]
        );
    }
}

impl Default for LockFreeIndex {
    fn default() -> Self {
        Self::new()
    }
}
