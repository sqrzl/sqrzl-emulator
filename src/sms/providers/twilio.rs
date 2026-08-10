use super::{decode_form, form_value, json_error, SmsAdapter};
use crate::auth::AuthConfig;
use crate::body::Body;
use crate::server::{RequestExt as SmsRequest, ResponseBuilder};
use crate::sms::model::{is_e164, valid_sender, NewSmsMedia, NewSmsMessage};
use crate::sms::{SmsChannel, SmsDirection, SmsProvider, SmsStore};
use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use http::{HeaderMap, Method, StatusCode, Uri};
use hyper::Response;
use serde_json::Value;
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

pub struct TwilioSmsAdapter;

impl TwilioSmsAdapter {
    fn message_metadata(
        fields: &[(String, String)],
        messaging_service_sid: Option<&str>,
    ) -> HashMap<String, Value> {
        let mut metadata = HashMap::new();
        if let Some(callback) = form_value(fields, "StatusCallback") {
            metadata.insert(
                "status_callback".to_string(),
                Value::String(callback.to_string()),
            );
        }
        if let Some(sid) = messaging_service_sid {
            metadata.insert(
                "messaging_service_sid".to_string(),
                Value::String(sid.to_string()),
            );
        }
        metadata
    }

    fn message_media(media_urls: &[String]) -> Vec<NewSmsMedia> {
        media_urls
            .iter()
            .enumerate()
            .map(|(index, url)| NewSmsMedia {
                filename: format!("media-{}", index + 1),
                content_type: "application/octet-stream".to_string(),
                content: None,
                external_url: Some(url.clone()),
            })
            .collect()
    }

    fn send_response(
        message: &crate::sms::SmsMessage,
        account_sid: &str,
        body: &str,
        from: Option<&str>,
        messaging_service_sid: Option<&str>,
        media_count: usize,
        to: &str,
    ) -> Response<Body> {
        let now = message.created_at.to_rfc2822();
        ResponseBuilder::new(StatusCode::CREATED)
            .content_type("application/json; charset=utf-8")
            .body(
                serde_json::json!({
                    "sid": message.provider_message_id,
                    "account_sid": account_sid,
                    "api_version": "2010-04-01",
                    "body": body,
                    "date_created": now,
                    "date_sent": null,
                    "date_updated": now,
                    "direction": "outbound-api",
                    "error_code": null,
                    "error_message": null,
                    "from": from,
                    "messaging_service_sid": messaging_service_sid,
                    "num_media": media_count.to_string(),
                    "num_segments": if messaging_service_sid.is_some() && from.is_none() { "0" } else { "1" },
                    "price": null,
                    "price_unit": null,
                    "status": if messaging_service_sid.is_some() && from.is_none() { "accepted" } else { "queued" },
                    "subresource_uris": {"media": format!("/2010-04-01/Accounts/{account_sid}/Messages/{}/Media.json", message.provider_message_id)},
                    "to": to,
                    "uri": format!("/2010-04-01/Accounts/{account_sid}/Messages/{}.json", message.provider_message_id)
                })
                .to_string()
                .into_bytes(),
            )
            .build()
    }

    fn path_parts(path: &str) -> Option<(&str, &str)> {
        let parts = path.trim_matches('/').split('/').collect::<Vec<_>>();
        match parts.as_slice() {
            ["2010-04-01", "Accounts", account, "Messages.json"] => Some((account, "messages")),
            ["2010-04-01", "Accounts", account, "Messages", _, "Media", _] => {
                Some((account, "media"))
            }
            _ => None,
        }
    }

    fn credentials() -> Option<(String, String)> {
        let account = std::env::var("SQRZL_TWILIO_ACCOUNT_SID").ok()?;
        let token = std::env::var("SQRZL_TWILIO_AUTH_TOKEN").ok()?;
        Some((account, token))
    }

    fn authorized(request: &SmsRequest, path_account: &str) -> bool {
        let Some((account, token)) = Self::credentials() else {
            return true;
        };
        if account != path_account {
            return false;
        }
        let Some(value) = request
            .header("authorization")
            .and_then(|value| value.strip_prefix("Basic "))
        else {
            return false;
        };
        let Ok(decoded) = BASE64.decode(value) else {
            return false;
        };
        decoded == format!("{account}:{token}").as_bytes()
    }

