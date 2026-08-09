use crate::error::{Error, Result};
use crate::sms::model::{
    is_e164, valid_sender, CallbackAttempt, CallbackAttemptState, CallbackKind, NewSmsMessage,
    SmsDeliveryState, SmsDirection, SmsMessage, SmsProvider,
};
use crate::sms::SmsStore;
use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use chrono::Utc;
use hmac::{Hmac, KeyInit, Mac};
use http::Uri;
use sha1::Sha1;
use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const DEFAULT_CALLBACK_TIMEOUT_MS: u64 = 5_000;
const MAX_RECORDED_RESPONSE_BYTES: usize = 64 * 1024;

pub struct SmsSimulator {
    store: Arc<dyn SmsStore>,
}

impl SmsSimulator {
    #[must_use]
    pub fn new(store: Arc<dyn SmsStore>) -> Self {
        Self { store }
    }

    /// Stores an inbound message and, when configured, immediately performs one callback.
    /// Callback failure deliberately does not roll back the canonical message.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid inbound data or persistence failures.
    pub async fn inject_inbound(&self, message: NewSmsMessage) -> Result<SmsMessage> {
        if message.direction != SmsDirection::Inbound {
            return Err(Error::InvalidRequest(
                "inbound simulation requires direction=inbound".to_string(),
            ));
        }
        if !valid_sender(&message.from) || !is_e164(&message.to) {
            return Err(Error::InvalidRequest(
                "inbound to must be E.164 and from must be a valid sender identity".to_string(),
            ));
        }
        if !message.media.is_empty() && message.provider != SmsProvider::Twilio {
            return Err(Error::InvalidRequest(
                "inbound media is supported only for Twilio".to_string(),
            ));
        }

        let stored = self.store.store_message(message)?;
        if let Ok(destination) = self.store.get_destination(stored.provider, &stored.to) {
            let attempt = self
                .attempt_callback(
                    &stored,
                    CallbackKind::Inbound,
                    &destination.callback_url,
                    None,
                )
                .await;
            self.store.record_callback(attempt)?;
        }
        Ok(stored)
    }

    /// Applies a terminal outbound delivery state and sends its configured callback.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid transitions, missing messages, or persistence failures.
    pub async fn transition_delivery(
        &self,
        message_id: &str,
        state: SmsDeliveryState,
    ) -> Result<SmsMessage> {
        let before = self.store.get_message(message_id)?;
        if before.direction != SmsDirection::Outbound {
            return Err(Error::InvalidRequest(
                "delivery transitions apply only to outbound messages".to_string(),
            ));
        }
        let transitioned = self.store.transition_delivery(message_id, state)?;
        if before.delivery_state != SmsDeliveryState::Accepted {
            return Ok(transitioned);
        }

        let callback_url = transitioned
            .metadata
            .get("status_callback")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string)
            .or_else(|| {
                self.store
                    .get_destination(transitioned.provider, &transitioned.from)
                    .ok()
                    .map(|destination| destination.callback_url)
            });
        if let Some(url) = callback_url {
            let attempt = self
                .attempt_callback(&transitioned, CallbackKind::Delivery, &url, None)
                .await;
            self.store.record_callback(attempt)?;
        }
        Ok(transitioned)
    }

    /// Creates and records a new callback attempt linked to a previous attempt.
    ///
    /// # Errors
    ///
    /// Returns an error when the prior attempt or message is missing or persistence fails.
    pub async fn retry(&self, attempt_id: &str) -> Result<CallbackAttempt> {
        let prior = self.store.get_callback(attempt_id)?;
        let message = self.store.get_message(&prior.message_id)?;
        let attempt = self
            .attempt_callback(&message, prior.kind, &prior.url, Some(prior.attempt_id))
            .await;
        self.store.record_callback(attempt.clone())?;
        Ok(attempt)
    }

    async fn attempt_callback(
        &self,
        message: &SmsMessage,
        kind: CallbackKind,
        url: &str,
        retry_of: Option<String>,
    ) -> CallbackAttempt {
        let (body, mut headers) = callback_payload(message, kind, url);
        let attempted_at = Utc::now();
        let attempt_id = format!("attempt-{}", uuid::Uuid::new_v4());
        let response = send_http_callback(url, &headers, body.as_bytes()).await;
        let (response_status, response_body, error, state) = match response {
            Ok((status, response_body)) if (200..300).contains(&status) => (
                Some(status),
                Some(response_body),
                None,
                CallbackAttemptState::Succeeded,
            ),
            Ok((status, response_body)) => (
                Some(status),
                Some(response_body),
                Some(format!("callback returned HTTP {status}")),
                CallbackAttemptState::Failed,
            ),
            Err(error) => (None, None, Some(error), CallbackAttemptState::Failed),
        };
        headers.insert("content-length".to_string(), body.len().to_string());

        CallbackAttempt {
            attempt_id,
            message_id: message.message_id.clone(),
            kind,
            provider: message.provider,
            url: url.to_string(),
            request_headers: headers,
            request_body: body,
            response_status,
            response_body,
            error,
            state,
            attempted_at,
            retry_of,
        }
    }
}

