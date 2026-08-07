use crate::api::admin::pagination::{
    contains_search, encode_next, paginate, parse_page_params, PageTokenKind,
};
use crate::body::{Body, RequestBody};
use crate::error::{Error, Result};
use crate::server::ResponseBuilder;
use crate::services::json_response;
use crate::sms::model::{is_e164, valid_sender, NewSmsMedia, NewSmsMessage};
use crate::sms::simulator::{validate_callback_url, SmsSimulator};
use crate::sms::{
    ListSmsParams, SmsChannel, SmsDeliveryState, SmsDirection, SmsMessage, SmsProvider, SmsStore,
};
use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use http_body_util::BodyExt;
use hyper::{Method, Request, Response, StatusCode};
use serde::Deserialize;
use serde_json::{json, Value};
use std::str::FromStr;
use std::sync::Arc;

const CONVERSATIONS: &str = "/admin/v1/text-conversations";
const SIMULATIONS: &str = "/admin/v1/text-simulations";
const MESSAGES: &str = "/admin/v1/text-messages";
const DESTINATIONS: &str = "/admin/v1/text-destinations";
const CALLBACKS: &str = "/admin/v1/text-callback-attempts";

#[must_use]
pub fn matches_path(path: &str) -> bool {
    [
        CONVERSATIONS,
        SIMULATIONS,
        MESSAGES,
        DESTINATIONS,
        CALLBACKS,
    ]
    .iter()
    .any(|prefix| path == *prefix || path.starts_with(&format!("{prefix}/")))
}

pub async fn handle_request(
    store: &Arc<dyn SmsStore>,
    request: Request<RequestBody>,
) -> Result<Response<Body>> {
    let method = request.method().clone();
    let path = request.uri().path().to_string();
    let query = request.uri().query().unwrap_or("").to_string();

    if let Some(rest) = path.strip_prefix(CONVERSATIONS) {
        let segments = decode_segments(rest)?;
        return match (method, segments.as_slice()) {
            (Method::GET, []) => list_conversations(store, &query),
            (Method::DELETE, [peer]) => {
                store.delete_conversation(peer)?;
                Ok(ResponseBuilder::new(StatusCode::NO_CONTENT).build())
            }
            (Method::GET, [peer, resource]) if resource == "messages" => {
                list_messages(store, peer, &query)
            }
            (Method::GET, [peer, resource, message_id]) if resource == "messages" => {
                get_message(store, peer, message_id)
            }
            (Method::DELETE, [peer, resource, message_id]) if resource == "messages" => {
                store.delete_message(peer, message_id)?;
                Ok(ResponseBuilder::new(StatusCode::NO_CONTENT).build())
            }
            (Method::GET, [peer, resource, message_id, media_resource, media_id])
                if resource == "messages" && media_resource == "media" =>
            {
                get_media(store, peer, message_id, media_id)
            }
            (method, _) => Err(Error::MethodNotAllowed(format!("{method} {path}"))),
        };
    }

    if path == format!("{SIMULATIONS}/inbound") && method == Method::POST {
        let payload = read_json::<InboundSimulationRequest>(request).await?;
        return simulate_inbound(store, payload).await;
    }

    if let Some(rest) = path.strip_prefix(MESSAGES) {
        let segments = decode_segments(rest)?;
        if let [message_id, resource] = segments.as_slice() {
            if resource == "delivery" && method == Method::POST {
                let payload = read_json::<DeliveryRequest>(request).await?;
                return transition_delivery(store, message_id, payload).await;
            }
        }
    }

    if let Some(rest) = path.strip_prefix(DESTINATIONS) {
        let segments = decode_segments(rest)?;
        if let [provider, local_number] = segments.as_slice() {
            let provider = parse_provider(provider)?;
            return match method {
                Method::GET => Ok(json_response(
                    StatusCode::OK,
                    &store.get_destination(provider, local_number)?,
                )),
                Method::PUT => {
                    let payload = read_json::<DestinationRequest>(request).await?;
                    validate_callback_url(&payload.callback_url)?;
                    Ok(json_response(
                        StatusCode::OK,
                        &store.put_destination(provider, local_number, &payload.callback_url)?,
                    ))
                }
                Method::DELETE => {
                    store.delete_destination(provider, local_number)?;
                    Ok(ResponseBuilder::new(StatusCode::NO_CONTENT).build())
                }
                _ => Err(Error::MethodNotAllowed(format!("{method} {path}"))),
            };
        }
    }

    if let Some(rest) = path.strip_prefix(CALLBACKS) {
        let segments = decode_segments(rest)?;
        if let [attempt_id, resource] = segments.as_slice() {
            if resource == "retry" && method == Method::POST {
                let simulator = SmsSimulator::new(store.clone());
                let attempt = simulator.retry(attempt_id).await?;
                return Ok(json_response(StatusCode::OK, &attempt));
            }
        }
    }

    Err(Error::MethodNotAllowed(format!("{method} {path}")))
}

