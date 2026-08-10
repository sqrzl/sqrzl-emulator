use super::{aws_auth, decode_form, form_value, SmsAdapter};
use crate::auth::AuthConfig;
use crate::body::Body;
use crate::server::{RequestExt as SmsRequest, ResponseBuilder};
use crate::sms::model::{is_e164, NewSmsMedia, NewSmsMessage};
use crate::sms::{SmsChannel, SmsDirection, SmsProvider, SmsStore};
use http::{HeaderMap, Method, StatusCode, Uri};
use hyper::Response;
use serde::de::{self, MapAccess, Visitor};
use serde::Deserialize;
use serde_json::Value;
use std::collections::HashMap;
use std::fmt;
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
                "<ErrorResponse xmlns=\"https://sns.amazonaws.com/doc/2010-03-31/\"><Error><Type>Sender</Type><Code>{code}</Code><Message>{message}</Message></Error><RequestId>{request_id}</RequestId></ErrorResponse>"
            ))
            .build()
    }

    // Keep SNS Query validation ahead of the sole persistence point.
    #[allow(clippy::too_many_lines)]
    fn publish(store: &dyn SmsStore, request: &SmsRequest) -> Response<Body> {
        let fields = sns_fields(request);
        if let Err(message) = validate_sns_fields(&fields) {
            return Self::error("InvalidParameter", &message, StatusCode::BAD_REQUEST);
        }
        if form_value(&fields, "Version") != Some("2010-03-31") {
            return Self::error(
                "InvalidParameter",
                "Version must be 2010-03-31",
                StatusCode::BAD_REQUEST,
            );
        }
        if form_value(&fields, "TopicArn").is_some() || form_value(&fields, "TargetArn").is_some() {
            return Self::error(
                "InvalidParameter",
                "Only direct-to-phone PhoneNumber publishing is supported",
                StatusCode::BAD_REQUEST,
            );
        }
        let (Some(phone), Some(raw_body)) = (
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
        if raw_body.chars().count() > 1_600 {
            return Self::error(
                "InvalidParameter",
                "Message must contain at most 1600 characters",
                StatusCode::BAD_REQUEST,
            );
        }
        let message_structure = form_value(&fields, "MessageStructure");
        let body = match message_structure {
            None => raw_body.to_string(),
            Some("json") => match sns_json_message(raw_body) {
                Ok(body) => body,
                Err(message) => {
                    return Self::error("InvalidParameter", &message, StatusCode::BAD_REQUEST)
                }
            },
            Some(_) => {
                return Self::error(
                    "InvalidParameter",
                    "MessageStructure must be json when specified",
                    StatusCode::BAD_REQUEST,
                )
            }
        };
        if message_structure.is_none() && body.is_empty() {
            return Self::error(
                "InvalidParameter",
                "Message must not be empty",
                StatusCode::BAD_REQUEST,
            );
        }
        let attributes = match sns_message_attributes(&fields) {
            Ok(attributes) => attributes,
            Err(message) => {
                return Self::error("InvalidParameter", &message, StatusCode::BAD_REQUEST)
            }
        };
        let sender = sns_sender_id(&attributes).unwrap_or_else(|| "SNS".to_string());
        if !valid_sns_sender_id(&sender) {
            return Self::error(
                "InvalidParameter",
                "SMS.SenderID is invalid",
                StatusCode::BAD_REQUEST,
            );
        }
        if attributes
            .get("AWS.SNS.SMS.SMSType")
            .and_then(Value::as_str)
            .is_some_and(|value| !matches!(value, "Transactional" | "Promotional"))
        {
            return Self::error(
                "InvalidParameter",
                "AWS.SNS.SMS.SMSType must be Transactional or Promotional",
                StatusCode::BAD_REQUEST,
            );
        }
        let mut metadata = HashMap::new();
        if !attributes.is_empty() {
            metadata.insert(
                "message_attributes".to_string(),
                Value::Object(attributes.clone()),
            );
        }
        if let Some(subject) = form_value(&fields, "Subject") {
            metadata.insert("subject".to_string(), Value::String(subject.to_string()));
        }
        let message = match store.store_message(NewSmsMessage {
            batch_id: None,
            provider: SmsProvider::Sns,
            provider_message_id: None,
            direction: SmsDirection::Outbound,
            channel: SmsChannel::Sms,
            from: sender,
            to: phone.to_string(),
            body,
            media: Vec::new(),
            metadata,
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
                "<PublishResponse xmlns=\"https://sns.amazonaws.com/doc/2010-03-31/\"><PublishResult><MessageId>{}</MessageId></PublishResult><ResponseMetadata><RequestId>{request_id}</RequestId></ResponseMetadata></PublishResponse>",
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
        if request.path() != "/" {
            return false;
        }
        let fields = sns_fields(request);
        form_value(&fields, "Action").is_some()
            || request
                .header("authorization")
                .is_some_and(|value| value.contains("/sns/aws4_request"))
            || (request.method() == Method::POST
                && request
                    .header("content-type")
                    .and_then(|value| value.split(';').next())
                    .is_some_and(|value| {
                        value
                            .trim()
                            .eq_ignore_ascii_case("application/x-www-form-urlencoded")
                    }))
    }

    fn matches_request_head(&self, method: &Method, uri: &Uri, headers: &HeaderMap) -> bool {
        method == Method::POST
            && uri.path() == "/"
            && (headers
                .get("authorization")
                .and_then(|value| value.to_str().ok())
                .is_some_and(|value| value.contains("/sns/aws4_request"))
                || headers
                    .get("content-type")
                    .and_then(|value| value.to_str().ok())
                    .and_then(|value| value.split(';').next())
                    .is_some_and(|value| {
                        value
                            .trim()
                            .eq_ignore_ascii_case("application/x-www-form-urlencoded")
                    }))
    }

    fn payload_too_large(&self, max_request_bytes: usize) -> Response<Body> {
        Self::error(
            "InvalidParameter",
            &format!("Request body exceeds the {max_request_bytes}-byte emulator limit"),
            StatusCode::PAYLOAD_TOO_LARGE,
        )
    }

    fn incomplete_body(&self) -> Response<Body> {
        Self::error(
            "InvalidParameter",
            "The request body ended before it was complete",
            StatusCode::BAD_REQUEST,
        )
    }

    fn handle<'a>(
        &'a self,
        store: Arc<dyn SmsStore>,
        auth: Arc<AuthConfig>,
        request: SmsRequest,
    ) -> Pin<Box<dyn Future<Output = Result<Response<Body>, String>> + Send + 'a>> {
        Box::pin(async move {
            if request.method() != Method::GET && request.method() != Method::POST {
                return Ok(Self::error(
                    "InvalidAction",
                    "SNS query requests require GET or POST",
                    StatusCode::METHOD_NOT_ALLOWED,
                ));
            }
            let fields = sns_fields(&request);
            match form_value(&fields, "Action") {
                None => {
                    return Ok(Self::error(
                        "MissingAction",
                        "Action is required",
                        StatusCode::BAD_REQUEST,
                    ))
                }
                Some("Publish") => {}
                Some(_) => {
                    return Ok(Self::error(
                        "InvalidAction",
                        "The action is not supported by this emulator",
                        StatusCode::BAD_REQUEST,
                    ))
                }
            }
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
    fn send_response(message_id: &str) -> Response<Body> {
        ResponseBuilder::new(StatusCode::OK)
            .content_type("application/x-amz-json-1.0")
            .body(
                serde_json::json!({"MessageId": message_id})
                    .to_string()
                    .into_bytes(),
            )
            .build()
    }

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

    // Both AWS operations share one validation-first transaction path.
    #[allow(clippy::too_many_lines)]
    fn send(store: &dyn SmsStore, request: &SmsRequest, target: &str) -> Response<Body> {
        let Ok(Value::Object(payload)) = serde_json::from_slice::<Value>(&request.body) else {
            return Self::error(
                StatusCode::BAD_REQUEST,
                "ValidationException",
                "Invalid JSON request body",
            );
        };
        let destination = payload
            .get("DestinationPhoneNumber")
            .and_then(Value::as_str);
        let origination = payload.get("OriginationIdentity").and_then(Value::as_str);
        let Some(destination) = destination else {
            return Self::error(
                StatusCode::BAD_REQUEST,
                "ValidationException",
                "DestinationPhoneNumber is required",
            );
        };
        let is_media = target.ends_with("SendMediaMessage");
        if let Err(message) = validate_aws_sms_fields(&payload, is_media) {
            return Self::error(StatusCode::BAD_REQUEST, "ValidationException", &message);
        }
        if is_media && origination.is_none() {
            return Self::error(
                StatusCode::BAD_REQUEST,
                "ValidationException",
                "OriginationIdentity is required for SendMediaMessage",
            );
        }
        if !valid_aws_destination(destination)
            || origination.is_some_and(|value| !valid_aws_origination(value))
        {
            return Self::error(
                StatusCode::BAD_REQUEST,
                "ValidationException",
                "DestinationPhoneNumber or OriginationIdentity is invalid",
            );
        }
        let media_urls = match payload.get("MediaUrls") {
            None => Vec::new(),
            Some(Value::Array(items)) => {
                let urls = items
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect::<Vec<_>>();
                if urls.len() != items.len() {
                    return Self::error(
                        StatusCode::BAD_REQUEST,
                        "ValidationException",
                        "MediaUrls must contain strings",
                    );
                }
                urls
            }
            Some(_) => {
                return Self::error(
                    StatusCode::BAD_REQUEST,
                    "ValidationException",
                    "MediaUrls must be an array",
                )
            }
        };
        if !is_media && payload.contains_key("MediaUrls") {
            return Self::error(
                StatusCode::BAD_REQUEST,
                "ValidationException",
                "MediaUrls is only supported by SendMediaMessage",
            );
        }
        if is_media
            && payload.contains_key("MediaUrls")
            && (media_urls.len() != 1 || media_urls.iter().any(|url| !valid_s3_media_uri(url)))
        {
            return Self::error(
                StatusCode::BAD_REQUEST,
                "ValidationException",
                "SendMediaMessage MediaUrls must contain exactly one valid S3 URI",
            );
        }
        let body = payload
            .get("MessageBody")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let dry_run = match payload.get("DryRun") {
            None => false,
            Some(Value::Bool(value)) => *value,
            Some(_) => {
                return Self::error(
                    StatusCode::BAD_REQUEST,
                    "ValidationException",
                    "DryRun must be a boolean",
                )
            }
        };
        let mut metadata = HashMap::new();
        for name in [
            "ConfigurationSetName",
            "Context",
            "DestinationCountryParameters",
            "Keyword",
            "MaxPrice",
            "MessageFeedbackEnabled",
            "MessageType",
            "ProtectConfigurationId",
            "TimeToLive",
        ] {
            if let Some(value) = payload.get(name) {
                metadata.insert(name.to_ascii_lowercase(), value.clone());
            }
        }
        if dry_run {
            return Self::send_response(&crate::sms::generate_provider_message_id(
                SmsProvider::AwsSmsVoiceV2,
            ));
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
            from: origination.unwrap_or("AWS").to_string(),
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
        Self::send_response(&message.provider_message_id)
    }
}

impl SmsAdapter for AwsSmsVoiceAdapter {
    fn name(&self) -> &'static str {
        "aws-sms-voice-v2"
    }

    fn matches(&self, request: &SmsRequest) -> bool {
        request.path() == "/"
            && (request
                .header("x-amz-target")
                .is_some_and(|value| value.starts_with("PinpointSMSVoiceV2."))
                || request
                    .header("authorization")
                    .is_some_and(|value| value.contains("/sms-voice/aws4_request")))
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

    fn incomplete_body(&self) -> Response<Body> {
        Self::error(
            StatusCode::BAD_REQUEST,
            "ValidationException",
            "The request body ended before it was complete",
        )
    }

    fn handle<'a>(
        &'a self,
        store: Arc<dyn SmsStore>,
        auth: Arc<AuthConfig>,
        request: SmsRequest,
    ) -> Pin<Box<dyn Future<Output = Result<Response<Body>, String>> + Send + 'a>> {
        Box::pin(async move {
            if request.method() != Method::POST {
                return Ok(Self::error(
                    StatusCode::BAD_REQUEST,
                    "UnknownOperationException",
                    "AWS SMS Voice V2 operations require HTTP POST",
                ));
            }
            let Some(target) = Self::target(&request).map(str::to_string) else {
                return Ok(Self::error(
                    StatusCode::BAD_REQUEST,
                    "UnknownOperationException",
                    "The requested AWS SMS Voice V2 operation is not supported",
                ));
            };
            if !aws_auth::authorized(&request, auth.as_ref(), "sms-voice") {
                return Ok(Self::error(
                    StatusCode::BAD_REQUEST,
                    "AccessDeniedException",
                    "The security token included in the request is invalid",
                ));
            }
            Ok(Self::send(store.as_ref(), &request, &target))
        })
    }
}

fn sns_sender_id(attributes: &serde_json::Map<String, Value>) -> Option<String> {
    attributes
        .get("AWS.SNS.SMS.SenderID")
        .or_else(|| attributes.get("AWS.SNS.SMS.SenderId"))
        .and_then(Value::as_str)
        .map(str::to_string)
}

fn sns_json_message(value: &str) -> Result<String, String> {
    let object = serde_json::from_str::<UniqueJsonObject>(value)
        .map_err(|error| format!("Message must be valid JSON when MessageStructure=json: {error}"))?
        .0;
    let default = object
        .get("default")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            "Message JSON must include a string default value when MessageStructure=json"
                .to_string()
        })?;
    // SNS ignores unrecognized protocol keys and keys whose values are not
    // strings. For direct phone publishing, only the optional `sms` string is
    // relevant; the required `default` value is the fallback.
    Ok(object
        .get("sms")
        .and_then(Value::as_str)
        .unwrap_or(default)
        .to_string())
}

