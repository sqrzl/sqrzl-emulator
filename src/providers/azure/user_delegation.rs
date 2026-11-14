//! Get User Delegation Key and user delegation SAS validation.
//!
//! The emulator has no Microsoft Entra ID. Get User Delegation Key accepts the
//! documented local bearer credential (`Bearer <account key>`), and issues a
//! key for one fixed local principal. The key value is derived from the account
//! key and the signed key fields, so a SAS signed with it validates statelessly,
//! including across restarts, and any edit to a signed key field invalidates it.

use super::{sign_hmac_base64, AzureBlobAdapter, AzureResource, Request};
use crate::auth::AuthConfig;
use crate::body::Body;
use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use chrono::{DateTime, SecondsFormat, Utc};
use http::{Method, StatusCode};
use hyper::Response;
use quick_xml::events::Event;
use quick_xml::Reader;

/// Object ID of the single local principal that receives delegation keys.
pub(super) const LOCAL_OBJECT_ID: &str = "00000000-0000-4000-8000-00000000a2e1";
/// Tenant ID of the local principal.
pub(super) const LOCAL_TENANT_ID: &str = "00000000-0000-4000-8000-000000000000";
const LOCAL_FALLBACK_SECRET: &[u8] = b"sqrzl-local-user-delegation-key";
const MAX_KEY_LIFETIME_DAYS: i64 = 7;
/// First signed version whose user delegation string-to-sign carries the
/// delegated-user tenant/object fields and the signed request header/query slots.
const EXTENDED_LAYOUT_VERSION: &str = "2026-04-06";

/// Whether the request is `POST /?restype=service&comp=userdelegationkey`.
pub(super) fn is_key_request(req: &Request, resource: &AzureResource) -> bool {
    resource.container.is_none()
        && req.method() == Method::POST
        && req.query_param("restype") == Some("service")
        && req.query_param("comp") == Some("userdelegationkey")
}

/// Authorizes Get User Delegation Key with the local bearer credential.
///
/// Azure accepts only OAuth for this operation, so Shared Key and SAS are refused.
///
/// # Errors
/// Returns the failure message for a missing, non-bearer, or wrong credential.
pub(super) fn authorize_key_request(req: &Request, config: &AuthConfig) -> Result<(), String> {
    let authorization = req
        .header("authorization")
        .ok_or_else(|| "Get User Delegation Key requires bearer authorization".to_string())?;
    let Some(token) = authorization.strip_prefix("Bearer ") else {
        return Err(
            "Get User Delegation Key accepts only bearer (OAuth) authorization".to_string(),
        );
    };
    if config.azure_account_key() == Some(token.trim()) {
        Ok(())
    } else {
        Err("Invalid bearer token".to_string())
    }
}

struct SignedKey<'a> {
    oid: &'a str,
    tid: &'a str,
    start: &'a str,
    expiry: &'a str,
    service: &'a str,
    version: &'a str,
    delegated_tid: &'a str,
}

fn key_secret(config: &AuthConfig) -> Vec<u8> {
    AzureBlobAdapter::shared_key_secret(config).unwrap_or_else(|| LOCAL_FALLBACK_SECRET.to_vec())
}

fn derive_key_value(secret: &[u8], key: &SignedKey<'_>) -> Result<String, String> {
    let material = [
        "sqrzl-azure-user-delegation-key",
        key.oid,
        key.tid,
        key.start,
        key.expiry,
        key.service,
        key.version,
        key.delegated_tid,
    ]
    .join("\n");
    sign_hmac_base64(secret, &material)
}

