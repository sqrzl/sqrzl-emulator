use crossbeam_skiplist::SkipMap;
use std::ops::Bound;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DirectoryEntryKind {
    Object,
    CommonPrefix,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirectoryEntry {
    pub path: String,
    pub kind: DirectoryEntryKind,
}

struct BucketIndex {
    objects: SkipMap<String, ()>,
    children: SkipMap<String, SkipMap<String, DirectoryEntryKind>>,
}

impl BucketIndex {
    fn new() -> Self {
        let children = SkipMap::new();
        children.insert(String::new(), SkipMap::new());
        Self {
            objects: SkipMap::new(),
            children,
        }
    }
}

/// Lock-free concurrent index for object keys and directory children by bucket.
/// Uses crossbeam's `SkipMap` for atomic, wait-free reads.
pub struct LockFreeIndex {
    buckets: SkipMap<String, BucketIndex>,
}

impl LockFreeIndex {
    #[must_use]
    pub fn new() -> Self {
        Self {
            buckets: SkipMap::new(),
        }
    }

    /// Insert an object key into a bucket's index
    pub fn insert(&self, bucket: &str, key: &str) {
        self.get_or_create_bucket(bucket.to_string());
        if let Some(entry) = self.buckets.get(bucket) {
            let bucket_index = entry.value();
            bucket_index.objects.insert(key.to_string(), ());
            Self::insert_directory_children(bucket_index, key);
        }
    }

    /// Remove an object key from a bucket's index
    pub fn remove(&self, bucket: &str, key: &str) -> bool {
        if let Some(entry) = self.buckets.get(bucket) {
            let bucket_index = entry.value();
            let removed = bucket_index.objects.remove(key).is_some();
            if removed {
                Self::remove_directory_children(bucket_index, key);
            }
            removed
        } else {
            false
        }
    }

    /// Check if an object exists in a bucket
    pub fn contains(&self, bucket: &str, key: &str) -> bool {
        if let Some(entry) = self.buckets.get(bucket) {
            entry.value().objects.get(key).is_some()
        } else {
            false
        }
    }

    /// Get all keys in a bucket matching optional prefix
    pub fn list(&self, bucket: &str, prefix: Option<&str>) -> Vec<String> {
        if let Some(entry) = self.buckets.get(bucket) {
            if let Some(prefix) = prefix {
                let mut result = Vec::new();
                let mut current = entry.value().objects.lower_bound(Bound::Included(prefix));
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
                    .objects
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
                    entry.value().objects.lower_bound(Bound::Excluded(marker))
                }
                (Some(prefix), _) => entry.value().objects.lower_bound(Bound::Included(prefix)),
                (None, Some(marker)) => entry.value().objects.lower_bound(Bound::Excluded(marker)),
                (None, None) => None,
            };

            let mut result = Vec::new();
            if current.is_none() && prefix.is_none() && marker.is_none() {
                current = entry.value().objects.iter().next();
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

    /// Get immediate child objects and common prefixes for a parent prefix.
    pub fn list_child_entries(
        &self,
        bucket: &str,
        parent_prefix: &str,
        marker: Option<&str>,
        max_entries: Option<usize>,
    ) -> Vec<DirectoryEntry> {
        let Some(bucket_entry) = self.buckets.get(bucket) else {
            return Vec::new();
        };
        let Some(children_entry) = bucket_entry.value().children.get(parent_prefix) else {
            return Vec::new();
        };

        let mut current = match marker {
            Some(marker) => children_entry.value().lower_bound(Bound::Excluded(marker)),
            None => children_entry.value().iter().next(),
        };
        let mut result = Vec::new();

        while let Some(entry) = current {
            result.push(DirectoryEntry {
                path: entry.key().clone(),
                kind: *entry.value(),
            });
            if let Some(max) = max_entries {
                if result.len() >= max {
                    break;
                }
            }
            current = entry.next();
        }

        result
    }

    /// Clear all keys from a bucket
    pub fn clear_bucket(&self, bucket: &str) {
        self.buckets.remove(bucket);
    }

    /// Get or create a bucket entry
    pub fn get_or_create_bucket(&self, bucket: String) {
        if self.buckets.get(&bucket).is_none() {
            self.buckets.insert(bucket, BucketIndex::new());
        }
    }

    /// Populate index from iterator of (bucket, keys) pairs
    pub fn rebuild<I>(&self, entries: I)
    where
        I: IntoIterator<Item = (String, Vec<String>)>,
    {
        for (bucket, keys) in entries {
            let bucket_index = BucketIndex::new();
            for key in keys {
                bucket_index.objects.insert(key.clone(), ());
                Self::insert_directory_children(&bucket_index, &key);
            }
            self.buckets.insert(bucket, bucket_index);
        }
    }

    /// Check if bucket exists
    pub fn bucket_exists(&self, bucket: &str) -> bool {
        self.buckets.contains_key(bucket)
    }

    fn insert_directory_children(bucket_index: &BucketIndex, key: &str) {
        let mut parent = String::new();

        for prefix in Self::parent_prefixes(key) {
            Self::insert_child(
                &bucket_index.children,
                &parent,
                prefix.clone(),
                DirectoryEntryKind::CommonPrefix,
            );
            parent = prefix;
        }

        Self::insert_child(
            &bucket_index.children,
            &parent,
            key.to_string(),
            DirectoryEntryKind::Object,
        );
    }

    fn remove_directory_children(bucket_index: &BucketIndex, key: &str) {
        let mut parent = String::new();
        let mut prefix_pairs = Vec::new();

        for prefix in Self::parent_prefixes(key) {
            prefix_pairs.push((parent.clone(), prefix.clone()));
            parent = prefix;
        }

        Self::remove_child(&bucket_index.children, &parent, key);

        for (parent, prefix) in prefix_pairs.into_iter().rev() {
            if !Self::has_object_with_prefix(&bucket_index.objects, &prefix) {
                Self::remove_child(&bucket_index.children, &parent, &prefix);
                bucket_index.children.remove(&prefix);
            }
        }
    }

    fn insert_child(
        children: &SkipMap<String, SkipMap<String, DirectoryEntryKind>>,
        parent: &str,
        path: String,
        kind: DirectoryEntryKind,
    ) {
        if children.get(parent).is_none() {
            children.insert(parent.to_string(), SkipMap::new());
        }

        if let Some(parent_entry) = children.get(parent) {
            parent_entry.value().insert(path, kind);
        }
    }

    fn remove_child(
        children: &SkipMap<String, SkipMap<String, DirectoryEntryKind>>,
        parent: &str,
        path: &str,
    ) {
        if let Some(parent_entry) = children.get(parent) {
            parent_entry.value().remove(path);
        }
    }

    fn has_object_with_prefix(objects: &SkipMap<String, ()>, prefix: &str) -> bool {
        objects
            .lower_bound(Bound::Included(prefix))
            .is_some_and(|entry| entry.key().starts_with(prefix))
    }

    fn parent_prefixes(key: &str) -> Vec<String> {
        key.match_indices('/')
            .map(|(index, _)| key[..=index].to_string())
            .collect()
    }
}

impl Default for LockFreeIndex {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::{DirectoryEntryKind, LockFreeIndex};

    #[test]
    fn should_use_start_bounds_when_listing_with_prefix_and_marker() {
        // Arrange
        let index = LockFreeIndex::new();
        index.insert("bucket", "apple");
        index.insert("bucket", "banana");
        index.insert("bucket", "cherry");

        // Act
        let result_from_marker = index.list_prefix_marker("bucket", None, Some("banana"), Some(10));
        let result_from_prefix = index.list_prefix_marker("bucket", Some("b"), None, Some(10));
        let result_from_prefix_and_earlier_marker =
            index.list_prefix_marker("bucket", Some("b"), Some("apple"), Some(10));
        let result_from_prefix_and_far_earlier_marker =
            index.list_prefix_marker("bucket", Some("c"), Some("a"), Some(10));

        // Assert
        assert_eq!(result_from_marker, vec!["cherry".to_string()]);
        assert_eq!(result_from_prefix, vec!["banana".to_string()]);
        assert_eq!(
            result_from_prefix_and_earlier_marker,
            vec!["banana".to_string()]
        );
        assert_eq!(
            result_from_prefix_and_far_earlier_marker,
            vec!["cherry".to_string()]
        );
    }

    #[test]
    fn should_limit_list_results_to_matching_prefix_range() {
        // Arrange
        let index = LockFreeIndex::new();
        index.insert("bucket", "alpha");
        index.insert("bucket", "beta");
        index.insert("bucket", "beta2");
        index.insert("bucket", "gamma");

        // Act
        let result = index.list("bucket", Some("beta"));

        // Assert
        assert_eq!(result, vec!["beta".to_string(), "beta2".to_string()]);
    }

    #[test]
    fn should_index_immediate_directory_children() {
        // Arrange
        let index = LockFreeIndex::new();
        index.insert("bucket", "docs/readme.txt");
        index.insert("bucket", "docs/api/openapi.json");
        index.insert("bucket", "image.png");

        // Act
        let root = index.list_child_entries("bucket", "", None, Some(10));
        let docs = index.list_child_entries("bucket", "docs/", None, Some(10));

        // Assert
        assert_eq!(root.len(), 2);
        assert_eq!(root[0].path, "docs/");
        assert_eq!(root[0].kind, DirectoryEntryKind::CommonPrefix);
        assert_eq!(root[1].path, "image.png");
        assert_eq!(root[1].kind, DirectoryEntryKind::Object);
        assert_eq!(docs.len(), 2);
        assert_eq!(docs[0].path, "docs/api/");
        assert_eq!(docs[0].kind, DirectoryEntryKind::CommonPrefix);
        assert_eq!(docs[1].path, "docs/readme.txt");
        assert_eq!(docs[1].kind, DirectoryEntryKind::Object);
    }

    #[test]
    fn should_prune_empty_directory_children_after_delete() {
        // Arrange
        let index = LockFreeIndex::new();
        index.insert("bucket", "docs/api/openapi.json");
        index.insert("bucket", "docs/readme.txt");

        // Act
        assert!(index.remove("bucket", "docs/api/openapi.json"));

        // Assert
        let root = index.list_child_entries("bucket", "", None, Some(10));
        let docs = index.list_child_entries("bucket", "docs/", None, Some(10));
        assert_eq!(root.len(), 1);
        assert_eq!(root[0].path, "docs/");
        assert_eq!(docs.len(), 1);
        assert_eq!(docs[0].path, "docs/readme.txt");

        assert!(index.remove("bucket", "docs/readme.txt"));
        assert!(index
            .list_child_entries("bucket", "", None, Some(10))
            .is_empty());
    }
}