fn list_conversations(store: &Arc<dyn SmsStore>, query: &str) -> Result<Response<Body>> {
    let page = parse_page_params(query, PageTokenKind::TextConversations)?;
    let conversations = store
        .list_conversations()?
        .into_iter()
        .filter(|conversation| {
            contains_search(&conversation.peer, page.search.as_deref())
                || contains_search(&conversation.last_message_body, page.search.as_deref())
        })
        .collect::<Vec<_>>();
    let (items, next) = paginate(conversations, &page);
    Ok(json_response(
        StatusCode::OK,
        &json!({
            "items": items,
            "next": encode_next(next, PageTokenKind::TextConversations),
        }),
    ))
}

fn list_messages(store: &Arc<dyn SmsStore>, peer: &str, query: &str) -> Result<Response<Body>> {
    let page = parse_page_params(query, PageTokenKind::TextMessages)?;
    let messages = store
        .list_messages(peer, ListSmsParams::default())?
        .messages
        .into_iter()
        .filter(|message| {
            contains_search(&message.body, page.search.as_deref())
                || contains_search(&message.from, page.search.as_deref())
                || contains_search(&message.to, page.search.as_deref())
        })
        .collect::<Vec<_>>();
    let (items, next) = paginate(messages, &page);
    Ok(json_response(
        StatusCode::OK,
        &json!({
            "items": items,
            "next": encode_next(next, PageTokenKind::TextMessages),
        }),
    ))
}

fn get_message(store: &Arc<dyn SmsStore>, peer: &str, message_id: &str) -> Result<Response<Body>> {
    let message = checked_message(store, peer, message_id)?;
    let attempts = store.list_callbacks(message_id)?;
    Ok(json_response(
        StatusCode::OK,
        &message_detail(message, attempts),
    ))
}

fn get_media(
    store: &Arc<dyn SmsStore>,
    peer: &str,
    message_id: &str,
    media_id: &str,
) -> Result<Response<Body>> {
    checked_message(store, peer, message_id)?;
    let (media, content) = store.read_media(message_id, media_id)?;
    Ok(ResponseBuilder::new(StatusCode::OK)
        .content_type(&media.content_type)
        .header(
            "content-disposition",
            &format!(
                "attachment; filename=\"{}\"",
                media.filename.replace('"', "")
            ),
        )
        .body(content)
        .build())
}

