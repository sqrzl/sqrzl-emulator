use crate::body::Body;
use crate::server::{RequestExt as Request, ResponseBuilder};
use http::{Method, StatusCode};
use http_body_util::BodyExt;
use hyper::Response;
use std::time::Duration;

pub(super) const HEADER: &str = "x-sqrzl-failpoint";

pub(super) enum Before {
    Continue,
    Respond(Response<Body>),
}

pub(super) async fn before(req: &Request, provider: &str) -> Before {
    let Some(name) = req.header(HEADER) else {
        return Before::Continue;
    };
    if name == "timeout-before-commit" {
        tokio::time::sleep(delay(req)).await;
        return Before::Continue;
    }
    if let Some(status) = redirect_status(name) {
        let location = req
            .header("x-sqrzl-redirect-location")
            .unwrap_or("http://127.0.0.1:1/sqrzl-redirect-target");
        return Before::Respond(
            ResponseBuilder::new(status)
                .header("location", location)
                .header("x-sqrzl-failpoint-applied", name)
                .empty(),
        );
    }
    if name == "conditional-request-conflict" && provider == "s3" && is_s3_conditional_write(req) {
        let request_id = uuid::Uuid::new_v4().to_string();
        let host_id = uuid::Uuid::new_v4().to_string();
        return Before::Respond(
            ResponseBuilder::new(StatusCode::CONFLICT)
                .content_type("application/xml")
                .header("x-amz-request-id", &request_id)
                .header("x-amz-id-2", &host_id)
                .header("x-sqrzl-failpoint-applied", name)
                .body_str(&format!(
                    "<?xml version=\"1.0\" encoding=\"UTF-8\"?><Error><Code>ConditionalRequestConflict</Code><Message>A conflicting conditional operation is currently in progress against this resource. Please try again.</Message><RequestId>{request_id}</RequestId><HostId>{host_id}</HostId></Error>"
                ))
                .build(),
        );
    }
    let status = match name {
        "throttle" if provider == "s3" || provider == "azure-blob" => {
            Some(StatusCode::SERVICE_UNAVAILABLE)
        }
        "throttle" => Some(StatusCode::TOO_MANY_REQUESTS),
        "transient-500" => Some(StatusCode::INTERNAL_SERVER_ERROR),
        "transient-502" => Some(StatusCode::BAD_GATEWAY),
        "transient-503" => Some(StatusCode::SERVICE_UNAVAILABLE),
        "transient-504" => Some(StatusCode::GATEWAY_TIMEOUT),
        _ => None,
    };
    status.map_or(Before::Continue, |status| {
        Before::Respond(provider_error(provider, status, name, req))
    })
}

pub(super) async fn after(
    req: &Request,
    response: Response<Body>,
) -> Result<Response<Body>, String> {
    let Some(name) = req.header(HEADER) else {
        return Ok(response);
    };
    let committed_mutation = is_mutation(req.method()) && response.status().is_success();
    if name == "timeout-after-commit" && committed_mutation {
        tokio::time::sleep(delay(req)).await;
        return Ok(response);
    }
    if name == "response-loss-after-commit" && committed_mutation {
        let (mut parts, _) = response.into_parts();
        parts
            .headers
            .insert("connection", http::HeaderValue::from_static("close"));
        parts
            .headers
            .insert("content-length", http::HeaderValue::from_static("1"));
        parts.headers.insert(
            "x-sqrzl-failpoint-applied",
            http::HeaderValue::from_static("response-loss-after-commit"),
        );
        return Ok(Response::from_parts(parts, Body::abort(1)));
    }
    if name == "truncate-response" {
        return truncate(response, name).await;
    }
    if matches!(
        name,
        "repeated-pagination-token" | "malformed-pagination-token"
    ) {
        return rewrite_pagination(response, req, name).await;
    }
    Ok(response)
}

fn redirect_status(name: &str) -> Option<StatusCode> {
    match name {
        "redirect-301" => Some(StatusCode::MOVED_PERMANENTLY),
        "redirect-302" => Some(StatusCode::FOUND),
        "redirect-303" => Some(StatusCode::SEE_OTHER),
        "redirect-307" => Some(StatusCode::TEMPORARY_REDIRECT),
        "redirect-308" => Some(StatusCode::PERMANENT_REDIRECT),
        _ => None,
    }
}

