use std::collections::BTreeMap;

use base64::{engine::general_purpose::STANDARD, Engine as _};
use chrono::Utc;
use http::Method;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sqrzl_core::auth::{aws_request, AuthConfig};
use sqrzl_core::server::RequestExt;
use uuid::Uuid;

use crate::{body, crypto, Fault, Store};

const KEY_SPEC: &str = "SYMMETRIC_DEFAULT";
const ACCOUNT: &str = "000000000000";
const CIPHERTEXT_HEADER: &[u8] = b"SQKMS1";
const PLAINTEXT_MAX: usize = 4096;

#[derive(Serialize, Deserialize)]
struct Key {
    material: [u8; 32],
    description: String,
    created: i64,
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "PascalCase", deny_unknown_fields)]
struct Parameters {
    #[serde(rename = "KeyId")]
    key_id: Option<String>,
    key_spec: Option<String>,
    customer_master_key_spec: Option<String>,
    key_usage: Option<String>,
    origin: Option<String>,
    multi_region: Option<bool>,
    description: Option<String>,
    encryption_algorithm: Option<String>,
    encryption_context: Option<BTreeMap<String, String>>,
    plaintext: Option<String>,
    ciphertext_blob: Option<String>,
    number_of_bytes: Option<usize>,
    dry_run: Option<bool>,
    policy: Option<String>,
    tags: Option<Vec<Value>>,
    grant_tokens: Option<Vec<String>>,
}

pub(crate) fn handle(
    store: &Store,
    request: &RequestExt,
    config: &AuthConfig,
) -> Result<Value, Fault> {
    if request.path() != "/" || request.method != Method::POST {
        return Err(Fault::Unsupported);
    }
    if !aws_request::authorized(request, config, "kms") {
        return Err(Fault::Denied(
            "The request lacks the configured emulator credentials",
        ));
    }
    let region = region(request)?;
    let operation = request
        .header("x-amz-target")
        .and_then(|target| target.strip_prefix("TrentService."))
        .ok_or(Fault::Unsupported)?;
    let request: Parameters = body(request)?;
    if request.dry_run == Some(true)
        || request.policy.is_some()
        || request.tags.as_ref().is_some_and(|tags| !tags.is_empty())
        || request
            .grant_tokens
            .as_ref()
            .is_some_and(|tokens| !tokens.is_empty())
        || request
            .encryption_algorithm
            .as_deref()
            .is_some_and(|algorithm| algorithm != KEY_SPEC)
    {
        return Err(Fault::Unsupported);
    }
    match operation {
        "CreateKey" => create(store, region, request),
        "DescribeKey" => {
            let id = key_id(request.key_id.as_deref(), region)?;
            let key = store.get(&record_name(region, id))?;
            Ok(json!({ "KeyMetadata": metadata(region, id, &key) }))
        }
        "Encrypt" => {
            let plaintext = bytes(request.plaintext.as_deref(), "Plaintext")?;
            if plaintext.is_empty() || plaintext.len() > PLAINTEXT_MAX {
                return Err(Fault::Invalid(
                    "Plaintext must contain between 1 and 4096 bytes",
                ));
            }
            encrypt(store, region, &request, &plaintext, false)
        }
        "Decrypt" => decrypt(store, region, &request),
        "GenerateDataKey" | "GenerateDataKeyWithoutPlaintext" => {
            let len = match (request.key_spec.as_deref(), request.number_of_bytes) {
                (Some("AES_128"), None) => 16,
                (Some("AES_256"), None) => 32,
                (None, Some(len @ 1..=1024)) => len,
                _ => return Err(Fault::Invalid("Supply KeySpec or NumberOfBytes")),
            };
            let mut plaintext = vec![0; len];
            getrandom::fill(&mut plaintext).map_err(|_| Fault::Internal)?;
            let mut result = encrypt(store, region, &request, &plaintext, true)?;
            if operation == "GenerateDataKey" {
                result["Plaintext"] = json!(STANDARD.encode(plaintext));
            }
            Ok(result)
        }
        _ => Err(Fault::Unsupported),
    }
}

fn region(request: &RequestExt) -> Result<&str, Fault> {
    let scope = request
        .header("authorization")
        .and_then(|header| header.strip_prefix("AWS4-HMAC-SHA256 "))
        .and_then(|header| {
            header
                .split(',')
                .find_map(|field| field.trim().strip_prefix("Credential="))
        });
    let region = scope
        .and_then(|scope| scope.split('/').nth(2))
        .unwrap_or("us-east-1");
    if region.is_empty()
        || region.len() > 64
        || !region
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    {
        return Err(Fault::Invalid("Invalid AWS region"));
    }
    Ok(region)
}

