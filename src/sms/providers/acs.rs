use super::SmsAdapter;
use crate::auth::{acs_hmac, parse_connection_string, AuthConfig};
use crate::body::Body;
use crate::server::{RequestExt as SmsRequest, ResponseBuilder};
use crate::sms::model::{is_e164, NewSmsMessage};
use crate::sms::{
    generate_batch_id, ListSmsParams, SmsChannel, SmsDirection, SmsProvider, SmsStore,
};
use http::{HeaderMap, Method, StatusCode, Uri};
use hyper::Response;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

pub struct AcsSmsAdapter;
const ACS_SMS_API_VERSIONS: &[&str] = &["2021-03-07", "2026-01-23"];

struct AcsRecipient {
    to: String,
    repeatability_request_id: Option<String>,
    repeatability_first_sent: Option<String>,
}

impl AcsSmsAdapter {
    fn validation_error(field: &str, message: &str) -> Response<Body> {
        ResponseBuilder::new(StatusCode::BAD_REQUEST)
            .content_type("application/problem+json")
            .body(
                serde_json::json!({
                    "type": "https://tools.ietf.org/html/rfc9110#section-15.5.1",
                    "title": "One or more validation errors occurred.",
                    "status": 400,
                    "errors": { field: [message] },
                    "traceId": format!("00-{}-{}-00", uuid::Uuid::new_v4().simple(), &uuid::Uuid::new_v4().simple().to_string()[..16]),
                })
                .to_string()
                .into_bytes(),
            )
            .build()
    }

    fn standard_error(status: StatusCode, code: &str, message: &str) -> Response<Body> {
        ResponseBuilder::new(status)
            .header("x-ms-error-code", code)
            .content_type("application/json; charset=utf-8")
            .body(
                serde_json::json!({"error": {"code": code, "message": message}})
                    .to_string()
                    .into_bytes(),
            )
            .build()
    }

    fn send_response(results: &[Value]) -> Response<Body> {
        ResponseBuilder::new(StatusCode::ACCEPTED)
            .content_type("application/json; charset=utf-8")
            .body(serde_json::to_vec(&serde_json::json!({ "value": results })).unwrap_or_default())
            .build()
    }

    fn store_recipient(
        store: &dyn SmsStore,
        batch_id: &str,
        sender: &str,
        message_body: &str,
        options: Option<&Value>,
        recipient: &AcsRecipient,
    ) -> crate::error::Result<(Value, Option<crate::sms::SmsMessage>)> {
        let to = recipient.to.as_str();
        let repeatability_request_id = recipient.repeatability_request_id.as_ref();
        let repeatability_first_sent = recipient.repeatability_first_sent.as_ref();
        let request_hash = acs_sms_request_hash(sender, to, message_body, options);
        let mut metadata = HashMap::new();
        if let Some(options) = options {
            metadata.insert("sms_send_options".to_string(), options.clone());
        }
        if let Some(value) = repeatability_request_id {
            metadata.insert(
                "repeatability_request_id".to_string(),
                Value::String(value.clone()),
            );
        }
        if let Some(value) = repeatability_first_sent {
            metadata.insert(
                "repeatability_first_sent".to_string(),
                Value::String(value.clone()),
            );
        }
        metadata.insert(
            "repeatability_request_hash".to_string(),
            Value::String(request_hash.clone()),
        );
        if let Some(request_id) = repeatability_request_id {
            if let Some(existing) = store
                .list_messages(to, ListSmsParams::default())?
                .messages
                .into_iter()
                .find(|message| {
                    message.provider == SmsProvider::Acs
                        && message.from == sender
                        && message
                            .metadata
                            .get("repeatability_request_id")
                            .and_then(Value::as_str)
                            == Some(request_id.as_str())
                })
            {
                let matches_first_sent = existing
                    .metadata
                    .get("repeatability_first_sent")
                    .and_then(Value::as_str)
                    == repeatability_first_sent.map(String::as_str);
                let matches_request = existing
                    .metadata
                    .get("repeatability_request_hash")
                    .and_then(Value::as_str)
                    == Some(request_hash.as_str());
                return Ok((
                    if matches_first_sent && matches_request {
                        serde_json::json!({
                            "to": to,
                            "messageId": existing.provider_message_id,
                            "successful": true,
                            "httpStatusCode": 202,
                            "repeatabilityResult": "accepted"
                        })
                    } else {
                        repeatability_error(
                            to,
                            "Repeatability request metadata does not match the original request",
                        )
                    },
                    None,
                ));
            }
        }
        let stored = store.store_message(NewSmsMessage {
            batch_id: Some(batch_id.to_string()),
            provider: SmsProvider::Acs,
            provider_message_id: None,
            direction: SmsDirection::Outbound,
            channel: SmsChannel::Sms,
            from: sender.to_string(),
            to: to.to_string(),
            body: message_body.to_string(),
            media: Vec::new(),
            metadata,
        })?;
        let mut result = serde_json::json!({
            "to": to,
            "messageId": stored.provider_message_id,
            "successful": true,
            "httpStatusCode": 202,
        });
        if repeatability_request_id.is_some() {
            result["repeatabilityResult"] = Value::String("accepted".to_string());
        }
        Ok((result, Some(stored)))
    }