fn callback_payload(
    message: &SmsMessage,
    kind: CallbackKind,
    url: &str,
) -> (String, HashMap<String, String>) {
    match message.provider {
        SmsProvider::Twilio => twilio_callback_payload(message, kind, url),
        SmsProvider::Sns | SmsProvider::AwsSmsVoiceV2 => aws_callback_payload(message, kind),
        SmsProvider::Acs => acs_callback_payload(message, kind),
    }
}

fn twilio_callback_payload(
    message: &SmsMessage,
    kind: CallbackKind,
    url: &str,
) -> (String, HashMap<String, String>) {
    let account_sid = std::env::var("SQRZL_TWILIO_ACCOUNT_SID")
        .unwrap_or_else(|_| "AC00000000000000000000000000000000".to_string());
    let mut fields = vec![
        ("AccountSid".to_string(), account_sid.clone()),
        (
            "MessageSid".to_string(),
            message.provider_message_id.clone(),
        ),
        ("SmsSid".to_string(), message.provider_message_id.clone()),
        ("To".to_string(), message.to.clone()),
        ("From".to_string(), message.from.clone()),
    ];
    match kind {
        CallbackKind::Inbound => {
            fields.push(("Body".to_string(), message.body.clone()));
            fields.push(("SmsStatus".to_string(), "received".to_string()));
            fields.push(("NumMedia".to_string(), message.media.len().to_string()));
            let api_port = std::env::var("SQRZL_API_PORT")
                .ok()
                .and_then(|value| value.parse::<u16>().ok())
                .unwrap_or(9000);
            for (index, media) in message.media.iter().enumerate() {
                fields.push((
                    format!("MediaUrl{index}"),
                    format!(
                        "http://localhost:{api_port}/2010-04-01/Accounts/{account_sid}/Messages/{}/Media/{}",
                        message.provider_message_id, media.media_id
                    ),
                ));
                fields.push((
                    format!("MediaContentType{index}"),
                    media.content_type.clone(),
                ));
            }
        }
        CallbackKind::Delivery => {
            let status = match message.delivery_state {
                SmsDeliveryState::Delivered => "delivered",
                SmsDeliveryState::Failed => "failed",
                SmsDeliveryState::Accepted => "accepted",
            };
            fields.push(("MessageStatus".to_string(), status.to_string()));
            fields.push(("SmsStatus".to_string(), status.to_string()));
        }
    }
    fields.sort_by(|left, right| left.0.cmp(&right.0));
    let body = form_encode(&fields);
    let mut headers = HashMap::from([(
        "content-type".to_string(),
        "application/x-www-form-urlencoded".to_string(),
    )]);
    if let Ok(token) = std::env::var("SQRZL_TWILIO_AUTH_TOKEN") {
        let signature = twilio_signature(url, &fields, &token);
        headers.insert("x-twilio-signature".to_string(), signature);
    }
    (body, headers)
}

