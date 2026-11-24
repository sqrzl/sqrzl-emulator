use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use base64::{engine::general_purpose::STANDARD, Engine as _};
use serde::{de::DeserializeOwned, Serialize};
use serde_json::Value;
use sqrzl_core::Error;
use uuid::Uuid;

use crate::{crypto, Fault};

const HEADER: &[u8] = b"SQRZLVAULT1";
const FILE_NAME: &str = ".sqrzl-vault";
type Records = BTreeMap<String, Value>;

struct Snapshot {
    path: PathBuf,
    key: [u8; 32],
}

pub(crate) struct Store {
    records: Mutex<Records>,
    snapshot: Option<Snapshot>,
}

impl Store {
    pub(crate) fn open(root: &Path, encoded_key: Option<&str>) -> sqrzl_core::Result<Self> {
        let path = root.join(FILE_NAME);
        let stored = match fs::read(&path) {
            Ok(bytes) => Some(bytes),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => {
                return Err(Error::InternalError(format!(
                    "Could not read vault: {error}"
                )))
            }
        };
        let snapshot = encoded_key
            .map(|key| {
                let key = STANDARD
                    .decode(key)
                    .ok()
                    .and_then(|bytes| bytes.try_into().ok())
                    .ok_or_else(|| {
                        Error::InvalidRequest(
                            "SQRZL_VAULT_KEY must be a base64-encoded 32-byte key".into(),
                        )
                    })?;
                Ok(Snapshot { path, key })
            })
            .transpose()?;
        let records = match (stored, snapshot.as_ref()) {
            (None, _) => BTreeMap::new(),
            (Some(_), None) => {
                return Err(Error::InvalidRequest(
                    "SQRZL_VAULT_KEY is required to reopen the stored vault".into(),
                ))
            }
            (Some(bytes), Some(snapshot)) => {
                let error = || {
                    Error::InvalidRequest(
                        "Could not decrypt the stored vault with SQRZL_VAULT_KEY".into(),
                    )
                };
                let envelope = bytes.strip_prefix(HEADER).ok_or_else(error)?;
                let plaintext =
                    crypto::open(&snapshot.key, envelope, HEADER).map_err(|_| error())?;
                serde_json::from_slice(&plaintext).map_err(|_| error())?
            }
        };
        Ok(Self {
            records: Mutex::new(records),
            snapshot,
        })
    }

    pub(crate) fn get<T: DeserializeOwned>(&self, key: &str) -> Result<T, Fault> {
        let records = self.records.lock().map_err(|_| Fault::Internal)?;
        let value = records.get(key).ok_or(Fault::Missing)?;
        serde_json::from_value(value.clone()).map_err(|_| Fault::Internal)
    }

    pub(crate) fn create(&self, key: String, record: &impl Serialize) -> Result<(), Fault> {
        self.update(|records| {
            if records.contains_key(&key) {
                return Err(Fault::Conflict);
            }
            records.insert(
                key,
                serde_json::to_value(record).map_err(|_| Fault::Internal)?,
            );
            Ok(())
        })
    }

    pub(crate) fn create_version(
        &self,
        name: String,
        version: &str,
        record: &impl Serialize,
    ) -> Result<(), Fault> {
        let version_key = format!("{name}/{version}");
        self.update(|records| {
            if records.contains_key(&version_key) {
                return Err(Fault::Conflict);
            }
            records.insert(
                version_key,
                serde_json::to_value(record).map_err(|_| Fault::Internal)?,
            );
            records.insert(name, Value::String(version.to_owned()));
            Ok(())
        })
    }

    pub(crate) fn get_version<T: DeserializeOwned>(
        &self,
        name: &str,
        version: Option<&str>,
    ) -> Result<T, Fault> {
        let records = self.records.lock().map_err(|_| Fault::Internal)?;
        let version = version
            .or_else(|| records.get(name).and_then(Value::as_str))
            .ok_or(Fault::Missing)?;
        let value = records
            .get(&format!("{name}/{version}"))
            .ok_or(Fault::Missing)?;
        serde_json::from_value(value.clone()).map_err(|_| Fault::Internal)
    }

