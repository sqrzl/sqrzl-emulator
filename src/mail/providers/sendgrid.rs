use crate::auth::AuthConfig;
use crate::body::Body;
use crate::mail::model::{Address, Attachment, Message, SourceProtocol};
use crate::mail::{fan_out_batch, MailStore};
use crate::server::RequestExt as MailRequest;
use crate::server::ResponseBuilder;
use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use http::Method;
use http::StatusCode;
use hyper::Response;
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

const ENV_SENDGRID_API_KEY: &str = "SQRZL_SENDGRID_API_KEY";
const SENDGRID_MAX_MESSAGE_BYTES: usize = 30 * 1024 * 1024;

pub struct SendGridAdapter;

impl SendGridAdapter {
    fn invalid_request_response(message: &str) -> Response<Body> {
        ResponseBuilder::new(StatusCode::BAD_REQUEST)
            .content_type("application/json; charset=utf-8")
            .body(
                serde_json::json!({"errors":[{"message":message}]})
                    .to_string()
                    .into_bytes(),
            )
            .build()
    }

    fn require_api_key() -> Option<String> {
        std::env::var(ENV_SENDGRID_API_KEY).ok()
    }

    fn is_authorized(req: &MailRequest) -> bool {
        let Some(expected_key) = Self::require_api_key() else {
            return true;
        };

        req.header("authorization")
            .is_some_and(|value| value == format!("Bearer {expected_key}").as_str())
    }

    // Keep the provider's nested request contract in one validation pass so no
    // field can be persisted before the complete payload has been checked.
    #[allow(clippy::too_many_lines)]
    fn parse_messages(req: &MailRequest) -> Result<Vec<Message>, String> {
        let payload = serde_json::from_slice::<Value>(&req.body)
            .map_err(|err| format!("invalid sendgrid request body: {err}"))?;
        let payload = payload
            .as_object()
            .ok_or_else(|| "sendgrid request body must be an object".to_string())?;
        reject_unsupported_fields(
            payload,
            &[
                "personalizations",
                "from",
                "reply_to",
                "reply_to_list",
                "subject",
                "content",
                "attachments",
                "headers",
            ],
            "sendgrid request",
        )?;

        let personalizations = payload
            .get("personalizations")
            .and_then(Value::as_array)
            .filter(|values| !values.is_empty())
            .ok_or_else(|| "sendgrid request must include personalizations".to_string())?;
        if personalizations.len() > 1_000 {
            return Err("sendgrid supports at most 1000 personalizations".to_string());
        }
        let global_from = parse_address(payload.get("from"))?;
        let (body_text, body_html) = parse_content(payload.get("content"))?;
        let attachments = match payload.get("attachments") {
            None => Vec::new(),
            Some(Value::Array(values)) if !values.is_empty() => parse_attachments(values)?,
            Some(Value::Array(_)) => {
                return Err("sendgrid attachments must contain at least one item".to_string())
            }
            Some(_) => return Err("sendgrid attachments must be an array".to_string()),
        };
        let global_headers = parse_headers(payload.get("headers"))?;
        let reply_to = parse_reply_to(payload)?;

        let mut seen_recipients = HashSet::new();
        let mut recipient_count = 0usize;
        let mut messages = Vec::with_capacity(personalizations.len());
        for personalization in personalizations {
            let personalization = personalization
                .as_object()
                .ok_or_else(|| "sendgrid personalizations must contain objects".to_string())?;
            reject_unsupported_fields(
                personalization,
                &["to", "cc", "bcc", "from", "subject", "headers"],
                "sendgrid personalization",
            )?;
            let to_values = personalization
                .get("to")
                .and_then(Value::as_array)
                .filter(|values| !values.is_empty())
                .ok_or_else(|| "sendgrid personalization must include to".to_string())?;
            let to = parse_addresses(to_values)?;
            let cc = parse_optional_addresses(personalization.get("cc"))?;
            let bcc = parse_optional_addresses(personalization.get("bcc"))?;
            recipient_count += to.len() + cc.len() + bcc.len();
            if recipient_count > 1_000 {
                return Err("sendgrid supports at most 1000 total recipients".to_string());
            }
            for recipient in to.iter().chain(cc.iter()).chain(bcc.iter()) {
                if !seen_recipients.insert(recipient.email.trim().to_ascii_lowercase()) {
                    return Err("sendgrid recipient email addresses must be unique".to_string());
                }
            }
            let from = parse_address(personalization.get("from"))?
                .or_else(|| global_from.clone())
                .ok_or_else(|| "sendgrid request must include from".to_string())?;

            let subject = first_string(
                personalization
                    .get("subject")
                    .or_else(|| payload.get("subject"))
                    .and_then(Value::as_str),
            )
            .ok_or_else(|| "sendgrid request must include subject".to_string())?;
            let mut headers = global_headers.clone();
            merge_headers(&mut headers, parse_headers(personalization.get("headers"))?);

            messages.push(Message {
                source_protocol: SourceProtocol::SendGrid,
                from,
                to,
                cc,
                bcc,
                reply_to: reply_to.clone(),
                subject,
                headers,
                body_text: body_text.clone(),
                body_html: body_html.clone(),
                attachments: attachments.clone(),
                user_engagement_tracking_disabled: None,
                provider_metadata: HashMap::new(),
                raw_mime: None,
                thread_id: None,
            });
        }
        Ok(messages)
    }
}

