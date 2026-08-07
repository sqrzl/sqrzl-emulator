use crate::auth::AuthConfig;
use crate::body::Body;
use crate::mail::model::{Address, Attachment, Message, SourceProtocol};
use crate::mail::{fan_out, MailStore};
use crate::server::RequestExt as MailRequest;
use crate::server::ResponseBuilder;
use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use http::Method;
use http::StatusCode;
use hyper::Response;
use serde_json::Value;
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

const ENV_SENDGRID_API_KEY: &str = "SQRZL_SENDGRID_API_KEY";

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

    fn parse_messages(req: &MailRequest) -> Result<Vec<Message>, String> {
        let payload = serde_json::from_slice::<Value>(&req.body)
            .map_err(|err| format!("invalid sendgrid request body: {err}"))?;

        let personalizations = payload
            .get("personalizations")
            .and_then(Value::as_array)
            .filter(|values| !values.is_empty())
            .ok_or_else(|| "sendgrid request must include personalizations".to_string())?;

        let from = parse_address(payload.get("from"));
        let from = match from {
            Some(value) => value,
            None => Address::new("unknown@localhost"),
        };

        let mut body_text = None;
        let mut body_html = None;
        if let Some(content_list) = payload.get("content").and_then(Value::as_array) {
            for content in content_list {
                let content_type = content.get("type").and_then(Value::as_str).unwrap_or("");
                let value = content
                    .get("value")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                if content_type == "text/plain" {
                    body_text = Some(value.clone());
                }
                if content_type == "text/html" {
                    body_html = Some(value);
                }
            }
        }

        let attachments = payload
            .get("attachments")
            .and_then(Value::as_array)
            .map_or_else(Vec::new, |values| parse_attachments(values.as_slice()));

        personalizations
            .iter()
            .map(|personalization| {
                let to = parse_addresses(
                    personalization
                        .get("to")
                        .and_then(Value::as_array)
                        .map_or(&[] as &[Value], Vec::as_slice),
                );
                let cc = parse_addresses(
                    personalization
                        .get("cc")
                        .and_then(Value::as_array)
                        .map_or(&[] as &[Value], Vec::as_slice),
                );
                let bcc = parse_addresses(
                    personalization
                        .get("bcc")
                        .and_then(Value::as_array)
                        .map_or(&[] as &[Value], Vec::as_slice),
                );
                if to.is_empty() && cc.is_empty() && bcc.is_empty() {
                    return Err("sendgrid personalization has no recipients".to_string());
                }

                let subject = first_string(
                    personalization
                        .get("subject")
                        .or_else(|| payload.get("subject"))
                        .and_then(Value::as_str),
                )
                .unwrap_or_default();
                let mut headers = personalization
                    .get("headers")
                    .and_then(Value::as_object)
                    .map_or_else(HashMap::new, |values| {
                        values
                            .iter()
                            .filter_map(|(name, value)| {
                                value
                                    .as_str()
                                    .map(|value| (name.clone(), value.to_string()))
                            })
                            .collect()
                    });
                if !subject.is_empty() {
                    headers.insert("subject".to_string(), subject.clone());
                }

                Ok(Message {
                    source_protocol: SourceProtocol::SendGrid,
                    from: from.clone(),
                    to,
                    cc,
                    bcc,
                    subject,
                    headers,
                    body_text: body_text.clone(),
                    body_html: body_html.clone(),
                    attachments: attachments.clone(),
                    raw_mime: None,
                    thread_id: None,
                })
            })
            .collect()
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
        headers: &http::HeaderMap,
    ) -> bool {
        method == Method::POST && uri.path() == "/v3/mail/send" && !headers.is_empty()
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
                        serde_json::json!({"errors":[{"message":"Unauthorized","field":"authorization","help":"Set Authorization: Bearer <SQRZL_SENDGRID_API_KEY>"}]})
                            .to_string()
                            .into_bytes(),
                    )
                    .build());
            }

            let messages = match Self::parse_messages(&req) {
                Ok(messages) => messages,
                Err(message) => return Ok(Self::invalid_request_response(&message)),
            };
            let mut message_id = None;
            for message in messages {
                let stored_messages = match fan_out(mail.as_ref(), &message) {
                    Ok(stored_messages) => stored_messages,
                    Err(crate::error::Error::InvalidRequest(message)) => {
                        return Ok(Self::invalid_request_response(&message));
                    }
                    Err(err) => return Err(err.to_string()),
                };
                if message_id.is_none() {
                    message_id = stored_messages
                        .first()
                        .map(|stored| stored.message_id.clone());
                }
            }
            let message_id = message_id.unwrap_or_else(crate::mail::generate_message_id);

            Ok(ResponseBuilder::new(StatusCode::ACCEPTED)
                .header("x-message-id", &message_id)
                .body(Vec::new())
                .build())
        })
    }
}

fn parse_address(value: Option<&Value>) -> Option<Address> {
    match value {
        Some(Value::String(email)) => Some(Address::new(email)),
        Some(Value::Object(object)) => {
            object
                .get("email")
                .and_then(Value::as_str)
                .map(|email| Address {
                    email: email.to_string(),
                    name: object
                        .get("name")
                        .and_then(Value::as_str)
                        .map(std::string::ToString::to_string),
                })
        }
        _ => None,
    }
}

fn parse_addresses(values: &[Value]) -> Vec<Address> {
    values
        .iter()
        .filter_map(|value| parse_address(Some(value)))
        .collect()
}

fn first_string(value: Option<&str>) -> Option<String> {
    value
        .filter(|value| !value.is_empty())
        .map(std::string::ToString::to_string)
}

fn parse_attachments(values: &[Value]) -> Vec<Attachment> {
    let mut attachments = Vec::new();
    for value in values {
        let name = value
            .get("filename")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .map(std::string::ToString::to_string);
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
            .and_then(|encoded| BASE64.decode(encoded).ok())
            .unwrap_or_default();

        if let Some(filename) = name {
            attachments.push(Attachment {
                filename,
                content_type,
                content,
            });
        }
    }
    attachments
}
