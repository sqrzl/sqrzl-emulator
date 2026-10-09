use crate::auth::HttpRequestLike;
use chrono::{DateTime, NaiveDateTime, TimeDelta, Utc};
use hmac::{Hmac, KeyInit, Mac};
use sha2::{Digest, Sha256};

/// Configuration for `SigV4` signature verification
#[derive(Clone)]
pub struct SigV4Config {
    pub access_key: String,
    pub secret_key: String,
}

/// AWS Signature Version 4 verifier
pub struct SignatureVerifier;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RequestValidationError {
    InvalidDate,
    ClockSkew,
    InvalidScope,
    InvalidSignedHeaders,
    PayloadMismatch,
    UnsupportedPayload,
}

impl SignatureVerifier {
    /// Validate the request contract before verifying its HMAC. Cryptographic
    /// verification alone does not establish freshness or payload integrity.
    pub(crate) fn validate_request(
        request: &dyn HttpRequestLike,
        credential_scope: &str,
        signed_headers: &[String],
        service: &str,
    ) -> Result<(), RequestValidationError> {
        Self::validate_request_at(
            request,
            credential_scope,
            signed_headers,
            service,
            Utc::now(),
        )
    }

    fn validate_request_at(
        request: &dyn HttpRequestLike,
        credential_scope: &str,
        signed_headers: &[String],
        service: &str,
        now: DateTime<Utc>,
    ) -> Result<(), RequestValidationError> {
        let (date_header, raw_date) = request
            .header("x-amz-date")
            .map(|value| ("x-amz-date", value))
            .or_else(|| request.header("date").map(|value| ("date", value)))
            .ok_or(RequestValidationError::InvalidDate)?;
        let date = if date_header == "x-amz-date" {
            NaiveDateTime::parse_from_str(raw_date, "%Y%m%dT%H%M%SZ")
                .ok()
                .filter(|parsed| parsed.format("%Y%m%dT%H%M%SZ").to_string() == raw_date)
                .map(|parsed| parsed.and_utc())
        } else {
            DateTime::parse_from_rfc2822(raw_date)
                .ok()
                .map(|parsed| parsed.with_timezone(&Utc))
        }
        .ok_or(RequestValidationError::InvalidDate)?;
        // S3 documents a fifteen-minute signing window; other supported AWS
        // services use the general five-minute SigV4 window.
        let window = TimeDelta::minutes(if service == "s3" { 15 } else { 5 });
        if date < now - window || date > now + window {
            return Err(RequestValidationError::ClockSkew);
        }
        let scope = credential_scope.split('/').collect::<Vec<_>>();
        if scope.len() != 4
            || scope[0] != date.format("%Y%m%d").to_string()
            || scope[1].is_empty()
            || scope[2] != service
            || scope[3] != "aws4_request"
        {
            return Err(RequestValidationError::InvalidScope);
        }
        if !signed_headers.iter().any(|name| name == "host")
            || !signed_headers.iter().any(|name| name == date_header)
            || signed_headers.windows(2).any(|pair| pair[0] >= pair[1])
            || signed_headers
                .iter()
                .any(|name| request.header(name).is_none())
            || request.headers().iter().any(|(name, _)| {
                name.starts_with("x-amz-")
                    && name != "x-amz-content-sha256"
                    && !signed_headers.iter().any(|signed| signed == name)
            })
        {
            return Err(RequestValidationError::InvalidSignedHeaders);
        }
        Self::validate_payload(request, service)
    }