async fn simulate_inbound(
    store: &Arc<dyn SmsStore>,
    payload: InboundSimulationRequest,
) -> Result<Response<Body>> {
    let provider = parse_provider(&payload.provider)?;
    if !valid_sender(&payload.from) || !is_e164(&payload.to) {
        return Err(Error::InvalidRequest(
            "to must be E.164 and from must be a valid sender identity".to_string(),
        ));
    }
    if !payload.media.is_empty() && provider != SmsProvider::Twilio {
        return Err(Error::InvalidRequest(
            "inbound media is supported only for Twilio".to_string(),
        ));
    }
    let media = payload
        .media
        .into_iter()
        .map(|media| {
            let content = BASE64.decode(&media.content_base64).map_err(|_| {
                Error::InvalidRequest("media content_base64 is invalid".to_string())
            })?;
            Ok(NewSmsMedia {
                filename: media.filename,
                content_type: media.content_type,
                content: Some(content),
                external_url: None,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    let channel = if media.is_empty() {
        SmsChannel::Sms
    } else {
        SmsChannel::Mms
    };
    let simulator = SmsSimulator::new(store.clone());
    let message = simulator
        .inject_inbound(NewSmsMessage {
            batch_id: None,
            provider,
            provider_message_id: None,
            direction: SmsDirection::Inbound,
            channel,
            from: payload.from,
            to: payload.to,
            body: payload.body,
            media,
            metadata: payload.metadata.unwrap_or_default(),
        })
        .await?;
    let attempts = store.list_callbacks(&message.message_id)?;
    Ok(json_response(
        StatusCode::CREATED,
        &message_detail(message, attempts),
    ))
}

async fn transition_delivery(
    store: &Arc<dyn SmsStore>,
    message_id: &str,
    payload: DeliveryRequest,
) -> Result<Response<Body>> {
    let state = match payload.state.as_str() {
        "delivered" => SmsDeliveryState::Delivered,
        "failed" => SmsDeliveryState::Failed,
        _ => {
            return Err(Error::InvalidRequest(
                "state must be delivered or failed".to_string(),
            ))
        }
    };
    let simulator = SmsSimulator::new(store.clone());
    let message = simulator.transition_delivery(message_id, state).await?;
    let attempts = store.list_callbacks(message_id)?;
    Ok(json_response(
        StatusCode::OK,
        &message_detail(message, attempts),
    ))
}

fn checked_message(store: &Arc<dyn SmsStore>, peer: &str, message_id: &str) -> Result<SmsMessage> {
    let message = store.get_message(message_id)?;
    if message.peer != peer {
        return Err(Error::MessageNotFound);
    }
    Ok(message)
}

fn message_detail(message: SmsMessage, attempts: Vec<crate::sms::CallbackAttempt>) -> Value {
    let mut value = serde_json::to_value(message).unwrap_or(Value::Null);
    if let Value::Object(object) = &mut value {
        object.insert(
            "callback_attempts".to_string(),
            serde_json::to_value(attempts).unwrap_or(Value::Array(Vec::new())),
        );
    }
    value
}

fn parse_provider(value: &str) -> Result<SmsProvider> {
    SmsProvider::from_str(value).map_err(Error::InvalidRequest)
}

fn decode_segments(rest: &str) -> Result<Vec<String>> {
    rest.trim_start_matches('/')
        .split('/')
        .filter(|segment| !segment.is_empty())
        .map(|segment| {
            urlencoding::decode(segment)
                .map(std::borrow::Cow::into_owned)
                .map_err(|_| Error::InvalidRequest("invalid URL path encoding".to_string()))
        })
        .collect()
}

async fn read_json<T: for<'de> Deserialize<'de>>(request: Request<RequestBody>) -> Result<T> {
    let bytes = request
        .into_body()
        .collect()
        .await
        .map_err(|err| Error::InvalidRequest(format!("failed to read request body: {err}")))?
        .to_bytes();
    serde_json::from_slice(&bytes)
        .map_err(|err| Error::InvalidRequest(format!("invalid JSON request body: {err}")))
}

#[derive(Deserialize)]
struct InboundSimulationRequest {
    provider: String,
    from: String,
    to: String,
    #[serde(default)]
    body: String,
    #[serde(default)]
    media: Vec<InboundMediaRequest>,
    #[serde(default)]
    metadata: Option<std::collections::HashMap<String, Value>>,
}

#[derive(Deserialize)]
struct InboundMediaRequest {
    filename: String,
    content_type: String,
    content_base64: String,
}

#[derive(Deserialize)]
struct DeliveryRequest {
    state: String,
}

#[derive(Deserialize)]
struct DestinationRequest {
    callback_url: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_decode_plus_and_percent_route_values_exactly_once() {
        assert_eq!(
            decode_segments("/%2B1555%2525/messages").unwrap(),
            vec!["+1555%25".to_string(), "messages".to_string()]
        );
    }

    #[test]
    fn should_match_only_text_admin_prefixes() {
        assert!(matches_path("/admin/v1/text-conversations"));
        assert!(matches_path("/admin/v1/text-destinations/twilio/%2B1555"));
        assert!(!matches_path("/admin/v1/mailboxes"));
    }
}
