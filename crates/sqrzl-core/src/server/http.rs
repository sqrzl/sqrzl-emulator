use crate::auth::HttpRequestLike;
use crate::body::Body;
use bytes::{Bytes, BytesMut};
use http::{HeaderMap, Method, Response as HttpResponse, StatusCode, Uri};
use http_body_util::BodyExt;
use hyper::Request as HyperRequest;
use std::collections::HashMap;
use std::fmt;
use std::str::FromStr;

/// Parsed HTTP request with extracted components
#[derive(Clone)]
pub struct Request {
    pub method: Method,
    pub uri: Uri,
    pub headers: http::HeaderMap,
    pub body: Bytes,
    pub path_params: HashMap<String, String>,
    pub query_params: HashMap<String, String>,
    /// Set when the request body was streamed straight to a file on disk
    /// instead of being buffered into `body` (see
    /// the server's streaming reader). `body` is empty whenever this is `Some`.
    pub spooled_body: Option<SpooledPayload>,
}

/// A request body already written to disk, with digests computed while it
/// was streamed there so callers never need to re-read it to hash it.
#[derive(Clone)]
pub struct SpooledPayload {
    pub path: std::path::PathBuf,
    pub len: u64,
    pub md5: [u8; 16],
    pub sha256_hex: String,
    pub sha384: [u8; 48],
    pub sha1: [u8; 20],
    pub crc32: u32,
    pub crc32c: u32,
    pub crc64_nvme: u64,
    _cleanup: std::sync::Arc<SpooledPayloadCleanup>,
}

struct SpooledPayloadCleanup {
    path: std::path::PathBuf,
}

impl Drop for SpooledPayloadCleanup {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

impl SpooledPayload {
    #[must_use]
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        path: std::path::PathBuf,
        len: u64,
        md5: [u8; 16],
        sha256_hex: String,
        sha384: [u8; 48],
        crc32c: u32,
        crc64_nvme: u64,
        sha1: [u8; 20],
        crc32: u32,
    ) -> Self {
        Self {
            _cleanup: std::sync::Arc::new(SpooledPayloadCleanup { path: path.clone() }),
            path,
            len,
            md5,
            sha256_hex,
            sha384,
            crc32c,
            crc64_nvme,
            sha1,
            crc32,
        }
    }
}

#[derive(Debug)]
pub enum RequestParseError {
    BodyRead {
        message: String,
        method: Method,
        uri: Uri,
        headers: HeaderMap,
    },
    BodyTooLarge {
        max_request_bytes: usize,
        method: Method,
        uri: Uri,
        headers: HeaderMap,
    },
}

impl fmt::Display for RequestParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BodyRead { message, .. } => write!(f, "{message}"),
            Self::BodyTooLarge {
                max_request_bytes, ..
            } => write!(
                f,
                "request body exceeds SQRZL_MAX_REQUEST_BYTES ({max_request_bytes} bytes)"
            ),
        }
    }
}

impl HttpRequestLike for Request {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).and_then(|h| h.to_str().ok())
    }

    fn query(&self) -> Option<&str> {
        self.uri.query()
    }

    fn method(&self) -> &str {
        self.method.as_str()
    }

    fn path(&self) -> &str {
        self.uri.path()
    }

    fn body(&self) -> &[u8] {
        &self.body
    }

    fn headers(&self) -> Vec<(String, String)> {
        self.headers
            .iter()
            .filter_map(|(name, value)| {
                value
                    .to_str()
                    .ok()
                    .map(|v| (name.as_str().to_lowercase(), v.to_string()))
            })
            .collect()
    }

    fn content_sha256_hint(&self) -> Option<&str> {
        self.spooled_body
            .as_ref()
            .map(|spooled| spooled.sha256_hex.as_str())
    }
}

impl Request {
    ///
    /// # Errors
    ///
    /// Returns an error when the underlying emulator operation fails.
    pub async fn from_hyper<B>(req: HyperRequest<B>) -> Result<Self, String>
    where
        B: hyper::body::Body<Data = Bytes> + Send + Unpin + 'static,
        B::Error: std::fmt::Display,
    {
        Self::from_hyper_with_max_body(req, None)
            .await
            .map_err(|err| err.to_string())
    }

    ///
    /// # Errors
    ///
    /// Returns an error when the underlying emulator operation fails.
    #[allow(clippy::result_large_err)]
    pub async fn from_hyper_with_max_body<B>(
        req: HyperRequest<B>,
        max_request_bytes: Option<usize>,
    ) -> Result<Self, RequestParseError>
    where
        B: hyper::body::Body<Data = Bytes> + Send + Unpin + 'static,
        B::Error: std::fmt::Display,
    {
        let (parts, body) = req.into_parts();
        let method = parts.method.clone();
        let uri = parts.uri.clone();
        let headers = parts.headers.clone();
        let body_bytes = collect_body(body, max_request_bytes)
            .await
            .map_err(|err| match err {
                CollectBodyError::BodyTooLarge { max_request_bytes } => {
                    RequestParseError::BodyTooLarge {
                        max_request_bytes,
                        method,
                        uri,
                        headers,
                    }
                }
                CollectBodyError::BodyRead(message) => RequestParseError::BodyRead {
                    message,
                    method,
                    uri,
                    headers,
                },
            })?;

        Ok(Self::from_parts(parts, body_bytes, None))
    }