    fn connection_key() -> Option<String> {
        std::env::var("SQRZL_ACS_CONNECTION_STRING")
            .ok()
            .and_then(|value| parse_connection_string(&value))
            .map(|connection| connection.access_key)
    }

    fn authorized(request: &SmsRequest) -> bool {
        let Some(access_key) = Self::connection_key() else {
            return true;
        };
        let Some(value) = request.header("authorization") else {
            return false;
        };
        let Some(parameters) = value.strip_prefix("HMAC-SHA256 ") else {
            return false;
        };
        let mut signed_headers = None;
        let mut signature = None;
        for parameter in parameters.split('&') {
            if let Some((name, value)) = parameter.split_once('=') {
                if name.eq_ignore_ascii_case("SignedHeaders") {
                    signed_headers = Some(value);
                } else if name.eq_ignore_ascii_case("Signature") {
                    signature = Some(value);
                }
            }
        }
        let Some(signed_headers) = signed_headers else {
            return false;
        };
        let names = signed_headers.split(';').collect::<Vec<_>>();
        let date_name = if names
            .iter()
            .any(|name| name.eq_ignore_ascii_case("x-ms-date"))
        {
            "x-ms-date"
        } else if names.iter().any(|name| name.eq_ignore_ascii_case("date")) {
            "date"
        } else {
            return false;
        };
        if !names.iter().any(|name| name.eq_ignore_ascii_case("host"))
            || !names
                .iter()
                .any(|name| name.eq_ignore_ascii_case("x-ms-content-sha256"))
        {
            return false;
        }
        let (Some(date), Some(host), Some(content_hash), Some(signature)) = (
            request.header(date_name),
            request.header("host"),
            request.header("x-ms-content-sha256"),
            signature,
        ) else {
            return false;
        };
        if content_hash != acs_hmac::content_hash(&request.body) {
            return false;
        }
        let path_and_query = request
            .uri
            .path_and_query()
            .map_or(request.path(), http::uri::PathAndQuery::as_str);
        let string_to_sign = format!(
            "{}\n{}\n{};{};{}",
            request.method(),
            path_and_query,
            date,
            host,
            content_hash
        );
        acs_hmac::validate_signature(&access_key, &string_to_sign, signature)
    }