fn xml_fields(body: &[u8]) -> Result<Vec<(String, String)>, ()> {
    let mut reader = Reader::from_reader(body);
    let mut path = Vec::new();
    let mut fields = Vec::new();
    loop {
        match reader.read_event().map_err(|_| ())? {
            Event::Start(start) => {
                path.push(start.local_name().as_ref().to_string());
            }
            Event::End(_) => {
                path.pop();
            }
            Event::Text(text) => {
                if path.len() == 2 && path[0] == "KeyInfo" {
                    let value = quick_xml::escape::unescape(text.as_ref())
                        .map_err(|_| ())?
                        .trim()
                        .to_string();
                    fields.push((path[1].clone(), value));
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }
    if fields.is_empty() && !String::from_utf8_lossy(body).contains("<KeyInfo") {
        return Err(());
    }
    Ok(fields)
}

fn invalid_node(message: &str) -> Response<Body> {
    AzureBlobAdapter::error_response(StatusCode::BAD_REQUEST, "InvalidXmlNodeValue", message)
}

fn parse_time(value: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(value)
        .ok()
        .map(|time| time.with_timezone(&Utc))
}

/// Handles Get User Delegation Key.
///
/// # Errors
/// Returns an error only when the key cannot be signed.
pub(super) fn get_user_delegation_key(
    req: &Request,
    config: &AuthConfig,
) -> Result<Response<Body>, String> {
    get_user_delegation_key_at(req, config, Utc::now())
}

fn get_user_delegation_key_at(
    req: &Request,
    config: &AuthConfig,
    now: DateTime<Utc>,
) -> Result<Response<Body>, String> {
    let Ok(fields) = xml_fields(&req.body) else {
        return Ok(AzureBlobAdapter::error_response(
            StatusCode::BAD_REQUEST,
            "InvalidXmlDocument",
            "The KeyInfo request body is not valid XML.",
        ));
    };
    let field = |name: &str| {
        fields
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
            .filter(|value| !value.is_empty())
    };
    let Some(expiry) = field("Expiry") else {
        return Ok(invalid_node("KeyInfo must include an Expiry time."));
    };
    let start = field("Start").map_or_else(
        || now.to_rfc3339_opts(SecondsFormat::Secs, true),
        str::to_string,
    );
    let (Some(start_time), Some(expiry_time)) = (parse_time(&start), parse_time(expiry)) else {
        return Ok(invalid_node(
            "KeyInfo Start and Expiry must be ISO 8601 UTC times.",
        ));
    };
    if expiry_time <= start_time || expiry_time <= now {
        return Ok(invalid_node(
            "KeyInfo Expiry must be later than Start and the current time.",
        ));
    }
    if expiry_time > now + chrono::Duration::days(MAX_KEY_LIFETIME_DAYS) {
        return Ok(invalid_node(
            "KeyInfo Expiry must be within 7 days of the current time.",
        ));
    }
    let version = AzureBlobAdapter::service_version(req).unwrap_or(super::AZURE_VERSION);
    let delegated_tid = field("DelegatedUserTid").unwrap_or_default();
    let key = SignedKey {
        oid: LOCAL_OBJECT_ID,
        tid: LOCAL_TENANT_ID,
        start: &start,
        expiry,
        service: "b",
        version,
        delegated_tid,
    };
    let value = derive_key_value(&key_secret(config), &key)?;
    let mut xml = String::from("<?xml version=\"1.0\" encoding=\"utf-8\"?><UserDelegationKey>");
    for (element, text) in [
        ("SignedOid", key.oid),
        ("SignedTid", key.tid),
        ("SignedStart", key.start),
        ("SignedExpiry", key.expiry),
        ("SignedService", key.service),
        ("SignedVersion", key.version),
    ] {
        push_element(&mut xml, element, text);
    }
    if !delegated_tid.is_empty() {
        push_element(&mut xml, "SignedDelegatedUserTid", delegated_tid);
    }
    push_element(&mut xml, "Value", &value);
    xml.push_str("</UserDelegationKey>");
    Ok(AzureBlobAdapter::xml_response(StatusCode::OK, xml))
}

fn push_element(xml: &mut String, element: &str, text: &str) {
    xml.push('<');
    xml.push_str(element);
    xml.push('>');
    crate::utils::xml::push_escaped_xml(xml, text);
    xml.push_str("</");
    xml.push_str(element);
    xml.push('>');
}

/// The signed SAS fields that select the user delegation path.
pub(super) const KEY_FIELDS: [&str; 6] = ["skoid", "sktid", "skt", "ske", "sks", "skv"];

/// Whether a SAS carries any user delegation key field.
pub(super) fn is_user_delegation_sas(req: &Request) -> bool {
    KEY_FIELDS
        .iter()
        .chain(std::iter::once(&"skdutid"))
        .any(|field| {
            req.query_param(field)
                .is_some_and(|value| !value.is_empty())
        })
}

/// Builds the user delegation string-to-sign and signing key for a SAS.
///
/// # Errors
/// Returns the authentication failure message for missing or inconsistent key
/// fields, or a key outside its validity window.
pub(super) fn string_to_sign_and_key(
    req: &Request,
    config: &AuthConfig,
    canonical_resource: &str,
    now: DateTime<Utc>,
) -> Result<(String, Vec<u8>), String> {
    let param = |name: &str| req.query_param(name).unwrap_or("");
    if KEY_FIELDS.iter().any(|field| param(field).is_empty()) {
        return Err(
            "A user delegation SAS must include skoid, sktid, skt, ske, sks, and skv".to_string(),
        );
    }
    if param("sks") != "b" {
        return Err("The user delegation key was not issued for the Blob service".to_string());
    }
    let (Some(key_start), Some(key_expiry)) = (parse_time(param("skt")), parse_time(param("ske")))
    else {
        return Err("Invalid user delegation key validity times".to_string());
    };
    if now < key_start {
        return Err("The user delegation key is not valid yet".to_string());
    }
    if now > key_expiry {
        return Err("The user delegation key has expired".to_string());
    }
    let version = param("sv");
    let extended = version >= EXTENDED_LAYOUT_VERSION;
    if !extended && !param("skdutid").is_empty() {
        return Err("skdutid requires signed version 2025-07-05 or later".to_string());
    }
    let key = SignedKey {
        oid: param("skoid"),
        tid: param("sktid"),
        start: param("skt"),
        expiry: param("ske"),
        service: param("sks"),
        version: param("skv"),
        delegated_tid: param("skdutid"),
    };
    let value = derive_key_value(&key_secret(config), &key)?;
    let signing_key = BASE64
        .decode(value)
        .map_err(|_| "Invalid derived user delegation key".to_string())?;
    let mut fields = vec![
        param("sp"),
        param("st"),
        param("se"),
        canonical_resource,
        key.oid,
        key.tid,
        key.start,
        key.expiry,
        key.service,
        key.version,
        "", // saoid (rejected before signing)
        "", // suoid
        "", // scid
    ];
    if extended {
        fields.extend([key.delegated_tid, ""]); // skdutid, sduoid
    }
    fields.extend(["", param("spr"), version, param("sr"), "", param("ses")]);
    if extended {
        fields.extend(["", ""]); // srh, srq
    }
    fields.extend(["", "", "", "", ""]); // rscc, rscd, rsce, rscl, rsct
    Ok((fields.join("\n"), signing_key))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use hyper::Request as HyperRequest;

    fn config() -> AuthConfig {
        Config {
            access_key_id: Some("devstoreaccount1".to_string()),
            secret_access_key: Some(BASE64.encode("topsecretkey")),
            enforce_auth: true,
            admin_auth_disabled: false,
            blobs_path: "./blobs".to_string(),
            lifecycle_interval: std::time::Duration::from_hours(1),
            api_port: 9000,
            ui_port: 9001,
            max_request_bytes: crate::config::DEFAULT_SQRZL_MAX_REQUEST_BYTES,
            smtp_port: crate::config::DEFAULT_SQRZL_SMTP_PORT,
            vendor_credentials: crate::config::VendorCredentials::default(),
            mcp: crate::config::McpConfig::default(),
        }
    }

    async fn request(method: &str, uri: &str, headers: &[(&str, &str)], body: &str) -> Request {
        let mut builder = HyperRequest::builder().method(method).uri(uri);
        for (name, value) in headers {
            builder = builder.header(*name, *value);
        }
        Request::from_hyper(builder.body(Body::from(body.as_bytes().to_vec())).unwrap())
            .await
            .unwrap()
    }

    fn element<'a>(xml: &'a str, name: &str) -> &'a str {
        let start = xml.find(&format!("<{name}>")).unwrap() + name.len() + 2;
        let end = xml.find(&format!("</{name}>")).unwrap();
        &xml[start..end]
    }

    #[tokio::test]
    async fn should_issue_delegation_keys_only_to_bearer_callers_within_seven_days() {
        // Arrange
        let config = config();
        let now = parse_time("2026-11-13T10:00:00Z").unwrap();
        let bearer = format!("Bearer {}", config.azure_account_key().unwrap());
        let body = "<KeyInfo><Start>2026-11-13T09:00:00Z</Start><Expiry>2026-11-14T09:00:00Z</Expiry></KeyInfo>";
        let uri = "http://localhost/devstoreaccount1?restype=service&comp=userdelegationkey";
        let valid = request(
            "POST",
            uri,
            &[("authorization", &bearer), ("x-ms-version", "2026-10-06")],
            body,
        )
        .await;
        let shared_key = request(
            "POST",
            uri,
            &[("authorization", "SharedKey devstoreaccount1:x")],
            body,
        )
        .await;
        let too_long = request(
            "POST",
            uri,
            &[("authorization", &bearer)],
            "<KeyInfo><Expiry>2026-11-21T10:00:01Z</Expiry></KeyInfo>",
        )
        .await;

        // Act
        let issued = get_user_delegation_key_at(&valid, &config, now).unwrap();
        let issued_xml = String::from_utf8(
            http_body_util::BodyExt::collect(issued.into_body())
                .await
                .unwrap()
                .to_bytes()
                .to_vec(),
        )
        .unwrap();
        let rejected = get_user_delegation_key_at(&too_long, &config, now).unwrap();

        // Assert
        assert!(authorize_key_request(&valid, &config).is_ok());
        assert!(authorize_key_request(&shared_key, &config).is_err());
        assert_eq!(element(&issued_xml, "SignedOid"), LOCAL_OBJECT_ID);
        assert_eq!(element(&issued_xml, "SignedStart"), "2026-11-13T09:00:00Z");
        assert_eq!(element(&issued_xml, "SignedService"), "b");
        assert_eq!(element(&issued_xml, "SignedVersion"), "2026-10-06");
        assert_eq!(
            BASE64.decode(element(&issued_xml, "Value")).unwrap().len(),
            32
        );
        assert_eq!(rejected.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn should_sign_user_delegation_sas_with_version_specific_layouts_until_key_expiry() {
        // Arrange
        let config = config();
        let now = parse_time("2026-11-13T10:00:00Z").unwrap();
        let key_query = format!(
            "skoid={LOCAL_OBJECT_ID}&sktid={LOCAL_TENANT_ID}&skt=2026-11-13T09:00:00Z&ske=2026-11-14T09:00:00Z&sks=b&skv=2026-10-06"
        );
        let modern = request(
            "GET",
            &format!("http://localhost/a/c/b?sp=r&se=2026-11-14T00:00:00Z&sv=2026-10-06&sr=b&{key_query}&sig=x"),
            &[],
            "",
        )
        .await;
        let legacy = request(
            "GET",
            &format!("http://localhost/a/c/b?sp=r&se=2026-11-14T00:00:00Z&sv=2025-01-05&sr=b&{key_query}&sig=x"),
            &[],
            "",
        )
        .await;
        let after_expiry = parse_time("2026-11-14T09:00:01Z").unwrap();

        // Act
        let (modern_sts, modern_key) =
            string_to_sign_and_key(&modern, &config, "/blob/a/c/b", now).unwrap();
        let (legacy_sts, legacy_key) =
            string_to_sign_and_key(&legacy, &config, "/blob/a/c/b", now).unwrap();
        let expired = string_to_sign_and_key(&modern, &config, "/blob/a/c/b", after_expiry);

        // Assert
        assert_eq!(modern_sts.split('\n').count(), 28);
        assert_eq!(legacy_sts.split('\n').count(), 24);
        assert!(modern_sts.starts_with(&format!(
            "r\n\n2026-11-14T00:00:00Z\n/blob/a/c/b\n{LOCAL_OBJECT_ID}\n"
        )));
        assert_eq!(modern_key, legacy_key);
        assert_eq!(modern_key.len(), 32);
        assert_eq!(expired.unwrap_err(), "The user delegation key has expired");
    }
}