impl crate::mail::providers::MailAdapter for SendGridAdapter {
    fn name(&self) -> &'static str {
        "sendgrid"
    }

    fn matches(&self, req: &MailRequest) -> bool {
        req.path() == "/v3/mail/send"
    }

    fn matches_request_head(
        &self,
        method: &Method,
        uri: &http::Uri,
        _headers: &http::HeaderMap,
    ) -> bool {
        method == Method::POST && uri.path() == "/v3/mail/send"
    }

    fn payload_too_large(&self, max_request_bytes: usize) -> Response<Body> {
        ResponseBuilder::new(StatusCode::PAYLOAD_TOO_LARGE)
            .content_type("application/json; charset=utf-8")
            .body(
                serde_json::json!({"errors":[{"message":format!("The request body exceeds the {max_request_bytes}-byte emulator limit.")} ]})
                    .to_string()
                    .into_bytes(),
            )
            .build()
    }

    fn incomplete_body(&self) -> Response<Body> {
        Self::invalid_request_response("The request body ended before it was complete.")
    }

    fn handle<'a>(
        &'a self,
        mail: Arc<dyn MailStore>,
        _auth_config: Arc<AuthConfig>,
        req: MailRequest,
    ) -> Pin<Box<dyn Future<Output = Result<Response<Body>, String>> + Send + 'a>> {
        Box::pin(async move {
            if req.method() != Method::POST {
                return Ok(ResponseBuilder::new(StatusCode::METHOD_NOT_ALLOWED)
                    .header("allow", "POST")
                    .content_type("application/json; charset=utf-8")
                    .body(
                        serde_json::json!({"errors":[{"message":"Method Not Allowed"}]})
                            .to_string()
                            .into_bytes(),
                    )
                    .build());
            }
            if !Self::is_authorized(&req) {
                return Ok(ResponseBuilder::new(StatusCode::UNAUTHORIZED)
                    .content_type("application/json; charset=utf-8")
                    .body(
                        serde_json::json!({"errors":[{"message":"Unauthorized","field":"authorization","help":"Set Authorization: Bearer <SQRZL_SENDGRID_API_KEY>"}]})
                            .to_string()
                            .into_bytes(),
                    )
                    .build());
            }
            if !req
                .header("content-type")
                .and_then(|value| value.split(';').next())
                .is_some_and(|value| value.trim().eq_ignore_ascii_case("application/json"))
            {
                return Ok(ResponseBuilder::new(StatusCode::UNSUPPORTED_MEDIA_TYPE)
                    .content_type("application/json; charset=utf-8")
                    .body(
                        serde_json::json!({"errors":[{"message":"Mail Send requires application/json content."}]})
                            .to_string()
                            .into_bytes(),
                    )
                    .build());
            }
            if req.body.len() >= SENDGRID_MAX_MESSAGE_BYTES {
                return Ok(ResponseBuilder::new(StatusCode::PAYLOAD_TOO_LARGE)
                    .content_type("application/json; charset=utf-8")
                    .body(
                        serde_json::json!({"errors":[{"message":"The total email size must be less than 30 MB."}]})
                            .to_string()
                            .into_bytes(),
                    )
                    .build());
            }

            let messages = match Self::parse_messages(&req) {
                Ok(messages) => messages,
                Err(message) => return Ok(Self::invalid_request_response(&message)),
            };
            let stored_batches = match fan_out_batch(mail.as_ref(), &messages) {
                Ok(stored) => stored,
                Err(crate::error::Error::InvalidRequest(message)) => {
                    return Ok(Self::invalid_request_response(&message));
                }
                Err(err) => return Err(err.to_string()),
            };
            let message_id = stored_batches
                .first()
                .and_then(|stored| stored.first())
                .map_or_else(crate::mail::generate_message_id, |stored| {
                    stored.message_id.clone()
                });

            Ok(ResponseBuilder::new(StatusCode::ACCEPTED)
                .header("x-message-id", &message_id)
                .body(Vec::new())
                .build())
        })
    }
}