    // Per-recipient validation/results and rollback are one ACS batch contract.
    #[allow(clippy::too_many_lines)]
    fn send(store: &dyn SmsStore, request: &SmsRequest) -> Response<Body> {
        let Ok(Value::Object(payload)) = serde_json::from_slice::<Value>(&request.body) else {
            return Self::validation_error("Body", "Invalid JSON request body");
        };
        if let Some(name) = payload.keys().find(|name| {
            !matches!(
                name.as_str(),
                "from" | "smsRecipients" | "message" | "smsSendOptions"
            )
        }) {
            return Self::validation_error(
                "Body",
                &format!("Field {name} is not supported by the ACS SMS emulator"),
            );
        }
        let sender = payload.get("from").and_then(Value::as_str);
        let recipients = payload.get("smsRecipients").and_then(Value::as_array);
        let message_body = payload.get("message").and_then(Value::as_str);
        let (Some(sender), Some(recipients), Some(message_body)) =
            (sender, recipients, message_body)
        else {
            return Self::validation_error(
                "Body",
                "from, smsRecipients, and message are required with their documented types",
            );
        };
        if !is_e164(sender) || recipients.is_empty() || recipients.len() > 100 {
            return Self::validation_error(
                "SmsRecipients",
                "from must be E.164 and smsRecipients must contain between 1 and 100 items",
            );
        }
        if message_body.chars().count() > 2_048 {
            return Self::validation_error(
                "Message",
                "message must contain at most 2048 characters",
            );
        }
        let parsed_recipients = recipients
            .iter()
            .map(|recipient| {
                let recipient = recipient.as_object().ok_or(())?;
                if recipient.keys().any(|name| {
                    !matches!(
                        name.as_str(),
                        "to" | "repeatabilityRequestId" | "repeatabilityFirstSent"
                    )
                }) {
                    return Err(());
                }
                let to = recipient.get("to").and_then(Value::as_str).ok_or(())?;
                let repeatability = optional_string(recipient, "repeatabilityRequestId")?;
                let first_sent = optional_string(recipient, "repeatabilityFirstSent")?;
                Ok(AcsRecipient {
                    to: to.to_string(),
                    repeatability_request_id: repeatability,
                    repeatability_first_sent: first_sent,
                })
            })
            .collect::<Result<Vec<_>, ()>>();
        let Ok(recipients) = parsed_recipients else {
            return Self::validation_error(
                "SmsRecipients",
                "Every smsRecipients item must contain only documented fields with valid types",
            );
        };
        let batch_id = generate_batch_id();
        let options = match validate_sms_send_options(payload.get("smsSendOptions")) {
            Ok(options) => options.cloned(),
            Err(message) => return Self::validation_error("SmsSendOptions", &message),
        };
        let mut results = Vec::with_capacity(recipients.len());
        let mut captured = Vec::with_capacity(recipients.len());
        for recipient in recipients {
            if !is_e164(&recipient.to) {
                results.push(serde_json::json!({
                    "to": recipient.to,
                    "successful": false,
                    "httpStatusCode": 400,
                    "errorMessage": "Invalid To phone number format."
                }));
                continue;
            }
            if recipient.repeatability_request_id.is_some()
                != recipient.repeatability_first_sent.is_some()
                || recipient
                    .repeatability_request_id
                    .as_deref()
                    .is_some_and(|value| !valid_uuid(value))
                || recipient
                    .repeatability_first_sent
                    .as_deref()
                    .is_some_and(|value| !valid_imf_fixdate(value))
            {
                results.push(repeatability_error(
                    &recipient.to,
                    "repeatabilityRequestId requires a UUID and repeatabilityFirstSent in RFC1123 format",
                ));
                continue;
            }
            let result = match Self::store_recipient(
                store,
                &batch_id,
                sender,
                message_body,
                options.as_ref(),
                &recipient,
            ) {
                Ok((result, stored)) => {
                    if let Some(stored) = stored {
                        captured.push(stored);
                    }
                    result
                }
                Err(error) => {
                    let rollback_errors = captured
                        .iter()
                        .filter_map(|stored: &crate::sms::SmsMessage| {
                            store
                                .delete_message(&stored.peer, &stored.message_id)
                                .err()
                                .map(|rollback| {
                                    format!("{}/{}: {rollback}", stored.peer, stored.message_id)
                                })
                        })
                        .collect::<Vec<_>>();
                    let message = if rollback_errors.is_empty() {
                        error.to_string()
                    } else {
                        format!(
                            "{error}; rollback failed for {}",
                            rollback_errors.join(", ")
                        )
                    };
                    return Self::standard_error(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "InternalError",
                        &message,
                    );
                }
            };
            results.push(result);
        }
        Self::send_response(&results)
    }
}

fn repeatability_error(to: &str, message: &str) -> Value {
    serde_json::json!({
        "to": to,
        "successful": false,
        "httpStatusCode": 400,
        "errorMessage": message,
        "repeatabilityResult": "rejected"
    })
}

fn optional_string(
    object: &serde_json::Map<String, Value>,
    name: &str,
) -> Result<Option<String>, ()> {
    match object.get(name) {
        None => Ok(None),
        Some(Value::String(value)) => Ok(Some(value.clone())),
        Some(_) => Err(()),
    }
}

