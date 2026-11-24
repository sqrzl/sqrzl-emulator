use std::collections::{BTreeMap, BTreeSet};

use base64::{engine::general_purpose::STANDARD, Engine as _};
use chrono::Utc;
use http::Method;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sqrzl_core::auth::{aws_request, AuthConfig};
use sqrzl_core::server::RequestExt;
use uuid::Uuid;

use crate::{aws, body, Fault, Store};

pub(crate) const TARGET_PREFIX: &str = "secretsmanager.";
const CURRENT: &str = "AWSCURRENT";
const PREVIOUS: &str = "AWSPREVIOUS";
const MAX_SECRET_BYTES: usize = 65_536;

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase", deny_unknown_fields)]
struct Create {
    name: String,
    description: Option<String>,
    client_request_token: Option<String>,
    secret_string: Option<String>,
    secret_binary: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase", deny_unknown_fields)]
struct Put {
    #[serde(rename = "SecretId")]
    id: String,
    client_request_token: Option<String>,
    secret_string: Option<String>,
    secret_binary: Option<String>,
    version_stages: Option<Vec<String>>,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase", deny_unknown_fields)]
struct Get {
    #[serde(rename = "SecretId")]
    id: String,
    #[serde(rename = "VersionId")]
    version_id: Option<String>,
    version_stage: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase", deny_unknown_fields)]
struct Describe {
    #[serde(rename = "SecretId")]
    id: String,
}

#[derive(Serialize, Deserialize, PartialEq, Eq)]
enum SecretValue {
    Text(String),
    Binary(String),
}

#[derive(Serialize, Deserialize)]
struct Version {
    created: i64,
    value: SecretValue,
}

#[derive(Serialize, Deserialize)]
struct Secret {
    name: String,
    arn: String,
    description: Option<String>,
    created: i64,
    changed: i64,
    versions: BTreeMap<String, Version>,
    stages: BTreeMap<String, String>,
}

impl Secret {
    fn add_version(
        &mut self,
        id: String,
        value: SecretValue,
        stages: BTreeSet<String>,
    ) -> Result<(), Fault> {
        if let Some(existing) = self.versions.get(&id) {
            return if existing.value == value {
                Ok(())
            } else {
                Err(Fault::Conflict)
            };
        }

        let mut stages = stages;
        if self.versions.is_empty() {
            stages.insert(CURRENT.to_string());
        }
        let previous = stages
            .contains(CURRENT)
            .then(|| self.stages.get(CURRENT).cloned())
            .flatten();
        for stage in stages {
            self.stages.insert(stage, id.clone());
        }
        if let Some(previous) = previous {
            self.stages.insert(PREVIOUS.to_string(), previous);
        }
        let created = Utc::now().timestamp();
        self.changed = created;
        self.versions.insert(id, Version { created, value });
        Ok(())
    }

    fn labels(&self, version: &str) -> Vec<&str> {
        self.stages
            .iter()
            .filter(|(_, id)| id.as_str() == version)
            .map(|(label, _)| label.as_str())
            .collect()
    }

    fn matches_id(&self, id: &str) -> bool {
        self.name == id || self.arn == id
    }

