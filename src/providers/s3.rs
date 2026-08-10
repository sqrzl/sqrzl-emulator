use super::ProviderAdapter;
use crate::auth::AuthConfig;
use crate::body::Body;
use crate::server::{
    handle_s3_request, RequestExt as Request, ResponseBuilder, RouteMatch, Router,
};
use crate::storage::Storage;
use http::{HeaderMap, Method, StatusCode, Uri};
use hyper::Response;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

pub struct S3Adapter;

pub(crate) fn payload_too_large_response(max_request_bytes: usize) -> Response<Body> {
    let message =
        format!("Request body exceeds SQRZL_MAX_REQUEST_BYTES ({max_request_bytes} bytes)");
    let req_id = crate::utils::headers::generate_request_id();
    let host_id = crate::utils::headers::generate_request_id();
    let body =
        crate::utils::xml::error_xml_with_host_id("EntityTooLarge", &message, &req_id, &host_id);
    ResponseBuilder::new(StatusCode::PAYLOAD_TOO_LARGE)
        .content_type("application/xml; charset=utf-8")
        .header("x-amz-request-id", &req_id)
        .header("x-amz-id-2", &host_id)
        .body(body.into_bytes())
        .build()
}

pub(crate) fn incomplete_body_response() -> Response<Body> {
    let req_id = crate::utils::headers::generate_request_id();
    let host_id = crate::utils::headers::generate_request_id();
    let body = crate::utils::xml::error_xml_with_host_id(
        "IncompleteBody",
        "The request body did not contain the declared number of bytes.",
        &req_id,
        &host_id,
    );
    ResponseBuilder::new(StatusCode::BAD_REQUEST)
        .content_type("application/xml; charset=utf-8")
        .header("x-amz-request-id", &req_id)
        .header("x-amz-id-2", &host_id)
        .body(body.into_bytes())
        .build()
}

impl Default for S3Adapter {
    fn default() -> Self {
        Self::new()
    }
}

impl S3Adapter {
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

impl ProviderAdapter for S3Adapter {
    fn name(&self) -> &'static str {
        "s3"
    }

    fn matches(&self, _req: &Request) -> bool {
        true
    }

    fn matches_request_head(&self, _method: &Method, _uri: &Uri, _headers: &HeaderMap) -> bool {
        true
    }

    fn render_payload_too_large(
        &self,
        _method: &Method,
        _uri: &Uri,
        _headers: &HeaderMap,
        max_request_bytes: usize,
    ) -> Response<Body> {
        payload_too_large_response(max_request_bytes)
    }

    fn validate_request_framing(&self, req: &Request) -> Option<Response<Body>> {
        let object_put = matches!(Router::route(req), RouteMatch::ObjectPut(_, _))
            && req.header("x-amz-copy-source").is_none()
            && ![
                "acl",
                "legal-hold",
                "retention",
                "tagging",
                "uploadId",
                "uploads",
                "versionId",
            ]
            .into_iter()
            .any(|name| req.has_query_param(name));
        if object_put && req.body.is_empty() && req.header("content-length").is_none() {
            let req_id = crate::utils::headers::generate_request_id();
            let host_id = crate::utils::headers::generate_request_id();
            let body = crate::utils::xml::error_xml_with_host_id(
                "MissingContentLength",
                "You must provide the Content-Length HTTP header.",
                &req_id,
                &host_id,
            );
            return Some(
                ResponseBuilder::new(StatusCode::LENGTH_REQUIRED)
                    .content_type("application/xml; charset=utf-8")
                    .header("x-amz-request-id", &req_id)
                    .header("x-amz-id-2", &host_id)
                    .body(body.into_bytes())
                    .build(),
            );
        }
        super::content_length_mismatch(req).then(incomplete_body_response)
    }

    fn handle<'a>(
        &'a self,
        storage: Arc<dyn Storage>,
        auth_config: Arc<AuthConfig>,
        req: Request,
    ) -> Pin<Box<dyn Future<Output = Result<Response<Body>, String>> + Send + 'a>> {
        Box::pin(async move { handle_s3_request(storage, auth_config, req).await })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use http_body_util::Full;
    use hyper::Request as HyperRequest;

    async fn request(uri: &str, headers: &[(&str, &str)]) -> Request {
        let mut builder = HyperRequest::builder().method("PUT").uri(uri);
        for (name, value) in headers {
            builder = builder.header(*name, *value);
        }
        Request::from_hyper(
            builder
                .body(Full::new(bytes::Bytes::new()))
                .expect("request should build"),
        )
        .await
        .expect("request should parse")
    }

    #[tokio::test]
    async fn should_require_zero_length_framing_for_path_and_virtual_hosted_put_object() {
        let adapter = S3Adapter::new();
        let path_style = request("http://localhost/bucket/key", &[]).await;
        let virtual_hosted = request(
            "http://localhost/key",
            &[("host", "bucket.s3.amazonaws.com")],
        )
        .await;

        for request in [&path_style, &virtual_hosted] {
            let response = adapter
                .validate_request_framing(request)
                .expect("plain empty PutObject should require framing");
            assert_eq!(response.status(), StatusCode::LENGTH_REQUIRED);
        }
    }

    #[tokio::test]
    async fn should_not_treat_copy_or_object_subresources_as_plain_put_object_framing() {
        let adapter = S3Adapter::new();
        let cases = [
            request(
                "http://localhost/bucket/copied",
                &[("x-amz-copy-source", "/bucket/source")],
            )
            .await,
            request("http://localhost/bucket/key?acl", &[]).await,
            request("http://localhost/bucket/key?tagging", &[]).await,
            request("http://localhost/bucket/key?retention", &[]).await,
            request("http://localhost/bucket/key?legal-hold", &[]).await,
            request(
                "http://localhost/bucket/key?partNumber=1&uploadId=session",
                &[],
            )
            .await,
            request("http://localhost/bucket/key?versionId=version", &[]).await,
            request("http://localhost/bucket", &[]).await,
        ];

        for request in &cases {
            assert!(adapter.validate_request_framing(request).is_none());
        }
    }
}