fn delay(req: &Request) -> Duration {
    let millis = req
        .header("x-sqrzl-failpoint-delay-ms")
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(1_000);
    Duration::from_millis(millis)
}

fn is_mutation(method: &Method) -> bool {
    matches!(
        *method,
        Method::PUT | Method::POST | Method::PATCH | Method::DELETE
    )
}

fn is_s3_conditional_write(req: &Request) -> bool {
    let conditional = req.header("if-match").is_some() || req.header("if-none-match").is_some();
    if !conditional {
        return false;
    }
    match *req.method() {
        Method::PUT => {
            !req.has_query_param("acl")
                && !req.has_query_param("tagging")
                && !req.has_query_param("partNumber")
                && req.header("x-amz-copy-source").is_none()
        }
        Method::POST => req.has_query_param("uploadId"),
        _ => false,
    }
}

#[allow(clippy::too_many_lines)] // Keep the provider status/body/header variants auditable in one dispatch table.
fn provider_error(provider: &str, status: StatusCode, name: &str, req: &Request) -> Response<Body> {
    let code = status.as_u16();
    let builder = ResponseBuilder::new(status)
        .header("retry-after", "1")
        .header("x-sqrzl-failpoint-applied", name);
    match provider {
        "azure-blob" => {
            let request_id = uuid::Uuid::new_v4().to_string();
            let error_code = match status {
                StatusCode::SERVICE_UNAVAILABLE => "ServerBusy",
                StatusCode::GATEWAY_TIMEOUT => "OperationTimedOut",
                _ => "InternalError",
            };
            let mut response = builder
                .content_type("application/xml")
                .header("x-ms-version", "2023-11-03")
                .header("x-ms-request-id", &request_id)
                .header("date", &crate::utils::headers::format_last_modified())
                .header("x-ms-error-code", error_code)
                .body_str(&format!("<?xml version=\"1.0\" encoding=\"utf-8\"?><Error><Code>{error_code}</Code><Message>Sqrzl deterministic failpoint\nRequestId:{request_id}</Message></Error>"))
                .build();
            echo_header(req, &mut response, "x-ms-client-request-id");
            response
        }
        "gcs"
            if req.path().starts_with("/storage/v1/")
                || req.path().starts_with("/upload/storage/v1/")
                || req.path().starts_with("/upload/resumable/")
                || req.path().starts_with("/download/storage/v1/") =>
        {
            // GCS documents 502 and 504 as non-JSON responses.
            if matches!(
                status,
                StatusCode::BAD_GATEWAY | StatusCode::GATEWAY_TIMEOUT
            ) {
                return builder.empty();
            }
            let (domain, reason) = match status {
                StatusCode::TOO_MANY_REQUESTS => ("usageLimits", "rateLimitExceeded"),
                StatusCode::SERVICE_UNAVAILABLE => ("global", "backendError"),
                _ => ("global", "internalError"),
            };
            builder
                .content_type("application/json")
                .body(
                    serde_json::json!({
                        "error": {
                            "errors": [{
                                "domain": domain,
                                "reason": reason,
                                "message": "Sqrzl deterministic failpoint"
                            }],
                            "code": code,
                            "message": "Sqrzl deterministic failpoint"
                        }
                    })
                    .to_string()
                    .into_bytes(),
                )
                .build()
        }
        "gcs" => {
            let error_code = if status == StatusCode::TOO_MANY_REQUESTS {
                "TooManyRequests"
            } else {
                "InternalError"
            };
            builder
                .content_type("application/xml")
                .body_str(&format!("<?xml version=\"1.0\" encoding=\"UTF-8\"?><Error><Code>{error_code}</Code><Message>Sqrzl deterministic failpoint</Message></Error>"))
                .build()
        }
        "oci-object" => {
            let request_id = uuid::Uuid::new_v4().to_string();
            let error_code = match status {
                StatusCode::TOO_MANY_REQUESTS => "TooManyRequests",
                StatusCode::BAD_GATEWAY => "BadGateway",
                StatusCode::SERVICE_UNAVAILABLE => "ServiceUnavailable",
                StatusCode::GATEWAY_TIMEOUT => "GatewayTimeout",
                _ => "InternalServerError",
            };
            let mut response = builder
                .content_type("application/json")
                .header("opc-request-id", &request_id)
                .header("date", &crate::utils::headers::format_last_modified())
                .body(serde_json::json!({"code":error_code,"message":"Sqrzl deterministic failpoint"}).to_string().into_bytes())
                .build();
            echo_header(req, &mut response, "opc-client-request-id");
            response
        }
        _ => {
            let request_id = uuid::Uuid::new_v4().to_string();
            let host_id = uuid::Uuid::new_v4().to_string();
            let error_code = if name == "throttle" {
                "SlowDown"
            } else if status == StatusCode::GATEWAY_TIMEOUT {
                "RequestTimeout"
            } else if status == StatusCode::SERVICE_UNAVAILABLE {
                "ServiceUnavailable"
            } else {
                "InternalError"
            };
            builder
                .content_type("application/xml")
                .header("x-amz-request-id", &request_id)
                .header("x-amz-id-2", &host_id)
                .body_str(&format!("<?xml version=\"1.0\" encoding=\"UTF-8\"?><Error><Code>{error_code}</Code><Message>Sqrzl deterministic failpoint</Message><RequestId>{request_id}</RequestId><HostId>{host_id}</HostId></Error>"))
                .build()
        }
    }
}

