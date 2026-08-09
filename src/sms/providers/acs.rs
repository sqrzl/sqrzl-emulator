use super::{json_error, SmsAdapter};
use crate::auth::{acs_hmac, parse_connection_string, AuthConfig};
use crate::body::Body;
use crate::server::{RequestExt as SmsRequest, ResponseBuilder};
use crate::sms::model::{is_e164, valid_sender, NewSmsMessage};
use crate::sms::{generate_batch_id, SmsChannel, SmsDirection, SmsProvider, SmsStore};
use http::{HeaderMap, Method, StatusCode, Uri};
use hyper::Response;
use serde_json::Value;
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

pub struct AcsSmsAdapter;

impl AcsSmsAdapter {
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
        to: &str,
        repeatability_request_id: Option<String>,
    ) -> crate::error::Result<Value> {
        let mut metadata = HashMap::new();
        if let Some(options) = options {
            metadata.insert("sms_send_options".to_string(), options.clone());
        }
        if let Some(value) = &repeatability_request_id {
            metadata.insert(
                "repeatability_request_id".to_string(),
                Value::String(value.clone()),
            );
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
        Ok(serde_json::json!({
            "to": to,
            "messageId": stored.provider_message_id,
            "successful": true,
            "httpStatusCode": 202,
            "errorMessage": null,
            "repeatabilityResult": repeatability_request_id.map(|request_id| serde_json::json!({
                "repeatabilityRequestId": request_id,
                "firstSent": stored.created_at.to_rfc3339(),
            }))
        }))
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

    fn send(store: &dyn SmsStore, request: &SmsRequest) -> Response<Body> {
        let Ok(Value::Object(payload)) = serde_json::from_slice::<Value>(&request.body) else {
            return json_error(
                StatusCode::BAD_REQUEST,
                "InvalidRequest",
                "Invalid JSON request body",
            );
        };
        if payload.contains_key("media")
            || payload.contains_key("mediaUrls")
            || payload.contains_key("attachments")
        {
            return json_error(
                StatusCode::BAD_REQUEST,
                "InvalidRequest",
                "Azure Communication Services SMS does not support MMS media",
            );
        }
        let sender = payload.get("from").and_then(Value::as_str);
        let recipients = payload.get("smsRecipients").and_then(Value::as_array);
        let message_body = payload.get("message").and_then(Value::as_str);
        let (Some(sender), Some(recipients), Some(message_body)) =
            (sender, recipients, message_body)
        else {
            return json_error(
                StatusCode::BAD_REQUEST,
                "InvalidRequest",
                "from, smsRecipients, and message are required",
            );
        };
        if !valid_sender(sender) || recipients.is_empty() {
            return json_error(
                StatusCode::BAD_REQUEST,
                "InvalidRequest",
                "from is invalid or smsRecipients is empty",
            );
        }
        let parsed_recipients = recipients
            .iter()
            .map(|recipient| {
                let to = recipient.get("to").and_then(Value::as_str)?;
                let repeatability = recipient
                    .get("repeatabilityRequestId")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                Some((to.to_string(), repeatability))
            })
            .collect::<Option<Vec<_>>>();
        let Some(recipients) = parsed_recipients else {
            return json_error(
                StatusCode::BAD_REQUEST,
                "InvalidRequest",
                "Every smsRecipients item must contain to",
            );
        };
        if recipients.iter().any(|(to, _)| !is_e164(to)) {
            return json_error(
                StatusCode::BAD_REQUEST,
                "InvalidRequest",
                "Every recipient must be an E.164 phone number",
            );
        }
        let batch_id = generate_batch_id();
        let options = payload.get("smsSendOptions").cloned();
        let mut results = Vec::with_capacity(recipients.len());
        for (to, repeatability_request_id) in recipients {
            let result = match Self::store_recipient(
                store,
                &batch_id,
                sender,
                message_body,
                options.as_ref(),
                &to,
                repeatability_request_id,
            ) {
                Ok(result) => result,
                Err(error) => {
                    return json_error(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "InternalError",
                        &error.to_string(),
                    )
                }
            };
            results.push(result);
        }
        Self::send_response(&results)
    }
}

impl SmsAdapter for AcsSmsAdapter {
    fn name(&self) -> &'static str {
        "acs"
    }

    fn matches(&self, request: &SmsRequest) -> bool {
        request.method() == Method::POST
            && request.path() == "/sms"
            && request.has_query_param("api-version")
    }

    fn matches_request_head(&self, method: &Method, uri: &Uri, _headers: &HeaderMap) -> bool {
        method == Method::POST && uri.path() == "/sms"
    }

    fn payload_too_large(&self, max_request_bytes: usize) -> Response<Body> {
        json_error(
            StatusCode::PAYLOAD_TOO_LARGE,
            "RequestEntityTooLarge",
            &format!("Request body exceeds the {max_request_bytes}-byte emulator limit"),
        )
    }

    fn handle<'a>(
        &'a self,
        store: Arc<dyn SmsStore>,
        _auth: Arc<AuthConfig>,
        request: SmsRequest,
    ) -> Pin<Box<dyn Future<Output = Result<Response<Body>, String>> + Send + 'a>> {
        Box::pin(async move {
            if !Self::authorized(&request) {
                return Ok(json_error(
                    StatusCode::UNAUTHORIZED,
                    "Unauthorized",
                    "Invalid ACS HMAC authentication",
                ));
            }
            Ok(Self::send(store.as_ref(), &request))
        })
    }
}
