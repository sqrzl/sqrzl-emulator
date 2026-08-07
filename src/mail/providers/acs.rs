use crate::auth::AuthConfig;
use crate::auth::{acs_hmac, parse_connection_string};
use crate::body::Body;
use crate::mail::model::{Address, Attachment, Message, SourceProtocol};
use crate::mail::providers::MailAdapter;
use crate::mail::{fan_out, MailStore};
use crate::server::{RequestExt as MailRequest, ResponseBuilder};
use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use http::{HeaderMap, Method, StatusCode, Uri};
use hyper::Response;
use serde_json::Value;
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

const ENV_ACS_CONNECTION_STRING: &str = "SQRZL_ACS_CONNECTION_STRING";

pub struct AcsEmailAdapter;

impl AcsEmailAdapter {
    fn invalid_request_response(message: &str) -> Response<Body> {
        ResponseBuilder::new(StatusCode::BAD_REQUEST)
            .content_type("application/json; charset=utf-8")
            .body(
                serde_json::json!({
                    "error": {
                        "code": "InvalidRequest",
                        "message": message,
                    }
                })
                .to_string()
                .into_bytes(),
            )
            .build()
    }

    fn load_connection_string() -> Option<String> {
        std::env::var(ENV_ACS_CONNECTION_STRING)
            .ok()
            .and_then(|value| parse_connection_string(&value))
            .map(|parsed| parsed.access_key)
    }

    fn is_authorized(req: &MailRequest) -> bool {
        let Some(access_key) = Self::load_connection_string() else {
            return true;
        };

        let Some(auth_value) = req.header("authorization") else {
            return false;
        };
        let Some((scheme, parameters)) = auth_value.split_once(' ') else {
            return false;
        };
        if !scheme.eq_ignore_ascii_case("HMAC-SHA256") {
            return false;
        }

        let mut signed_headers = None;
        let mut signature = None;
        for parameter in parameters.split('&') {
            let Some((name, value)) = parameter.split_once('=') else {
                return false;
            };
            if name.eq_ignore_ascii_case("SignedHeaders") {
                signed_headers = Some(value);
            } else if name.eq_ignore_ascii_case("Signature") {
                signature = Some(value);
            }
        }

        let Some(signed_headers) = signed_headers else {
            return false;
        };
        let signed_headers = signed_headers.split(';').collect::<Vec<_>>();
        if signed_headers.len() != 3
            || !signed_headers
                .iter()
                .any(|name| name.eq_ignore_ascii_case("host"))
            || !signed_headers
                .iter()
                .any(|name| name.eq_ignore_ascii_case("x-ms-content-sha256"))
        {
            return false;
        }

        let date_header = if signed_headers
            .iter()
            .any(|name| name.eq_ignore_ascii_case("x-ms-date"))
        {
            "x-ms-date"
        } else if signed_headers
            .iter()
            .any(|name| name.eq_ignore_ascii_case("date"))
        {
            "date"
        } else {
            return false;
        };

        let (Some(date), Some(host), Some(content_hash), Some(signature)) = (
            req.header(date_header),
            req.header("host"),
            req.header("x-ms-content-sha256"),
            signature,
        ) else {
            return false;
        };
        if content_hash != acs_hmac::content_hash(&req.body) {
            return false;
        }

        let path_and_query = req
            .uri
            .path_and_query()
            .map_or(req.path(), http::uri::PathAndQuery::as_str);
        let string_to_sign = format!(
            "{}\n{}\n{};{};{}",
            req.method().as_str(),
            path_and_query,
            date,
            host,
            content_hash
        );
        acs_hmac::validate_signature(&access_key, &string_to_sign, signature)
    }