struct UniqueJsonObject(serde_json::Map<String, Value>);

impl<'de> Deserialize<'de> for UniqueJsonObject {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct UniqueObjectVisitor;

        impl<'de> Visitor<'de> for UniqueObjectVisitor {
            type Value = UniqueJsonObject;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a JSON object with unique keys")
            }

            fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
            where
                A: MapAccess<'de>,
            {
                let mut object = serde_json::Map::new();
                while let Some((key, value)) = map.next_entry::<String, Value>()? {
                    if object.insert(key.clone(), value).is_some() {
                        return Err(de::Error::custom(format!(
                            "duplicate protocol key {key} is not allowed"
                        )));
                    }
                }
                Ok(UniqueJsonObject(object))
            }
        }

        deserializer.deserialize_map(UniqueObjectVisitor)
    }
}

fn sns_fields(request: &SmsRequest) -> Vec<(String, String)> {
    if request.method() == Method::GET {
        request
            .uri
            .query()
            .map_or_else(Vec::new, |query| decode_form(query.as_bytes()))
    } else {
        decode_form(&request.body)
    }
}

fn validate_sns_fields(fields: &[(String, String)]) -> Result<(), String> {
    const SINGULAR: &[&str] = &[
        "Action",
        "Version",
        "PhoneNumber",
        "Message",
        "MessageStructure",
        "Subject",
        "TopicArn",
        "TargetArn",
        "MessageDeduplicationId",
        "MessageGroupId",
    ];
    let mut seen = std::collections::HashSet::new();
    for (name, _) in fields {
        if SINGULAR.contains(&name.as_str()) {
            if !seen.insert(name.as_str()) {
                return Err(format!("SNS query parameter {name} must not be repeated"));
            }
        } else if !name.starts_with("MessageAttributes.") {
            return Err(format!(
                "SNS Publish query parameter {name} is not supported by this emulator"
            ));
        }
    }
    if form_value(fields, "MessageDeduplicationId").is_some()
        || form_value(fields, "MessageGroupId").is_some()
    {
        return Err(
            "FIFO topic parameters are not valid for direct PhoneNumber publishing".to_string(),
        );
    }
    if let Some(subject) = form_value(fields, "Subject") {
        if subject.is_empty() || subject.chars().count() > 100 || subject.contains(['\r', '\n']) {
            return Err("Subject must contain 1 to 100 characters without line breaks".to_string());
        }
    }
    Ok(())
}