fn parse_address(value: Option<&Value>) -> Result<Option<Address>, String> {
    match value {
        Some(Value::Object(object)) => {
            reject_unsupported_fields(object, &["email", "name"], "sendgrid address")?;
            let email = object
                .get("email")
                .and_then(Value::as_str)
                .filter(|email| valid_email_address(email))
                .ok_or_else(|| "sendgrid address must include a valid email".to_string())?;
            let name = match object.get("name") {
                None => None,
                Some(Value::String(name)) => Some(name.clone()),
                Some(_) => return Err("sendgrid address name must be a string".to_string()),
            };
            Ok(Some(Address {
                email: email.to_string(),
                name,
            }))
        }
        None => Ok(None),
        _ => Err("sendgrid addresses must be objects".to_string()),
    }
}

fn parse_addresses(values: &[Value]) -> Result<Vec<Address>, String> {
    values
        .iter()
        .map(|value| {
            parse_address(Some(value))?
                .ok_or_else(|| "sendgrid address must include email".to_string())
        })
        .collect()
}

fn parse_optional_addresses(value: Option<&Value>) -> Result<Vec<Address>, String> {
    match value {
        None => Ok(Vec::new()),
        Some(Value::Array(values)) if !values.is_empty() => parse_addresses(values),
        Some(Value::Array(_)) => {
            Err("sendgrid recipient collections must not be empty".to_string())
        }
        Some(_) => Err("sendgrid recipient collections must be arrays".to_string()),
    }
}

fn first_string(value: Option<&str>) -> Option<String> {
    value
        .filter(|value| !value.is_empty())
        .map(std::string::ToString::to_string)
}

fn parse_content(value: Option<&Value>) -> Result<(Option<String>, Option<String>), String> {
    let values = value
        .and_then(Value::as_array)
        .filter(|values| !values.is_empty())
        .ok_or_else(|| "sendgrid request must include content".to_string())?;
    let mut body_text = None;
    let mut body_html = None;
    for item in values {
        let item = item
            .as_object()
            .ok_or_else(|| "sendgrid content entries must be objects".to_string())?;
        reject_unsupported_fields(item, &["type", "value"], "sendgrid content")?;
        let content_type = item
            .get("type")
            .and_then(Value::as_str)
            .ok_or_else(|| "sendgrid content must include type".to_string())?;
        let content = item
            .get("value")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| "sendgrid content must include value".to_string())?
            .to_string();
        match content_type {
            "text/plain" if body_text.is_none() && body_html.is_none() => body_text = Some(content),
            "text/html" if body_html.is_none() => body_html = Some(content),
            "text/plain" => {
                return Err("sendgrid text/plain content must precede text/html".to_string())
            }
            "text/html" => return Err("sendgrid content types must be unique".to_string()),
            _ => return Err(format!("unsupported sendgrid content type: {content_type}")),
        }
    }
    Ok((body_text, body_html))
}