    fn version_response(&self, version: &str, stages: bool) -> Value {
        let mut response = json!({"ARN": self.arn, "Name": self.name, "VersionId": version});
        if stages {
            response["VersionStages"] = json!(self.labels(version));
        }
        response
    }
}

pub(crate) fn handle(
    store: &Store,
    request: &RequestExt,
    config: &AuthConfig,
) -> Result<Value, Fault> {
    if request.path() != "/" || request.method != Method::POST {
        return Err(Fault::Unsupported);
    }
    if !aws_request::authorized(request, config, "secretsmanager") {
        return Err(Fault::Denied(
            "The request lacks the configured emulator credentials",
        ));
    }
    let region = aws::region(request)?;
    match request
        .header("x-amz-target")
        .and_then(|target| target.strip_prefix(TARGET_PREFIX))
    {
        Some("CreateSecret") => create(store, region, body(request)?),
        Some("PutSecretValue") => put(store, region, body(request)?),
        Some("GetSecretValue") => get(store, region, &body(request)?),
        Some("DescribeSecret") => describe(store, region, &body(request)?),
        _ => Err(Fault::Unsupported),
    }
}

fn create(store: &Store, region: &str, request: Create) -> Result<Value, Fault> {
    if !valid_name(&request.name) {
        return Err(Fault::Invalid("Invalid secret name"));
    }
    if request
        .description
        .as_ref()
        .is_some_and(|description| description.len() > 2048)
    {
        return Err(Fault::Invalid("The description exceeds 2048 bytes"));
    }
    let token = request.client_request_token;
    let version = version_id(token.as_deref())?;
    let value = secret_value(request.secret_string, request.secret_binary)?;
    store.alter(
        record_name(region, &request.name),
        |existing: Option<Secret>| {
            if let Some(secret) = existing {
                if token.is_some()
                    && secret
                        .versions
                        .get(&version)
                        .is_some_and(|existing| value.as_ref() == Some(&existing.value))
                {
                    let response = secret.version_response(&version, false);
                    return Ok((secret, response));
                }
                return Err(Fault::Conflict);
            }
            let name = request.name;
            let suffix = Uuid::new_v4().simple().to_string();
            let arn = format!("{}{}-{}", arn_prefix(region), name, &suffix[..6]);
            let created = Utc::now().timestamp();
            let mut secret = Secret {
                name,
                arn,
                description: request.description,
                created,
                changed: created,
                versions: BTreeMap::new(),
                stages: BTreeMap::new(),
            };
            let response = if let Some(value) = value {
                secret.add_version(version.clone(), value, BTreeSet::new())?;
                secret.version_response(&version, false)
            } else {
                json!({"ARN": secret.arn, "Name": secret.name})
            };
            Ok((secret, response))
        },
    )
}

fn put(store: &Store, region: &str, request: Put) -> Result<Value, Fault> {
    let value = secret_value(request.secret_string, request.secret_binary)?
        .ok_or(Fault::Invalid("Supply SecretString or SecretBinary"))?;
    let version = version_id(request.client_request_token.as_deref())?;
    let stages = request
        .version_stages
        .unwrap_or_else(|| vec![CURRENT.to_string()]);
    if !(1..=20).contains(&stages.len())
        || stages
            .iter()
            .any(|stage| stage.is_empty() || stage.len() > 256)
    {
        return Err(Fault::Invalid("Invalid version staging labels"));
    }
    let stages = stages.into_iter().collect();
    store.alter(
        secret_key(region, &request.id)?,
        |existing: Option<Secret>| {
            let mut secret = existing
                .filter(|secret| secret.matches_id(&request.id))
                .ok_or(Fault::Missing)?;
            secret.add_version(version.clone(), value, stages)?;
            let response = secret.version_response(&version, true);
            Ok((secret, response))
        },
    )
}

fn get(store: &Store, region: &str, request: &Get) -> Result<Value, Fault> {
    let secret = find(store, region, &request.id)?;
    let stage = match (
        request.version_stage.as_deref(),
        request.version_id.as_deref(),
    ) {
        (None, None) => Some(CURRENT),
        (stage, _) => stage,
    };
    let labeled = stage
        .map(|stage| secret.stages.get(stage).ok_or(Fault::Missing))
        .transpose()?
        .map(String::as_str);
    if matches!((request.version_id.as_deref(), labeled), (Some(id), Some(label)) if id != label) {
        return Err(Fault::Invalid(
            "VersionId and VersionStage must identify the same version",
        ));
    }
    let id = request
        .version_id
        .as_deref()
        .or(labeled)
        .ok_or(Fault::Missing)?;
    let version = secret.versions.get(id).ok_or(Fault::Missing)?;
    let mut response = secret.version_response(id, true);
    response["CreatedDate"] = json!(version.created);
    match &version.value {
        SecretValue::Text(value) => response["SecretString"] = json!(value),
        SecretValue::Binary(value) => response["SecretBinary"] = json!(value),
    }
    Ok(response)
}

fn describe(store: &Store, region: &str, request: &Describe) -> Result<Value, Fault> {
    let secret = find(store, region, &request.id)?;
    let versions = secret
        .versions
        .keys()
        .filter_map(|id| {
            let labels = secret.labels(id);
            (!labels.is_empty()).then_some((id, labels))
        })
        .collect::<BTreeMap<_, _>>();
    let mut response = json!({
        "ARN": secret.arn, "Name": secret.name, "CreatedDate": secret.created,
        "LastChangedDate": secret.changed, "VersionIdsToStages": versions,
    });
    if let Some(description) = secret.description {
        response["Description"] = json!(description);
    }
    Ok(response)
}

fn find(store: &Store, region: &str, id: &str) -> Result<Secret, Fault> {
    let secret: Secret = store.get(&secret_key(region, id)?)?;
    if !secret.matches_id(id) {
        return Err(Fault::Missing);
    }
    Ok(secret)
}

fn secret_value(
    text: Option<String>,
    binary: Option<String>,
) -> Result<Option<SecretValue>, Fault> {
    match (text, binary) {
        (None, None) => Ok(None),
        (Some(text), None) if (1..=MAX_SECRET_BYTES).contains(&text.len()) => {
            Ok(Some(SecretValue::Text(text)))
        }
        (None, Some(encoded)) => {
            let bytes = STANDARD
                .decode(encoded)
                .map_err(|_| Fault::Invalid("Invalid SecretBinary encoding"))?;
            if !(1..=MAX_SECRET_BYTES).contains(&bytes.len()) {
                return Err(Fault::Invalid("SecretBinary must contain 1 to 65536 bytes"));
            }
            Ok(Some(SecretValue::Binary(STANDARD.encode(bytes))))
        }
        (Some(_), None) => Err(Fault::Invalid("SecretString must contain 1 to 65536 bytes")),
        (Some(_), Some(_)) => Err(Fault::Invalid("Supply only one secret value")),
    }
}

fn version_id(token: Option<&str>) -> Result<String, Fault> {
    match token {
        Some(token) if (32..=64).contains(&token.len()) => Ok(token.to_string()),
        Some(_) => Err(Fault::Invalid(
            "ClientRequestToken must contain 32 to 64 bytes",
        )),
        None => Ok(Uuid::new_v4().to_string()),
    }
}

fn valid_name(name: &str) -> bool {
    (1..=512).contains(&name.len())
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"/_+=.@-".contains(&byte))
}

fn arn_prefix(region: &str) -> String {
    format!("arn:aws:secretsmanager:{region}:{}:secret:", aws::ACCOUNT)
}

fn record_name(region: &str, name: &str) -> String {
    format!("aws-secrets/{region}/{name}")
}

fn secret_key(region: &str, id: &str) -> Result<String, Fault> {
    let name = if id.starts_with("arn:") {
        id.strip_prefix(&arn_prefix(region))
            .and_then(|resource| resource.rsplit_once('-'))
            .map(|(name, _)| name)
            .ok_or(Fault::Missing)?
    } else {
        id
    };
    if !valid_name(name) {
        return Err(Fault::Invalid("Invalid SecretId"));
    }
    Ok(record_name(region, name))
}