    pub(crate) fn alter<T, R>(
        &self,
        key: String,
        change: impl FnOnce(Option<T>) -> Result<(T, R), Fault>,
    ) -> Result<R, Fault>
    where
        T: DeserializeOwned + Serialize,
    {
        self.update(|records| {
            let record = records
                .get(&key)
                .map(|value| serde_json::from_value(value.clone()).map_err(|_| Fault::Internal))
                .transpose()?;
            let (record, result) = change(record)?;
            records.insert(
                key,
                serde_json::to_value(record).map_err(|_| Fault::Internal)?,
            );
            Ok(result)
        })
    }

    fn update<T>(&self, change: impl FnOnce(&mut Records) -> Result<T, Fault>) -> Result<T, Fault> {
        let mut current = self.records.lock().map_err(|_| Fault::Internal)?;
        let mut next = current.clone();
        let result = change(&mut next)?;
        if let Some(snapshot) = &self.snapshot {
            snapshot.publish(&next)?;
        }
        // A successful rename publishes the new state to readers and to disk.
        *current = next;
        #[cfg(unix)]
        if let Some(snapshot) = &self.snapshot {
            let parent = snapshot.path.parent().ok_or(Fault::Internal)?;
            fs::File::open(parent)
                .and_then(|directory| directory.sync_all())
                .map_err(|_| Fault::Internal)?;
        }
        Ok(result)
    }
}

impl Snapshot {
    fn publish(&self, records: &Records) -> Result<(), Fault> {
        let plaintext = serde_json::to_vec(records).map_err(|_| Fault::Internal)?;
        let ciphertext = crypto::seal(&self.key, &plaintext, HEADER)?;
        let parent = self.path.parent().ok_or(Fault::Internal)?;
        let staging = parent.join(format!(".sqrzl-vault-{}.tmp", Uuid::new_v4()));
        let result = (|| {
            let mut options = OpenOptions::new();
            options.create_new(true).write(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt as _;
                options.mode(0o600);
            }
            let mut file = options.open(&staging).map_err(|_| Fault::Internal)?;
            file.write_all(HEADER).map_err(|_| Fault::Internal)?;
            file.write_all(&ciphertext).map_err(|_| Fault::Internal)?;
            file.sync_all().map_err(|_| Fault::Internal)?;
            drop(file);
            fs::rename(&staging, &self.path).map_err(|_| Fault::Internal)
        })();
        if result.is_err() {
            let _ = fs::remove_file(&staging);
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_reopen_only_with_the_configured_key_given_an_encrypted_snapshot() {
        // Arrange
        let root = std::env::temp_dir().join(format!("sqrzl-vault-test-{}", Uuid::new_v4()));
        fs::create_dir(&root).unwrap();
        let key = STANDARD.encode(crypto::random::<32>().unwrap());
        let original = Store::open(&root, Some(&key)).unwrap();
        let secret = "fixture-plaintext-must-not-appear-in-the-snapshot";
        original
            .create("azure/test/secret".into(), &secret)
            .unwrap();
        drop(original);

        // Act
        let snapshot = fs::read(root.join(FILE_NAME)).unwrap();
        let reopened = Store::open(&root, Some(&key)).unwrap();
        let value = reopened.get::<String>("azure/test/secret").unwrap();

        // Assert
        assert!(snapshot.starts_with(HEADER));
        assert!(!snapshot
            .windows(secret.len())
            .any(|part| part == secret.as_bytes()));
        assert_eq!(value, secret);
        assert!(Store::open(&root, None).is_err());
        assert!(Store::open(&root, Some(&STANDARD.encode([0; 32]))).is_err());
        assert_eq!(fs::read_dir(&root).unwrap().count(), 1);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn should_leave_the_data_directory_empty_given_an_ephemeral_vault() {
        // Arrange
        let root = std::env::temp_dir().join(format!("sqrzl-vault-test-{}", Uuid::new_v4()));
        fs::create_dir(&root).unwrap();
        let store = Store::open(&root, None).unwrap();

        // Act
        store.create("test".into(), &"value").unwrap();

        // Assert
        assert_eq!(fs::read_dir(&root).unwrap().count(), 0);
        fs::remove_dir_all(root).unwrap();
    }
}
