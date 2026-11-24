use crate::body::Body;
use crate::server::ResponseBuilder;
use crate::utils::headers::generate_request_id;
use http::{Response, StatusCode};

/// XML declaration used in provider responses.
#[must_use]
pub fn xml_declaration() -> String {
    r#"<?xml version="1.0" encoding="UTF-8"?>"#.to_string()
}

/// Error response
#[must_use]
pub fn error_xml(code: &str, message: &str, request_id: &str) -> String {
    error_xml_with_host_id(code, message, request_id, "")
}

/// S3 REST error response with both correlation identifiers.
#[must_use]
pub fn error_xml_with_host_id(
    code: &str,
    message: &str,
    request_id: &str,
    host_id: &str,
) -> String {
    let host_id = if host_id.is_empty() {
        String::new()
    } else {
        format!("\n  <HostId>{}</HostId>", escape_xml(host_id))
    };
    format!(
        r"{}
<Error>
  <Code>{}</Code>
  <Message>{}</Message>
  <RequestId>{}</RequestId>{}
</Error>",
        xml_declaration(),
        escape_xml(code),
        escape_xml(message),
        escape_xml(request_id),
        host_id
    )
}

pub fn push_escaped_xml(out: &mut String, s: &str) {
    let mut start = 0;
    let bytes = s.as_bytes();

    for (index, byte) in bytes.iter().enumerate() {
        let replacement = match byte {
            b'&' => Some("&amp;"),
            b'<' => Some("&lt;"),
            b'>' => Some("&gt;"),
            b'"' => Some("&quot;"),
            b'\'' => Some("&apos;"),
            _ => None,
        };

        if let Some(replacement) = replacement {
            if start < index {
                out.push_str(&s[start..index]);
            }
            out.push_str(replacement);
            start = index + 1;
        }
    }

    if start < s.len() {
        out.push_str(&s[start..]);
    }
}

/// Helper to escape XML special characters
#[must_use]
pub fn escape_xml(s: &str) -> String {
    let mut escaped = String::with_capacity(s.len());
    push_escaped_xml(&mut escaped, s);
    escaped
}

#[must_use]
pub fn payload_too_large_response(max_request_bytes: usize) -> Response<Body> {
    let message =
        format!("Request body exceeds SQRZL_MAX_REQUEST_BYTES ({max_request_bytes} bytes)");
    let req_id = generate_request_id();
    let host_id = generate_request_id();
    let body = error_xml_with_host_id("EntityTooLarge", &message, &req_id, &host_id);
    ResponseBuilder::new(StatusCode::PAYLOAD_TOO_LARGE)
        .content_type("application/xml; charset=utf-8")
        .header("x-amz-request-id", &req_id)
        .header("x-amz-id-2", &host_id)
        .body(body.into_bytes())
        .build()
}
