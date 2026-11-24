use http::Method;
pub(crate) use sqrzl_emulator_core::server::http::Request;

pub(crate) use sqrzl_emulator_core::server::http::{parse_query_params, ResponseBuilder};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::body::Body;
    use hyper::Request as HyperRequest;

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
        Self::route_from_parts(req.method(), req.path(), req.host())
    }

    /// Same routing rules as [`Self::route`], operating on just the method,
    /// path, and `Host` header rather than a fully parsed [`Request`] — so a
    /// route can be determined before a request body has been read at all.
    pub fn route_from_parts(method: &Method, path: &str, host: Option<&str>) -> RouteMatch {
        let path = path.strip_prefix('/').unwrap_or(path);
        let host_bucket = host.and_then(Self::bucket_from_host);

        // Virtual-hosted-style object operations take precedence over path-style parsing.
        if let Some(bucket) = host_bucket {
            return if path.is_empty() {
                Self::bucket_route(method, bucket)
            } else {
                Self::object_route(method, bucket, path)
            };
        }

        if path.is_empty() {
            return if *method == Method::GET {
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
