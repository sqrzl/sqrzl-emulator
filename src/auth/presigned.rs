use chrono::{DateTime, Duration, NaiveDateTime, Utc};
use std::collections::HashMap;

const REGION: &str = "us-east-1";
const SERVICE: &str = "s3";

/// Configuration for presigned URL generation
#[derive(Clone)]
pub struct PresignedUrlConfig {
    pub access_key: String,
    pub secret_key: String,
}

/// Presigned URL for temporary access to S3 resources
#[derive(Debug, Clone)]
pub struct PresignedUrl {
    pub bucket: String,
    pub key: String,
    pub method: String,
    pub date: DateTime<Utc>,
    pub expires_in: i64,
    pub signature: String,
    pub credential: String,
}

impl PresignedUrl {
    /// Generate a presigned URL for GET access using AWS Signature Version 4
    pub fn generate_get_url(
        bucket: &str,
        key: &str,
        expires_in_seconds: i64,
        base_url: &str,
        config: &PresignedUrlConfig,
    ) -> String {
        Self::generate_url(bucket, key, "GET", expires_in_seconds, base_url, config)
    }

    /// Generate a presigned URL for PUT access using AWS Signature Version 4
    pub fn generate_put_url(
        bucket: &str,
        key: &str,
        expires_in_seconds: i64,
        base_url: &str,
        config: &PresignedUrlConfig,
    ) -> String {
        Self::generate_url(bucket, key, "PUT", expires_in_seconds, base_url, config)
    }

    fn generate_url(
        bucket: &str,
        key: &str,
        method: &str,
        expires_in_seconds: i64,
        base_url: &str,
        config: &PresignedUrlConfig,
    ) -> String {
        let now = Utc::now();
        let amz_date = now.format("%Y%m%dT%H%M%SZ").to_string();
        let date_stamp = now.format("%Y%m%d").to_string();
        let credential_scope = format!("{}/{}/{}/aws4_request", date_stamp, REGION, SERVICE);
        let credential = format!("{}/{}", config.access_key, credential_scope);

        // Canonical URI (path component)
        let canonical_uri = format!("/{}/{}", bucket, key);

        // Canonical query string (must be sorted)
        let expires_str = expires_in_seconds.to_string();
        let mut query_params = [
            ("X-Amz-Algorithm", "AWS4-HMAC-SHA256"),
            ("X-Amz-Credential", &credential),
            ("X-Amz-Date", &amz_date),
            ("X-Amz-Expires", &expires_str),
            ("X-Amz-SignedHeaders", "host"),
        ];
        query_params.sort_by_key(|k| k.0);
        let canonical_query_string: String = query_params
            .iter()
            .map(|(k, v)| format!("{}={}", k, uri_encode(v)))
            .collect::<Vec<_>>()
            .join("&");

        // Canonical headers
        let host = base_url
            .trim_start_matches("http://")
            .trim_start_matches("https://");
        let canonical_headers = format!("host:{}\n", host);
        let signed_headers = "host";

        // Canonical request
        let canonical_request = format!(
            "{}\n{}\n{}\n{}\n{}\nUNSIGNED-PAYLOAD",
            method, canonical_uri, canonical_query_string, canonical_headers, signed_headers
        );

        // String to sign
        let canonical_request_hash = sha256_hex(canonical_request.as_bytes());
        let string_to_sign = format!(
            "AWS4-HMAC-SHA256\n{}\n{}\n{}",
            amz_date, credential_scope, canonical_request_hash
        );

        // Signing key
        let signing_key = get_signature_key(&config.secret_key, &date_stamp, REGION, SERVICE);
        let signature = hmac_sha256_hex(&signing_key, string_to_sign.as_bytes());

        format!(
            "{}/{}?{}&X-Amz-Signature={}",
            base_url,
            canonical_uri.trim_start_matches('/'),
            canonical_query_string,
            signature
        )
    }