    /// Builds a [`Request`] from already-split hyper parts and an
    /// already-obtained body, either buffered (`spooled_body: None`) or
    /// already spooled to disk (`spooled_body: Some(..)`, in which case
    /// `body` should be empty).
    pub fn from_parts(
        parts: http::request::Parts,
        body: Bytes,
        spooled_body: Option<SpooledPayload>,
    ) -> Self {
        Request {
            query_params: parse_query_params(parts.uri.query()),
            method: parts.method,
            uri: parts.uri,
            headers: parts.headers,
            body,
            path_params: HashMap::new(),
            spooled_body,
        }
    }

    pub fn path(&self) -> &str {
        self.uri.path()
    }

    pub fn method(&self) -> &Method {
        &self.method
    }

    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).and_then(|h| h.to_str().ok())
    }

    pub fn host(&self) -> Option<&str> {
        self.header("host")
    }

    pub fn query_param(&self, name: &str) -> Option<&str> {
        self.query_params.get(name).map(std::string::String::as_str)
    }

    pub fn has_query_param(&self, name: &str) -> bool {
        self.query_params.contains_key(name)
    }

    /// Number of bytes in the request payload, regardless of whether it was
    /// buffered or spooled to disk.
    #[must_use]
    pub fn payload_len(&self) -> u64 {
        self.spooled_body
            .as_ref()
            .map_or(self.body.len() as u64, |payload| payload.len)
    }

    /// Whether the request payload is empty.
    #[must_use]
    pub fn payload_is_empty(&self) -> bool {
        self.payload_len() == 0
    }

    /// MD5 digest of the request payload without materializing a spooled body.
    #[must_use]
    pub fn payload_md5(&self) -> [u8; 16] {
        self.spooled_body
            .as_ref()
            .map_or_else(|| md5::compute(&self.body).0, |payload| payload.md5)
    }

    /// SHA-384 digest of the request payload.
    #[must_use]
    pub fn payload_sha384(&self) -> [u8; 48] {
        use sha2::Digest as _;
        self.spooled_body.as_ref().map_or_else(
            || sha2::Sha384::digest(&self.body).into(),
            |payload| payload.sha384,
        )
    }

    /// SHA-256 digest of the request payload.
    #[must_use]
    pub fn payload_sha256(&self) -> [u8; 32] {
        use sha2::Digest as _;
        self.spooled_body.as_ref().map_or_else(
            || sha2::Sha256::digest(&self.body).into(),
            |payload| {
                let decoded = hex::decode(&payload.sha256_hex).unwrap_or_default();
                decoded.try_into().unwrap_or([0_u8; 32])
            },
        )
    }

    /// SHA-1 digest of the request payload without loading a spooled body.
    #[must_use]
    pub fn payload_sha1(&self) -> [u8; 20] {
        use sha1::Digest as _;
        self.spooled_body.as_ref().map_or_else(
            || sha1::Sha1::digest(&self.body).into(),
            |payload| payload.sha1,
        )
    }

    /// CRC32 digest of the request payload without loading a spooled body.
    #[must_use]
    pub fn payload_crc32(&self) -> u32 {
        self.spooled_body
            .as_ref()
            .map_or_else(|| crc32fast::hash(&self.body), |payload| payload.crc32)
    }

    /// CRC32C digest of the request payload.
    #[must_use]
    pub fn payload_crc32c(&self) -> u32 {
        self.spooled_body
            .as_ref()
            .map_or_else(|| crc32c::crc32c(&self.body), |payload| payload.crc32c)
    }

    /// Azure transactional CRC64-NVME digest of the request payload.
    #[must_use]
    pub fn payload_crc64_nvme(&self) -> u64 {
        self.spooled_body.as_ref().map_or_else(
            || {
                let mut digest = crc64fast_nvme::Digest::new();
                digest.write(&self.body);
                digest.sum64()
            },
            |payload| payload.crc64_nvme,
        )
    }
}

#[must_use]
pub fn parse_query_params(query: Option<&str>) -> HashMap<String, String> {
    let mut query_params = HashMap::new();
    let Some(query) = query else {
        return query_params;
    };
    for param in query.split('&') {
        if param.is_empty() {
            continue;
        }

        if let Some((key, value)) = param.split_once('=') {
            let decoded_key = urlencoding::decode(key).unwrap_or_default().to_string();
            let decoded_value = urlencoding::decode(value).unwrap_or_default().to_string();
            query_params.insert(decoded_key, decoded_value);
        } else {
            let decoded_key = urlencoding::decode(param).unwrap_or_default().to_string();
            query_params.insert(decoded_key, String::new());
        }
    }
    query_params
}

#[derive(Debug)]
pub enum CollectBodyError {
    BodyRead(String),
    BodyTooLarge { max_request_bytes: usize },
}