    fn parse_message(req: &MailRequest) -> Result<Message, String> {
        let payload = serde_json::from_slice::<Value>(&req.body)
            .map_err(|err| format!("invalid ACS request body: {err}"))?;

        let from = payload
            .get("senderAddress")
            .or_else(|| payload.get("from"))
            .and_then(parse_address_value)
            .unwrap_or_else(|| Address::new("unknown@localhost"));

        let recipients = payload
            .get("recipients")
            .and_then(Value::as_object)
            .map_or_else(
                || {
                    (
                        parse_addresses(payload.get("to").and_then(Value::as_array)),
                        parse_addresses(payload.get("cc").and_then(Value::as_array)),
                        parse_addresses(payload.get("bcc").and_then(Value::as_array)),
                    )
                },
                parse_recipients,
            );

        let subject = payload
            .get("subject")
            .or_else(|| {
                payload
                    .get("content")
                    .and_then(|content| content.get("subject"))
            })
            .and_then(Value::as_str)
            .filter(|subject| !subject.is_empty())
            .map(ToString::to_string)
            .unwrap_or_default();

        let content = payload.get("content").unwrap_or(&Value::Null);
        let body_text = content
            .get("plainText")
            .or_else(|| content.get("text"))
            .and_then(Value::as_str)
            .map(ToString::to_string);
        let mut body_html = content
            .get("html")
            .and_then(Value::as_str)
            .map(ToString::to_string);
        if body_html.is_none() {
            body_html.clone_from(&body_text);
        }

        let mut headers = HashMap::new();
        if !subject.is_empty() {
            headers.insert("subject".to_string(), subject.clone());
        }

        let attachments = payload
            .get("attachments")
            .and_then(Value::as_array)
            .map_or_else(Vec::new, |attachments| {
                parse_attachments(attachments.as_slice())
            });

        Ok(Message {
            source_protocol: SourceProtocol::Acs,
            from,
            to: recipients.0,
            cc: recipients.1,
            bcc: recipients.2,
            subject,
            headers,
            body_text,
            body_html,
            attachments,
            raw_mime: None,
            thread_id: None,
        })
    }
}

impl MailAdapter for AcsEmailAdapter {
    fn name(&self) -> &'static str {
        "acs"
    }

    fn matches(&self, req: &MailRequest) -> bool {
        (req.method() == Method::POST
            && req.path() == "/emails:send"
            && req.has_query_param("api-version"))
            || (req.method() == Method::GET
                && req.path().starts_with("/emails/operations/")
                && req.has_query_param("api-version"))
    }

    fn matches_request_head(&self, method: &Method, uri: &Uri, headers: &HeaderMap) -> bool {
        method == Method::POST && uri.path() == "/emails:send" && !headers.is_empty()
    }

    fn handle<'a>(
        &'a self,
        mail: Arc<dyn MailStore>,
        _auth_config: Arc<AuthConfig>,
        req: MailRequest,
    ) -> Pin<Box<dyn Future<Output = Result<Response<Body>, String>> + Send + 'a>> {
        Box::pin(async move {
            if !Self::is_authorized(&req) {
                return Ok(ResponseBuilder::new(StatusCode::UNAUTHORIZED)
                    .content_type("application/json; charset=utf-8")
                    .body(
                        serde_json::json!({"errors":[{"message":"Unauthorized","field":"authorization","help":"Set Authorization: HMAC-SHA256 SignedHeaders=x-ms-date;host;x-ms-content-sha256&Signature=<base64>"}]})
                            .to_string()
                            .into_bytes(),
                    )
                    .build());
            }

            if req.method() == Method::GET {
                let operation_id = req
                    .path()
                    .strip_prefix("/emails/operations/")
                    .filter(|value| !value.is_empty())
                    .ok_or_else(|| "invalid ACS operation path".to_string())?;
                return Ok(ResponseBuilder::new(StatusCode::OK)
                    .content_type("application/json; charset=utf-8")
                    .body(
                        serde_json::json!({
                            "id": operation_id,
                            "status": "Succeeded"
                        })
                        .to_string()
                        .into_bytes(),
                    )
                    .build());
            }

            let message = match Self::parse_message(&req) {
                Ok(message) => message,
                Err(message) => return Ok(Self::invalid_request_response(&message)),
            };
            let stored_messages = match fan_out(mail.as_ref(), &message) {
                Ok(stored_messages) => stored_messages,
                Err(crate::error::Error::InvalidRequest(message)) => {
                    return Ok(Self::invalid_request_response(&message));
                }
                Err(err) => return Err(err.to_string()),
            };
            let message_id = stored_messages
                .first()
                .map_or_else(crate::mail::generate_message_id, |stored| {
                    stored.message_id.clone()
                });

            let scheme = req
                .uri
                .scheme_str()
                .or_else(|| req.header("x-forwarded-proto"))
                .unwrap_or("http");
            let authority = req
                .uri
                .authority()
                .map(http::uri::Authority::as_str)
                .or_else(|| req.header("host"))
                .unwrap_or("localhost");
            let api_version = req.query_param("api-version").unwrap_or("2023-03-31");
            let operation_location = format!(
                "{scheme}://{authority}/emails/operations/{message_id}?api-version={api_version}"
            );

            Ok(ResponseBuilder::new(StatusCode::ACCEPTED)
                .header("operation-location", &operation_location)
                .header("retry-after", "0")
                .content_type("application/json; charset=utf-8")
                .body(
                    serde_json::json!({
                        "id": message_id,
                        "status": "Running"
                    })
                    .to_string()
                    .into_bytes(),
                )
                .build())
        })
    }
}