fn sns_message_attributes(
    fields: &[(String, String)],
) -> Result<serde_json::Map<String, Value>, String> {
    let mut attributes = serde_json::Map::new();
    for (name, attribute_name) in fields {
        let Some(prefix) = name.strip_suffix(".Name") else {
            continue;
        };
        if !prefix.starts_with("MessageAttributes.") || attribute_name.is_empty() {
            return Err("SNS MessageAttributes names must not be empty".to_string());
        }
        if attributes.len() >= 10 {
            return Err("SNS SMS supports at most 10 MessageAttributes".to_string());
        }
        let data_type_key = format!("{prefix}.Value.DataType");
        let string_key = format!("{prefix}.Value.StringValue");
        let binary_key = format!("{prefix}.Value.BinaryValue");
        let data_type = form_value(fields, &data_type_key)
            .ok_or_else(|| format!("SNS MessageAttribute {attribute_name} requires DataType"))?;
        let value = match data_type.split('.').next() {
            Some("String" | "Number") => form_value(fields, &string_key)
                .map(|value| Value::String(value.to_string()))
                .ok_or_else(|| {
                    format!("SNS MessageAttribute {attribute_name} requires StringValue")
                })?,
            Some("Binary") => form_value(fields, &binary_key)
                .map(|value| Value::String(value.to_string()))
                .ok_or_else(|| {
                    format!("SNS MessageAttribute {attribute_name} requires BinaryValue")
                })?,
            _ => {
                return Err(format!(
                    "SNS MessageAttribute {attribute_name} has an invalid DataType"
                ))
            }
        };
        if attributes.insert(attribute_name.clone(), value).is_some() {
            return Err(format!(
                "SNS MessageAttribute {attribute_name} is duplicated"
            ));
        }
    }
    let recognized_parts = fields
        .iter()
        .filter(|(name, _)| name.starts_with("MessageAttributes."))
        .count();
    let expected_parts = fields
        .iter()
        .filter(|(name, _)| name.ends_with(".Name"))
        .count()
        * 3;
    if recognized_parts != expected_parts {
        return Err("SNS MessageAttributes contain incomplete or unsupported members".to_string());
    }
    Ok(attributes)
}