    fn error(status: StatusCode, code: u16, message: &str) -> Response<Body> {
        ResponseBuilder::new(status)
            .content_type("application/json; charset=utf-8")
            .body(
                serde_json::json!({
                    "code": code,
                    "message": message,
                    "more_info": "https://www.twilio.com/docs/errors",
                    "status": status.as_u16(),
                })
                .to_string()
                .into_bytes(),
            )
            .build()
    }

    // Twilio's form validation must complete before the one store mutation.
    #[allow(clippy::too_many_lines)]
    fn send(store: &dyn SmsStore, request: &SmsRequest, account_sid: &str) -> Response<Body> {
        let fields = decode_form(&request.body);
        if !request
            .header("content-type")
            .and_then(|value| value.split(';').next())
            .is_some_and(|value| {
                value
                    .trim()
                    .eq_ignore_ascii_case("application/x-www-form-urlencoded")
            })
        {
            return Self::error(
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                21_606,
                "Message creation requires application/x-www-form-urlencoded content.",
            );
        }
        if let Err(message) = validate_send_fields(&fields) {
            return Self::error(StatusCode::BAD_REQUEST, 21_606, &message);
        }
        let Some(to) = form_value(&fields, "To") else {
            return Self::error(
                StatusCode::BAD_REQUEST,
                21_601,
                "A 'To' phone number is required.",
            );
        };
        if !is_e164(to) {
            return Self::error(
                StatusCode::BAD_REQUEST,
                21_214,
                "'To' must be an E.164 phone number.",
            );
        }
        let from = form_value(&fields, "From");
        let messaging_service_sid = form_value(&fields, "MessagingServiceSid");
        if from.is_none() && messaging_service_sid.is_none() {
            return Self::error(
                StatusCode::BAD_REQUEST,
                21_606,
                "Either 'From' or 'MessagingServiceSid' is required.",
            );
        }
        if from.is_some_and(|value| !valid_sender(value)) {
            return Self::error(StatusCode::BAD_REQUEST, 21_612, "Invalid 'From' sender.");
        }
        if messaging_service_sid.is_some_and(|value| !valid_twilio_sid(value, "MG")) {
            return Self::error(
                StatusCode::BAD_REQUEST,
                21_606,
                "MessagingServiceSid must be a valid MG SID.",
            );
        }
        let media_urls = fields
            .iter()
            .filter(|(name, _)| name == "MediaUrl")
            .map(|(_, value)| value.clone())
            .collect::<Vec<_>>();
        let body = form_value(&fields, "Body").filter(|value| !value.is_empty());
        if body.is_none() && media_urls.is_empty() {
            return Self::error(
                StatusCode::BAD_REQUEST,
                21_602,
                "Either 'Body' or 'MediaUrl' is required.",
            );
        }
        if body.is_some_and(|value| value.chars().count() > 1_600) {
            return Self::error(
                StatusCode::BAD_REQUEST,
                21_617,
                "The concatenated message body exceeds the 1600 character limit.",
            );
        }
        if media_urls.len() > 10 {
            return Self::error(
                StatusCode::BAD_REQUEST,
                21_651,
                "At most 10 MediaUrl values are supported.",
            );
        }
        if media_urls.iter().any(|value| !valid_http_uri(value)) {
            return Self::error(
                StatusCode::BAD_REQUEST,
                21_606,
                "Every MediaUrl must be an absolute HTTP or HTTPS URI.",
            );
        }
        if let Some(callback) = form_value(&fields, "StatusCallback") {
            if let Err(error) = crate::sms::simulator::validate_callback_url(callback) {
                return Self::error(StatusCode::BAD_REQUEST, 21_606, &error.to_string());
            }
        }

        let metadata = Self::message_metadata(&fields, messaging_service_sid);
        let body = body.unwrap_or("").to_string();
        let message = match store.store_message(NewSmsMessage {
            batch_id: None,
            provider: SmsProvider::Twilio,
            provider_message_id: None,
            direction: SmsDirection::Outbound,
            channel: if media_urls.is_empty() {
                SmsChannel::Sms
            } else {
                SmsChannel::Mms
            },
            from: from
                .or(messaging_service_sid)
                .unwrap_or_default()
                .to_string(),
            to: to.to_string(),
            body: body.clone(),
            media: Self::message_media(&media_urls),
            metadata,
        }) {
            Ok(message) => message,
            Err(error) => {
                return json_error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "InternalError",
                    &error.to_string(),
                )
            }
        };
        Self::send_response(
            &message,
            account_sid,
            &body,
            from,
            messaging_service_sid,
            media_urls.len(),
            to,
        )
    }

    fn media(store: &dyn SmsStore, request: &SmsRequest) -> Response<Body> {
        let parts = request
            .path()
            .trim_matches('/')
            .split('/')
            .collect::<Vec<_>>();
        let provider_id = parts[4];
        let media_id = parts[6];
        let Ok(message) = store.get_message_by_provider_id(provider_id) else {
            return Self::error(
                StatusCode::NOT_FOUND,
                20_404,
                "The requested media was not found.",
            );
        };
        let Ok((media, bytes)) = store.read_media(&message.message_id, media_id) else {
            return Self::error(
                StatusCode::NOT_FOUND,
                20_404,
                "The requested media was not found.",
            );
        };
        ResponseBuilder::new(StatusCode::OK)
            .content_type(&media.content_type)
            .header(
                "content-disposition",
                &format!("inline; filename=\"{}\"", media.filename.replace('"', "")),
            )
            .body(bytes)
            .build()
    }
}

