use super::{aws_auth, decode_form, form_value, SmsAdapter};
use crate::auth::AuthConfig;
use crate::body::Body;
use crate::server::{RequestExt as SmsRequest, ResponseBuilder};
use crate::sms::model::{is_e164, valid_sender, NewSmsMedia, NewSmsMessage};
use crate::sms::{SmsChannel, SmsDirection, SmsProvider, SmsStore};
use http::{HeaderMap, Method, StatusCode, Uri};
use hyper::Response;
use serde_json::Value;
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

pub struct SnsSmsAdapter;
pub struct AwsSmsVoiceAdapter;

impl SnsSmsAdapter {
    fn error(code: &str, message: &str, status: StatusCode) -> Response<Body> {
        let request_id = uuid::Uuid::new_v4();
        ResponseBuilder::new(status)
            .content_type("text/xml; charset=utf-8")
            .body_str(&format!(
                "<ErrorResponse xmlns=\"http://sns.amazonaws.com/doc/2010-03-31/\"><Error><Type>Sender</Type><Code>{code}</Code><Message>{message}</Message></Error><RequestId>{request_id}</RequestId></ErrorResponse>"
            ))
            .build()
    }

    fn publish(store: &dyn SmsStore, request: &SmsRequest) -> Response<Body> {
        let fields = decode_form(&request.body);
        if form_value(&fields, "TopicArn").is_some() || form_value(&fields, "TargetArn").is_some() {
            return Self::error(
                "InvalidParameter",
                "Only direct-to-phone PhoneNumber publishing is supported",
                StatusCode::BAD_REQUEST,
            );
        }
        let (Some(phone), Some(body)) = (
            form_value(&fields, "PhoneNumber"),
            form_value(&fields, "Message"),
        ) else {
            return Self::error(
                "InvalidParameter",
                "PhoneNumber and Message are required",
                StatusCode::BAD_REQUEST,
            );
        };
        if !is_e164(phone) {
            return Self::error(
                "InvalidParameter",
                "PhoneNumber must be E.164",
                StatusCode::BAD_REQUEST,
            );
        }
        let sender = sns_sender_id(&fields).unwrap_or_else(|| "SNS".to_string());
        if !valid_sender(&sender) {
            return Self::error(
                "InvalidParameter",
                "SMS.SenderID is invalid",
                StatusCode::BAD_REQUEST,
            );
        }
        let message = match store.store_message(NewSmsMessage {
            batch_id: None,
            provider: SmsProvider::Sns,
            provider_message_id: None,
            direction: SmsDirection::Outbound,
            channel: SmsChannel::Sms,
            from: sender,
            to: phone.to_string(),
            body: body.to_string(),
            media: Vec::new(),
            metadata: HashMap::new(),
        }) {
            Ok(message) => message,
            Err(error) => {
                return Self::error(
                    "InternalError",
                    &error.to_string(),
                    StatusCode::INTERNAL_SERVER_ERROR,
                )
            }
        };
        let request_id = uuid::Uuid::new_v4();
        ResponseBuilder::new(StatusCode::OK)
            .content_type("text/xml; charset=utf-8")
            .body_str(&format!(
                "<PublishResponse xmlns=\"http://sns.amazonaws.com/doc/2010-03-31/\"><PublishResult><MessageId>{}</MessageId></PublishResult><ResponseMetadata><RequestId>{request_id}</RequestId></ResponseMetadata></PublishResponse>",
                message.provider_message_id
            ))
            .build()
    }
}

impl SmsAdapter for SnsSmsAdapter {
    fn name(&self) -> &'static str {
        "sns"
    }

    fn matches(&self, request: &SmsRequest) -> bool {
        if request.method() != Method::POST || request.path() != "/" {
            return false;
        }
        let fields = decode_form(&request.body);
        form_value(&fields, "Action") == Some("Publish")
    }

    fn matches_request_head(&self, method: &Method, uri: &Uri, headers: &HeaderMap) -> bool {
        method == Method::POST
            && uri.path() == "/"
            && headers
                .get("authorization")
                .and_then(|value| value.to_str().ok())
                .is_some_and(|value| value.contains("/sns/aws4_request"))
    }

    fn payload_too_large(&self, max_request_bytes: usize) -> Response<Body> {
        Self::error(
            "InvalidParameter",
            &format!("Request body exceeds the {max_request_bytes}-byte emulator limit"),
            StatusCode::PAYLOAD_TOO_LARGE,
        )
    }

    fn handle<'a>(
        &'a self,
        store: Arc<dyn SmsStore>,
        auth: Arc<AuthConfig>,
        request: SmsRequest,
    ) -> Pin<Box<dyn Future<Output = Result<Response<Body>, String>> + Send + 'a>> {
        Box::pin(async move {
            if !aws_auth::authorized(&request, auth.as_ref(), "sns") {
                return Ok(Self::error(
                    "AuthorizationError",
                    "The security token included in the request is invalid",
                    StatusCode::FORBIDDEN,
                ));
            }
            Ok(Self::publish(store.as_ref(), &request))
        })
    }
}

impl AwsSmsVoiceAdapter {
    fn target(request: &SmsRequest) -> Option<&str> {
        request.header("x-amz-target").filter(|target| {
            matches!(
                *target,
                "PinpointSMSVoiceV2.SendTextMessage" | "PinpointSMSVoiceV2.SendMediaMessage"
            )
        })
    }

