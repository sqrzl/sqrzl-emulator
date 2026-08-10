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

        let mut query_params = HashMap::new();
        if let Some(query) = parts.uri.query() {
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
        }

        Ok(Request {
            method: parts.method,
            uri: parts.uri,
            headers: parts.headers,
            body: body_bytes,
            path_params: HashMap::new(),
            query_params,
        })
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
}

enum CollectBodyError {
    BodyRead(String),
    BodyTooLarge { max_request_bytes: usize },
}

async fn collect_body<B>(
    mut body: B,
    max_request_bytes: Option<usize>,
) -> Result<Bytes, CollectBodyError>
where
    B: hyper::body::Body<Data = Bytes> + Unpin,
    B::Error: std::fmt::Display,
{
    let Some(max_request_bytes) = max_request_bytes else {
        return body
            .collect()
            .await
            .map(http_body_util::Collected::to_bytes)
            .map_err(|err| CollectBodyError::BodyRead(err.to_string()));
    };

    let mut bytes = BytesMut::new();
    while let Some(frame) = body.frame().await {
        let frame = frame.map_err(|err| CollectBodyError::BodyRead(err.to_string()))?;
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
    use super::{Request, RouteMatch, Router};
    use bytes::Bytes;
    use http_body_util::Full;
    type Body = Full<Bytes>;
    use hyper::Request as HyperRequest;

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

    #[tokio::test]
    async fn should_route_virtual_hosted_style_bucket_requests() {
        let request = HyperRequest::builder()
            .method("GET")
            .uri("http://localhost/photos/kitten.jpg")
            .header("host", "media.localhost")
            .body(Body::default())
            .expect("request should build");

        let parsed = Request::from_hyper(request)
            .await
            .expect("request should parse");

        match Router::route(&parsed) {
            RouteMatch::ObjectGet(bucket, key) => {
                assert_eq!(bucket, "media");
                assert_eq!(key, "photos/kitten.jpg");
            }
            route => panic!("unexpected route: {route:?}"),
        }
    }

    #[tokio::test]
    async fn should_route_options_requests_to_existing_bucket_and_object_paths() {
        let bucket_request = HyperRequest::builder()
            .method("OPTIONS")
            .uri("http://localhost/media")
            .body(Body::default())
            .expect("request should build");
        let bucket_parsed = Request::from_hyper(bucket_request)
            .await
            .expect("request should parse");

        match Router::route(&bucket_parsed) {
            RouteMatch::BucketGet(bucket) => assert_eq!(bucket, "media"),
            route => panic!("unexpected route: {route:?}"),
        }

        let object_request = HyperRequest::builder()
            .method("OPTIONS")
            .uri("http://localhost/media/kitten.jpg")
            .body(Body::default())
            .expect("request should build");
        let object_parsed = Request::from_hyper(object_request)
            .await
            .expect("request should parse");

        match Router::route(&object_parsed) {
            RouteMatch::ObjectGet(bucket, key) => {
                assert_eq!(bucket, "media");
                assert_eq!(key, "kitten.jpg");
            }
            route => panic!("unexpected route: {route:?}"),
        }
    }

    #[tokio::test]
    async fn should_decode_s3_object_paths_once_without_collapsing_key_components() {
        let cases = [
            ("space%20name.txt", "space name.txt"),
            ("percent%25name.txt", "percent%name.txt"),
            ("nested%2Fname.txt", "nested/name.txt"),
            ("snowman-%E2%98%83.txt", "snowman-☃.txt"),
            ("literal%252Fescape.txt", "literal%2Fescape.txt"),
            ("dir/", "dir/"),
            ("a//b", "a//b"),
            ("/leading-slash", "/leading-slash"),
        ];

        for (encoded, expected) in cases {
            let path_style = HyperRequest::builder()
                .method("GET")
                .uri(format!("http://localhost/bucket/{encoded}"))
                .body(Body::default())
                .expect("path-style request should build");
            let path_style = Request::from_hyper(path_style)
                .await
                .expect("path-style request should parse");
            match Router::route(&path_style) {
                RouteMatch::ObjectGet(bucket, key) => {
                    assert_eq!(bucket, "bucket");
                    assert_eq!(key, expected);
                }
                route => panic!("unexpected path-style route: {route:?}"),
            }

            let virtual_hosted = HyperRequest::builder()
                .method("GET")
                .uri(format!("http://localhost/{encoded}"))
                .header("host", "bucket.s3.amazonaws.com")
                .body(Body::default())
                .expect("virtual-hosted request should build");
            let virtual_hosted = Request::from_hyper(virtual_hosted)
                .await
                .expect("virtual-hosted request should parse");
            match Router::route(&virtual_hosted) {
                RouteMatch::ObjectGet(bucket, key) => {
                    assert_eq!(bucket, "bucket");
                    assert_eq!(key, expected);
                }
                route => panic!("unexpected virtual-hosted route: {route:?}"),
            }
        }
    }

    #[tokio::test]
    async fn should_reject_malformed_s3_object_path_encoding() {
        let request = HyperRequest::builder()
            .method("PUT")
            .uri("http://localhost/bucket/bad%2")
            .body(Body::default())
            .expect("request should build");
        let parsed = Request::from_hyper(request)
            .await
            .expect("request should parse");

        assert!(matches!(
            Router::route(&parsed),
            RouteMatch::InvalidObjectPath
        ));
    }

    #[tokio::test]
    async fn should_preserve_dotted_s3_virtual_bucket_and_ignore_custom_endpoint_hosts() {
        let dotted = HyperRequest::builder()
            .method("PUT")
            .uri("http://localhost/object")
            .header("host", "my.bucket.s3.us-east-1.amazonaws.com")
            .body(Body::default())
            .expect("dotted virtual-host request should build");
        let dotted = Request::from_hyper(dotted)
            .await
            .expect("dotted virtual-host request should parse");
        assert!(matches!(
            Router::route(&dotted),
            RouteMatch::ObjectPut(bucket, key) if bucket == "my.bucket" && key == "object"
        ));

        let custom = HyperRequest::builder()
            .method("PUT")
            .uri("http://localhost/path-bucket/object")
            .header("host", "tenant.example.com")
            .body(Body::default())
            .expect("custom-endpoint request should build");
        let custom = Request::from_hyper(custom)
            .await
            .expect("custom-endpoint request should parse");
        assert!(matches!(
            Router::route(&custom),
            RouteMatch::ObjectPut(bucket, key) if bucket == "path-bucket" && key == "object"
        ));
    }
}

/// Router for S3 API endpoints
pub struct Router;

impl Router {
    fn bucket_from_host(host: &str) -> Option<String> {
        let host_without_port = host.split(':').next().unwrap_or(host);
        let lowercase = host_without_port.to_ascii_lowercase();
        if let Some(bucket) = lowercase.strip_suffix(".localhost") {
            return (!bucket.is_empty()).then(|| host_without_port[..bucket.len()].to_string());
        }

        let marker = lowercase.find(".s3")?;
        let endpoint = &lowercase[marker + 1..];
        let aws_domain =
            endpoint.ends_with(".amazonaws.com") || endpoint.ends_with(".amazonaws.com.cn");
        let bucket_endpoint = endpoint == "s3.amazonaws.com"
            || (aws_domain
                && (endpoint.starts_with("s3.")
                    || endpoint.starts_with("s3-")
                    || endpoint.starts_with("s3-accelerate."))
                && !endpoint.starts_with("s3-accesspoint")
                && !endpoint.starts_with("s3-control")
                && !endpoint.starts_with("s3-object-lambda")
                && !endpoint.starts_with("s3-outposts"));
        (marker > 0 && bucket_endpoint).then(|| host_without_port[..marker].to_string())
    }

    fn bucket_route(method: &Method, bucket: String) -> RouteMatch {
        match *method {
            Method::GET | Method::OPTIONS => RouteMatch::BucketGet(bucket),
            Method::PUT => RouteMatch::BucketPut(bucket),
            Method::DELETE => RouteMatch::BucketDelete(bucket),
            Method::HEAD => RouteMatch::BucketHead(bucket),
            Method::POST => RouteMatch::BucketPost(bucket),
            _ => RouteMatch::NotFound,
        }
    }

    fn object_route(method: &Method, bucket: String, encoded_key: &str) -> RouteMatch {
        let Ok(key) = crate::utils::request::decode_uri_path(encoded_key) else {
            return RouteMatch::InvalidObjectPath;
        };
        match *method {
            Method::GET | Method::OPTIONS => RouteMatch::ObjectGet(bucket, key),
            Method::PUT => RouteMatch::ObjectPut(bucket, key),
            Method::DELETE => RouteMatch::ObjectDelete(bucket, key),
            Method::HEAD => RouteMatch::ObjectHead(bucket, key),
            Method::POST => RouteMatch::ObjectPost(bucket, key),
            _ => RouteMatch::NotFound,
        }
    }

    pub fn route(req: &Request) -> RouteMatch {
        let method = req.method();
        let path = req.path().strip_prefix('/').unwrap_or(req.path());
        let host_bucket = req.host().and_then(Self::bucket_from_host);

        // Virtual-hosted-style object operations take precedence over path-style parsing.
        if let Some(bucket) = host_bucket {
            return if path.is_empty() {
                Self::bucket_route(method, bucket)
            } else {
                Self::object_route(method, bucket, path)
            };
        }

        if path.is_empty() {
            return if method == Method::GET {
                RouteMatch::ListBuckets
            } else {
                RouteMatch::NotFound
            };
        }

        match path.split_once('/') {
            Some((bucket, key)) if !bucket.is_empty() && !key.is_empty() => {
                Self::object_route(method, bucket.to_string(), key)
            }
            Some((bucket, "")) if !bucket.is_empty() => {
                Self::bucket_route(method, bucket.to_string())
            }
            None => Self::bucket_route(method, path.to_string()),
            _ => RouteMatch::NotFound,
        }
    }
}

#[derive(Debug)]
pub enum RouteMatch {
    ListBuckets,
    BucketGet(String),
    BucketPut(String),
    BucketDelete(String),
    BucketHead(String),
    BucketPost(String),
    ObjectGet(String, String),
    ObjectPut(String, String),
    ObjectDelete(String, String),
    ObjectHead(String, String),
    ObjectPost(String, String),
    InvalidObjectPath,
    NotFound,
}