fn valid_twilio_sid(value: &str, prefix: &str) -> bool {
    value.len() == 34
        && value.starts_with(prefix)
        && value[prefix.len()..]
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit())
}

fn valid_http_uri(value: &str) -> bool {
    value.parse::<Uri>().is_ok_and(|uri| {
        matches!(uri.scheme_str(), Some("http" | "https")) && uri.authority().is_some()
    })
}

impl SmsAdapter for TwilioSmsAdapter {
    fn name(&self) -> &'static str {
        "twilio"
    }

    fn matches(&self, request: &SmsRequest) -> bool {
        Self::path_parts(request.path()).is_some()
    }

    fn matches_request_head(&self, method: &Method, uri: &Uri, _headers: &HeaderMap) -> bool {
        method == Method::POST
            && Self::path_parts(uri.path()).is_some_and(|(_, kind)| kind == "messages")
    }

    fn payload_too_large(&self, max_request_bytes: usize) -> Response<Body> {
        Self::error(
            StatusCode::PAYLOAD_TOO_LARGE,
            21_617,
            &format!("The request body exceeds the {max_request_bytes}-byte emulator limit."),
        )
    }

    fn incomplete_body(&self) -> Response<Body> {
        Self::error(
            StatusCode::BAD_REQUEST,
            21_606,
            "The request body ended before it was complete.",
        )
    }

    fn handle<'a>(
        &'a self,
        store: Arc<dyn SmsStore>,
        _auth: Arc<AuthConfig>,
        request: SmsRequest,
    ) -> Pin<Box<dyn Future<Output = Result<Response<Body>, String>> + Send + 'a>> {
        Box::pin(async move {
            let Some((account, kind)) = Self::path_parts(request.path()) else {
                return Ok(Self::error(
                    StatusCode::NOT_FOUND,
                    20_404,
                    "Resource not found.",
                ));
            };
            if !valid_twilio_sid(account, "AC") {
                return Ok(Self::error(
                    StatusCode::BAD_REQUEST,
                    20_001,
                    "AccountSid must be a valid AC SID.",
                ));
            }
            if !Self::authorized(&request, account) {
                return Ok(Self::error(
                    StatusCode::UNAUTHORIZED,
                    20_003,
                    "Authenticate.",
                ));
            }
            match (request.method(), kind) {
                (&Method::POST, "messages") => Ok(Self::send(store.as_ref(), &request, account)),
                (&Method::GET, "media") => Ok(Self::media(store.as_ref(), &request)),
                _ => Ok(Self::error(
                    StatusCode::METHOD_NOT_ALLOWED,
                    20_405,
                    "Method not allowed.",
                )),
            }
        })
    }
}

fn validate_send_fields(fields: &[(String, String)]) -> Result<(), String> {
    const SUPPORTED: &[&str] = &[
        "To",
        "From",
        "MessagingServiceSid",
        "Body",
        "MediaUrl",
        "StatusCallback",
    ];
    let mut singular = HashMap::<&str, usize>::new();
    for (name, _) in fields {
        if !SUPPORTED.contains(&name.as_str()) {
            return Err(format!(
                "Parameter {name} is not supported by this Twilio SMS/MMS emulator"
            ));
        }
        if name != "MediaUrl" {
            let count = singular.entry(name.as_str()).or_default();
            *count += 1;
            if *count > 1 {
                return Err(format!("Parameter {name} must not be repeated"));
            }
        }
    }
    Ok(())
}