    /// Parse and validate a presigned URL from query parameters
    pub fn from_query_params(
        bucket: &str,
        key: &str,
        method: &str,
        params: &HashMap<String, String>,
    ) -> Result<Self, String> {
        let signature = params
            .get("X-Amz-Signature")
            .or_else(|| params.get("Signature"))
            .ok_or("Missing signature")?;

        let expires_in = params
            .get("X-Amz-Expires")
            .or_else(|| params.get("Expires"))
            .and_then(|s| s.parse::<i64>().ok())
            .ok_or("Missing or invalid expires parameter")?;

        let amz_date = params
            .get("X-Amz-Date")
            .ok_or("Missing X-Amz-Date parameter")?;

        let credential = params
            .get("X-Amz-Credential")
            .ok_or("Missing X-Amz-Credential parameter")?;

        // Parse date from X-Amz-Date (format: 20240101T120000Z)
        let naive = NaiveDateTime::parse_from_str(amz_date, "%Y%m%dT%H%M%SZ")
            .map_err(|_| "Invalid X-Amz-Date format")?;
        let date = naive.and_utc();

        Ok(PresignedUrl {
            bucket: bucket.to_string(),
            key: key.to_string(),
            method: method.to_string(),
            date,
            expires_in,
            signature: signature.clone(),
            credential: credential.clone(),
        })
    }

    /// Validate the presigned URL signature and expiration
    pub fn validate(&self, host: &str, config: &PresignedUrlConfig) -> Result<(), String> {
        // Check expiration
        let expires_at = self.date + Duration::seconds(self.expires_in);
        if Utc::now() > expires_at {
            return Err("Presigned URL has expired".to_string());
        }

        // Compute expected signature using SigV4
        let date_stamp = self.date.format("%Y%m%d").to_string();
        let amz_date = self.date.format("%Y%m%dT%H%M%SZ").to_string();
        let credential_scope = format!("{}/{}/{}/aws4_request", date_stamp, REGION, SERVICE);

        // Canonical URI
        let canonical_uri = format!("/{}/{}", self.bucket, self.key);

        // Canonical query string (without signature)
        let expires_str = self.expires_in.to_string();
        let mut query_params = [
            ("X-Amz-Algorithm", "AWS4-HMAC-SHA256"),
            ("X-Amz-Credential", &self.credential),
            ("X-Amz-Date", &amz_date),
            ("X-Amz-Expires", &expires_str),
            ("X-Amz-SignedHeaders", "host"),
        ];
        query_params.sort_by_key(|k| k.0);
        let canonical_query_string: String = query_params
            .iter()
            .map(|(k, v)| format!("{}={}", k, uri_encode(v)))
            .collect::<Vec<_>>()
            .join("&");

        // Canonical headers
        let canonical_headers = format!("host:{}\n", host);
        let signed_headers = "host";

        // Canonical request
        let canonical_request = format!(
            "{}\n{}\n{}\n{}\n{}\nUNSIGNED-PAYLOAD",
            self.method, canonical_uri, canonical_query_string, canonical_headers, signed_headers
        );

        // String to sign
        let canonical_request_hash = sha256_hex(canonical_request.as_bytes());
        let string_to_sign = format!(
            "AWS4-HMAC-SHA256\n{}\n{}\n{}",
            amz_date, credential_scope, canonical_request_hash
        );

        // Signing key and signature
        let signing_key = get_signature_key(&config.secret_key, &date_stamp, REGION, SERVICE);
        let expected_sig = hmac_sha256_hex(&signing_key, string_to_sign.as_bytes());

        if self.signature != expected_sig {
            return Err("Invalid signature".to_string());
        }

        Ok(())
    }
}

// AWS SigV4 cryptographic helpers

fn sha256_hex(data: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(data);
    let result = hasher.finalize();
    hex::encode(result)
}

fn hmac_sha256(key: &[u8], data: &[u8]) -> Vec<u8> {
    use hmac::{Hmac, KeyInit, Mac};
    use sha2::Sha256;

    type HmacSha256 = Hmac<Sha256>;
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC can take key of any size");
    mac.update(data);
    mac.finalize().into_bytes().to_vec()
}

fn hmac_sha256_hex(key: &[u8], data: &[u8]) -> String {
    hex::encode(hmac_sha256(key, data))
}

fn get_signature_key(secret: &str, date_stamp: &str, region: &str, service: &str) -> Vec<u8> {
    let k_secret = format!("AWS4{}", secret);
    let k_date = hmac_sha256(k_secret.as_bytes(), date_stamp.as_bytes());
    let k_region = hmac_sha256(&k_date, region.as_bytes());
    let k_service = hmac_sha256(&k_region, service.as_bytes());
    hmac_sha256(&k_service, b"aws4_request")
}

