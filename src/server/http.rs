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
    BodyRead(String),
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
            Self::BodyRead(message) => write!(f, "{message}"),
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
    pub async fn from_hyper<B>(req: HyperRequest<B>) -> Result<Self, String>
    where
        B: hyper::body::Body<Data = Bytes> + Send + Unpin + 'static,
        B::Error: std::fmt::Display,
    {
        Self::from_hyper_with_max_body(req, None)
            .await
            .map_err(|err| err.to_string())
    }

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
                RequestParseError::BodyTooLarge {
                    max_request_bytes, ..
                } => RequestParseError::BodyTooLarge {
                    max_request_bytes,
                    method,
                    uri,
                    headers,
                },
                other => other,
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
        self.query_params.get(name).map(|s| s.as_str())
    }

    pub fn has_query_param(&self, name: &str) -> bool {
        self.query_params.contains_key(name)
    }
}

async fn collect_body<B>(
    mut body: B,
    max_request_bytes: Option<usize>,
) -> Result<Bytes, RequestParseError>
where
    B: hyper::body::Body<Data = Bytes> + Unpin,
    B::Error: std::fmt::Display,
{
    let Some(max_request_bytes) = max_request_bytes else {
        return body
            .collect()
            .await
            .map(|collected| collected.to_bytes())
            .map_err(|err| RequestParseError::BodyRead(err.to_string()));
    };

    let mut bytes = BytesMut::new();
    while let Some(frame) = body.frame().await {
        let frame = frame.map_err(|err| RequestParseError::BodyRead(err.to_string()))?;
        if let Some(data) = frame.data_ref() {
            let next_len = bytes.len().saturating_add(data.len());
            if next_len > max_request_bytes {
                return Err(RequestParseError::BodyTooLarge {
                    max_request_bytes,
                    method: Method::GET,
                    uri: Uri::from_static("/"),
                    headers: HeaderMap::new(),
                });
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
    pub fn new(status: StatusCode) -> Self {
        Self {
            status,
            headers: http::HeaderMap::new(),
            body: Vec::new(),
        }
    }

    pub fn header(mut self, name: &str, value: &str) -> Self {
        if let Ok(header_name) = http::HeaderName::from_str(name) {
            if let Ok(header_value) = http::HeaderValue::from_str(value) {
                self.headers.insert(header_name, header_value);
            }
        }
        self
    }

    pub fn content_type(self, ct: &str) -> Self {
        self.header("content-type", ct)
    }

    pub fn body(mut self, body: Vec<u8>) -> Self {
        self.body = body;
        self
    }

    pub fn body_str(self, body: &str) -> Self {
        self.body(body.as_bytes().to_vec())
    }

    pub fn build(self) -> HttpResponse<Body> {
        let content_length = self.body.len();

        let mut response = HttpResponse::builder().status(self.status);

        for (name, value) in self.headers.iter() {
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

    pub fn empty(self) -> HttpResponse<Body> {
        let mut response = HttpResponse::builder().status(self.status);

        for (name, value) in self.headers.iter() {
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
            route => panic!("unexpected route: {:?}", route),
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
            route => panic!("unexpected route: {:?}", route),
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
            route => panic!("unexpected route: {:?}", route),
        }
    }
}

/// Router for S3 API endpoints
pub struct Router;

impl Router {
    fn bucket_from_host(host: &str) -> Option<String> {
        let host_without_port = host.split(':').next().unwrap_or(host);

        if host_without_port.eq_ignore_ascii_case("localhost")
            || host_without_port.parse::<std::net::IpAddr>().is_ok()
        {
            return None;
        }

        let labels: Vec<&str> = host_without_port.split('.').collect();
        if labels.len() < 2 {
            return None;
        }

        let candidate = labels[0];
        if candidate.is_empty()
            || candidate.eq_ignore_ascii_case("s3")
            || candidate.eq_ignore_ascii_case("blob")
            || candidate.eq_ignore_ascii_case("storage")
        {
            return None;
        }

        Some(candidate.to_string())
    }

    pub fn route(req: &Request) -> RouteMatch {
        let method = req.method();
        let path = req.path();
        let parts: Vec<&str> = path
            .trim_start_matches('/')
            .split('/')
            .filter(|s| !s.is_empty())
            .collect();
        let host_bucket = req.host().and_then(Self::bucket_from_host);

        match parts.as_slice() {
            // List buckets: GET /
            [] if method == Method::GET && host_bucket.is_none() => RouteMatch::ListBuckets,
            [] if host_bucket.is_some() => match *method {
                Method::GET => RouteMatch::BucketGet(host_bucket.unwrap_or_default()),
                Method::OPTIONS => RouteMatch::BucketGet(host_bucket.unwrap_or_default()),
                Method::PUT => RouteMatch::BucketPut(host_bucket.unwrap_or_default()),
                Method::DELETE => RouteMatch::BucketDelete(host_bucket.unwrap_or_default()),
                Method::HEAD => RouteMatch::BucketHead(host_bucket.unwrap_or_default()),
                Method::POST => RouteMatch::BucketPost(host_bucket.unwrap_or_default()),
                _ => RouteMatch::NotFound,
            },

            // Virtual-hosted-style object operations take precedence over path-style parsing.
            key if !key.is_empty() && host_bucket.is_some() => {
                let key = key.join("/");
                let bucket = host_bucket.unwrap_or_default();
                match *method {
                    Method::GET => RouteMatch::ObjectGet(bucket, key),
                    Method::OPTIONS => RouteMatch::ObjectGet(bucket, key),
                    Method::PUT => RouteMatch::ObjectPut(bucket, key),
                    Method::DELETE => RouteMatch::ObjectDelete(bucket, key),
                    Method::HEAD => RouteMatch::ObjectHead(bucket, key),
                    Method::POST => RouteMatch::ObjectPost(bucket, key),
                    _ => RouteMatch::NotFound,
                }
            }

            // Bucket operations
            [bucket] => match *method {
                Method::GET => RouteMatch::BucketGet(bucket.to_string()),
                Method::OPTIONS => RouteMatch::BucketGet(bucket.to_string()),
                Method::PUT => RouteMatch::BucketPut(bucket.to_string()),
                Method::DELETE => RouteMatch::BucketDelete(bucket.to_string()),
                Method::HEAD => RouteMatch::BucketHead(bucket.to_string()),
                Method::POST => RouteMatch::BucketPost(bucket.to_string()),
                _ => RouteMatch::NotFound,
            },

            // Object operations
            [bucket, key @ ..] if !key.is_empty() => {
                let key = key.join("/");
                match *method {
                    Method::GET => RouteMatch::ObjectGet(bucket.to_string(), key),
                    Method::OPTIONS => RouteMatch::ObjectGet(bucket.to_string(), key),
                    Method::PUT => RouteMatch::ObjectPut(bucket.to_string(), key),
                    Method::DELETE => RouteMatch::ObjectDelete(bucket.to_string(), key),
                    Method::HEAD => RouteMatch::ObjectHead(bucket.to_string(), key),
                    Method::POST => RouteMatch::ObjectPost(bucket.to_string(), key),
                    _ => RouteMatch::NotFound,
                }
            }
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
    NotFound,
}