fn key_id(value: Option<&str>, region: &str) -> Result<Uuid, Fault> {
    let value = value
        .filter(|id| !id.is_empty())
        .ok_or(Fault::Invalid("KeyId is required"))?;
    let value = if value.starts_with("arn:") {
        value
            .strip_prefix(&format!("arn:aws:kms:{region}:{ACCOUNT}:key/"))
            .ok_or(Fault::Missing)?
    } else {
        value
    };
    Uuid::parse_str(value).map_err(|_| Fault::Missing)
}

fn arn(region: &str, id: Uuid) -> String {
    format!("arn:aws:kms:{region}:{ACCOUNT}:key/{id}")
}

fn record_name(region: &str, id: Uuid) -> String {
    format!("aws/{region}/{id}")
}

fn metadata(region: &str, id: Uuid, key: &Key) -> Value {
    json!({
        "AWSAccountId": ACCOUNT, "KeyId": id, "Arn": arn(region, id),
        "Description": key.description, "CreationDate": key.created,
        "KeyUsage": "ENCRYPT_DECRYPT", "KeyState": "Enabled", "Enabled": true,
        "KeyManager": "CUSTOMER", "Origin": "AWS_KMS", "MultiRegion": false,
        "CustomerMasterKeySpec": KEY_SPEC, "KeySpec": KEY_SPEC, "EncryptionAlgorithms": [KEY_SPEC]
    })
}

fn create(store: &Store, region: &str, request: Parameters) -> Result<Value, Fault> {
    if request
        .key_spec
        .as_deref()
        .is_some_and(|spec| spec != KEY_SPEC)
        || request
            .customer_master_key_spec
            .as_deref()
            .is_some_and(|spec| spec != KEY_SPEC)
        || request
            .key_usage
            .as_deref()
            .is_some_and(|usage| usage != "ENCRYPT_DECRYPT")
        || request
            .origin
            .as_deref()
            .is_some_and(|origin| origin != "AWS_KMS")
        || request.multi_region == Some(true)
    {
        return Err(Fault::Unsupported);
    }
    let id = Uuid::new_v4();
    let key = Key {
        material: crypto::random()?,
        description: request.description.unwrap_or_default(),
        created: Utc::now().timestamp(),
    };
    store.create(record_name(region, id), &key)?;
    Ok(json!({ "KeyMetadata": metadata(region, id, &key) }))
}

fn bytes(value: Option<&str>, field: &'static str) -> Result<Vec<u8>, Fault> {
    let value = value.ok_or(Fault::Invalid(field))?;
    STANDARD
        .decode(value)
        .map_err(|_| Fault::Invalid("Invalid base64 field"))
}

fn context(request: &Parameters) -> Result<Vec<u8>, Fault> {
    serde_json::to_vec(&request.encryption_context.clone().unwrap_or_default())
        .map_err(|_| Fault::Internal)
}

fn encrypt(
    store: &Store,
    region: &str,
    request: &Parameters,
    plaintext: &[u8],
    data_key: bool,
) -> Result<Value, Fault> {
    let id = key_id(request.key_id.as_deref(), region)?;
    let key: Key = store.get(&record_name(region, id))?;
    let mut blob = CIPHERTEXT_HEADER.to_vec();
    blob.extend_from_slice(id.as_bytes());
    blob.extend(crypto::seal(&key.material, plaintext, &context(request)?)?);
    let mut response = json!({ "KeyId": arn(region, id), "CiphertextBlob": STANDARD.encode(blob) });
    if !data_key {
        response["EncryptionAlgorithm"] = json!(KEY_SPEC);
    }
    Ok(response)
}

fn decrypt(store: &Store, region: &str, request: &Parameters) -> Result<Value, Fault> {
    let blob = bytes(request.ciphertext_blob.as_deref(), "CiphertextBlob")?;
    let (id, ciphertext) = blob
        .strip_prefix(CIPHERTEXT_HEADER)
        .and_then(<[u8]>::split_first_chunk::<16>)
        .ok_or(Fault::Ciphertext)?;
    let id = Uuid::from_bytes(*id);
    if let Some(ref supplied) = request.key_id {
        if key_id(Some(supplied), region)? != id {
            return Err(Fault::WrongKey);
        }
    }
    let key: Key = store.get(&record_name(region, id))?;
    let plaintext = crypto::open(&key.material, ciphertext, &context(request)?)?;
    Ok(
        json!({ "KeyId": arn(region, id), "Plaintext": STANDARD.encode(plaintext), "EncryptionAlgorithm": KEY_SPEC }),
    )
}