fn aws_callback_payload(
    message: &SmsMessage,
    kind: CallbackKind,
) -> (String, HashMap<String, String>) {
    let event = match kind {
        CallbackKind::Inbound => serde_json::json!({
            "originationNumber": message.from,
            "destinationNumber": message.to,
            "messageKeyword": message.body.split_whitespace().next().unwrap_or(""),
            "messageBody": message.body,
            "inboundMessageId": message.provider_message_id,
            "previousPublishedMessageId": null,
        }),
        CallbackKind::Delivery => serde_json::json!({
            "eventType": match message.delivery_state {
                SmsDeliveryState::Delivered => "TEXT_DELIVERED",
                SmsDeliveryState::Failed => "TEXT_FAILURE",
                SmsDeliveryState::Accepted => "TEXT_SENT",
            },
            "messageId": message.provider_message_id,
            "destinationPhoneNumber": message.to,
            "originationIdentity": message.from,
        }),
    };
    let envelope = serde_json::json!({
        "Type": "Notification",
        "MessageId": uuid::Uuid::new_v4().to_string(),
        "TopicArn": "arn:aws:sns:us-east-1:000000000000:sqrzl-text-events",
        "Message": event.to_string(),
        "Timestamp": Utc::now().to_rfc3339(),
        "SignatureVersion": "1",
        "Signature": "UNSIGNED-EMULATOR-NOTIFICATION",
        "SigningCertURL": "http://localhost/sqrzl-emulator-no-production-certificate",
        "UnsubscribeURL": "http://localhost/sqrzl-emulator-no-subscription-management"
    });
    (
        envelope.to_string(),
        HashMap::from([
            (
                "content-type".to_string(),
                "text/plain; charset=UTF-8".to_string(),
            ),
            (
                "x-amz-sns-message-type".to_string(),
                "Notification".to_string(),
            ),
        ]),
    )
}

fn acs_callback_payload(
    message: &SmsMessage,
    kind: CallbackKind,
) -> (String, HashMap<String, String>) {
    let (event_type, data) = match kind {
        CallbackKind::Inbound => (
            "Microsoft.Communication.SMSReceived",
            serde_json::json!({
                "messageId": message.provider_message_id,
                "from": message.from,
                "to": message.to,
                "message": message.body,
                "receivedTimestamp": message.created_at.to_rfc3339(),
            }),
        ),
        CallbackKind::Delivery => (
            "Microsoft.Communication.SMSDeliveryReportReceived",
            serde_json::json!({
                "messageId": message.provider_message_id,
                "from": message.from,
                "to": message.to,
                "deliveryStatus": match message.delivery_state {
                    SmsDeliveryState::Delivered => "Delivered",
                    SmsDeliveryState::Failed => "Failed",
                    SmsDeliveryState::Accepted => "Queued",
                },
                "deliveryStatusDetails": "Sqrzl emulator explicit transition",
                "receivedTimestamp": message.updated_at.to_rfc3339(),
            }),
        ),
    };
    let batch = serde_json::json!([{
        "id": uuid::Uuid::new_v4().to_string(),
        "topic": "/subscriptions/00000000/resourceGroups/sqrzl/providers/Microsoft.Communication/communicationServices/emulator",
        "subject": format!("/phonenumber/{}", message.to),
        "data": data,
        "eventType": event_type,
        "dataVersion": "1.0",
        "metadataVersion": "1",
        "eventTime": Utc::now().to_rfc3339(),
    }]);
    (
        batch.to_string(),
        HashMap::from([
            (
                "content-type".to_string(),
                "application/json; charset=utf-8".to_string(),
            ),
            ("aeg-event-type".to_string(), "Notification".to_string()),
        ]),
    )
}

fn form_encode(fields: &[(String, String)]) -> String {
    fields
        .iter()
        .map(|(name, value)| {
            format!(
                "{}={}",
                urlencoding::encode(name),
                urlencoding::encode(value)
            )
        })
        .collect::<Vec<_>>()
        .join("&")
}

