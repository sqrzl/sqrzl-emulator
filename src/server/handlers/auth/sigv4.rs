use crate::auth::{AuthConfig, SigV4Config, SignatureVerifier};
use crate::body::Body;
use crate::services::xml_error_response;
use crate::utils::headers as header_utils;
use hex;
use http::StatusCode;
use hyper::Response;
use sha2::{Digest, Sha256};
use tracing::warn;

/// Verify SigV4 signature in the request.
#[allow(clippy::result_large_err)]
pub(crate) fn verify_sigv4_signature(
    req: &dyn crate::auth::HttpRequestLike,
    auth_config: &AuthConfig,
) -> Result<bool, Response<Body>> {
    if !auth_config.enforce_auth {
        return Ok(true);
    }

    let auth_header = match req.header("authorization") {
        Some(h) => h,
        None => return Ok(true),
    };

    if !auth_header.starts_with("AWS4-HMAC-SHA256") {
        return Ok(true);
    }

    let req_id = header_utils::generate_request_id();

    let amz_date = match req.header("x-amz-date").or_else(|| req.header("date")) {
        Some(d) => d.to_string(),
        None => {
            return Err(xml_error_response(
                StatusCode::BAD_REQUEST,
                "InvalidRequest",
                "Missing date header",
                &req_id,
            ));
        }
    };

    let signature = match extract_sigv4_signature(auth_header) {
        Some(sig) => sig,
        None => {
            return Err(xml_error_response(
                StatusCode::BAD_REQUEST,
                "InvalidRequest",
                "Missing signature in authorization header",
                &req_id,
            ));
        }
    };

    let signed_headers = match extract_signed_headers(auth_header) {
        Some(headers) if !headers.is_empty() => headers,
        _ => {
            return Err(xml_error_response(
                StatusCode::BAD_REQUEST,
                "InvalidRequest",
                "Missing signed headers in authorization header",
                &req_id,
            ));
        }
    };

    let credential_scope = match extract_credential_scope(auth_header) {
        Some(scope) => scope,
        None => {
            return Err(xml_error_response(
                StatusCode::BAD_REQUEST,
                "InvalidRequest",
                "Missing credential in authorization header",
                &req_id,
            ));
        }
    };

    let secret_key = match auth_config.secret_key() {
        Some(key) => key,
        None => {
            warn!("SigV4 signature verification requested but no secret key configured");
            return Ok(true);
        }
    };

    let access_key = match auth_config.access_key() {
        Some(key) => key,
        None => {
            warn!("SigV4 signature verification requested but no access key configured");
            return Ok(true);
        }
    };

    let canonical_request = build_canonical_request(req, &signed_headers);
    let sigv4_config = SigV4Config {
        access_key: access_key.to_string(),
        secret_key: secret_key.to_string(),
    };

    let is_valid = SignatureVerifier::verify(
        &signature,
        &canonical_request,
        &amz_date,
        &credential_scope,
        &sigv4_config,
    );

    if !is_valid {
        warn!("SigV4 signature verification failed");
        return Err(xml_error_response(
            StatusCode::FORBIDDEN,
            "SignatureDoesNotMatch",
            "The provided signature does not match",
            &req_id,
        ));
    }

    Ok(true)
}

/// Extract signature from SigV4 Authorization header.
#[cfg_attr(test, allow(dead_code))]
pub(crate) fn extract_sigv4_signature(auth_header: &str) -> Option<String> {
    for part in auth_header.split(',') {
        let part = part.trim();
        if let Some(stripped) = part.strip_prefix("Signature=") {
            return Some(stripped.to_string());
        }
    }
    None
}

/// Extract credential scope from SigV4 Authorization header.
#[cfg_attr(test, allow(dead_code))]
pub(crate) fn extract_credential_scope(auth_header: &str) -> Option<String> {
    for part in auth_header.split(',') {
        let part = part.trim();
        if let Some(cred_start) = part.find("Credential=") {
            let credential = &part[cred_start + 11..];
            if let Some(slash_pos) = credential.find('/') {
                let scope = &credential[slash_pos + 1..];
                return Some(scope.split(',').next().unwrap_or(scope).to_string());
            }
        }
    }
    None
}

/// Extract the SignedHeaders list from SigV4 Authorization header.
#[cfg_attr(test, allow(dead_code))]
pub(crate) fn extract_signed_headers(auth_header: &str) -> Option<Vec<String>> {
    for part in auth_header.split(',') {
        let part = part.trim();
        if let Some(headers) = part.strip_prefix("SignedHeaders=") {
            let parsed: Vec<String> = headers
                .split(';')
                .map(|h| h.trim().to_lowercase())
                .filter(|h| !h.is_empty())
                .collect();
            return Some(parsed);
        }
    }
    None
}

fn sha256_hex(data: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(data);
    hex::encode(hasher.finalize())
}

fn uri_encode(value: &str, encode_slash: bool) -> String {
    let mut out = String::new();
    for byte in value.bytes() {
        let ch = byte as char;
        let should_keep = ch.is_ascii_alphanumeric()
            || matches!(ch, '-' | '_' | '.' | '~')
            || (!encode_slash && ch == '/');

        if should_keep {
            out.push(ch);
        } else {
            out.push_str(&format!("%{:02X}", byte));
        }
    }
    out
}

fn canonical_uri(path: &str) -> String {
    if path.is_empty() {
        "/".to_string()
    } else {
        uri_encode(path, false)
    }
}

fn canonical_query_string(query: Option<&str>) -> String {
    let Some(query) = query else {
        return String::new();
    };

    let mut params: Vec<(String, String)> = query
        .split('&')
        .filter(|part| !part.is_empty())
        .map(|part| {
            let (raw_key, raw_value) = part.split_once('=').unwrap_or((part, ""));
            let key = urlencoding::decode(raw_key)
                .map(|decoded| decoded.into_owned())
                .unwrap_or_else(|_| raw_key.to_string());
            let value = urlencoding::decode(raw_value)
                .map(|decoded| decoded.into_owned())
                .unwrap_or_else(|_| raw_value.to_string());
            (uri_encode(&key, true), uri_encode(&value, true))
        })
        .collect();

    params.sort();

    params
        .into_iter()
        .map(|(key, value)| format!("{}={}", key, value))
        .collect::<Vec<_>>()
        .join("&")
}

/// Build canonical request for SigV4 verification using the same rules as the TS SDK signer.
#[cfg_attr(test, allow(dead_code))]
pub(crate) fn build_canonical_request(
    req: &dyn crate::auth::HttpRequestLike,
    signed_headers: &[String],
) -> String {
    let method = req.method();
    let canonical_uri = canonical_uri(req.path());
    let canonical_query = canonical_query_string(req.query());

    let mut canonical_headers: Vec<String> = signed_headers
        .iter()
        .map(|name| {
            let value = req.header(name).unwrap_or("");
            let normalized_value = value.split_whitespace().collect::<Vec<_>>().join(" ");
            format!("{}:{}", name, normalized_value)
        })
        .collect();

    canonical_headers.sort();

    let canonical_headers_str = canonical_headers.join("\n");
    let signed_headers_str = {
        let mut names = signed_headers.to_vec();
        names.sort();
        names.join(";")
    };

    let payload_hash = req
        .header("x-amz-content-sha256")
        .filter(|value| !value.is_empty())
        .map(|value| value.to_string())
        .unwrap_or_else(|| sha256_hex(req.body()));

    format!(
        "{}\n{}\n{}\n{}\n\n{}\n{}",
        method,
        canonical_uri,
        canonical_query,
        canonical_headers_str,
        signed_headers_str,
        payload_hash
    )
}
