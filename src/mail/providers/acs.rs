use crate::auth::AuthConfig;
use crate::auth::{acs_hmac, parse_connection_string};
use crate::body::Body;
use crate::mail::model::{Address, Attachment, Message, SourceProtocol};
use crate::mail::providers::MailAdapter;
use crate::mail::{fan_out_with_id, MailStore, ALL_MAILBOX};
use crate::server::{RequestExt as MailRequest, ResponseBuilder};
use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use chrono::{DateTime, TimeDelta, Utc};
use http::{HeaderMap, Method, StatusCode, Uri};
use hyper::Response;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

const ENV_ACS_CONNECTION_STRING: &str = "SQRZL_ACS_CONNECTION_STRING";
const ACS_EMAIL_MAX_REQUEST_BYTES: usize = 10 * 1024 * 1024;
const ACS_EMAIL_MAX_RECIPIENTS: usize = 50;
const ACS_EMAIL_API_VERSIONS: &[&str] = &["2023-03-01", "2023-03-31", "2025-09-01"];

pub struct AcsEmailAdapter;
type RecipientGroups = (Vec<Address>, Vec<Address>, Vec<Address>);

struct Repeatability {
    request_id: String,
    first_sent: String,
    request_hash: String,
}

impl AcsEmailAdapter {
    fn invalid_request_response(message: &str) -> Response<Body> {
        Self::error_response(StatusCode::BAD_REQUEST, "InvalidRequest", message)
    }