fn twilio_signature(url: &str, fields: &[(String, String)], token: &str) -> String {
    type HmacSha1 = Hmac<Sha1>;
    let mut payload = url.to_string();
    for (name, value) in fields {
        payload.push_str(name);
        payload.push_str(value);
    }
    let mut mac = HmacSha1::new_from_slice(token.as_bytes()).expect("HMAC accepts any key size");
    mac.update(payload.as_bytes());
    BASE64.encode(mac.finalize().into_bytes())
}

async fn send_http_callback(
    url: &str,
    headers: &HashMap<String, String>,
    body: &[u8],
) -> std::result::Result<(u16, String), String> {
    let uri = url
        .parse::<Uri>()
        .map_err(|err| format!("invalid callback URL: {err}"))?;
    if uri.scheme_str() != Some("http") {
        return Err(
            "callback URL must use http; redirects and TLS callbacks are disabled".to_string(),
        );
    }
    let host = uri
        .host()
        .ok_or_else(|| "callback URL must include a host".to_string())?;
    if !callback_host_allowed(host) {
        return Err(format!("callback host is not allowlisted: {host}"));
    }
    let port = uri.port_u16().unwrap_or(80);
    let authority = uri
        .authority()
        .ok_or_else(|| "callback URL must include an authority".to_string())?
        .as_str();
    let path = uri
        .path_and_query()
        .map_or("/", http::uri::PathAndQuery::as_str);
    let timeout = callback_timeout();

    tokio::time::timeout(timeout, async {
        let mut stream = tokio::net::TcpStream::connect((host, port))
            .await
            .map_err(|err| format!("callback connection failed: {err}"))?;
        let mut request =
            format!("POST {path} HTTP/1.1\r\nHost: {authority}\r\nConnection: close\r\n");
        for (name, value) in headers {
            if name.eq_ignore_ascii_case("host")
                || name.eq_ignore_ascii_case("connection")
                || name.eq_ignore_ascii_case("content-length")
                || name.contains(['\r', '\n'])
                || value.contains(['\r', '\n'])
            {
                continue;
            }
            request.push_str(name);
            request.push_str(": ");
            request.push_str(value);
            request.push_str("\r\n");
        }
        let _ = write!(request, "Content-Length: {}\r\n\r\n", body.len());
        stream
            .write_all(request.as_bytes())
            .await
            .map_err(|err| format!("callback write failed: {err}"))?;
        stream
            .write_all(body)
            .await
            .map_err(|err| format!("callback body write failed: {err}"))?;
        stream
            .shutdown()
            .await
            .map_err(|err| format!("callback shutdown failed: {err}"))?;

        let mut response = Vec::new();
        stream
            .take((MAX_RECORDED_RESPONSE_BYTES + 64 * 1024) as u64)
            .read_to_end(&mut response)
            .await
            .map_err(|err| format!("callback response read failed: {err}"))?;
        parse_http_response(&response)
    })
    .await
    .map_err(|_| format!("callback timed out after {}ms", timeout.as_millis()))?
}

fn parse_http_response(response: &[u8]) -> std::result::Result<(u16, String), String> {
    let header_end = response
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .ok_or_else(|| "callback returned an invalid HTTP response".to_string())?;
    let head = String::from_utf8_lossy(&response[..header_end]);
    let status = head
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|value| value.parse::<u16>().ok())
        .ok_or_else(|| "callback returned an invalid HTTP status".to_string())?;
    let body = &response[header_end + 4..];
    let bounded = &body[..body.len().min(MAX_RECORDED_RESPONSE_BYTES)];
    Ok((status, String::from_utf8_lossy(bounded).into_owned()))
}