fn echo_header(req: &Request, response: &mut Response<Body>, name: &'static str) {
    let Some(value) = req.headers.get(name).cloned() else {
        return;
    };
    response.headers_mut().insert(name, value);
}

async fn truncate(response: Response<Body>, name: &str) -> Result<Response<Body>, String> {
    let (mut parts, body) = response.into_parts();
    let bytes = body
        .collect()
        .await
        .map_err(|err| err.to_string())?
        .to_bytes();
    let declared = bytes.len();
    let kept = declared / 2;
    parts.headers.insert(
        "content-length",
        http::HeaderValue::from_str(&declared.to_string()).map_err(|err| err.to_string())?,
    );
    parts
        .headers
        .insert("connection", http::HeaderValue::from_static("close"));
    parts.headers.insert(
        "x-sqrzl-failpoint-applied",
        http::HeaderValue::from_str(name).map_err(|err| err.to_string())?,
    );
    Ok(Response::from_parts(
        parts,
        Body::truncated(bytes.slice(..kept), declared as u64),
    ))
}

async fn rewrite_pagination(
    response: Response<Body>,
    req: &Request,
    name: &str,
) -> Result<Response<Body>, String> {
    let (mut parts, body) = response.into_parts();
    let bytes = body
        .collect()
        .await
        .map_err(|err| err.to_string())?
        .to_bytes();
    let token = if name == "malformed-pagination-token" {
        "%%%not-a-valid-token%%%"
    } else {
        req.query_param("pageToken")
            .or_else(|| req.query_param("marker"))
            .or_else(|| req.query_param("continuation-token"))
            .or_else(|| req.query_param("key-marker"))
            .or_else(|| req.query_param("start"))
            .or_else(|| req.query_param("startAfter"))
            .unwrap_or("sqrzl-repeated-token")
    };
    let mut text = String::from_utf8_lossy(&bytes).into_owned();
    if let Ok(mut json) = serde_json::from_str::<serde_json::Value>(&text) {
        if let Some(object) = json.as_object_mut() {
            let key = if req.path().starts_with("/n/") {
                "nextStartWith"
            } else {
                "nextPageToken"
            };
            object.insert(
                key.to_string(),
                serde_json::Value::String(token.to_string()),
            );
            text = json.to_string();
        }
    } else if text.starts_with('<') {
        let tag = if req.has_query_param("list-type") {
            "NextContinuationToken"
        } else if req.has_query_param("versions") {
            "NextKeyMarker"
        } else {
            "NextMarker"
        };
        let mut escaped_token = String::with_capacity(token.len());
        crate::utils::xml::push_escaped_xml(&mut escaped_token, token);
        let open = format!("<{tag}>");
        let close = format!("</{tag}>");
        if let Some(start) = text.find(&open) {
            if let Some(relative_end) = text[start..].find(&close) {
                let value_start = start + open.len();
                text.replace_range(value_start..start + relative_end, &escaped_token);
            }
        } else if let Some(root_end) = text.rfind("</") {
            text.insert_str(root_end, &format!("{open}{escaped_token}{close}"));
        }
        if let Some(start) = text.find("<IsTruncated>") {
            if let Some(relative_end) = text[start..].find("</IsTruncated>") {
                let value_start = start + "<IsTruncated>".len();
                text.replace_range(value_start..start + relative_end, "true");
            }
        }
    }
    parts.headers.insert(
        "content-length",
        http::HeaderValue::from_str(&text.len().to_string()).map_err(|err| err.to_string())?,
    );
    parts.headers.insert(
        "x-sqrzl-failpoint-applied",
        http::HeaderValue::from_str(name).map_err(|err| err.to_string())?,
    );
    Ok(Response::from_parts(parts, Body::from(text)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use http_body_util::BodyExt;

    async fn request(uri: &str, failpoint: &str) -> Request {
        Request::from_hyper(
            hyper::Request::builder()
                .uri(uri)
                .header(HEADER, failpoint)
                .body(Body::default())
                .expect("request should build"),
        )
        .await
        .expect("request should parse")
    }

    async fn body(response: Response<Body>) -> String {
        String::from_utf8(
            response
                .into_body()
                .collect()
                .await
                .expect("response body should collect")
                .to_bytes()
                .to_vec(),
        )
        .expect("response body should be utf8")
    }

    fn assert_well_formed_xml(xml: &str) {
        let mut reader = quick_xml::Reader::from_str(xml);
        loop {
            match reader.read_event() {
                Ok(quick_xml::events::Event::Eof) => break,
                Ok(_) => {}
                Err(error) => panic!("rewritten response should be well-formed XML: {error}"),
            }
        }
    }

    #[test]
    fn should_map_every_supported_redirect_failpoint() {
        // Arrange
        let cases = [
            ("redirect-301", StatusCode::MOVED_PERMANENTLY),
            ("redirect-302", StatusCode::FOUND),
            ("redirect-303", StatusCode::SEE_OTHER),
            ("redirect-307", StatusCode::TEMPORARY_REDIRECT),
            ("redirect-308", StatusCode::PERMANENT_REDIRECT),
        ];

        // Act
        // Assert
        for (name, status) in cases {
            assert_eq!(redirect_status(name), Some(status));
        }
    }

    #[tokio::test]
    async fn should_limit_conditional_conflict_to_conditional_put_and_completion_requests() {
        // Arrange
        let conditional_put = Request::from_hyper(
            hyper::Request::builder()
                .method(Method::PUT)
                .uri("http://localhost/bucket/object")
                .header(HEADER, "conditional-request-conflict")
                .header("if-none-match", "*")
                .body(Body::default())
                .expect("request should build"),
        )
        .await
        .expect("request should parse");
        let unconditional_delete = Request::from_hyper(
            hyper::Request::builder()
                .method(Method::DELETE)
                .uri("http://localhost/bucket/object")
                .header(HEADER, "conditional-request-conflict")
                .body(Body::default())
                .expect("request should build"),
        )
        .await
        .expect("request should parse");

        // Act
        let put_result = before(&conditional_put, "s3").await;
        let delete_result = before(&unconditional_delete, "s3").await;

        // Assert
        assert!(matches!(put_result, Before::Respond(_)));
        assert!(matches!(delete_result, Before::Continue));
    }

    #[tokio::test]
    async fn should_rewrite_provider_specific_pagination_tokens() {
        // Arrange
        let cases = [
            (
                "http://localhost/storage/v1/b/bucket/o?pageToken=old",
                r#"{"items":[],"nextPageToken":"new"}"#,
                "nextPageToken",
            ),
            (
                "http://localhost/n/ns/b/bucket/o?start=old",
                r#"{"objects":[],"nextStartWith":"new"}"#,
                "nextStartWith",
            ),
        ];

        // Act
        // Assert
        for (uri, original, field) in cases {
            let req = request(uri, "repeated-pagination-token").await;
            let response = ResponseBuilder::new(StatusCode::OK)
                .content_type("application/json")
                .body_str(original)
                .build();
            let rewritten = after(&req, response)
                .await
                .expect("failpoint should rewrite response");
            let json: serde_json::Value = serde_json::from_str(&body(rewritten).await)
                .expect("rewritten response should be json");
            assert_eq!(json[field], "old");
        }
    }

    #[tokio::test]
    async fn should_rewrite_s3_v2_continuation_token_and_force_more_pages() {
        // Arrange
        let req = request(
            "http://localhost/bucket?list-type=2&continuation-token=opaque",
            "repeated-pagination-token",
        )
        .await;
        let response = ResponseBuilder::new(StatusCode::OK)
            .content_type("application/xml")
            .body_str("<ListBucketResult><IsTruncated>false</IsTruncated></ListBucketResult>")
            .build();

        // Act
        let rewritten = after(&req, response)
            .await
            .expect("failpoint should rewrite response");

        // Assert
        let rewritten_body = body(rewritten).await;
        assert!(rewritten_body.contains("<NextContinuationToken>opaque</NextContinuationToken>"));
        assert!(rewritten_body.contains("<IsTruncated>true</IsTruncated>"));
    }

    #[tokio::test]
    async fn should_keep_injected_xml_pagination_tokens_well_formed() {
        // Arrange
        let cases = [
            (
                "http://localhost/bucket?marker=unsafe%3C%26%3E%22%27",
                "repeated-pagination-token",
                "unsafe&lt;&amp;&gt;&quot;&apos;",
            ),
            (
                "http://localhost/bucket",
                "malformed-pagination-token",
                "%%%not-a-valid-token%%%",
            ),
        ];

        // Act
        // Assert
        for (uri, failpoint, expected_token) in cases {
            let req = request(uri, failpoint).await;
            let response = ResponseBuilder::new(StatusCode::OK)
                .content_type("application/xml")
                .body_str(
                    "<ListBucketResult><IsTruncated>false</IsTruncated><NextMarker>old</NextMarker></ListBucketResult>",
                )
                .build();
            let rewritten = after(&req, response)
                .await
                .expect("failpoint should rewrite response");
            let rewritten_body = body(rewritten).await;
            assert!(rewritten_body.contains(&format!("<NextMarker>{expected_token}</NextMarker>")));
            assert!(rewritten_body.contains("<IsTruncated>true</IsTruncated>"));
            assert_well_formed_xml(&rewritten_body);
        }
    }

    #[tokio::test]
    async fn should_include_provider_correlation_headers_in_fault_responses() {
        // Arrange
        let azure = Request::from_hyper(
            hyper::Request::builder()
                .uri("http://localhost/account/container/blob")
                .header("x-ms-client-request-id", "azure-client-id")
                .body(Body::default())
                .expect("Azure request should build"),
        )
        .await
        .expect("Azure request should parse");
        let oci = Request::from_hyper(
            hyper::Request::builder()
                .uri("http://localhost/n/ns/b/bucket/o/object")
                .header("opc-client-request-id", "oci-client-id")
                .body(Body::default())
                .expect("OCI request should build"),
        )
        .await
        .expect("OCI request should parse");
        let s3 = request("http://localhost/bucket/key", "transient-503").await;

        // Act
        let azure_response = provider_error(
            "azure-blob",
            StatusCode::SERVICE_UNAVAILABLE,
            "transient-503",
            &azure,
        );
        let oci_response = provider_error(
            "oci-object",
            StatusCode::SERVICE_UNAVAILABLE,
            "transient-503",
            &oci,
        );
        let s3_response =
            provider_error("s3", StatusCode::SERVICE_UNAVAILABLE, "transient-503", &s3);

        // Assert
        assert!(azure_response.headers().contains_key("x-ms-request-id"));
        assert_eq!(azure_response.headers()["x-ms-version"], "2023-11-03");
        assert_eq!(
            azure_response.headers()["x-ms-client-request-id"],
            "azure-client-id"
        );
        assert!(oci_response.headers().contains_key("opc-request-id"));
        assert_eq!(
            oci_response.headers()["opc-client-request-id"],
            "oci-client-id"
        );
        let s3_request_id = s3_response.headers()["x-amz-request-id"]
            .to_str()
            .expect("S3 request ID should be ASCII")
            .to_string();
        let s3_host_id = s3_response.headers()["x-amz-id-2"]
            .to_str()
            .expect("S3 host ID should be ASCII")
            .to_string();
        let s3_body = body(s3_response).await;
        assert!(s3_body.contains(&format!("<RequestId>{s3_request_id}</RequestId>")));
        assert!(s3_body.contains(&format!("<HostId>{s3_host_id}</HostId>")));
    }
}