fn parse_attachments(values: &[Value]) -> Result<Vec<Attachment>, String> {
    let mut attachments = Vec::new();
    for value in values {
        let value = value
            .as_object()
            .ok_or_else(|| "sendgrid attachments must contain objects".to_string())?;
        reject_unsupported_fields(
            value,
            &["filename", "type", "content", "disposition", "content_id"],
            "sendgrid attachment",
        )?;
        let name = value
            .get("filename")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .map(std::string::ToString::to_string)
            .ok_or_else(|| "sendgrid attachment must include filename".to_string())?;
        let content_type = value
            .get("type")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .map_or_else(
                || "application/octet-stream".to_string(),
                std::string::ToString::to_string,
            );
        let content = value
            .get("content")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| "sendgrid attachment must include content".to_string())?;
        let content = BASE64
            .decode(content)
            .map_err(|_| "sendgrid attachment content must be valid base64".to_string())?;
        attachments.push(Attachment {
            filename: name,
            content_type,
            content,
            disposition: match value.get("disposition") {
                None => None,
                Some(Value::String(value)) if matches!(value.as_str(), "inline" | "attachment") => {
                    Some(value.clone())
                }
                Some(_) => {
                    return Err(
                        "sendgrid attachment disposition must be inline or attachment".to_string(),
                    )
                }
            },
            content_id: match value.get("content_id") {
                None => None,
                Some(Value::String(value)) if !value.is_empty() => Some(value.clone()),
                Some(_) => {
                    return Err(
                        "sendgrid attachment content_id must be a non-empty string".to_string()
                    )
                }
            },
        });
    }
    Ok(attachments)
}

fn parse_headers(value: Option<&Value>) -> Result<HashMap<String, String>, String> {
    const RESERVED: &[&str] = &[
        "x-sg-id",
        "x-sg-eid",
        "received",
        "dkim-signature",
        "content-type",
        "content-transfer-encoding",
        "to",
        "from",
        "subject",
        "reply-to",
        "cc",
        "bcc",
    ];
    let Some(value) = value else {
        return Ok(HashMap::new());
    };
    let values = value
        .as_object()
        .ok_or_else(|| "sendgrid headers must be an object".to_string())?;
    let mut headers = HashMap::new();
    let mut names = HashSet::new();
    for (name, value) in values {
        if name.is_empty()
            || !name
                .bytes()
                .all(|byte| byte.is_ascii() && (33..=126).contains(&byte) && byte != b':')
        {
            return Err(format!("sendgrid header {name:?} has an invalid name"));
        }
        if RESERVED
            .iter()
            .any(|reserved| name.eq_ignore_ascii_case(reserved))
        {
            return Err(format!("sendgrid header {name} cannot be overridden"));
        }
        if !names.insert(name.to_ascii_lowercase()) {
            return Err(format!("sendgrid header {name} is duplicated"));
        }
        let value = value
            .as_str()
            .ok_or_else(|| format!("sendgrid header {name} must be a string"))?;
        headers.insert(name.clone(), value.to_string());
    }
    Ok(headers)
}

fn merge_headers(target: &mut HashMap<String, String>, overrides: HashMap<String, String>) {
    for (name, value) in overrides {
        if let Some(existing) = target
            .keys()
            .find(|existing| existing.eq_ignore_ascii_case(&name))
            .cloned()
        {
            target.remove(&existing);
        }
        target.insert(name, value);
    }
}

fn parse_reply_to(payload: &serde_json::Map<String, Value>) -> Result<Vec<Address>, String> {
    if payload.contains_key("reply_to") && payload.contains_key("reply_to_list") {
        return Err("sendgrid reply_to and reply_to_list are mutually exclusive".to_string());
    }
    if let Some(value) = payload.get("reply_to") {
        return parse_address(Some(value))?.map_or_else(
            || Err("sendgrid reply_to must include an address".to_string()),
            |address| Ok(vec![address]),
        );
    }
    let reply_to = parse_optional_addresses(payload.get("reply_to_list"))?;
    if reply_to.len() > 1_000 {
        return Err("sendgrid supports at most 1000 reply_to_list addresses".to_string());
    }
    let mut seen = HashSet::new();
    if reply_to
        .iter()
        .any(|address| !seen.insert(address.email.trim().to_ascii_lowercase()))
    {
        return Err("sendgrid reply_to_list addresses must be unique".to_string());
    }
    Ok(reply_to)
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
