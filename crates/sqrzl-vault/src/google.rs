use base64::{engine::general_purpose::STANDARD, Engine as _};
use chrono::{SecondsFormat, Utc};
use http::Method;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sqrzl_core::server::RequestExt;

use crate::{body, crypto, Fault, Store};

const ALGORITHM: &str = "GOOGLE_SYMMETRIC_ENCRYPTION";
const MAX_PLAINTEXT_BYTES: usize = 64 * 1024;

#[derive(Serialize, Deserialize)]
struct KeyRing {
    created: String,
}

#[derive(Serialize, Deserialize)]
struct Key {
    created: String,
    material: [u8; 32],
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CreateKey {
    purpose: Value,
    version_template: Option<VersionTemplate>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct VersionTemplate {
    algorithm: Option<Value>,
    protection_level: Option<Value>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Encrypt {
    #[serde(default)]
    plaintext: String,
    #[serde(rename = "plaintextCrc32c")]
    plaintext_crc32c: Option<Value>,
    #[serde(rename = "additionalAuthenticatedData")]
    aad: Option<String>,
    #[serde(rename = "additionalAuthenticatedDataCrc32c")]
    aad_crc32c: Option<Value>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Decrypt {
    ciphertext: String,
    #[serde(rename = "ciphertextCrc32c")]
    ciphertext_crc32c: Option<Value>,
    #[serde(rename = "additionalAuthenticatedData")]
    aad: Option<String>,
    #[serde(rename = "additionalAuthenticatedDataCrc32c")]
    aad_crc32c: Option<Value>,
}

pub(crate) fn handle(store: &Store, request: &RequestExt) -> Result<Value, Fault> {
    let parts: Vec<_> = request.path().split('/').collect();
    let ["", "v1", "projects", project, "locations", location, "keyRings", tail @ ..] =
        parts.as_slice()
    else {
        return Err(Fault::Unsupported);
    };
    if !valid_id(project) || !valid_id(location) {
        return Err(Fault::Invalid("Invalid project or location"));
    }

    let parent = format!("projects/{project}/locations/{location}");
    match tail {
        [] if request.method == Method::POST => {
            let ring = required_id(request, "keyRingId")?;
            if !request.body.is_empty() && body::<Value>(request)? != json!({}) {
                return Err(Fault::Invalid("Key ring creation takes an empty object"));
            }
            let name = format!("{parent}/keyRings/{ring}");
            let ring = KeyRing {
                created: timestamp(),
            };
            store.create(ring_record(&name), &ring)?;
            Ok(json!({ "name": name, "createTime": ring.created }))
        }
        [ring, rest @ ..] if valid_id(ring) => {
            let parent = format!("{parent}/keyRings/{ring}");
            key_request(store, request, &parent, rest)
        }
        _ => Err(Fault::Unsupported),
    }
}

fn key_request(
    store: &Store,
    request: &RequestExt,
    parent: &str,
    path: &[&str],
) -> Result<Value, Fault> {
    match (request.method(), path) {
        (&Method::GET, []) => {
            let ring: KeyRing = store.get(&ring_record(parent))?;
            Ok(json!({ "name": parent, "createTime": ring.created }))
        }
        (&Method::POST, ["cryptoKeys"]) => create_key(store, request, parent),
        (&Method::GET, ["cryptoKeys", key]) if valid_id(key) => {
            let name = format!("{parent}/cryptoKeys/{key}");
            let key = store.get(&key_record(&name))?;
            Ok(key_metadata(&name, &key))
        }
        (&Method::GET, ["cryptoKeys", key, "cryptoKeyVersions", "1"]) if valid_id(key) => {
            let name = format!("{parent}/cryptoKeys/{key}");
            let key = store.get(&key_record(&name))?;
            Ok(key_version(&name, &key))
        }
        (&Method::POST, ["cryptoKeys", action]) => {
            let (key, operation) = action.split_once(':').ok_or(Fault::Unsupported)?;
            if !valid_id(key) {
                return Err(Fault::Invalid("Invalid crypto key ID"));
            }
            run_crypto(
                store,
                request,
                &format!("{parent}/cryptoKeys/{key}"),
                operation,
            )
        }
        (&Method::POST, ["cryptoKeys", key, "cryptoKeyVersions", "1:encrypt"]) if valid_id(key) => {
            run_crypto(
                store,
                request,
                &format!("{parent}/cryptoKeys/{key}"),
                "encrypt",
            )
        }
        _ => Err(Fault::Unsupported),
    }
}

fn create_key(store: &Store, request: &RequestExt, parent: &str) -> Result<Value, Fault> {
    let id = required_id(request, "cryptoKeyId")?;
    if request.query_param("skipInitialVersionCreation") == Some("true") {
        return Err(Fault::Unsupported);
    }
    let options: CreateKey = body(request)?;
    if !enum_value(&options.purpose, "ENCRYPT_DECRYPT", 1)
        || options.version_template.is_some_and(|template| {
            template
                .algorithm
                .as_ref()
                .is_some_and(|value| !enum_value(value, ALGORITHM, 1))
                || template
                    .protection_level
                    .as_ref()
                    .is_some_and(|value| !enum_value(value, "SOFTWARE", 1))
        })
    {
        return Err(Fault::Unsupported);
    }
    let _: KeyRing = store.get(&ring_record(parent))?;
    let name = format!("{parent}/cryptoKeys/{id}");
    let key = Key {
        created: timestamp(),
        material: crypto::random()?,
    };
    store.create(key_record(&name), &key)?;
    Ok(key_metadata(&name, &key))
}

fn run_crypto(
    store: &Store,
    request: &RequestExt,
    name: &str,
    operation: &str,
) -> Result<Value, Fault> {
    match operation {
        "encrypt" => encrypt(store, request, name),
        "decrypt" => decrypt(store, request, name),
        _ => Err(Fault::Unsupported),
    }
}

fn encrypt(store: &Store, request: &RequestExt, name: &str) -> Result<Value, Fault> {
    let key: Key = store.get(&key_record(name))?;
    let request: Encrypt = body(request)?;
    let plaintext = decoded(&request.plaintext, request.plaintext_crc32c.as_ref())?;
    let aad = decoded(
        request.aad.as_deref().unwrap_or_default(),
        request.aad_crc32c.as_ref(),
    )?;
    if plaintext.len() > MAX_PLAINTEXT_BYTES || aad.len() > MAX_PLAINTEXT_BYTES {
        return Err(Fault::Invalid(
            "Plaintext or additional authenticated data exceeds 64 KiB",
        ));
    }
    let ciphertext = crypto::seal(&key.material, &plaintext, &aad)?;
    Ok(json!({
        "name": format!("{name}/cryptoKeyVersions/1"),
        "ciphertext": STANDARD.encode(&ciphertext),
        "ciphertextCrc32c": crc32c::crc32c(&ciphertext).to_string(),
        "verifiedPlaintextCrc32c": request.plaintext_crc32c.is_some(),
        "verifiedAdditionalAuthenticatedDataCrc32c": request.aad_crc32c.is_some(),
        "protectionLevel": "SOFTWARE",
    }))
}

fn decrypt(store: &Store, request: &RequestExt, name: &str) -> Result<Value, Fault> {
    let key: Key = store.get(&key_record(name))?;
    let request: Decrypt = body(request)?;
    let ciphertext = decoded(&request.ciphertext, request.ciphertext_crc32c.as_ref())?;
    let aad = decoded(
        request.aad.as_deref().unwrap_or_default(),
        request.aad_crc32c.as_ref(),
    )?;
    let plaintext = crypto::open(&key.material, &ciphertext, &aad)?;
    Ok(json!({
        "plaintext": STANDARD.encode(&plaintext),
        "plaintextCrc32c": crc32c::crc32c(&plaintext).to_string(),
        "usedPrimary": true,
        "protectionLevel": "SOFTWARE",
    }))
}

fn enum_value(value: &Value, name: &str, number: u64) -> bool {
    value.as_str() == Some(name) || value.as_u64() == Some(number)
}

fn decoded(value: &str, checksum: Option<&Value>) -> Result<Vec<u8>, Fault> {
    let bytes = STANDARD
        .decode(value)
        .map_err(|_| Fault::Invalid("Invalid base64 field"))?;
    if let Some(expected) = checksum {
        let expected = expected
            .as_u64()
            .or_else(|| expected.as_str()?.parse().ok());
        if expected != Some(u64::from(crc32c::crc32c(&bytes))) {
            return Err(Fault::Invalid("CRC32C checksum mismatch"));
        }
    }
    Ok(bytes)
}

fn required_id<'a>(request: &'a RequestExt, name: &str) -> Result<&'a str, Fault> {
    request
        .query_param(name)
        .filter(|id| valid_id(id))
        .ok_or(Fault::Invalid("Missing or invalid resource ID"))
}

fn valid_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 63
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

fn timestamp() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true)
}

fn ring_record(name: &str) -> String {
    format!("google/ring/{name}")
}

fn key_record(name: &str) -> String {
    format!("google/key/{name}")
}

fn key_metadata(name: &str, key: &Key) -> Value {
    json!({
        "name": name, "purpose": "ENCRYPT_DECRYPT", "createTime": key.created,
        "versionTemplate": { "algorithm": ALGORITHM, "protectionLevel": "SOFTWARE" },
        "primary": key_version(name, key),
    })
}

fn key_version(name: &str, key: &Key) -> Value {
    json!({
        "name": format!("{name}/cryptoKeyVersions/1"), "state": "ENABLED",
        "algorithm": ALGORITHM, "protectionLevel": "SOFTWARE",
        "createTime": key.created, "generateTime": key.created,
    })
}
