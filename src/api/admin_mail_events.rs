//! Admin routes for simulated SES event publishing.
//!
//! - `PUT|GET|DELETE /admin/v1/ses-event-destinations/{configurationSet}`
//! - `POST /admin/v1/ses-events` simulates one event for a captured message.
//! - `GET /admin/v1/ses-events[?message_id=...]` lists simulated records.

use crate::body::Body;
use crate::error::{Error, Result};
use crate::mail::ses_events::{
    validate_destination_url, SesEventDestination, SesEventRequest, SesEventSimulator, SesEventType,
};
use crate::mail::MailStore;
use crate::server::ResponseBuilder;
use crate::services::json_response;
use http_body_util::BodyExt;
use hyper::{Method, Request, Response, StatusCode};
use serde::Deserialize;
use serde_json::json;
use std::sync::Arc;

pub const SES_EVENT_DESTINATIONS: &str = "/admin/v1/ses-event-destinations";
pub const SES_EVENTS: &str = "/admin/v1/ses-events";

#[must_use]
pub fn matches_path(path: &str) -> bool {
    [SES_EVENT_DESTINATIONS, SES_EVENTS]
        .iter()
        .any(|prefix| path == *prefix || path.starts_with(&format!("{prefix}/")))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DestinationRequest {
    callback_url: String,
    #[serde(default)]
    event_types: Vec<SesEventType>,
}

/// Routes an authenticated SES event admin request.
///
/// # Errors
/// Returns an error for invalid routes, payloads, or persistence failures.
pub async fn handle_request<B>(
    mail: &Arc<dyn MailStore>,
    request: Request<B>,
) -> Result<Response<Body>>
where
    B: hyper::body::Body<Data = bytes::Bytes> + Send + 'static,
    B::Error: std::fmt::Display,
{
    let method = request.method().clone();
    let path = request.uri().path().to_string();

    if let Some(rest) = path.strip_prefix(SES_EVENT_DESTINATIONS) {
        let name = rest.trim_start_matches('/');
        let name = urlencoding::decode(name)
            .map_err(|_| Error::InvalidRequest("invalid URL path encoding".to_string()))?
            .into_owned();
        if name.is_empty() || name.contains('/') {
            return Err(Error::MethodNotAllowed(format!("{method} {path}")));
        }
        return match method {
            Method::GET => Ok(json_response(
                StatusCode::OK,
                &mail.get_ses_event_destination(&name)?,
            )),
            Method::PUT => {
                let payload = read_json::<DestinationRequest, _>(request).await?;
                validate_destination_url(&payload.callback_url)?;
                let now = chrono::Utc::now();
                let destination = mail.put_ses_event_destination(&SesEventDestination {
                    configuration_set: name,
                    callback_url: payload.callback_url,
                    event_types: payload.event_types,
                    created_at: now,
                    updated_at: now,
                })?;
                Ok(json_response(StatusCode::OK, &destination))
            }
            Method::DELETE => {
                mail.delete_ses_event_destination(&name)?;
                Ok(ResponseBuilder::new(StatusCode::NO_CONTENT).build())
            }
            _ => Err(Error::MethodNotAllowed(format!("{method} {path}"))),
        };
    }

    if path == SES_EVENTS {
        return match method {
            Method::POST => {
                let payload = read_json::<SesEventRequest, _>(request).await?;
                let event = SesEventSimulator::new(mail.clone())
                    .simulate(payload)
                    .await?;
                Ok(json_response(StatusCode::CREATED, &event))
            }
            Method::GET => {
                let message_id = request.uri().query().and_then(|query| {
                    query.split('&').find_map(|pair| {
                        let (name, value) = pair.split_once('=').unwrap_or((pair, ""));
                        if name != "message_id" {
                            return None;
                        }
                        urlencoding::decode(&value.replace('+', " "))
                            .ok()
                            .map(std::borrow::Cow::into_owned)
                    })
                });
                let items = mail.list_ses_events(message_id.as_deref())?;
                Ok(json_response(StatusCode::OK, &json!({ "items": items })))
            }
            _ => Err(Error::MethodNotAllowed(format!("{method} {path}"))),
        };
    }

    Err(Error::MethodNotAllowed(format!("{method} {path}")))
}

async fn read_json<T, B>(request: Request<B>) -> Result<T>
where
    T: for<'de> Deserialize<'de>,
    B: hyper::body::Body<Data = bytes::Bytes> + Send + 'static,
    B::Error: std::fmt::Display,
{
    let bytes = request
        .into_body()
        .collect()
        .await
        .map_err(|err| Error::InvalidRequest(format!("failed to read request body: {err}")))?
        .to_bytes();
    serde_json::from_slice(&bytes)
        .map_err(|err| Error::InvalidRequest(format!("invalid JSON request body: {err}")))
}
