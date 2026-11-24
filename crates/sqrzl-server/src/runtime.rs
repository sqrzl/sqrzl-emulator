pub use sqrzl_core::runtime::*;

/// Rejects overload before reading a body using the selected provider's error shape.
#[must_use]
pub fn overloaded(provider: Option<&str>, uri: &http::Uri) -> hyper::Response<crate::body::Body> {
    use crate::server::ResponseBuilder;
    use http::StatusCode;
    let builder = ResponseBuilder::new(StatusCode::SERVICE_UNAVAILABLE)
        .header("retry-after", "1")
        .header("connection", "close");
    match provider.unwrap_or("admin") {
        "azure-blob" => builder.content_type("application/xml").header("x-ms-error-code", "ServerBusy")
            .body_str("<Error><Code>ServerBusy</Code><Message>Emulator capacity exhausted or draining</Message></Error>").build(),
        "gcs" if uri.path().starts_with("/storage/") || uri.path().starts_with("/upload/") => builder.content_type("application/json")
            .body(serde_json::json!({"error":{"code":503,"message":"Emulator capacity exhausted or draining","errors":[{"domain":"global","reason":"backendError","message":"Emulator capacity exhausted or draining"}]}}).to_string().into_bytes()).build(),
        "s3" | "s3-family" | "sns" | "gcs" => builder.content_type("application/xml")
            .body_str("<Error><Code>SlowDown</Code><Message>Emulator capacity exhausted or draining</Message></Error>").build(),
        "ses" | "aws-sms-voice-v2" => builder.content_type("application/json").header("x-amzn-errortype", "ServiceUnavailableException")
            .body_str("{\"__type\":\"ServiceUnavailableException\",\"message\":\"Emulator capacity exhausted or draining\"}").build(),
        "oci-object" => builder.content_type("application/json").body_str("{\"code\":\"ServiceUnavailable\",\"message\":\"Emulator capacity exhausted or draining\"}").build(),
        "sendgrid" => builder.content_type("application/json").body_str("{\"errors\":[{\"message\":\"Emulator capacity exhausted or draining\"}]}").build(),
        "acs" => builder.content_type("application/json").body_str("{\"error\":{\"code\":\"ServiceUnavailable\",\"message\":\"Emulator capacity exhausted or draining\"}}").build(),
        "twilio" => builder.content_type("application/json").body_str("{\"code\":20429,\"message\":\"Emulator capacity exhausted or draining\",\"status\":503}").build(),
        _ => builder.content_type("application/json").body_str("{\"code\":\"ServiceUnavailable\",\"error\":\"Emulator capacity exhausted or draining\"}").build(),
    }
}