fn parse_address_value(value: &Value) -> Option<Address> {
    match value {
        Value::String(address) => Some(Address::new(address)),
        Value::Object(object) => {
            let email = object
                .get("email")
                .or_else(|| object.get("address"))
                .and_then(Value::as_str)?;
            Some(Address {
                email: email.to_string(),
                name: object
                    .get("name")
                    .or_else(|| object.get("displayName"))
                    .and_then(Value::as_str)
                    .map(ToString::to_string),
            })
        }
        _ => None,
    }
}

fn parse_recipients(
    recipients: &serde_json::Map<String, Value>,
) -> (Vec<Address>, Vec<Address>, Vec<Address>) {
    let to = parse_addresses(recipients.get("to").and_then(Value::as_array));
    let cc = parse_addresses(recipients.get("cc").and_then(Value::as_array));
    let bcc = parse_addresses(recipients.get("bcc").and_then(Value::as_array));
    (to, cc, bcc)
}

fn parse_addresses(values: Option<&Vec<Value>>) -> Vec<Address> {
    values
        .unwrap_or(&Vec::new())
        .iter()
        .filter_map(parse_address_value)
        .collect()
}

fn parse_attachments(values: &[Value]) -> Vec<Attachment> {
    values
        .iter()
        .filter_map(|value| {
            let filename = value.get("name")?.as_str()?.to_string();
            let content_type = value
                .get("contentType")
                .and_then(Value::as_str)
                .unwrap_or("application/octet-stream")
                .to_string();
            let content = value
                .get("contentInBase64")
                .or_else(|| value.get("content"))
                .and_then(Value::as_str)
                .and_then(|encoded| BASE64.decode(encoded).ok())
                .unwrap_or_default();

            Some(Attachment {
                filename,
                content_type,
                content,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::acs_hmac;
    use crate::mail::filesystem::FilesystemMailStore;
    use crate::mail::model::ListMessagesParams;
    use crate::mail::providers::MailAdapter;
    use crate::Config;
    use std::sync::Arc;

    fn temp_mail() -> Arc<dyn MailStore> {
        let dir = std::env::temp_dir().join(format!("sqrzl-mail-acs-{}", uuid::Uuid::new_v4()));
        Arc::new(FilesystemMailStore::open(dir).expect("mail store should open"))
    }

    async fn request_with_signature(
        method: &str,
        uri: &str,
        body: &str,
        access_key: &str,
    ) -> MailRequest {
        let date = "Thu, 07 Aug 2026 12:00:00 GMT";
        let host = "localhost";
        let content_hash = acs_hmac::content_hash(body.as_bytes());
        let parsed_uri: Uri = uri.parse().expect("ACS test URI should parse");
        let path_and_query = parsed_uri
            .path_and_query()
            .expect("ACS test URI should have a path")
            .as_str();
        let string_to_sign = format!("{method}\n{path_and_query}\n{date};{host};{content_hash}");
        let signature = acs_hmac::sign_request(access_key, &string_to_sign)
            .expect("ACS test access key should be valid base64");
        std::env::set_var(
            ENV_ACS_CONNECTION_STRING,
            format!("endpoint=http://localhost;accesskey={access_key}"),
        );
        crate::server::RequestExt::from_hyper(
            hyper::Request::builder()
                .method(method)
                .uri(uri)
                .header("host", host)
                .header("x-ms-date", date)
                .header("x-ms-content-sha256", content_hash)
                .header(
                    "authorization",
                    format!(
                        "HMAC-SHA256 SignedHeaders=x-ms-date;host;x-ms-content-sha256&Signature={signature}"
                    ),
                )
                .body(Body::from(body.to_string()))
                .expect("request should build"),
        )
        .await
        .expect("request should parse")
    }

    #[tokio::test]
    async fn should_parse_valid_acs_request_and_store_messages() {
        let mail = temp_mail();
        let body = r#"{"senderAddress":"alice@example.com","recipients":{"to":["bob@example.com"]},"content":{"text":"hello","html":"<p>hello</p>"},"subject":"acs test"}"#;
        let access_key = BASE64.encode("shared-secret");
        let req = request_with_signature(
            "POST",
            "http://localhost/emails:send?api-version=2023-03-31",
            body,
            &access_key,
        )
        .await;
        let response = AcsEmailAdapter
            .handle(
                mail.clone(),
                Arc::new(Config {
                    access_key_id: None,
                    secret_access_key: None,
                    enforce_auth: false,
                    admin_auth_disabled: false,
                    blobs_path: "./blobs".into(),
                    lifecycle_interval: std::time::Duration::from_hours(1),
                    api_port: 9000,
                    ui_port: 9001,
                    max_request_bytes: crate::config::DEFAULT_SQRZL_MAX_REQUEST_BYTES,
                    smtp_port: crate::config::DEFAULT_SQRZL_SMTP_PORT,
                }),
                req,
            )
            .await
            .expect("acs adapter should handle request");
        assert_eq!(response.status(), StatusCode::ACCEPTED);
        let operation_location = response
            .headers()
            .get("operation-location")
            .and_then(|value| value.to_str().ok())
            .expect("ACS send should return an operation location")
            .to_string();
        let poll_request =
            request_with_signature("GET", &operation_location, "", &access_key).await;
        let poll_response = AcsEmailAdapter
            .handle(
                mail.clone(),
                Arc::new(Config {
                    access_key_id: None,
                    secret_access_key: None,
                    enforce_auth: false,
                    admin_auth_disabled: false,
                    blobs_path: "./blobs".into(),
                    lifecycle_interval: std::time::Duration::from_hours(1),
                    api_port: 9000,
                    ui_port: 9001,
                    max_request_bytes: crate::config::DEFAULT_SQRZL_MAX_REQUEST_BYTES,
                    smtp_port: crate::config::DEFAULT_SQRZL_SMTP_PORT,
                }),
                poll_request,
            )
            .await
            .expect("ACS adapter should handle poll request");
        assert_eq!(poll_response.status(), StatusCode::OK);
        let messages = mail
            .list_messages("bob@example.com", ListMessagesParams::default())
            .expect("list should succeed");
        assert_eq!(messages.messages.len(), 1);
        assert_eq!(messages.messages[0].message.subject, "acs test");
        assert!(messages.messages[0]
            .message
            .body_text
            .as_deref()
            .unwrap_or("")
            .contains("hello"));
    }

    #[tokio::test]
    async fn should_return_bad_request_for_malformed_acs_payload() {
        let access_key = BASE64.encode("shared-secret");
        let req = request_with_signature(
            "POST",
            "http://localhost/emails:send?api-version=2023-03-31",
            "{",
            &access_key,
        )
        .await;

        let response = AcsEmailAdapter
            .handle(
                temp_mail(),
                Arc::new(Config {
                    access_key_id: None,
                    secret_access_key: None,
                    enforce_auth: false,
                    admin_auth_disabled: false,
                    blobs_path: "./blobs".into(),
                    lifecycle_interval: std::time::Duration::from_hours(1),
                    api_port: 9000,
                    ui_port: 9001,
                    max_request_bytes: crate::config::DEFAULT_SQRZL_MAX_REQUEST_BYTES,
                    smtp_port: crate::config::DEFAULT_SQRZL_SMTP_PORT,
                }),
                req,
            )
            .await
            .expect("ACS adapter should render malformed payload errors");

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            response
                .headers()
                .get("content-type")
                .and_then(|value| value.to_str().ok()),
            Some("application/json; charset=utf-8")
        );
    }

    #[test]
    fn should_generate_authorized_signature_roundtrip() {
        let access_key = BASE64.encode("shared-secret");
        let content_hash = acs_hmac::content_hash(b"abc");
        let string_to_sign = format!(
            "POST\n/emails:send?api-version=2023-03-31\nThu, 07 Aug 2026 12:00:00 GMT;localhost;{content_hash}"
        );
        let signature = acs_hmac::sign_request(&access_key, &string_to_sign)
            .expect("ACS test access key should be valid base64");
        assert!(acs_hmac::validate_signature(
            &access_key,
            &string_to_sign,
            &signature,
        ));
    }
}