    fn error(status: StatusCode, kind: &str, message: &str) -> Response<Body> {
        ResponseBuilder::new(status)
            .header("x-amzn-errortype", kind)
            .content_type("application/x-amz-json-1.0")
            .body(
                serde_json::json!({"message": message})
                    .to_string()
                    .into_bytes(),
            )
            .build()
    }

    fn send(store: &dyn SmsStore, request: &SmsRequest, target: &str) -> Response<Body> {
        let payload = match serde_json::from_slice::<Value>(&request.body) {
            Ok(Value::Object(payload)) => payload,
            _ => {
                return Self::error(
                    StatusCode::BAD_REQUEST,
                    "ValidationException",
                    "Invalid JSON request body",
                )
            }
        };
        let destination = payload
            .get("DestinationPhoneNumber")
            .and_then(Value::as_str);
        let origination = payload.get("OriginationIdentity").and_then(Value::as_str);
        let (Some(destination), Some(origination)) = (destination, origination) else {
            return Self::error(
                StatusCode::BAD_REQUEST,
                "ValidationException",
                "DestinationPhoneNumber and OriginationIdentity are required",
            );
        };
        if !is_e164(destination) || !valid_sender(origination) {
            return Self::error(
                StatusCode::BAD_REQUEST,
                "ValidationException",
                "DestinationPhoneNumber or OriginationIdentity is invalid",
            );
        }
        let is_media = target.ends_with("SendMediaMessage");
        let media_urls = payload
            .get("MediaUrls")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        if is_media
            && (media_urls.is_empty() || media_urls.iter().any(|url| !url.starts_with("s3://")))
        {
            return Self::error(
                StatusCode::BAD_REQUEST,
                "ValidationException",
                "SendMediaMessage requires one or more s3:// MediaUrls",
            );
        }
        let body = payload
            .get("MessageBody")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let mut metadata = HashMap::new();
        for name in [
            "ConfigurationSetName",
            "Context",
            "DestinationCountryParameters",
        ] {
            if let Some(value) = payload.get(name) {
                metadata.insert(name.to_ascii_lowercase(), value.clone());
            }
        }
        let message = match store.store_message(NewSmsMessage {
            batch_id: None,
            provider: SmsProvider::AwsSmsVoiceV2,
            provider_message_id: None,
            direction: SmsDirection::Outbound,
            channel: if is_media {
                SmsChannel::Mms
            } else {
                SmsChannel::Sms
            },
            from: origination.to_string(),
            to: destination.to_string(),
            body,
            media: media_urls
                .into_iter()
                .enumerate()
                .map(|(index, url)| NewSmsMedia {
                    filename: format!("media-{}", index + 1),
                    content_type: "application/octet-stream".to_string(),
                    content: None,
                    external_url: Some(url),
                })
                .collect(),
            metadata,
        }) {
            Ok(message) => message,
            Err(error) => {
                return Self::error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "InternalServerException",
                    &error.to_string(),
                )
            }
        };
        ResponseBuilder::new(StatusCode::OK)
            .content_type("application/x-amz-json-1.0")
            .body(
                serde_json::json!({"MessageId": message.provider_message_id})
                    .to_string()
                    .into_bytes(),
            )
            .build()
    }
}

impl SmsAdapter for AwsSmsVoiceAdapter {
    fn name(&self) -> &'static str {
        "aws-sms-voice-v2"
    }

    fn matches(&self, request: &SmsRequest) -> bool {
        request.method() == Method::POST && request.path() == "/" && Self::target(request).is_some()
    }

    fn matches_request_head(&self, method: &Method, uri: &Uri, headers: &HeaderMap) -> bool {
        method == Method::POST
            && uri.path() == "/"
            && headers
                .get("x-amz-target")
                .and_then(|value| value.to_str().ok())
                .is_some_and(|value| value.starts_with("PinpointSMSVoiceV2.Send"))
    }

    fn payload_too_large(&self, max_request_bytes: usize) -> Response<Body> {
        Self::error(
            StatusCode::PAYLOAD_TOO_LARGE,
            "ValidationException",
            &format!("Request body exceeds the {max_request_bytes}-byte emulator limit"),
        )
    }

    fn handle<'a>(
        &'a self,
        store: Arc<dyn SmsStore>,
        auth: Arc<AuthConfig>,
        request: SmsRequest,
    ) -> Pin<Box<dyn Future<Output = Result<Response<Body>, String>> + Send + 'a>> {
        Box::pin(async move {
            if !aws_auth::authorized(&request, auth.as_ref(), "sms-voice") {
                return Ok(Self::error(
                    StatusCode::FORBIDDEN,
                    "AccessDeniedException",
                    "The security token included in the request is invalid",
                ));
            }
            let target = Self::target(&request).unwrap_or_default().to_string();
            Ok(Self::send(store.as_ref(), &request, &target))
        })
    }
}

fn sns_sender_id(fields: &[(String, String)]) -> Option<String> {
    for (name, value) in fields {
        let Some(prefix) = name.strip_suffix(".Name") else {
            continue;
        };
        if value == "AWS.SNS.SMS.SenderID" || value == "AWS.SNS.SMS.SenderId" {
            let value_key = format!("{prefix}.Value.StringValue");
            return form_value(fields, &value_key).map(str::to_string);
        }
    }
    None
}
