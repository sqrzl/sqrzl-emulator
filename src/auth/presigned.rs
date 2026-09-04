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
    query_params: HashMap<String, String>,
}

impl PresignedUrl {
    /// Generate a presigned URL for GET access using AWS Signature Version 4
    #[must_use]
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
    #[must_use]
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
        let credential_scope = format!("{date_stamp}/{REGION}/{SERVICE}/aws4_request");
        let credential = format!("{}/{}", config.access_key, credential_scope);

        // Canonical URI (path component)
        let canonical_uri = canonical_uri(&format!("/{bucket}/{key}"));

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
            .trim_start_matches("https://")
            .trim_end_matches('/');
        let canonical_headers = format!("host:{host}\n");
        let signed_headers = "host";

        // Canonical request
        let canonical_request = format!(
            "{method}\n{canonical_uri}\n{canonical_query_string}\n{canonical_headers}\n{signed_headers}\nUNSIGNED-PAYLOAD"
        );

        // String to sign
        let canonical_request_hash = sha256_hex(canonical_request.as_bytes());
        let string_to_sign =
            format!("AWS4-HMAC-SHA256\n{amz_date}\n{credential_scope}\n{canonical_request_hash}");

        // Signing key
        let signing_key = get_signature_key(&config.secret_key, &date_stamp, REGION, SERVICE);
        let signature = hmac_sha256_hex(&signing_key, string_to_sign.as_bytes());

        format!(
            "{}/{}?{}&X-Amz-Signature={}",
            base_url.trim_end_matches('/'),
            canonical_uri.trim_start_matches('/'),
            canonical_query_string,
            signature
        )
    }

    /// Parse and validate a presigned URL from query parameters
    ///
    /// # Errors
    ///
    /// Returns an error when the underlying emulator operation fails.
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
            query_params: params.clone(),
        })
    }

    /// Validate the presigned URL signature and expiration
    ///
    /// # Errors
    ///
    /// Returns an error when the underlying emulator operation fails.
    pub fn validate(&self, host: &str, config: &PresignedUrlConfig) -> Result<(), String> {
        let path = format!("/{}/{}", self.bucket, self.key);
        let headers = HashMap::from([("host".to_string(), host.to_string())]);
        self.validate_components(&self.method, &path, &headers, config)
    }

    /// Validate this presign against the request that carried it.
    ///
    /// # Errors
    ///
    /// Returns an error when the signature fields, request headers, scope, or
    /// expiration do not match.
    pub fn validate_request(
        &self,
        request: &dyn crate::auth::HttpRequestLike,
        config: &PresignedUrlConfig,
    ) -> Result<(), String> {
        let headers = request
            .headers()
            .into_iter()
            .map(|(name, value)| (name.to_ascii_lowercase(), value))
            .collect::<HashMap<_, _>>();
        self.validate_components(request.method(), request.path(), &headers, config)
    }

    fn validate_components(
        &self,
        method: &str,
        path: &str,
        headers: &HashMap<String, String>,
        config: &PresignedUrlConfig,
    ) -> Result<(), String> {
        if self.query_params.get("X-Amz-Algorithm").map(String::as_str) != Some("AWS4-HMAC-SHA256")
        {
            return Err("Unsupported or missing X-Amz-Algorithm".to_string());
        }
        if !(1..=604_800).contains(&self.expires_in) {
            return Err("X-Amz-Expires must be between 1 and 604800 seconds".to_string());
        }

        let expires_at = self.date + Duration::seconds(self.expires_in);
        if Utc::now() > expires_at {
            return Err("Presigned URL has expired".to_string());
        }

        let scope = parse_credential_scope(&self.credential)?;
        if scope.access_key != config.access_key {
            return Err("Presigned URL access key does not match".to_string());
        }
        let date_stamp = self.date.format("%Y%m%d").to_string();
        if scope.date != date_stamp
            || scope.service != SERVICE
            || scope.terminator != "aws4_request"
        {
            return Err("Invalid credential scope".to_string());
        }
        let amz_date = self.date.format("%Y%m%dT%H%M%SZ").to_string();
        let credential_scope = format!(
            "{}/{}/{}/{}",
            scope.date, scope.region, scope.service, scope.terminator
        );

        let canonical_uri = canonical_uri(path);
        let canonical_query_string = canonical_query_string(&self.query_params);

        let signed_headers = self
            .query_params
            .get("X-Amz-SignedHeaders")
            .ok_or("Missing X-Amz-SignedHeaders")?;
        let signed_header_names = signed_headers
            .split(';')
            .map(|name| name.trim().to_ascii_lowercase())
            .filter(|name| !name.is_empty())
            .collect::<Vec<_>>();
        if !signed_header_names.iter().any(|name| name == "host") {
            return Err("X-Amz-SignedHeaders must include host".to_string());
        }
        let mut canonical_headers = String::new();
        for name in &signed_header_names {
            let value = headers
                .get(name)
                .ok_or_else(|| format!("Missing signed header: {name}"))?;
            canonical_headers.push_str(name);
            canonical_headers.push(':');
            canonical_headers.push_str(&normalize_header_value(value));
            canonical_headers.push('\n');
        }
        let signed_headers = signed_header_names.join(";");

        let canonical_request = format!(
            "{method}\n{canonical_uri}\n{canonical_query_string}\n{canonical_headers}\n{signed_headers}\nUNSIGNED-PAYLOAD"
        );

        let canonical_request_hash = sha256_hex(canonical_request.as_bytes());
        let string_to_sign =
            format!("AWS4-HMAC-SHA256\n{amz_date}\n{credential_scope}\n{canonical_request_hash}");

        let signing_key =
            get_signature_key(&config.secret_key, scope.date, scope.region, scope.service);
        let expected_sig = hmac_sha256_hex(&signing_key, string_to_sign.as_bytes());

        if self.signature != expected_sig {
            return Err("Invalid signature".to_string());
        }

        Ok(())
    }
}