fn validate_sms_send_options(value: Option<&Value>) -> Result<Option<&Value>, String> {
    let Some(value) = value else {
        return Ok(None);
    };
    let object = value
        .as_object()
        .ok_or_else(|| "smsSendOptions must be an object".to_string())?;
    if let Some(name) = object.keys().find(|name| {
        !matches!(
            name.as_str(),
            "enableDeliveryReport" | "tag" | "deliveryReportTimeoutInSeconds" | "messagingConnect"
        )
    }) {
        return Err(format!("smsSendOptions field {name} is not supported"));
    }
    if object.contains_key("messagingConnect") {
        return Err(
            "smsSendOptions.messagingConnect is not supported by this emulator".to_string(),
        );
    }
    if object
        .get("enableDeliveryReport")
        .is_some_and(|value| !value.is_boolean())
    {
        return Err("enableDeliveryReport must be a boolean".to_string());
    }
    if object.get("tag").is_some_and(|value| !value.is_string()) {
        return Err("tag must be a string".to_string());
    }
    if object
        .get("deliveryReportTimeoutInSeconds")
        .is_some_and(|value| {
            !value
                .as_i64()
                .is_some_and(|seconds| (60..=43_200).contains(&seconds))
        })
    {
        return Err(
            "deliveryReportTimeoutInSeconds must be an integer between 60 and 43200".to_string(),
        );
    }
    Ok(Some(value))
}

fn valid_uuid(value: &str) -> bool {
    value.len() == 36
        && [8, 13, 18, 23]
            .into_iter()
            .all(|index| value.as_bytes().get(index) == Some(&b'-'))
        && uuid::Uuid::parse_str(value).is_ok()
}

fn valid_imf_fixdate(value: &str) -> bool {
    chrono::DateTime::parse_from_rfc2822(value).is_ok_and(|parsed| {
        parsed
            .with_timezone(&chrono::Utc)
            .format("%a, %d %b %Y %H:%M:%S GMT")
            .to_string()
            == value
    })
}

fn acs_sms_request_hash(sender: &str, to: &str, message: &str, options: Option<&Value>) -> String {
    let mut digest = Sha256::new();
    for value in [sender, to, message] {
        digest.update(value.as_bytes());
        digest.update([0]);
    }
    if let Some(options) = options {
        digest.update(serde_json::to_vec(options).unwrap_or_default());
    }
    hex::encode(digest.finalize())
}

impl SmsAdapter for AcsSmsAdapter {
    fn name(&self) -> &'static str {
        "acs"
    }

    fn matches(&self, request: &SmsRequest) -> bool {
        request.path() == "/sms"
    }

    fn matches_request_head(&self, method: &Method, uri: &Uri, _headers: &HeaderMap) -> bool {
        method == Method::POST && uri.path() == "/sms"
    }

    fn payload_too_large(&self, max_request_bytes: usize) -> Response<Body> {
        Self::standard_error(
            StatusCode::PAYLOAD_TOO_LARGE,
            "RequestEntityTooLarge",
            &format!("Request body exceeds the {max_request_bytes}-byte emulator limit"),
        )
    }

    fn incomplete_body(&self) -> Response<Body> {
        Self::validation_error("Body", "The request body ended before it was complete")
    }

    fn handle<'a>(
        &'a self,
        store: Arc<dyn SmsStore>,
        _auth: Arc<AuthConfig>,
        request: SmsRequest,
    ) -> Pin<Box<dyn Future<Output = Result<Response<Body>, String>> + Send + 'a>> {
        Box::pin(async move {
            if request.method() != Method::POST {
                return Ok(Self::standard_error(
                    StatusCode::METHOD_NOT_ALLOWED,
                    "MethodNotAllowed",
                    "The ACS SMS Send operation requires HTTP POST",
                ));
            }
            let Some(api_version) = request.query_param("api-version") else {
                return Ok(Self::validation_error(
                    "api-version",
                    "api-version is required",
                ));
            };
            if !ACS_SMS_API_VERSIONS.contains(&api_version) {
                return Ok(Self::validation_error(
                    "api-version",
                    "api-version is not supported",
                ));
            }
            if !Self::authorized(&request) {
                return Ok(Self::standard_error(
                    StatusCode::UNAUTHORIZED,
                    "Unauthorized",
                    "",
                ));
            }
            if !request
                .header("content-type")
                .and_then(|value| value.split(';').next())
                .is_some_and(|value| value.trim().eq_ignore_ascii_case("application/json"))
            {
                return Ok(Self::standard_error(
                    StatusCode::UNSUPPORTED_MEDIA_TYPE,
                    "UnsupportedMediaType",
                    "ACS SMS Send requires application/json content",
                ));
            }
            Ok(Self::send(store.as_ref(), &request))
        })
    }
}