fn callback_host_allowed(host: &str) -> bool {
    let mut hosts = HashSet::from([
        "localhost".to_string(),
        "127.0.0.1".to_string(),
        "::1".to_string(),
    ]);
    if let Ok(configured) = std::env::var("SQRZL_TEXT_CALLBACK_ALLOWED_HOSTS") {
        hosts.extend(
            configured
                .split(',')
                .map(str::trim)
                .filter(|host| !host.is_empty())
                .map(str::to_ascii_lowercase),
        );
    }
    hosts.contains(&host.to_ascii_lowercase())
}

pub(crate) fn validate_callback_url(url: &str) -> Result<()> {
    let uri = url
        .parse::<Uri>()
        .map_err(|err| Error::InvalidRequest(format!("invalid callback URL: {err}")))?;
    if uri.scheme_str() != Some("http") {
        return Err(Error::InvalidRequest(
            "callback URL must use http; redirects and TLS callbacks are disabled".to_string(),
        ));
    }
    let host = uri
        .host()
        .ok_or_else(|| Error::InvalidRequest("callback URL must include a host".to_string()))?;
    if !callback_host_allowed(host) {
        return Err(Error::InvalidRequest(format!(
            "callback host is not allowlisted: {host}"
        )));
    }
    Ok(())
}