    fn error_response(status: StatusCode, code: &str, message: &str) -> Response<Body> {
        ResponseBuilder::new(status)
            .header("x-ms-error-code", code)
            .content_type("application/json; charset=utf-8")
            .body(
                serde_json::json!({
                    "error": {
                        "code": code,
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
        let payload = payload
            .as_object()
            .ok_or_else(|| "ACS request body must be an object".to_string())?;
        reject_unsupported_fields(
            payload,
            &[
                "senderAddress",
                "recipients",
                "content",
                "attachments",
                "headers",
                "replyTo",
                "userEngagementTrackingDisabled",
            ],
            "ACS email request",
        )?;

        let from = payload
            .get("senderAddress")
            .and_then(Value::as_str)
            .filter(|value| valid_email_address(value))
            .map(Address::new)
            .ok_or_else(|| "senderAddress must be a valid email address".to_string())?;

        let recipients = payload
            .get("recipients")
            .and_then(Value::as_object)
            .ok_or_else(|| "recipients is required".to_string())?;
        reject_unsupported_fields(recipients, &["to", "cc", "bcc"], "ACS recipients")?;
        let recipients = parse_recipients(recipients)?;
        let recipient_count = recipients.0.len() + recipients.1.len() + recipients.2.len();
        if recipient_count == 0 {
            return Err("recipients must include at least one to, cc, or bcc address".to_string());
        }
        if recipient_count > ACS_EMAIL_MAX_RECIPIENTS {
            return Err(format!(
                "ACS email supports at most {ACS_EMAIL_MAX_RECIPIENTS} recipients"
            ));
        }

        let content = payload
            .get("content")
            .and_then(Value::as_object)
            .ok_or_else(|| "content is required".to_string())?;
        reject_unsupported_fields(content, &["subject", "plainText", "html"], "ACS content")?;
        let subject = content
            .get("subject")
            .and_then(Value::as_str)
            .filter(|subject| !subject.is_empty())
            .map(ToString::to_string)
            .ok_or_else(|| "content.subject is required".to_string())?;

        let body_text = content
            .get("plainText")
            .and_then(Value::as_str)
            .map(ToString::to_string);
        let body_html = content
            .get("html")
            .and_then(Value::as_str)
            .map(ToString::to_string);
        if body_text.is_none() && body_html.is_none() {
            return Err("content must include plainText or html".to_string());
        }

        let headers = parse_headers(payload.get("headers"))?;
        let reply_to = parse_addresses(payload.get("replyTo"))?;
        let user_engagement_tracking_disabled = match payload.get("userEngagementTrackingDisabled")
        {
            None => None,
            Some(Value::Bool(value)) => Some(*value),
            Some(_) => return Err("userEngagementTrackingDisabled must be a boolean".to_string()),
        };

        let attachments = match payload.get("attachments") {
            None => Vec::new(),
            Some(Value::Array(attachments)) => parse_attachments(attachments)?,
            Some(_) => return Err("ACS attachments must be an array".to_string()),
        };

        Ok(Message {
            source_protocol: SourceProtocol::Acs,
            from,
            to: recipients.0,
            cc: recipients.1,
            bcc: recipients.2,
            reply_to,
            subject,
            headers,
            body_text,
            body_html,
            attachments,
            user_engagement_tracking_disabled,
            provider_metadata: HashMap::new(),
            raw_mime: None,
            thread_id: None,
        })
    }

    #[allow(clippy::result_large_err)]
    fn repeatability(req: &MailRequest) -> Result<Option<Repeatability>, Response<Body>> {
        let request_id = req.header("repeatability-request-id");
        let first_sent = req.header("repeatability-first-sent");
        let (request_id, first_sent) = match (request_id, first_sent) {
            (None, None) => return Ok(None),
            (Some(request_id), Some(first_sent)) => (request_id, first_sent),
            _ => return Err(Self::invalid_request_response(
                "Repeatability-Request-Id and Repeatability-First-Sent must be supplied together",
            )),
        };
        if !valid_uuid(request_id) {
            return Err(Self::invalid_request_response(
                "Repeatability-Request-Id must be a GUID",
            ));
        }
        let Some(first_sent_time) = parse_imf_fixdate(first_sent) else {
            return Err(Self::invalid_request_response(
                "Repeatability-First-Sent must use IMF-fixdate format",
            ));
        };
        if Utc::now().signed_duration_since(first_sent_time) > TimeDelta::minutes(5) {
            return Err(Self::error_response(
                StatusCode::PRECONDITION_FAILED,
                "PreconditionFailed",
                "Repeatability first sent header was not in 5 minutes window.",
            ));
        }

        let mut digest = Sha256::new();
        digest.update(
            req.uri
                .path_and_query()
                .map_or(req.path(), http::uri::PathAndQuery::as_str)
                .as_bytes(),
        );
        digest.update([0]);
        digest.update(req.header("operation-id").unwrap_or("").as_bytes());
        digest.update([0]);
        digest.update(&req.body);
        Ok(Some(Repeatability {
            request_id: request_id.to_string(),
            first_sent: first_sent.to_string(),
            request_hash: hex::encode(digest.finalize()),
        }))
    }

    fn existing_repeatability(
        mail: &dyn MailStore,
        repeatability: &Repeatability,
    ) -> crate::error::Result<Option<crate::mail::StoredMessage>> {
        Ok(mail
            .list_messages(ALL_MAILBOX, crate::mail::ListMessagesParams::default())?
            .messages
            .into_iter()
            .find(|stored| {
                stored.message.source_protocol == SourceProtocol::Acs
                    && stored
                        .message
                        .provider_metadata
                        .get("repeatability_request_id")
                        .and_then(Value::as_str)
                        == Some(repeatability.request_id.as_str())
            }))
    }

    fn accepted_response(req: &MailRequest, message_id: &str) -> Response<Body> {
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

        ResponseBuilder::new(StatusCode::ACCEPTED)
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
            .build()
    }
}

impl MailAdapter for AcsEmailAdapter {
    fn name(&self) -> &'static str {
        "acs"
    }

    fn matches(&self, req: &MailRequest) -> bool {
        req.path() == "/emails:send" || req.path().starts_with("/emails/operations/")
    }

    fn matches_request_head(&self, method: &Method, uri: &Uri, _headers: &HeaderMap) -> bool {
        method == Method::POST && uri.path() == "/emails:send"
    }

    fn payload_too_large(&self, max_request_bytes: usize) -> Response<Body> {
        ResponseBuilder::new(StatusCode::PAYLOAD_TOO_LARGE)
            .header("x-ms-error-code", "RequestBodyTooLarge")
            .content_type("application/json; charset=utf-8")
            .body(
                serde_json::json!({"error":{"code":"RequestBodyTooLarge","message":format!("Request body exceeds the {max_request_bytes}-byte emulator limit")}})
                    .to_string()
                    .into_bytes(),
            )
            .build()
    }

    fn incomplete_body(&self) -> Response<Body> {
        Self::invalid_request_response("The request body ended before it was complete")
    }

    // This single dispatch keeps ACS authentication, repeatability, polling,
    // and mutation ordering visible as one provider transaction.
    #[allow(clippy::too_many_lines)]
    fn handle<'a>(
        &'a self,
        mail: Arc<dyn MailStore>,
        _auth_config: Arc<AuthConfig>,
        req: MailRequest,
    ) -> Pin<Box<dyn Future<Output = Result<Response<Body>, String>> + Send + 'a>> {
        Box::pin(async move {
            let is_send = req.path() == "/emails:send";
            let is_poll = req.path().starts_with("/emails/operations/");
            if (is_send && req.method() != Method::POST) || (is_poll && req.method() != Method::GET)
            {
                return Ok(ResponseBuilder::new(StatusCode::METHOD_NOT_ALLOWED)
                    .header("allow", if is_send { "POST" } else { "GET" })
                    .header("x-ms-error-code", "MethodNotAllowed")
                    .content_type("application/json; charset=utf-8")
                    .body(
                        serde_json::json!({"error":{"code":"MethodNotAllowed","message":"Method not allowed"}})
                            .to_string()
                            .into_bytes(),
                    )
                    .build());
            }
            let Some(api_version) = req.query_param("api-version") else {
                return Ok(Self::invalid_request_response("api-version is required"));
            };
            if !ACS_EMAIL_API_VERSIONS.contains(&api_version) {
                return Ok(Self::invalid_request_response(
                    "api-version is not supported",
                ));
            }
            if is_send && req.body.len() > ACS_EMAIL_MAX_REQUEST_BYTES {
                return Ok(Self::error_response(
                    StatusCode::PAYLOAD_TOO_LARGE,
                    "RequestBodyTooLarge",
                    "The ACS Email request exceeds the 10 MB service limit",
                ));
            }
            if !Self::is_authorized(&req) {
                return Ok(ResponseBuilder::new(StatusCode::UNAUTHORIZED)
                    .header("x-ms-error-code", "Unauthorized")
                    .content_type("application/json; charset=utf-8")
                    .body(
                        serde_json::json!({"error":{"code":"Unauthorized","message":""}})
                            .to_string()
                            .into_bytes(),
                    )
                    .build());
            }
            if is_send
                && !req
                    .header("content-type")
                    .and_then(|value| value.split(';').next())
                    .is_some_and(|value| value.trim().eq_ignore_ascii_case("application/json"))
            {
                return Ok(Self::error_response(
                    StatusCode::UNSUPPORTED_MEDIA_TYPE,
                    "UnsupportedMediaType",
                    "ACS Email Send requires application/json content",
                ));
            }

            if req.method() == Method::GET {
                let operation_id = req
                    .path()
                    .strip_prefix("/emails/operations/")
                    .filter(|value| !value.is_empty())
                    .ok_or_else(|| "invalid ACS operation path".to_string())?;
                if mail.get_message(ALL_MAILBOX, operation_id).is_err() {
                    return Ok(ResponseBuilder::new(StatusCode::NOT_FOUND)
                        .header("x-ms-error-code", "NotFound")
                        .content_type("application/json; charset=utf-8")
                        .body(
                            serde_json::json!({"error":{"code":"NotFound","message":"Email operation not found"}})
                                .to_string()
                                .into_bytes(),
                        )
                        .build());
                }
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

            let repeatability = match Self::repeatability(&req) {
                Ok(value) => value,
                Err(response) => return Ok(response),
            };
            if let Some(repeatability) = repeatability.as_ref() {
                let existing = match Self::existing_repeatability(mail.as_ref(), repeatability) {
                    Ok(existing) => existing,
                    Err(error) => return Err(error.to_string()),
                };
                if let Some(existing) = existing {
                    let metadata = &existing.message.provider_metadata;
                    let matches = metadata
                        .get("repeatability_first_sent")
                        .and_then(Value::as_str)
                        == Some(repeatability.first_sent.as_str())
                        && metadata
                            .get("repeatability_request_hash")
                            .and_then(Value::as_str)
                            == Some(repeatability.request_hash.as_str());
                    if !matches {
                        return Ok(Self::invalid_request_response(
                            "Repeated request does not match the original request",
                        ));
                    }
                    return Ok(Self::accepted_response(&req, &existing.message_id));
                }
            }

            let mut message = match Self::parse_message(&req) {
                Ok(message) => message,
                Err(message) => return Ok(Self::invalid_request_response(&message)),
            };
            let operation_id = match req.header("operation-id") {
                Some(value) if valid_uuid(value) => value.to_string(),
                Some(_) => {
                    return Ok(Self::invalid_request_response(
                        "Operation-Id must be a UUID",
                    ))
                }
                None => uuid::Uuid::new_v4().to_string(),
            };
            if let Some(repeatability) = repeatability {
                message.provider_metadata.insert(
                    "repeatability_request_id".to_string(),
                    Value::String(repeatability.request_id),
                );
                message.provider_metadata.insert(
                    "repeatability_first_sent".to_string(),
                    Value::String(repeatability.first_sent),
                );
                message.provider_metadata.insert(
                    "repeatability_request_hash".to_string(),
                    Value::String(repeatability.request_hash),
                );
            }
            let stored_messages = match fan_out_with_id(mail.as_ref(), &message, &operation_id) {
                Ok(stored_messages) => stored_messages,
                Err(crate::error::Error::InvalidRequest(message)) => {
                    return Ok(Self::invalid_request_response(&message));
                }
                Err(err) => return Err(err.to_string()),
            };
            let message_id = stored_messages
                .first()
                .map_or(operation_id, |stored| stored.message_id.clone());
            Ok(Self::accepted_response(&req, &message_id))
        })
    }
}

fn parse_address_value(value: &Value) -> Result<Address, String> {
    let object = value
        .as_object()
        .ok_or_else(|| "ACS email addresses must be objects".to_string())?;
    reject_unsupported_fields(object, &["address", "displayName"], "ACS email address")?;
    let email = object
        .get("address")
        .and_then(Value::as_str)
        .filter(|value| valid_email_address(value))
        .ok_or_else(|| "ACS email address must include a valid address".to_string())?;
    let name = match object.get("displayName") {
        None => None,
        Some(Value::String(name)) => Some(name.clone()),
        Some(_) => return Err("ACS email displayName must be a string".to_string()),
    };
    Ok(Address {
        email: email.to_string(),
        name,
    })
}

fn parse_recipients(
    recipients: &serde_json::Map<String, Value>,
) -> Result<RecipientGroups, String> {
    Ok((
        parse_addresses(recipients.get("to"))?,
        parse_addresses(recipients.get("cc"))?,
        parse_addresses(recipients.get("bcc"))?,
    ))
}

fn parse_addresses(value: Option<&Value>) -> Result<Vec<Address>, String> {
    let Some(value) = value else {
        return Ok(Vec::new());
    };
    let values = value
        .as_array()
        .ok_or_else(|| "ACS recipient collections must be arrays".to_string())?;
    values.iter().map(parse_address_value).collect()
}

fn parse_attachments(values: &[Value]) -> Result<Vec<Attachment>, String> {
    values
        .iter()
        .map(|value| {
            let value = value
                .as_object()
                .ok_or_else(|| "ACS attachments must contain objects".to_string())?;
            reject_unsupported_fields(
                value,
                &["name", "contentType", "contentInBase64", "contentId"],
                "ACS attachment",
            )?;
            let filename = value
                .get("name")
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .ok_or_else(|| "ACS attachment must include name".to_string())?
                .to_string();
            let content_type = value
                .get("contentType")
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .ok_or_else(|| "ACS attachment must include contentType".to_string())?
                .to_string();
            let encoded = value
                .get("contentInBase64")
                .and_then(Value::as_str)
                .ok_or_else(|| "ACS attachment must include contentInBase64".to_string())?;
            let content = BASE64
                .decode(encoded)
                .map_err(|_| "ACS attachment contentInBase64 must be valid base64".to_string())?;

            Ok(Attachment {
                filename,
                content_type,
                content,
                disposition: None,
                content_id: match value.get("contentId") {
                    None => None,
                    Some(Value::String(value)) if !value.is_empty() => Some(value.clone()),
                    Some(_) => {
                        return Err(
                            "ACS attachment contentId must be a non-empty string".to_string()
                        )
                    }
                },
            })
        })
        .collect()
}

fn parse_headers(value: Option<&Value>) -> Result<HashMap<String, String>, String> {
    let Some(value) = value else {
        return Ok(HashMap::new());
    };
    let values = value
        .as_object()
        .ok_or_else(|| "ACS headers must be an object".to_string())?;
    values
        .iter()
        .map(|(name, value)| {
            if name.is_empty()
                || !name
                    .bytes()
                    .all(|byte| byte.is_ascii() && (33..=126).contains(&byte) && byte != b':')
            {
                return Err(format!("ACS header {name:?} has an invalid name"));
            }
            let value = value
                .as_str()
                .ok_or_else(|| format!("ACS header {name} must be a string"))?;
            if value.contains(['\r', '\n']) {
                return Err(format!("ACS header {name} contains a line break"));
            }
            Ok((name.clone(), value.to_string()))
        })
        .collect()
}

fn reject_unsupported_fields(
    object: &serde_json::Map<String, Value>,
    allowed: &[&str],
    scope: &str,
) -> Result<(), String> {
    if let Some(name) = object.keys().find(|name| !allowed.contains(&name.as_str())) {
        return Err(format!(
            "{scope} field {name} is not supported by this emulator"
        ));
    }
    Ok(())
}

fn valid_uuid(value: &str) -> bool {
    value.len() == 36
        && [8, 13, 18, 23]
            .into_iter()
            .all(|index| value.as_bytes().get(index) == Some(&b'-'))
        && uuid::Uuid::parse_str(value).is_ok()
}

fn parse_imf_fixdate(value: &str) -> Option<DateTime<Utc>> {
    let parsed = DateTime::parse_from_rfc2822(value)
        .ok()?
        .with_timezone(&Utc);
    (parsed.format("%a, %d %b %Y %H:%M:%S GMT").to_string() == value).then_some(parsed)
}

fn valid_email_address(value: &str) -> bool {
    if value.is_empty()
        || value.len() > 320
        || !value.is_ascii()
        || value
            .bytes()
            .any(|byte| byte.is_ascii_whitespace() || byte.is_ascii_control())
    {
        return false;
    }
    let mut parts = value.split('@');
    let (Some(local), Some(domain), None) = (parts.next(), parts.next(), parts.next()) else {
        return false;
    };
    !local.is_empty()
        && local.len() <= 64
        && !local.starts_with('.')
        && !local.ends_with('.')
        && !local.contains("..")
        && !domain.is_empty()
        && domain.len() <= 255
        && !domain.starts_with('.')
        && !domain.ends_with('.')
        && !domain.contains("..")
        && domain.split('.').all(|label| {
            !label.is_empty()
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        })
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
                .header("content-type", "application/json")
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
        let body = r#"{"senderAddress":"alice@example.com","recipients":{"to":[{"address":"bob@example.com"}]},"content":{"subject":"acs test","plainText":"hello","html":"<p>hello</p>"}}"#;
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
        // Arrange
        // Act
        // Assert
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