    pub(crate) fn validate_payload(
        request: &dyn HttpRequestLike,
        service: &str,
    ) -> Result<(), RequestValidationError> {
        if let Some(provided) = request.header("x-amz-content-sha256") {
            if provided == "UNSIGNED-PAYLOAD" && service == "s3" {
                return Ok(());
            }
            if provided.len() != 64 || !provided.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                return Err(RequestValidationError::UnsupportedPayload);
            }
            let actual = request
                .content_sha256_hint()
                .map_or_else(|| Self::sha256_hex(request.body()), str::to_string);
            if !Self::constant_time_compare(
                provided.to_ascii_lowercase().as_bytes(),
                actual.as_bytes(),
            ) {
                return Err(RequestValidationError::PayloadMismatch);
            }
        }
        Ok(())
    }

    /// Verify the cryptographic AWS `SigV4` signature. Request callers must
    /// separately validate timestamp, credential scope, headers and body.
    #[must_use]
    pub fn verify(
        signature: &str,
        canonical_request: &str,
        amz_date: &str,
        credential_scope: &str,
        config: &SigV4Config,
    ) -> bool {
        // Compute the string to sign
        let canonical_request_hash = Self::sha256_hex(canonical_request.as_bytes());
        let string_to_sign =
            format!("AWS4-HMAC-SHA256\n{amz_date}\n{credential_scope}\n{canonical_request_hash}");

        // Extract date and region from credential scope (format: YYYYMMDD/region/s3/aws4_request)
        let parts: Vec<&str> = credential_scope.split('/').collect();
        if parts.len() < 2 {
            return false;
        }

        let date_stamp = parts[0];
        let region = parts.get(1).copied().unwrap_or("us-east-1");
        let service = parts.get(2).copied().unwrap_or("s3");

        // Compute the signing key
        let signing_key = Self::get_signature_key(&config.secret_key, date_stamp, region, service);

        // Compute the expected signature
        let expected_signature = Self::hmac_sha256_hex(&signing_key, string_to_sign.as_bytes());

        // Constant-time comparison to prevent timing attacks
        Self::constant_time_compare(signature.as_bytes(), expected_signature.as_bytes())
    }

    /// Hash data with SHA256
    fn sha256_hex(data: &[u8]) -> String {
        let mut hasher = Sha256::new();
        hasher.update(data);
        let result = hasher.finalize();
        hex::encode(result)
    }

    /// HMAC-SHA256
    fn hmac_sha256(key: &[u8], data: &[u8]) -> Vec<u8> {
        type HmacSha256 = Hmac<Sha256>;
        let mut mac = HmacSha256::new_from_slice(key).expect("HMAC can take key of any size");
        mac.update(data);
        mac.finalize().into_bytes().to_vec()
    }

    /// HMAC-SHA256 as hex string
    fn hmac_sha256_hex(key: &[u8], data: &[u8]) -> String {
        hex::encode(Self::hmac_sha256(key, data))
    }

    /// Derive the `SigV4` signing key
    fn get_signature_key(secret: &str, date_stamp: &str, region: &str, service: &str) -> Vec<u8> {
        let k_secret = format!("AWS4{secret}");
        let k_date = Self::hmac_sha256(k_secret.as_bytes(), date_stamp.as_bytes());
        let k_region = Self::hmac_sha256(&k_date, region.as_bytes());
        let k_service = Self::hmac_sha256(&k_region, service.as_bytes());
        Self::hmac_sha256(&k_service, b"aws4_request")
    }

    /// Constant-time comparison to prevent timing attacks
    fn constant_time_compare(a: &[u8], b: &[u8]) -> bool {
        if a.len() != b.len() {
            return false;
        }

        let mut result = 0u8;
        for (x, y) in a.iter().zip(b.iter()) {
            result |= x ^ y;
        }

        result == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_verify_valid_sigv4_signature() {
        // Arrange
        let config = SigV4Config {
            access_key: "AKIAIOSFODNN7EXAMPLE".to_string(),
            secret_key: "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY".to_string(),
        };

        // Example canonical request for a GET request
        let canonical_request =
            "GET\n/test-bucket/test-key\n\nhost:s3.amazonaws.com\n\nhost\nUNSIGNED-PAYLOAD";
        let amz_date = "20240101T120000Z";
        let credential_scope = "20240101/us-east-1/s3/aws4_request";

        // Pre-computed signature for this request
        let expected_hash = sha256_hex(canonical_request.as_bytes());
        let string_to_sign =
            format!("AWS4-HMAC-SHA256\n{amz_date}\n{credential_scope}\n{expected_hash}");

        let signing_key =
            SignatureVerifier::get_signature_key(&config.secret_key, "20240101", "us-east-1", "s3");
        let signature = SignatureVerifier::hmac_sha256_hex(&signing_key, string_to_sign.as_bytes());

        // Act
        // Verify the signature

        // Assert
        assert!(SignatureVerifier::verify(
            &signature,
            canonical_request,
            amz_date,
            credential_scope,
            &config
        ));
    }

    #[test]
    fn should_reject_invalid_signature() {
        // Arrange
        let config = SigV4Config {
            access_key: "AKIAIOSFODNN7EXAMPLE".to_string(),
            secret_key: "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY".to_string(),
        };

        let canonical_request =
            "GET\n/test-bucket/test-key\n\nhost:s3.amazonaws.com\n\nhost\nUNSIGNED-PAYLOAD";
        let amz_date = "20240101T120000Z";
        let credential_scope = "20240101/us-east-1/s3/aws4_request";

        // Act
        let invalid_signature = "invalid_signature_here";

        // Assert
        assert!(!SignatureVerifier::verify(
            invalid_signature,
            canonical_request,
            amz_date,
            credential_scope,
            &config
        ));
    }

    #[test]
    fn should_reject_modified_canonical_request() {
        // Arrange
        let config = SigV4Config {
            access_key: "AKIAIOSFODNN7EXAMPLE".to_string(),
            secret_key: "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY".to_string(),
        };

        let canonical_request =
            "GET\n/test-bucket/test-key\n\nhost:s3.amazonaws.com\n\nhost\nUNSIGNED-PAYLOAD";
        let amz_date = "20240101T120000Z";
        let credential_scope = "20240101/us-east-1/s3/aws4_request";

        // Compute signature for original request
        let expected_hash = sha256_hex(canonical_request.as_bytes());
        let string_to_sign =
            format!("AWS4-HMAC-SHA256\n{amz_date}\n{credential_scope}\n{expected_hash}");

        let signing_key =
            SignatureVerifier::get_signature_key(&config.secret_key, "20240101", "us-east-1", "s3");
        let signature = SignatureVerifier::hmac_sha256_hex(&signing_key, string_to_sign.as_bytes());

        // Act
        // Try to verify with modified request
        let modified_request =
            "GET\n/test-bucket/different-key\n\nhost:s3.amazonaws.com\n\nhost\nUNSIGNED-PAYLOAD";

        // Assert
        assert!(!SignatureVerifier::verify(
            &signature,
            modified_request,
            amz_date,
            credential_scope,
            &config
        ));
    }

    fn sha256_hex(data: &[u8]) -> String {
        SignatureVerifier::sha256_hex(data)
    }
}