fn callback_timeout() -> Duration {
    let milliseconds = std::env::var("SQRZL_TEXT_CALLBACK_TIMEOUT_MS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(DEFAULT_CALLBACK_TIMEOUT_MS);
    Duration::from_millis(milliseconds)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sms::{FilesystemSmsStore, NewSmsMessage, SmsChannel, SmsDirection, SmsProvider};
    use std::collections::HashMap;

    async fn callback_receiver() -> (String, tokio::task::JoinHandle<String>) {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let port = listener.local_addr().unwrap().port();
        let receiver = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut bytes = Vec::new();
            let mut buffer = [0_u8; 4096];
            loop {
                let read = stream.read(&mut buffer).await.unwrap();
                if read == 0 {
                    break;
                }
                bytes.extend_from_slice(&buffer[..read]);
                let Some(header_end) = bytes.windows(4).position(|part| part == b"\r\n\r\n") else {
                    continue;
                };
                let head = String::from_utf8_lossy(&bytes[..header_end]);
                let length = head
                    .lines()
                    .find_map(|line| {
                        line.to_ascii_lowercase()
                            .strip_prefix("content-length: ")
                            .and_then(|value| value.parse::<usize>().ok())
                    })
                    .unwrap_or(0);
                if bytes.len() >= header_end + 4 + length {
                    break;
                }
            }
            stream
                .write_all(
                    b"HTTP/1.1 202 Accepted\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok",
                )
                .await
                .unwrap();
            String::from_utf8(bytes).unwrap()
        });
        (format!("http://127.0.0.1:{port}/events"), receiver)
    }

    #[test]
    fn should_default_callback_allowlist_to_loopback() {
        assert!(callback_host_allowed("localhost"));
        assert!(callback_host_allowed("127.0.0.1"));
        assert!(!callback_host_allowed("example.com"));
    }

    #[test]
    fn should_bound_parsed_callback_response() {
        // Arrange
        // Act
        // Assert
        let response = b"HTTP/1.1 202 Accepted\r\nContent-Length: 2\r\n\r\nok";
        assert_eq!(
            parse_http_response(response).unwrap(),
            (202, "ok".to_string())
        );
    }

    #[tokio::test]
    async fn should_send_signed_twilio_callback_record_twiml_and_keep_retry_history() {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let port = listener.local_addr().unwrap().port();
        let callback_url = format!("http://127.0.0.1:{port}/inbound");
        let receiver = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut bytes = Vec::new();
            let mut buffer = [0_u8; 4096];
            loop {
                let read = stream.read(&mut buffer).await.unwrap();
                if read == 0 {
                    break;
                }
                bytes.extend_from_slice(&buffer[..read]);
                if let Some(header_end) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
                    let head = String::from_utf8_lossy(&bytes[..header_end]);
                    let length = head
                        .lines()
                        .find_map(|line| {
                            line.to_ascii_lowercase()
                                .strip_prefix("content-length: ")
                                .and_then(|value| value.parse::<usize>().ok())
                        })
                        .unwrap_or(0);
                    if bytes.len() >= header_end + 4 + length {
                        break;
                    }
                }
            }
            stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 32\r\nConnection: close\r\n\r\n<Response><Message>ignored</Message>").await.unwrap();
            bytes
        });

        std::env::set_var("SQRZL_TWILIO_AUTH_TOKEN", "callback-secret");
        let store: Arc<dyn SmsStore> = Arc::new(
            FilesystemSmsStore::open(
                std::env::temp_dir().join(format!("sqrzl-simulator-test-{}", uuid::Uuid::new_v4())),
            )
            .unwrap(),
        );
        store
            .put_destination(SmsProvider::Twilio, "+15550000001", &callback_url)
            .unwrap();
        let simulator = SmsSimulator::new(store.clone());
        let message = simulator
            .inject_inbound(NewSmsMessage {
                batch_id: None,
                provider: SmsProvider::Twilio,
                provider_message_id: None,
                direction: SmsDirection::Inbound,
                channel: SmsChannel::Sms,
                from: "+15550000002".to_string(),
                to: "+15550000001".to_string(),
                body: "hello".to_string(),
                media: Vec::new(),
                metadata: HashMap::new(),
            })
            .await
            .unwrap();
        let request = String::from_utf8(receiver.await.unwrap()).unwrap();
        assert!(request.to_ascii_lowercase().contains("x-twilio-signature:"));
        assert!(request.contains("Body=hello"));

        let attempts = store.list_callbacks(&message.message_id).unwrap();
        assert_eq!(attempts.len(), 1);
        assert_eq!(attempts[0].state, CallbackAttemptState::Succeeded);
        assert!(attempts[0]
            .response_body
            .as_deref()
            .unwrap()
            .contains("<Response>"));

        let retry = simulator.retry(&attempts[0].attempt_id).await.unwrap();
        assert_eq!(retry.state, CallbackAttemptState::Failed);
        assert_eq!(
            retry.retry_of.as_deref(),
            Some(attempts[0].attempt_id.as_str())
        );
        assert_eq!(store.list_callbacks(&message.message_id).unwrap().len(), 2);
        assert_eq!(
            store.get_message(&message.message_id).unwrap().body,
            "hello"
        );
        std::env::remove_var("SQRZL_TWILIO_AUTH_TOKEN");
    }

    #[test]
    fn should_render_provider_specific_event_batches() {
        // Arrange
        // Act
        // Assert
        let now = Utc::now();
        let base = SmsMessage {
            message_id: "txt-1".to_string(),
            batch_id: None,
            provider: SmsProvider::AwsSmsVoiceV2,
            provider_message_id: "provider-1".to_string(),
            direction: SmsDirection::Inbound,
            channel: SmsChannel::Sms,
            from: "+15550000002".to_string(),
            to: "+15550000001".to_string(),
            body: "STOP now".to_string(),
            media: Vec::new(),
            metadata: HashMap::new(),
            peer: "+15550000002".to_string(),
            delivery_state: SmsDeliveryState::Accepted,
            created_at: now,
            updated_at: now,
        };
        let (aws, _) = aws_callback_payload(&base, CallbackKind::Inbound);
        assert!(aws.contains("\"Type\":\"Notification\""));
        assert!(aws.contains("originationNumber"));

        let mut acs = base;
        acs.provider = SmsProvider::Acs;
        let (event_grid, _) = acs_callback_payload(&acs, CallbackKind::Inbound);
        assert!(event_grid.starts_with('['));
        assert!(event_grid.contains("Microsoft.Communication.SMSReceived"));
    }

    #[tokio::test]
    async fn should_deliver_aws_envelopes_and_acs_batches_to_real_callback_receivers() {
        for (provider, header, event_name) in [
            (
                SmsProvider::AwsSmsVoiceV2,
                "x-amz-sns-message-type: Notification",
                "originationNumber",
            ),
            (
                SmsProvider::Acs,
                "aeg-event-type: Notification",
                "Microsoft.Communication.SMSReceived",
            ),
        ] {
            let (callback_url, receiver) = callback_receiver().await;
            let store: Arc<dyn SmsStore> = Arc::new(
                FilesystemSmsStore::open(std::env::temp_dir().join(format!(
                    "sqrzl-provider-callback-test-{}",
                    uuid::Uuid::new_v4()
                )))
                .unwrap(),
            );
            store
                .put_destination(provider, "+15550000001", &callback_url)
                .unwrap();
            let message = SmsSimulator::new(store.clone())
                .inject_inbound(NewSmsMessage {
                    batch_id: None,
                    provider,
                    provider_message_id: None,
                    direction: SmsDirection::Inbound,
                    channel: SmsChannel::Sms,
                    from: "+15550000002".to_string(),
                    to: "+15550000001".to_string(),
                    body: "HELP now".to_string(),
                    media: Vec::new(),
                    metadata: HashMap::new(),
                })
                .await
                .unwrap();
            let request = receiver.await.unwrap();
            assert!(request
                .to_ascii_lowercase()
                .contains(&header.to_ascii_lowercase()));
            assert!(request.contains(event_name));
            let attempts = store.list_callbacks(&message.message_id).unwrap();
            assert_eq!(attempts.len(), 1);
            assert_eq!(attempts[0].state, CallbackAttemptState::Succeeded);
        }
    }

    #[tokio::test]
    async fn should_emit_delivery_only_on_the_first_explicit_terminal_transition() {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let port = listener.local_addr().unwrap().port();
        let callback_url = format!("http://127.0.0.1:{port}/status");
        let receiver = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut bytes = Vec::new();
            let mut buffer = [0_u8; 4096];
            loop {
                let read = stream.read(&mut buffer).await.unwrap();
                if read == 0 {
                    break;
                }
                bytes.extend_from_slice(&buffer[..read]);
                if bytes.windows(4).any(|part| part == b"\r\n\r\n")
                    && bytes
                        .windows(b"MessageStatus=delivered".len())
                        .any(|part| part == b"MessageStatus=delivered")
                {
                    break;
                }
            }
            stream
                .write_all(
                    b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                )
                .await
                .unwrap();
        });
        let store: Arc<dyn SmsStore> = Arc::new(
            FilesystemSmsStore::open(
                std::env::temp_dir().join(format!("sqrzl-delivery-test-{}", uuid::Uuid::new_v4())),
            )
            .unwrap(),
        );
        let mut metadata = HashMap::new();
        metadata.insert(
            "status_callback".to_string(),
            serde_json::Value::String(callback_url),
        );
        let message = store
            .store_message(NewSmsMessage {
                batch_id: None,
                provider: SmsProvider::Twilio,
                provider_message_id: None,
                direction: SmsDirection::Outbound,
                channel: SmsChannel::Sms,
                from: "+15550000001".to_string(),
                to: "+15550000002".to_string(),
                body: "outbound".to_string(),
                media: Vec::new(),
                metadata,
            })
            .unwrap();
        assert!(store
            .list_callbacks(&message.message_id)
            .unwrap()
            .is_empty());

        let simulator = SmsSimulator::new(store.clone());
        simulator
            .transition_delivery(&message.message_id, SmsDeliveryState::Delivered)
            .await
            .unwrap();
        receiver.await.unwrap();
        assert_eq!(store.list_callbacks(&message.message_id).unwrap().len(), 1);

        simulator
            .transition_delivery(&message.message_id, SmsDeliveryState::Delivered)
            .await
            .unwrap();
        assert_eq!(store.list_callbacks(&message.message_id).unwrap().len(), 1);
        assert!(simulator
            .transition_delivery(&message.message_id, SmsDeliveryState::Failed)
            .await
            .is_err());
    }
}
