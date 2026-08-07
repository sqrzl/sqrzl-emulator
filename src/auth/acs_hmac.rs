use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use hmac::{Hmac, KeyInit, Mac};
use sha2::{Digest, Sha256};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConnectionString {
    pub endpoint: String,
    pub access_key: String,
}

/// Parse an Azure Communication Services connection string in the form
/// `endpoint=<url>;accesskey=<key>`.
#[must_use]
pub fn parse_connection_string(value: &str) -> Option<ConnectionString> {
    let mut endpoint: Option<String> = None;
    let mut access_key: Option<String> = None;

    for pair in value.split(';') {
        if pair.trim().is_empty() {
            continue;
        }
        let (key, value) = pair.split_once('=')?;
        match key.to_ascii_lowercase().as_str() {
            "endpoint" => endpoint = Some(value.to_string()),
            "accesskey" => access_key = Some(value.to_string()),
            _ => {}
        }
    }

    Some(ConnectionString {
        endpoint: endpoint?,
        access_key: access_key?,
    })
}

/// Base64-encoded SHA-256 hash used by the ACS `x-ms-content-sha256` header.
#[must_use]
pub fn content_hash(value: &[u8]) -> String {
    BASE64.encode(Sha256::digest(value))
}

/// Sign an ACS canonical request using the base64-decoded connection-string key.
#[must_use]
pub fn sign_request(access_key: &str, string_to_sign: &str) -> Option<String> {
    type HmacSha256 = Hmac<Sha256>;
    let key = BASE64.decode(access_key).ok()?;
    let mut mac = HmacSha256::new_from_slice(&key).ok()?;
    mac.update(string_to_sign.as_bytes());
    Some(BASE64.encode(mac.finalize().into_bytes()))
}

/// Validate the base64 ACS HMAC without converting it to a non-constant-time
/// textual comparison.
#[must_use]
pub fn validate_signature(
    access_key: &str,
    string_to_sign: &str,
    expected_signature: &str,
) -> bool {
    type HmacSha256 = Hmac<Sha256>;
    let (Ok(key), Ok(signature)) = (BASE64.decode(access_key), BASE64.decode(expected_signature))
    else {
        return false;
    };
    let Ok(mut mac) = HmacSha256::new_from_slice(&key) else {
        return false;
    };
    mac.update(string_to_sign.as_bytes());
    mac.verify_slice(&signature).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_parse_acs_connection_string() {
        let parsed = parse_connection_string(
            "endpoint=https://my.endpoint.communication.azure.com/;accesskey=test-key;",
        )
        .expect("connection string should parse");

        assert_eq!(
            parsed.endpoint,
            "https://my.endpoint.communication.azure.com/"
        );
        assert_eq!(parsed.access_key, "test-key");
    }

    #[test]
    fn should_sign_and_validate_official_acs_payload() {
        let access_key = BASE64.encode("shared-secret");
        let body_hash = content_hash(b"payload");
        let string_to_sign = format!(
            "POST\n/emails:send?api-version=2023-03-31\nThu, 07 Aug 2026 12:00:00 GMT;localhost:9000;{body_hash}"
        );
        let signature = sign_request(&access_key, &string_to_sign)
            .expect("base64 access key should be signable");

        assert!(validate_signature(&access_key, &string_to_sign, &signature));
        assert!(!validate_signature(
            &access_key,
            &format!("{string_to_sign}-changed"),
            &signature
        ));
    }
}
