use crate::storage::Storage;
use serde::de::DeserializeOwned;
use serde::Serialize;

pub(super) fn load_json<T>(
    storage: &dyn Storage,
    provider: &str,
    key: &str,
) -> Result<Option<T>, String>
where
    T: DeserializeOwned,
{
    match storage.get_provider_state(provider, key) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|err| format!("Failed to parse provider state: {err}")),
        Err(crate::error::Error::KeyNotFound) => Ok(None),
        Err(err) => Err(err.to_string()),
    }
}

pub(super) fn save_json<T>(
    storage: &dyn Storage,
    provider: &str,
    key: &str,
    value: &T,
) -> Result<(), String>
where
    T: Serialize,
{
    let bytes = serde_json::to_vec(value)
        .map_err(|err| format!("Failed to serialize provider state: {err}"))?;
    storage
        .put_provider_state(provider, key, bytes)
        .map_err(|err| err.to_string())
}