fn valid_sns_sender_id(value: &str) -> bool {
    (1..=11).contains(&value.len())
        && value.bytes().all(|byte| byte.is_ascii_alphanumeric())
        && value.bytes().any(|byte| byte.is_ascii_alphabetic())
}

fn valid_aws_destination(value: &str) -> bool {
    let digits = value.strip_prefix('+').unwrap_or(value);
    (2..=19).contains(&digits.len())
        && digits.as_bytes().first().is_some_and(u8::is_ascii_digit)
        && digits.as_bytes().first() != Some(&b'0')
        && digits.bytes().all(|byte| byte.is_ascii_digit())
}

fn valid_aws_origination(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 256
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b':' | b'/' | b'+' | b'-')
        })
}

fn validate_aws_sms_fields(
    payload: &serde_json::Map<String, Value>,
    is_media: bool,
) -> Result<(), String> {
    const COMMON: &[&str] = &[
        "ConfigurationSetName",
        "Context",
        "DestinationPhoneNumber",
        "DryRun",
        "MaxPrice",
        "MessageBody",
        "MessageFeedbackEnabled",
        "OriginationIdentity",
        "ProtectConfigurationId",
        "TimeToLive",
    ];
    const TEXT_ONLY: &[&str] = &["DestinationCountryParameters", "Keyword", "MessageType"];
    let unsupported = payload.keys().find(|name| {
        !COMMON.contains(&name.as_str())
            && if is_media {
                name.as_str() != "MediaUrls"
            } else {
                !TEXT_ONLY.contains(&name.as_str())
            }
    });
    if let Some(name) = unsupported {
        return Err(format!(
            "Field {name} is not supported by this AWS SMS Voice V2 operation"
        ));
    }

    validate_optional_string(payload, "ConfigurationSetName", 1, 256, |byte| {
        byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b':' | b'/' | b'-')
    })?;
    validate_optional_string(payload, "ProtectConfigurationId", 1, 256, |byte| {
        byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b':' | b'/' | b'-')
    })?;
    if let Some(value) = payload.get("OriginationIdentity") {
        if !value.as_str().is_some_and(valid_aws_origination) {
            return Err("OriginationIdentity is invalid".to_string());
        }
    }
    if let Some(value) = payload.get("MessageBody") {
        let Some(value) = value.as_str() else {
            return Err("MessageBody must be a string".to_string());
        };
        if value.is_empty() || value.trim().is_empty() || value.chars().count() > 1_600 {
            return Err("MessageBody must contain 1 to 1600 non-blank characters".to_string());
        }
    }
    for name in ["DryRun", "MessageFeedbackEnabled"] {
        if payload.get(name).is_some_and(|value| !value.is_boolean()) {
            return Err(format!("{name} must be a boolean"));
        }
    }
    if let Some(value) = payload.get("TimeToLive") {
        if !value
            .as_i64()
            .is_some_and(|value| (5..=259_200).contains(&value))
        {
            return Err("TimeToLive must be an integer between 5 and 259200".to_string());
        }
    }
    if let Some(value) = payload.get("MaxPrice") {
        if !value.as_str().is_some_and(valid_max_price) {
            return Err("MaxPrice must match [0-9]{0,2}.[0-9]{1,5}".to_string());
        }
    }
    if let Some(value) = payload.get("Context") {
        validate_string_map(value, 5, 100, 800, None, false)?;
    }
    if !is_media {
        if let Some(value) = payload.get("DestinationCountryParameters") {
            validate_string_map(
                value,
                10,
                64,
                64,
                Some(&["IN_TEMPLATE_ID", "IN_ENTITY_ID"]),
                true,
            )?;
        }
        if let Some(value) = payload.get("Keyword") {
            let Some(value) = value.as_str() else {
                return Err("Keyword must be a string".to_string());
            };
            if value.is_empty()
                || value.len() > 30
                || !value.chars().all(|ch| ch == ' ' || !ch.is_whitespace())
            {
                return Err("Keyword must contain 1 to 30 characters".to_string());
            }
        }
        if payload
            .get("MessageType")
            .is_some_and(|value| !matches!(value.as_str(), Some("TRANSACTIONAL" | "PROMOTIONAL")))
        {
            return Err("MessageType must be TRANSACTIONAL or PROMOTIONAL".to_string());
        }
    }
    Ok(())
}

