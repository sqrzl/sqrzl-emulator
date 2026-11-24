use std::collections::BTreeMap;

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use chrono::Utc;
use http::Method;
use rsa::{
    pkcs8::{DecodePrivateKey, EncodePrivateKey},
    traits::PublicKeyParts as _,
    Oaep, PrivateKey,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha1::Sha1;
use sha2::Sha256;
use sqrzl_core::server::RequestExt;
use uuid::Uuid;

use crate::{authority, body, Fault, Store};

const RSA_OPERATIONS: [&str; 4] = ["encrypt", "decrypt", "wrapKey", "unwrapKey"];

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SetSecret {
    value: String,
    content_type: Option<String>,
    attributes: Option<Attributes>,
    tags: Option<BTreeMap<String, String>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateKey {
    kty: String,
    key_size: Option<usize>,
    key_ops: Option<Vec<String>>,
    attributes: Option<Attributes>,
    tags: Option<BTreeMap<String, String>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CryptoRequest {
    alg: String,
    value: String,
}

#[derive(Clone, Copy, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Attributes {
    enabled: Option<bool>,
    nbf: Option<i64>,
    exp: Option<i64>,
}

#[derive(Deserialize, Serialize)]
struct Secret {
    value: String,
    version: String,
    created: i64,
    attributes: Attributes,
    content_type: Option<String>,
    tags: Option<BTreeMap<String, String>>,
}

#[derive(Deserialize, Serialize)]
struct Key {
    der: Vec<u8>,
    version: String,
    created: i64,
    attributes: Attributes,
    ops: Vec<String>,
    tags: Option<BTreeMap<String, String>>,
}

pub(crate) fn handle(store: &Store, request: &RequestExt) -> Result<Value, Fault> {
    if request.query_param("api-version").is_none_or(str::is_empty) {
        return Err(Fault::Invalid("api-version is required"));
    }
    let parts: Vec<&str> = request.path().trim_end_matches('/').split('/').collect();
    let ["", kind @ ("keys" | "secrets"), name, rest @ ..] = parts.as_slice() else {
        return Err(Fault::Unsupported);
    };
    if !valid_name(name) {
        return Err(Fault::Invalid("Invalid Azure vault resource name"));
    }
    let authority =
        authority(&request.uri, &request.headers).ok_or(Fault::Invalid("Host is required"))?;
    let host = authority.host().to_ascii_lowercase();
    let scheme = request.uri.scheme_str().unwrap_or_else(|| {
        if host.ends_with(".vault.azure.net") {
            "https"
        } else {
            "http"
        }
    });
    let name = name.to_ascii_lowercase();
    let record = format!("azure/{host}/{kind}/{name}");
    let id = format!("{scheme}://{authority}/{kind}/{name}");
    match (*kind, request.method(), rest) {
        ("secrets", &Method::PUT, []) => set_secret(store, request, record, &id),
        ("secrets", &Method::GET, [] | [_]) => {
            let version = version(rest.first().copied())?;
            let secret: Secret = store.get_version(&record, version)?;
            if secret.attributes.enabled == Some(false) {
                return Err(Fault::Denied("The selected secret version is disabled"));
            }
            Ok(secret_response(&id, &secret))
        }
        ("keys", &Method::POST, ["create"]) => create_key(store, request, record, &id),
        ("keys", &Method::GET, [] | [_]) => {
            let version = version(rest.first().copied())?;
            let key = store.get_version(&record, version)?;
            key_response(&id, &key)
        }
        (
            "keys",
            &Method::POST,
            [version_id, operation @ ("encrypt" | "decrypt" | "wrapkey" | "unwrapkey")],
        ) => {
            let version = version(Some(version_id))?;
            let key = store.get_version(&record, version)?;
            run_crypto(request, &id, &key, operation)
        }
        _ => Err(Fault::Unsupported),
    }
}

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 127
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

fn version(value: Option<&str>) -> Result<Option<&str>, Fault> {
    if let Some(value) = value {
        if value.len() != 32 || !value.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(Fault::Missing);
        }
    }
    Ok(value)
}

fn set_secret(store: &Store, request: &RequestExt, name: String, id: &str) -> Result<Value, Fault> {
    let request: SetSecret = body(request)?;
    if request.value.len() > 25 * 1024 {
        return Err(Fault::Invalid("Secret value exceeds 25 KiB"));
    }
    let secret = Secret {
        value: request.value,
        version: Uuid::new_v4().simple().to_string(),
        created: Utc::now().timestamp(),
        attributes: request.attributes.unwrap_or_default(),
        content_type: request.content_type,
        tags: request.tags,
    };
    store.create_version(name, &secret.version, &secret)?;
    Ok(secret_response(id, &secret))
}

fn create_key(store: &Store, request: &RequestExt, name: String, id: &str) -> Result<Value, Fault> {
    let request: CreateKey = body(request)?;
    if request.kty != "RSA" {
        return Err(Fault::Unsupported);
    }
    let size = request.key_size.unwrap_or(2048);
    if !matches!(size, 2048 | 3072 | 4096) {
        return Err(Fault::Invalid("RSA key size must be 2048, 3072, or 4096"));
    }
    let ops = request
        .key_ops
        .unwrap_or_else(|| RSA_OPERATIONS.map(str::to_owned).to_vec());
    if ops.iter().any(|op| !RSA_OPERATIONS.contains(&op.as_str())) {
        return Err(Fault::Unsupported);
    }
    let private = PrivateKey::new(&mut rand::rng(), size).map_err(|_| Fault::Internal)?;
    let der = private.to_pkcs8_der().map_err(|_| Fault::Internal)?;
    let key = Key {
        der: der.as_bytes().to_vec(),
        version: Uuid::new_v4().simple().to_string(),
        created: Utc::now().timestamp(),
        attributes: request.attributes.unwrap_or_default(),
        ops,
        tags: request.tags,
    };
    let response = key_response(id, &key)?;
    store.create_version(name, &key.version, &key)?;
    Ok(response)
}

fn attributes(created: i64, attributes: Attributes) -> Value {
    let mut value = json!({ "enabled": attributes.enabled.unwrap_or(true), "created": created, "updated": created });
    if let Some(nbf) = attributes.nbf {
        value["nbf"] = json!(nbf);
    }
    if let Some(exp) = attributes.exp {
        value["exp"] = json!(exp);
    }
    value
}

fn secret_response(id: &str, secret: &Secret) -> Value {
    let mut value = json!({
        "value": secret.value,
        "id": format!("{id}/{}", secret.version),
        "attributes": attributes(secret.created, secret.attributes),
    });
    if let Some(content_type) = &secret.content_type {
        value["contentType"] = json!(content_type);
    }
    if let Some(tags) = &secret.tags {
        value["tags"] = json!(tags);
    }
    value
}

fn key_response(id: &str, key: &Key) -> Result<Value, Fault> {
    let private = PrivateKey::from_pkcs8_der(&key.der).map_err(|_| Fault::Internal)?;
    let public = private.to_public_key();
    let modulus = public.n().to_be_bytes();
    let exponent = public.e().to_be_bytes();
    let leading_zeroes = exponent.iter().position(|value| *value != 0).unwrap_or(0);
    let mut value = json!({
        "key": {
            "kid": format!("{id}/{}", key.version), "kty": "RSA", "key_ops": key.ops,
            "n": URL_SAFE_NO_PAD.encode(&modulus), "e": URL_SAFE_NO_PAD.encode(&exponent[leading_zeroes..]),
        },
        "attributes": attributes(key.created, key.attributes),
    });
    if let Some(tags) = &key.tags {
        value["tags"] = json!(tags);
    }
    Ok(value)
}

fn run_crypto(request: &RequestExt, id: &str, key: &Key, operation: &str) -> Result<Value, Fault> {
    let usage = match operation {
        "wrapkey" => "wrapKey",
        "unwrapkey" => "unwrapKey",
        usage => usage,
    };
    let now = Utc::now().timestamp();
    if key.attributes.enabled == Some(false)
        || key.attributes.nbf.is_some_and(|value| value > now)
        || key.attributes.exp.is_some_and(|value| value <= now)
        || !key.ops.iter().any(|op| op == usage)
    {
        return Err(Fault::Denied(
            "The selected key version cannot perform this operation",
        ));
    }
    let request: CryptoRequest = body(request)?;
    let value = URL_SAFE_NO_PAD
        .decode(request.value)
        .map_err(|_| Fault::Invalid("Invalid base64url value"))?;
    let private = PrivateKey::from_pkcs8_der(&key.der).map_err(|_| Fault::Internal)?;
    let encrypt = matches!(operation, "encrypt" | "wrapkey");
    let mut rng = rand::rng();
    let result = match (request.alg.as_str(), encrypt) {
        ("RSA-OAEP", true) => {
            private
                .to_public_key()
                .encrypt(&mut rng, Oaep::<Sha1>::new(), &value)
        }
        ("RSA-OAEP-256", true) => {
            private
                .to_public_key()
                .encrypt(&mut rng, Oaep::<Sha256>::new(), &value)
        }
        ("RSA-OAEP", false) => private.decrypt_blinded(&mut rng, Oaep::<Sha1>::new(), &value),
        ("RSA-OAEP-256", false) => private.decrypt_blinded(&mut rng, Oaep::<Sha256>::new(), &value),
        _ => return Err(Fault::Unsupported),
    }
    .map_err(|_| {
        if encrypt {
            Fault::Invalid("Plaintext does not fit the RSA key")
        } else {
            Fault::Ciphertext
        }
    })?;
    Ok(json!({ "kid": format!("{id}/{}", key.version), "value": URL_SAFE_NO_PAD.encode(result) }))
}
