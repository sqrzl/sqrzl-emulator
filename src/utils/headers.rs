/// HTTP header handling for S3 responses
use chrono::{DateTime, Utc};
use md5;
use std::collections::HashMap;
use uuid::Uuid;

/// Compute MD5 hash (`ETag`) of data
#[must_use]
pub fn compute_etag(data: &[u8]) -> String {
    format!("{:x}", md5::compute(data))
}

/// Generate unique request ID
#[must_use]
pub fn generate_request_id() -> String {
    Uuid::new_v4().to_string()
}

/// Format a timestamp as RFC2822 (Last-Modified)
#[must_use]
pub fn format_last_modified_at(last_modified: &DateTime<Utc>) -> String {
    last_modified.to_rfc2822()
}

/// Format current time as RFC2822 (Last-Modified)
#[must_use]
pub fn format_last_modified() -> String {
    format_last_modified_at(&Utc::now())
}

/// Extract user-defined metadata headers (x-amz-meta-*) from HTTP headers
pub fn extract_metadata_from_http_headers(
    req: &dyn crate::auth::HttpRequestLike,
) -> HashMap<String, String> {
    let mut metadata = HashMap::new();

    for (name, value) in req.headers() {
        if let Some(key) = name.strip_prefix("x-amz-meta-") {
            metadata.insert(key.to_string(), value);
        }
    }

    metadata
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::body::Body;
    use crate::server::RequestExt as ParsedRequest;
    use bytes::Bytes;
    use chrono::{Duration as ChronoDuration, TimeZone, Utc};
    use hyper::Request as HyperRequest;
    use uuid::{Uuid, Variant, Version};

    #[test]
    fn should_compute_known_md5_vectors_when_compute_etag_called() {
        // Arrange
        // Act
        // Assert
        assert_eq!(compute_etag(b""), "d41d8cd98f00b204e9800998ecf8427e");
        assert_eq!(
            compute_etag(b"test data"),
            "eb733a00c0c9d336e65691a37ab54293"
        );
        assert_eq!(
            compute_etag(b"hello world"),
            "5eb63bbbe01eeed093cb22bb8f5acdc3"
        );
    }

    #[test]
    fn should_generate_parseable_uuid_v4_when_generate_request_id_called() {
        // Arrange
        // Act
        let request_id = generate_request_id();
        let uuid = Uuid::parse_str(&request_id).expect("request id should parse as a UUID");

        // Assert
        assert_eq!(uuid.to_string(), request_id);
        assert_eq!(uuid.get_variant(), Variant::RFC4122);
        assert_eq!(uuid.get_version(), Some(Version::Random));
    }

    #[test]
    fn should_format_parseable_rfc2822_timestamp_in_utc_when_format_last_modified_called() {
        // Arrange
        let before = Utc::now();

        // Act
        let formatted = format_last_modified();
        let parsed = chrono::DateTime::parse_from_rfc2822(&formatted)
            .expect("formatted timestamp should parse as RFC2822");
        let after = Utc::now();

        // Assert
        assert!(formatted.ends_with("+0000"));

        let parsed_utc = parsed.with_timezone(&Utc);
        let tolerance = ChronoDuration::seconds(10);
        assert!(parsed_utc >= before - tolerance);
        assert!(parsed_utc <= after + tolerance);
    }

    #[test]
    fn should_format_supplied_rfc2822_timestamp_in_utc_when_format_last_modified_at_called() {
        // Arrange
        let timestamp = Utc.with_ymd_and_hms(2024, 4, 10, 12, 34, 56).unwrap();

        // Act
        let formatted = format_last_modified_at(&timestamp);
        let parsed = chrono::DateTime::parse_from_rfc2822(&formatted)
            .expect("formatted timestamp should parse as RFC2822");

        // Assert
        assert_eq!(parsed.with_timezone(&Utc), timestamp);
    }

    async fn build_parsed_request(headers: &[(&str, &str)]) -> ParsedRequest {
        let mut builder = HyperRequest::builder()
            .method("PUT")
            .uri("http://localhost/example/object");

        for (name, value) in headers {
            builder = builder.header(*name, *value);
        }

        let request: HyperRequest<Body> = builder
            .body(Body::from(Bytes::new()))
            .expect("request should build");

        ParsedRequest::from_hyper(request)
            .await
            .expect("request should parse")
    }

    #[tokio::test]
    async fn should_extract_only_metadata_headers_from_real_request() {
        // Arrange
        let request = build_parsed_request(&[
            ("X-Amz-Meta-Owner", "alice"),
            ("x-amz-meta-environment", "prod"),
            ("Content-Type", "text/plain"),
            ("X-Custom-Header", "ignored"),
        ])
        .await;

        // Act
        let metadata = extract_metadata_from_http_headers(&request);

        // Assert
        assert_eq!(metadata.len(), 2);
        assert_eq!(metadata.get("owner"), Some(&"alice".to_string()));
        assert_eq!(metadata.get("environment"), Some(&"prod".to_string()));
        assert!(!metadata.contains_key("content-type"));
        assert!(!metadata.contains_key("x-custom-header"));
    }
}