fn validate_optional_string(
    payload: &serde_json::Map<String, Value>,
    name: &str,
    minimum: usize,
    maximum: usize,
    allowed: impl Fn(u8) -> bool,
) -> Result<(), String> {
    let Some(value) = payload.get(name) else {
        return Ok(());
    };
    let Some(value) = value.as_str() else {
        return Err(format!("{name} must be a string"));
    };
    if !(minimum..=maximum).contains(&value.len()) || !value.bytes().all(allowed) {
        return Err(format!("{name} is invalid"));
    }
    Ok(())
}

fn validate_string_map(
    value: &Value,
    maximum_entries: usize,
    maximum_key_length: usize,
    maximum_value_length: usize,
    valid_keys: Option<&[&str]>,
    value_must_not_contain_whitespace: bool,
) -> Result<(), String> {
    let object = value
        .as_object()
        .ok_or_else(|| "AWS SMS map fields must be JSON objects".to_string())?;
    if object.len() > maximum_entries {
        return Err("AWS SMS map field has too many entries".to_string());
    }
    for (key, value) in object {
        let value = value
            .as_str()
            .ok_or_else(|| "AWS SMS map values must be strings".to_string())?;
        if key.is_empty()
            || key.len() > maximum_key_length
            || key.bytes().any(|byte| byte.is_ascii_whitespace())
            || valid_keys.is_some_and(|keys| !keys.contains(&key.as_str()))
            || value.is_empty()
            || value.len() > maximum_value_length
            || value.trim() != value
            || (value_must_not_contain_whitespace && value.chars().any(char::is_whitespace))
        {
            return Err("AWS SMS map entry is invalid".to_string());
        }
    }
    Ok(())
}

fn valid_max_price(value: &str) -> bool {
    if !(2..=8).contains(&value.len()) {
        return false;
    }
    let Some((whole, fraction)) = value.split_once('.') else {
        return false;
    };
    whole.len() <= 2
        && whole.bytes().all(|byte| byte.is_ascii_digit())
        && (1..=5).contains(&fraction.len())
        && fraction.bytes().all(|byte| byte.is_ascii_digit())
}

fn valid_s3_media_uri(value: &str) -> bool {
    if !(1..=2_048).contains(&value.len()) {
        return false;
    }
    let Some((bucket, key)) = value
        .strip_prefix("s3://")
        .and_then(|rest| rest.split_once('/'))
    else {
        return false;
    };
    (3..=63).contains(&bucket.len())
        && bucket.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'.' | b'-')
        })
        && !key.is_empty()
}