fn uri_encode(s: &str) -> String {
    // URL encode per RFC 3986
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' || c == '~' {
                c.to_string()
            } else {
                format!("%{:02X}", c as u8)
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_generate_valid_get_url() {
        // Arrange
        let config = PresignedUrlConfig {
            access_key: "AKIAIOSFODNN7EXAMPLE".to_string(),
            secret_key: "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY".to_string(),
        };

        // Act
        let url = PresignedUrl::generate_get_url(
            "test-bucket",
            "test-key.txt",
            3600,
            "http://localhost:9000",
            &config,
        );

        // Assert
        assert!(url.contains("test-bucket"));
        assert!(url.contains("test-key.txt"));
        assert!(url.contains("X-Amz-Expires=3600"));
        assert!(url.contains("X-Amz-Signature="));
    }

    #[test]
    fn should_generate_valid_put_url() {
        // Arrange
        let config = PresignedUrlConfig {
            access_key: "AKIAIOSFODNN7EXAMPLE".to_string(),
            secret_key: "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY".to_string(),
        };

        // Act
        let url = PresignedUrl::generate_put_url(
            "test-bucket",
            "upload.txt",
            1800,
            "http://localhost:9000",
            &config,
        );

        // Assert
        assert!(url.contains("test-bucket"));
        assert!(url.contains("upload.txt"));
        assert!(url.contains("X-Amz-Expires=1800"));
    }

    #[test]
    fn should_parse_presigned_url_from_params() {
        // Arrange
        let access_key = "AKIAIOSFODNN7EXAMPLE";
        let now = Utc::now();
        let amz_date = now.format("%Y%m%dT%H%M%SZ").to_string();
        let date_stamp = now.format("%Y%m%d").to_string();
        let credential = format!(
            "{}/{}/{}/{}/aws4_request",
            access_key, date_stamp, REGION, SERVICE
        );

        let mut params = HashMap::new();
        params.insert("X-Amz-Signature".to_string(), "abc123".to_string());
        params.insert("X-Amz-Expires".to_string(), "3600".to_string());
        params.insert("X-Amz-Date".to_string(), amz_date);
        params.insert("X-Amz-Credential".to_string(), credential);

        // Act
        let result = PresignedUrl::from_query_params("bucket", "key", "GET", &params);

        // Assert
        assert!(result.is_ok(), "Failed to parse: {:?}", result.err());

        let presigned = result.unwrap();
        assert_eq!(presigned.bucket, "bucket");
        assert_eq!(presigned.key, "key");
        assert_eq!(presigned.method, "GET");
    }

    #[test]
    fn should_reject_expired_url() {
        // Arrange
        let config = PresignedUrlConfig {
            access_key: "AKIAIOSFODNN7EXAMPLE".to_string(),
            secret_key: "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY".to_string(),
        };
        let access_key = &config.access_key;
        let now = Utc::now();
        let past_date = (now - Duration::seconds(7200))
            .format("%Y%m%dT%H%M%SZ")
            .to_string();
        let date_stamp = (now - Duration::seconds(7200)).format("%Y%m%d").to_string();
        let credential = format!(
            "{}/{}/{}/{}/aws4_request",
            access_key, date_stamp, REGION, SERVICE
        );

        let mut params = HashMap::new();
        params.insert("X-Amz-Signature".to_string(), "abc123".to_string());
        params.insert("X-Amz-Expires".to_string(), "3600".to_string());
        params.insert("X-Amz-Date".to_string(), past_date);
        params.insert("X-Amz-Credential".to_string(), credential);

        // Act
        let presigned = PresignedUrl::from_query_params("bucket", "key", "GET", &params);

        // Assert
        assert!(presigned.is_ok(), "Failed to parse: {:?}", presigned.err());
        assert!(presigned
            .unwrap()
            .validate("localhost:9000", &config)
            .is_err());
    }

    #[test]
    fn should_validate_signature_correctly() {
        // Arrange
        let config = PresignedUrlConfig {
            access_key: "AKIAIOSFODNN7EXAMPLE".to_string(),
            secret_key: "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY".to_string(),
        };

        // Act
        let url = PresignedUrl::generate_get_url(
            "test-bucket",
            "test-key",
            3600,
            "http://localhost:9000",
            &config,
        );

        let query_start = url.find('?').unwrap();
        let query_str = &url[query_start + 1..];
        let mut params = HashMap::new();
        for param in query_str.split('&') {
            let parts: Vec<&str> = param.split('=').collect();
            if parts.len() == 2 {
                let decoded = parts[1].replace("%2F", "/");
                params.insert(parts[0].to_string(), decoded);
            }
        }

        let presigned = PresignedUrl::from_query_params("test-bucket", "test-key", "GET", &params);

        // Assert
        assert!(presigned.is_ok(), "Failed to parse: {:?}", presigned.err());
        assert!(presigned
            .unwrap()
            .validate("localhost:9000", &config)
            .is_ok());
    }
}
