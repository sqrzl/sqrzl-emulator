use crate::auth::{AuthConfig, SigV4Config, SignatureVerifier};
use crate::server::RequestExt;
use sha2::{Digest, Sha256};

pub(super) fn authorized(request: &RequestExt, config: &AuthConfig, service: &str) -> bool {
    if !config.enforce_auth {
        return true;
    }
    let (Some(header), Some(access_key), Some(secret_key)) = (
        request.header("authorization"),
        config.access_key(),
        config.secret_key(),
    ) else {
        return false;
    };
    if !header.starts_with("AWS4-HMAC-SHA256") {
        return false;
    }
    let Some(signature) = parameter(header, "Signature=") else {
        return false;
    };
    let Some(signed_headers) = parameter(header, "SignedHeaders=") else {
        return false;
    };
    let signed_headers = signed_headers
        .split(';')
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_ascii_lowercase)
        .collect::<Vec<_>>();
    if signed_headers.is_empty() {
        return false;
    }
    let Some(credential) = parameter(header, "Credential=") else {
        return false;
    };
    let parts = credential.split('/').collect::<Vec<_>>();
    if parts.len() != 5
        || parts[0] != access_key
        || parts[3] != service
        || parts[4] != "aws4_request"
    {
        return false;
    }
    let scope = parts[1..].join("/");
    let Some(amz_date) = request
        .header("x-amz-date")
        .or_else(|| request.header("date"))
    else {
        return false;
    };
    let canonical = canonical_request(request, &signed_headers);
    SignatureVerifier::verify(
        &signature,
        &canonical,
        amz_date,
        &scope,
        &SigV4Config {
            access_key: access_key.to_string(),
            secret_key: secret_key.to_string(),
        },
    )
}

fn parameter(header: &str, prefix: &str) -> Option<String> {
    header.split(',').find_map(|part| {
        let part = part.trim();
        part.find(prefix).map(|offset| {
            part[offset + prefix.len()..]
                .split_whitespace()
                .next()
                .unwrap_or("")
                .to_string()
        })
    })
}

fn canonical_request(request: &RequestExt, signed_headers: &[String]) -> String {
    let mut headers = signed_headers
        .iter()
        .map(|name| {
            let value = request
                .header(name)
                .unwrap_or("")
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ");
            format!("{name}:{value}")
        })
        .collect::<Vec<_>>();
    headers.sort();
    let mut names = signed_headers.to_vec();
    names.sort();
    let payload_hash = request
        .header("x-amz-content-sha256")
        .filter(|value| !value.is_empty())
        .map_or_else(|| sha256_hex(&request.body), str::to_string);
    format!(
        "{}\n{}\n{}\n{}\n\n{}\n{}",
        request.method(),
        canonical_uri(request.path()),
        canonical_query(request.uri.query()),
        headers.join("\n"),
        names.join(";"),
        payload_hash
    )
}

fn canonical_uri(path: &str) -> String {
    if path.is_empty() {
        return "/".to_string();
    }
    aws_encode(path, false)
}

fn canonical_query(query: Option<&str>) -> String {
    let Some(query) = query else {
        return String::new();
    };
    let mut fields = query
        .split('&')
        .filter(|part| !part.is_empty())
        .map(|part| {
            let (key, value) = part.split_once('=').unwrap_or((part, ""));
            let key = urlencoding::decode(key)
                .map_or_else(|_| key.to_string(), std::borrow::Cow::into_owned);
            let value = urlencoding::decode(value)
                .map_or_else(|_| value.to_string(), std::borrow::Cow::into_owned);
            (aws_encode(&key, true), aws_encode(&value, true))
        })
        .collect::<Vec<_>>();
    fields.sort();
    fields
        .into_iter()
        .map(|(key, value)| format!("{key}={value}"))
        .collect::<Vec<_>>()
        .join("&")
}

fn aws_encode(value: &str, encode_slash: bool) -> String {
    let mut encoded = String::new();
    for byte in value.bytes() {
        let character = byte as char;
        if character.is_ascii_alphanumeric()
            || matches!(character, '-' | '_' | '.' | '~')
            || (!encode_slash && character == '/')
        {
            encoded.push(character);
        } else {
            use std::fmt::Write as _;
            let _ = write!(encoded, "%{byte:02X}");
        }
    }
    encoded
}

fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}