/// # Errors
/// Returns an error if the trailers contain an unsupported S3 checksum.
pub fn reject_s3_checksum_trailers(trailers: Option<&HeaderMap>) -> Result<(), CollectBodyError> {
    if trailers.is_some_and(|headers| {
        headers
            .keys()
            .any(|name| name.as_str().starts_with("x-amz-checksum-"))
    }) {
        return Err(CollectBodyError::BodyRead(
            "S3 checksum trailers are unsupported".to_string(),
        ));
    }
    Ok(())
}

/// # Errors
/// Returns an error for failed reads, unsupported trailers, or bodies above the limit.
pub async fn collect_body<B>(
    mut body: B,
    max_request_bytes: Option<usize>,
) -> Result<Bytes, CollectBodyError>
where
    B: hyper::body::Body<Data = Bytes> + Unpin,
    B::Error: std::fmt::Display,
{
    let Some(max_request_bytes) = max_request_bytes else {
        let collected = body
            .collect()
            .await
            .map_err(|err| CollectBodyError::BodyRead(err.to_string()))?;
        reject_s3_checksum_trailers(collected.trailers())?;
        return Ok(collected.to_bytes());
    };

    let mut bytes = BytesMut::new();
    while let Some(frame) = body.frame().await {
        let frame = frame.map_err(|err| CollectBodyError::BodyRead(err.to_string()))?;
        reject_s3_checksum_trailers(frame.trailers_ref())?;
        if let Some(data) = frame.data_ref() {
            let next_len = bytes.len().saturating_add(data.len());
            if next_len > max_request_bytes {
                return Err(CollectBodyError::BodyTooLarge { max_request_bytes });
            }
            bytes.extend_from_slice(data);
        }
    }

    Ok(bytes.freeze())
}

/// Builder for HTTP responses
pub struct ResponseBuilder {
    status: StatusCode,
    headers: http::HeaderMap,
    body: Vec<u8>,
}

impl ResponseBuilder {
    #[must_use]
    pub fn new(status: StatusCode) -> Self {
        Self {
            status,
            headers: http::HeaderMap::new(),
            body: Vec::new(),
        }
    }

    #[must_use]
    pub fn header(mut self, name: &str, value: &str) -> Self {
        if let Ok(header_name) = http::HeaderName::from_str(name) {
            if let Ok(header_value) = http::HeaderValue::from_str(value) {
                self.headers.insert(header_name, header_value);
            }
        }
        self
    }

    #[must_use]
    pub fn content_type(self, ct: &str) -> Self {
        self.header("content-type", ct)
    }

    #[must_use]
    pub fn body(mut self, body: Vec<u8>) -> Self {
        self.body = body;
        self
    }

    #[must_use]
    pub fn body_str(self, body: &str) -> Self {
        self.body(body.as_bytes().to_vec())
    }

    #[must_use]
    pub fn build(self) -> HttpResponse<Body> {
        let content_length = self.body.len();

        let mut response = HttpResponse::builder().status(self.status);

        for (name, value) in &self.headers {
            response = response.header(name.clone(), value.clone());
        }

        if content_length > 0 && !self.headers.contains_key("content-length") {
            response = response.header("content-length", content_length.to_string());
        }

        response.body(Body::from(self.body)).unwrap_or_else(|_| {
            // Last resort fallback - should never fail
            HttpResponse::new(Body::from("Internal Server Error"))
        })
    }

    #[must_use]
    pub fn empty(self) -> HttpResponse<Body> {
        let mut response = HttpResponse::builder().status(self.status);

        for (name, value) in &self.headers {
            response = response.header(name.clone(), value.clone());
        }

        response.body(Body::default()).unwrap_or_else(|_| {
            // Last resort fallback - should never fail
            HttpResponse::new(Body::default())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::Request;
    use bytes::Bytes;
    use http_body_util::Full;
    type Body = Full<Bytes>;
    use hyper::Request as HyperRequest;

    #[tokio::test]
    async fn should_reject_s3_checksum_trailers_in_both_buffered_collectors() {
        use http_body_util::BodyExt as _;
        for limit in [None, Some(1024)] {
            let mut trailers = http::HeaderMap::new();
            trailers.insert(
                "x-amz-checksum-crc32",
                http::HeaderValue::from_static("AAAAAA=="),
            );
            let body = Body::from(Bytes::from_static(b"payload"))
                .with_trailers(std::future::ready(Some(Ok(trailers))));
            let result = super::collect_body(body, limit).await;
            assert!(matches!(result, Err(super::CollectBodyError::BodyRead(_))));
        }
    }

    #[tokio::test]
    async fn should_preserve_bare_query_flags_when_parsing_requests() {
        // Arrange
        let request = HyperRequest::builder()
            .method("GET")
            .uri("http://localhost/bucket?versions&prefix=logs%2F")
            .body(Body::default())
            .expect("request should build");

        // Act
        let parsed = Request::from_hyper(request)
            .await
            .expect("request should parse");

        // Assert
        assert!(parsed.has_query_param("versions"));
        assert_eq!(parsed.query_param("versions"), Some(""));
        assert_eq!(parsed.query_param("prefix"), Some("logs/"));
    }
}