struct CredentialScope<'a> {
    access_key: &'a str,
    date: &'a str,
    region: &'a str,
    service: &'a str,
    terminator: &'a str,
}

fn parse_credential_scope(credential: &str) -> Result<CredentialScope<'_>, String> {
    let segments = credential.split('/').collect::<Vec<_>>();
    if segments.len() != 5 || segments.iter().any(|segment| segment.is_empty()) {
        return Err("Invalid X-Amz-Credential".to_string());
    }
    Ok(CredentialScope {
        access_key: segments[0],
        date: segments[1],
        region: segments[2],
        service: segments[3],
        terminator: segments[4],
    })
}

fn normalize_header_value(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn canonical_uri(path: &str) -> String {
    let path = if path.is_empty() { "/" } else { path };
    path.split('/')
        .map(|segment| {
            let decoded = urlencoding::decode(segment)
                .map_or_else(|_| segment.to_string(), std::borrow::Cow::into_owned);
            uri_encode(&decoded)
        })
        .collect::<Vec<_>>()
        .join("/")
}

fn canonical_query_string(params: &HashMap<String, String>) -> String {
    let mut params = params
        .iter()
        .filter(|(name, _)| name.as_str() != "X-Amz-Signature")
        .map(|(name, value)| (uri_encode(name), uri_encode(value)))
        .collect::<Vec<_>>();
    params.sort();
    params
        .into_iter()
        .map(|(name, value)| format!("{name}={value}"))
        .collect::<Vec<_>>()
        .join("&")
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
    let k_secret = format!("AWS4{secret}");
    let k_date = hmac_sha256(k_secret.as_bytes(), date_stamp.as_bytes());
    let k_region = hmac_sha256(&k_date, region.as_bytes());
    let k_service = hmac_sha256(&k_region, service.as_bytes());
    hmac_sha256(&k_service, b"aws4_request")
}

fn uri_encode(s: &str) -> String {
    s.as_bytes()
        .iter()
        .map(|byte| {
            let ch = *byte as char;
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.' | '~') {
                ch.to_string()
            } else {
                format!("%{byte:02X}")
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
            "upload file.txt",
            1800,
            "http://localhost:9000",
            &config,
        );

        // Assert
        assert!(url.contains("test-bucket"));
        assert!(url.contains("upload%20file.txt"));
        assert!(url.contains("X-Amz-Expires=1800"));
    }

    #[test]
    fn should_parse_presigned_url_from_params() {
        // Arrange
        let access_key = "AKIAIOSFODNN7EXAMPLE";
        let now = Utc::now();
        let amz_date = now.format("%Y%m%dT%H%M%SZ").to_string();
        let date_stamp = now.format("%Y%m%d").to_string();
        let credential = format!("{access_key}/{date_stamp}/{REGION}/{SERVICE}/aws4_request");

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
        let credential = format!("{access_key}/{date_stamp}/{REGION}/{SERVICE}/aws4_request");

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
