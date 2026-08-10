use crate::auth::AuthConfig;
use crate::body::Body;
use crate::server::RequestExt as SmsRequest;
use crate::sms::SmsStore;
use http::{HeaderMap, Method, Uri};
use hyper::Response;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

mod acs;
mod aws;
mod aws_auth;
mod twilio;

pub use acs::AcsSmsAdapter;
pub use aws::{AwsSmsVoiceAdapter, SnsSmsAdapter};
pub use twilio::TwilioSmsAdapter;

pub trait SmsAdapter: Send + Sync {
    fn name(&self) -> &'static str;
    fn matches(&self, request: &SmsRequest) -> bool;
    fn matches_request_head(&self, _method: &Method, _uri: &Uri, _headers: &HeaderMap) -> bool {
        false
    }
    fn payload_too_large(&self, max_request_bytes: usize) -> Response<Body> {
        crate::providers::payload_too_large_response(max_request_bytes)
    }
    fn incomplete_body(&self) -> Response<Body> {
        json_error(
            http::StatusCode::BAD_REQUEST,
            "InvalidRequest",
            "The request body ended before it was complete",
        )
    }
    fn handle<'a>(
        &'a self,
        store: Arc<dyn SmsStore>,
        auth: Arc<AuthConfig>,
        request: SmsRequest,
    ) -> Pin<Box<dyn Future<Output = Result<Response<Body>, String>> + Send + 'a>>;
}

pub struct SmsAdapterRegistry {
    adapters: Vec<Arc<dyn SmsAdapter>>,
}

impl Default for SmsAdapterRegistry {
    fn default() -> Self {
        Self::new(vec![
            Arc::new(TwilioSmsAdapter),
            Arc::new(SnsSmsAdapter),
            Arc::new(AwsSmsVoiceAdapter),
            Arc::new(AcsSmsAdapter),
        ])
    }
}

impl SmsAdapterRegistry {
    #[must_use]
    pub fn new(adapters: Vec<Arc<dyn SmsAdapter>>) -> Self {
        Self { adapters }
    }

    pub async fn route(
        &self,
        store: Arc<dyn SmsStore>,
        auth: Arc<AuthConfig>,
        request: SmsRequest,
    ) -> Option<Result<Response<Body>, String>> {
        for adapter in &self.adapters {
            if adapter.matches(&request) {
                return Some(adapter.handle(store.clone(), auth.clone(), request).await);
            }
        }
        None
    }

    #[must_use]
    pub fn render_payload_too_large(
        &self,
        method: &Method,
        uri: &Uri,
        headers: &HeaderMap,
        max_request_bytes: usize,
    ) -> Option<Response<Body>> {
        self.adapters.iter().find_map(|adapter| {
            adapter
                .matches_request_head(method, uri, headers)
                .then(|| adapter.payload_too_large(max_request_bytes))
        })
    }

    #[must_use]
    pub fn render_incomplete_body(
        &self,
        method: &Method,
        uri: &Uri,
        headers: &HeaderMap,
    ) -> Option<Response<Body>> {
        self.adapters.iter().find_map(|adapter| {
            adapter
                .matches_request_head(method, uri, headers)
                .then(|| adapter.incomplete_body())
        })
    }
}

pub(super) fn json_error(status: http::StatusCode, code: &str, message: &str) -> Response<Body> {
    crate::server::ResponseBuilder::new(status)
        .content_type("application/json; charset=utf-8")
        .body(
            serde_json::json!({"code": code, "message": message})
                .to_string()
                .into_bytes(),
        )
        .build()
}

pub(super) fn decode_form(body: &[u8]) -> Vec<(String, String)> {
    String::from_utf8_lossy(body)
        .split('&')
        .filter(|part| !part.is_empty())
        .map(|part| {
            let (key, value) = part.split_once('=').unwrap_or((part, ""));
            (decode_form_component(key), decode_form_component(value))
        })
        .collect()
}

fn decode_form_component(value: &str) -> String {
    let value = value.replace('+', " ");
    urlencoding::decode(&value).map_or(value.clone(), std::borrow::Cow::into_owned)
}

pub(super) fn form_value<'a>(fields: &'a [(String, String)], name: &str) -> Option<&'a str> {
    fields
        .iter()
        .find(|(key, _)| key == name)
        .map(|(_, value)| value.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::body::Body;
    use crate::sms::{FilesystemSmsStore, ListSmsParams};
    use http::StatusCode;
    use http_body_util::BodyExt;

    fn store() -> Arc<dyn SmsStore> {
        Arc::new(
            FilesystemSmsStore::open(
                std::env::temp_dir()
                    .join(format!("sqrzl-sms-provider-test-{}", uuid::Uuid::new_v4())),
            )
            .unwrap(),
        )
    }

    fn auth() -> Arc<AuthConfig> {
        Arc::new(AuthConfig {
            access_key_id: None,
            secret_access_key: None,
            enforce_auth: false,
            admin_auth_disabled: false,
            blobs_path: "./blobs".to_string(),
            lifecycle_interval: std::time::Duration::from_hours(1),
            api_port: 9000,
            ui_port: 9001,
            max_request_bytes: crate::config::DEFAULT_SQRZL_MAX_REQUEST_BYTES,
            smtp_port: crate::config::DEFAULT_SQRZL_SMTP_PORT,
        })
    }

    fn enforced_auth() -> Arc<AuthConfig> {
        let mut config = (*auth()).clone();
        config.access_key_id = Some("test-key".to_string());
        config.secret_access_key = Some("test-secret".to_string());
        config.enforce_auth = true;
        Arc::new(config)
    }

    async fn request(method: &str, uri: &str, headers: &[(&str, &str)], body: &str) -> SmsRequest {
        let mut builder = hyper::Request::builder().method(method).uri(uri);
        for (name, value) in headers {
            builder = builder.header(*name, *value);
        }
        SmsRequest::from_hyper(builder.body(Body::from(body.as_bytes().to_vec())).unwrap())
            .await
            .unwrap()
    }

    async fn body(response: Response<Body>) -> String {
        String::from_utf8(
            response
                .into_body()
                .collect()
                .await
                .unwrap()
                .to_bytes()
                .to_vec(),
        )
        .unwrap()
    }

    #[tokio::test]
    async fn should_capture_twilio_repeated_media_and_return_sdk_resource() {
        let store = store();
        let structured_request = request(
            "POST",
            "http://localhost/2010-04-01/Accounts/AC00000000000000000000000000000001/Messages.json",
            &[("content-type", "application/x-www-form-urlencoded")],
            "To=%2B15550000002&From=%2B15550000001&Body=hello+world&MediaUrl=https%3A%2F%2Fexample.com%2Fa.jpg&MediaUrl=https%3A%2F%2Fexample.com%2Fb.jpg",
        )
        .await;
        let response = SmsAdapterRegistry::default()
            .route(store.clone(), auth(), structured_request)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(response.status(), StatusCode::CREATED);
        let payload = body(response).await;
        assert!(payload.contains("\"num_media\":\"2\""));
        let messages = store
            .list_messages("+15550000002", ListSmsParams::default())
            .unwrap();
        assert_eq!(messages.messages[0].body, "hello world");
        assert_eq!(messages.messages[0].media.len(), 2);
    }

    #[tokio::test]
    async fn should_return_twilio_accepted_only_while_messaging_service_selects_sender() {
        // Arrange
        let registry = SmsAdapterRegistry::default();
        let store = store();
        let service_only = request(
            "POST",
            "http://localhost/2010-04-01/Accounts/AC00000000000000000000000000000001/Messages.json",
            &[("content-type", "application/x-www-form-urlencoded")],
            "To=%2B15550000002&MessagingServiceSid=MGaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa&Body=hello",
        )
        .await;
        let explicit_sender = request(
            "POST",
            "http://localhost/2010-04-01/Accounts/AC00000000000000000000000000000001/Messages.json",
            &[("content-type", "application/x-www-form-urlencoded")],
            "To=%2B15550000003&From=%2B15550000001&MessagingServiceSid=MGaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa&Body=hello",
        )
        .await;

        // Act
        let service_response = registry
            .route(store.clone(), auth(), service_only)
            .await
            .unwrap()
            .unwrap();
        let sender_response = registry
            .route(store, auth(), explicit_sender)
            .await
            .unwrap()
            .unwrap();

        // Assert
        let service_payload: serde_json::Value =
            serde_json::from_str(&body(service_response).await).unwrap();
        let sender_payload: serde_json::Value =
            serde_json::from_str(&body(sender_response).await).unwrap();
        assert_eq!(service_payload["status"], "accepted");
        assert_eq!(sender_payload["status"], "queued");
    }

    #[tokio::test]
    async fn should_reject_twilio_body_over_1600_characters_without_capture() {
        // Arrange
        let registry = SmsAdapterRegistry::default();
        let store = store();
        let request_body = format!(
            "To=%2B15550000002&From=%2B15550000001&Body={}",
            "a".repeat(1_601)
        );
        let request = request(
            "POST",
            "http://localhost/2010-04-01/Accounts/AC00000000000000000000000000000001/Messages.json",
            &[("content-type", "application/x-www-form-urlencoded")],
            &request_body,
        )
        .await;

        // Act
        let response = registry
            .route(store.clone(), auth(), request)
            .await
            .unwrap()
            .unwrap();

        // Assert
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let payload = body(response).await;
        assert!(payload.contains("\"code\":21617"));
        assert!(store.list_conversations().unwrap().is_empty());
    }

    #[tokio::test]
    async fn should_reject_invalid_twilio_sids_media_and_unsupported_fields_without_capture() {
        let registry = SmsAdapterRegistry::default();
        let store = store();
        let cases = [
            (
                "http://localhost/2010-04-01/Accounts/ACshort/Messages.json",
                "To=%2B15550000002&From=%2B15550000001&Body=hello",
            ),
            (
                "http://localhost/2010-04-01/Accounts/AC00000000000000000000000000000001/Messages.json",
                "To=%2B15550000002&MessagingServiceSid=MGshort&Body=hello",
            ),
            (
                "http://localhost/2010-04-01/Accounts/AC00000000000000000000000000000001/Messages.json",
                "To=%2B15550000002&From=%2B15550000001&MediaUrl=ftp%3A%2F%2Fexample.com%2Fa.jpg",
            ),
            (
                "http://localhost/2010-04-01/Accounts/AC00000000000000000000000000000001/Messages.json",
                "To=%2B15550000002&From=%2B15550000001&Body=hello&ValidityPeriod=60",
            ),
        ];

        for (uri, payload) in cases {
            let request = request(
                "POST",
                uri,
                &[("content-type", "application/x-www-form-urlencoded")],
                payload,
            )
            .await;
            let response = registry
                .route(store.clone(), auth(), request)
                .await
                .expect("Twilio path should be claimed")
                .unwrap();
            assert!(matches!(
                response.status(),
                StatusCode::BAD_REQUEST | StatusCode::UNSUPPORTED_MEDIA_TYPE
            ));
        }
        assert!(store.list_conversations().unwrap().is_empty());
    }

    #[tokio::test]
    async fn should_distinguish_sns_publish_and_sms_voice_from_ordinary_root_requests() {
        let registry = SmsAdapterRegistry::default();
        let store = store();
        let ordinary = request("POST", "http://localhost/", &[], "not-an-sns-query").await;
        assert!(registry
            .route(store.clone(), auth(), ordinary)
            .await
            .is_none());

        let sns = request(
            "POST",
            "http://localhost/",
            &[("content-type", "application/x-www-form-urlencoded")],
            "Action=Publish&Version=2010-03-31&PhoneNumber=%2B15550000002&Message=hello",
        )
        .await;
        let response = registry
            .route(store.clone(), auth(), sns)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert!(body(response).await.contains("<PublishResponse"));

        let voice = request(
            "POST",
            "http://localhost/",
            &[("x-amz-target", "PinpointSMSVoiceV2.SendTextMessage")],
            r#"{"DestinationPhoneNumber":"+15550000003","OriginationIdentity":"+15550000001","MessageBody":"voice"}"#,
        )
        .await;
        let response = registry
            .route(store.clone(), auth(), voice)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert!(body(response).await.contains("MessageId"));
        assert_eq!(store.list_conversations().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn should_apply_sns_json_protocol_selection_and_reject_malformed_structures() {
        let registry = SmsAdapterRegistry::default();
        let store = store();
        let structured =
            urlencoding::encode(r#"{"default":"fallback","sms":"provider-specific body"}"#);
        let payload = format!(
            "Action=Publish&Version=2010-03-31&PhoneNumber=%2B15550000020&MessageStructure=json&Message={structured}&MessageAttributes.entry.1.Name=AWS.SNS.SMS.SenderID&MessageAttributes.entry.1.Value.DataType=String&MessageAttributes.entry.1.Value.StringValue=Sqrzl"
        );
        let structured_request = request(
            "POST",
            "http://localhost/",
            &[("content-type", "application/x-www-form-urlencoded")],
            &payload,
        )
        .await;
        let response = registry
            .route(store.clone(), auth(), structured_request)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let stored = store
            .list_messages("+15550000020", ListSmsParams::default())
            .unwrap()
            .messages
            .remove(0);
        assert_eq!(stored.body, "provider-specific body");
        assert_eq!(stored.from, "Sqrzl");

        for accepted in [
            r#"{"default":"fallback","unknown":"ignored"}"#,
            r#"{"default":"fallback","sms":7}"#,
        ] {
            let encoded = urlencoding::encode(accepted);
            let payload = format!(
                "Action=Publish&Version=2010-03-31&PhoneNumber=%2B15550000021&MessageStructure=json&Message={encoded}"
            );
            let accepted_request = request(
                "POST",
                "http://localhost/",
                &[("content-type", "application/x-www-form-urlencoded")],
                &payload,
            )
            .await;
            let response = registry
                .route(store.clone(), auth(), accepted_request)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
        }
        assert!(store
            .list_messages("+15550000021", ListSmsParams::default())
            .unwrap()
            .messages
            .iter()
            .all(|message| message.body == "fallback"));

        for malformed in [
            r#"{"sms":"missing default"}"#,
            r#"{"default":"first","default":"second"}"#,
            "[]",
        ] {
            let encoded = urlencoding::encode(malformed);
            let payload = format!(
                "Action=Publish&Version=2010-03-31&PhoneNumber=%2B15550000022&MessageStructure=json&Message={encoded}"
            );
            let request = request(
                "POST",
                "http://localhost/",
                &[("content-type", "application/x-www-form-urlencoded")],
                &payload,
            )
            .await;
            let response = registry
                .route(store.clone(), auth(), request)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        }
        assert!(store
            .list_messages("+15550000022", ListSmsParams::default())
            .unwrap()
            .messages
            .is_empty());
    }

    #[tokio::test]
    async fn should_claim_malformed_provider_requests_before_storage_routing() {
        let registry = SmsAdapterRegistry::default();
        let store = store();

        let malformed_sns = request(
            "POST",
            "http://localhost/",
            &[("content-type", "application/x-www-form-urlencoded")],
            "Version=2010-03-31&PhoneNumber=%2B15550000022&Message=hello",
        )
        .await;
        let response = registry
            .route(store.clone(), auth(), malformed_sns)
            .await
            .expect("form-encoded SNS request should be claimed")
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert!(body(response).await.contains("MissingAction"));

        let unknown_target = request(
            "POST",
            "http://localhost/",
            &[("x-amz-target", "PinpointSMSVoiceV2.Unknown")],
            "{}",
        )
        .await;
        let response = registry
            .route(store.clone(), auth(), unknown_target)
            .await
            .expect("AWS SMS Voice target should be claimed")
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert!(body(response).await.contains("not supported"));

        let missing_version = request(
            "POST",
            "http://localhost/sms",
            &[("content-type", "application/json")],
            r#"{"from":"+15550000001","smsRecipients":[{"to":"+15550000023"}],"message":"hello"}"#,
        )
        .await;
        let response = registry
            .route(store.clone(), auth(), missing_version)
            .await
            .expect("ACS SMS path should be claimed")
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            response.headers().get("content-type").unwrap(),
            "application/problem+json"
        );

        let unauthorized_voice = request(
            "POST",
            "http://localhost/",
            &[("x-amz-target", "PinpointSMSVoiceV2.SendTextMessage")],
            r#"{"DestinationPhoneNumber":"+15550000024","MessageBody":"hello"}"#,
        )
        .await;
        let response = registry
            .route(store.clone(), enforced_auth(), unauthorized_voice)
            .await
            .expect("AWS SMS Voice request should be claimed")
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            response.headers().get("x-amzn-errortype").unwrap(),
            "AccessDeniedException"
        );
        assert!(store.list_conversations().unwrap().is_empty());
    }

    #[tokio::test]
    async fn should_validate_every_aws_sms_voice_field_before_capture() {
        let registry = SmsAdapterRegistry::default();
        let store = store();
        let cases = [
            (
                "PinpointSMSVoiceV2.SendTextMessage",
                r#"{"DestinationPhoneNumber":"012345","MessageBody":"hello"}"#,
            ),
            (
                "PinpointSMSVoiceV2.SendTextMessage",
                r#"{"DestinationPhoneNumber":"+15550000030","OriginationIdentity":"bad value","MessageBody":"hello"}"#,
            ),
            (
                "PinpointSMSVoiceV2.SendTextMessage",
                r#"{"DestinationPhoneNumber":"+15550000030","MessageBody":"   "}"#,
            ),
            (
                "PinpointSMSVoiceV2.SendTextMessage",
                r#"{"DestinationPhoneNumber":"+15550000030","MessageBody":"hello","TimeToLive":4}"#,
            ),
            (
                "PinpointSMSVoiceV2.SendTextMessage",
                r#"{"DestinationPhoneNumber":"+15550000030","MessageBody":"hello","DestinationCountryParameters":{"IN_TEMPLATE_ID":"has space"}}"#,
            ),
            (
                "PinpointSMSVoiceV2.SendMediaMessage",
                r#"{"DestinationPhoneNumber":"+15550000030","OriginationIdentity":"+15550000001","MediaUrls":[]}"#,
            ),
            (
                "PinpointSMSVoiceV2.SendMediaMessage",
                r#"{"DestinationPhoneNumber":"+15550000030","OriginationIdentity":"+15550000001","MediaUrls":["https://example.com/file.jpg"]}"#,
            ),
        ];

        for (target, payload) in cases {
            let request = request(
                "POST",
                "http://localhost/",
                &[("x-amz-target", target)],
                payload,
            )
            .await;
            let response = registry
                .route(store.clone(), auth(), request)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{payload}");
            assert_eq!(
                response.headers().get("x-amzn-errortype").unwrap(),
                "ValidationException"
            );
        }
        assert!(store.list_conversations().unwrap().is_empty());
    }

    #[tokio::test]
    async fn should_reject_missing_aws_auth_and_malformed_phone_numbers() {
        let registry = SmsAdapterRegistry::default();
        let store = store();
        let unauthorized = request(
            "POST",
            "http://localhost/",
            &[],
            "Action=Publish&PhoneNumber=%2B15550000002&Message=hello",
        )
        .await;
        let response = registry
            .route(store.clone(), enforced_auth(), unauthorized)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);

        let malformed = request(
            "POST",
            "http://localhost/",
            &[],
            "Action=Publish&Version=2010-03-31&PhoneNumber=5550000002&Message=hello",
        )
        .await;
        let response = registry
            .route(store, auth(), malformed)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert!(body(response).await.contains("PhoneNumber must be E.164"));
    }

    #[tokio::test]
    async fn should_store_each_acs_recipient_with_one_shared_batch_id() {
        if std::env::var("SQRZL_ACS_CONNECTION_STRING").is_ok() {
            return;
        }
        let registry = SmsAdapterRegistry::default();
        let store = store();
        let acs = request(
            "POST",
            "http://localhost/sms?api-version=2021-03-07",
            &[("content-type", "application/json")],
            r#"{"from":"+15550000001","smsRecipients":[{"to":"+15550000002"},{"to":"+15550000003"}],"message":"batch"}"#,
        )
        .await;
        let response = registry
            .route(store.clone(), auth(), acs)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(response.status(), StatusCode::ACCEPTED);
        let payload: serde_json::Value = serde_json::from_str(&body(response).await).unwrap();
        assert_eq!(payload["value"].as_array().unwrap().len(), 2);
        let first = store
            .list_messages("+15550000002", ListSmsParams::default())
            .unwrap()
            .messages
            .remove(0);
        let second = store
            .list_messages("+15550000003", ListSmsParams::default())
            .unwrap()
            .messages
            .remove(0);
        assert_eq!(first.batch_id, second.batch_id);
        assert!(first.batch_id.is_some());
        assert_ne!(first.message_id, second.message_id);
    }

    #[tokio::test]
    async fn should_render_provider_shaped_payload_too_large_responses() {
        let registry = SmsAdapterRegistry::default();
        let cases = [
            (
                "http://localhost/2010-04-01/Accounts/AC00000000000000000000000000000001/Messages.json",
                HeaderMap::new(),
                "\"code\":21617",
            ),
            (
                "http://localhost/",
                HeaderMap::from_iter([(
                    http::header::AUTHORIZATION,
                    "AWS4-HMAC-SHA256 Credential=test/20260807/us-east-1/sns/aws4_request"
                        .parse()
                        .unwrap(),
                )]),
                "<ErrorResponse",
            ),
            (
                "http://localhost/",
                HeaderMap::from_iter([(
                    http::HeaderName::from_static("x-amz-target"),
                    "PinpointSMSVoiceV2.SendTextMessage".parse().unwrap(),
                )]),
                "Request body exceeds",
            ),
            (
                "http://localhost/sms?api-version=2021-03-07",
                HeaderMap::new(),
                "RequestEntityTooLarge",
            ),
        ];

        for (uri, headers, expected) in cases {
            let response = registry
                .render_payload_too_large(
                    &Method::POST,
                    &uri.parse::<Uri>().unwrap(),
                    &headers,
                    128,
                )
                .expect("SMS request head should be recognized");
            assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
            assert!(body(response).await.contains(expected));
        }
    }

    #[tokio::test]
    async fn should_render_provider_shaped_incomplete_body_responses() {
        let registry = SmsAdapterRegistry::default();
        let cases = [
            (
                "http://localhost/2010-04-01/Accounts/AC00000000000000000000000000000001/Messages.json",
                HeaderMap::new(),
                "\"code\":21606",
                None,
            ),
            (
                "http://localhost/",
                HeaderMap::from_iter([(
                    http::header::CONTENT_TYPE,
                    "application/x-www-form-urlencoded".parse().unwrap(),
                )]),
                "<ErrorResponse",
                None,
            ),
            (
                "http://localhost/",
                HeaderMap::from_iter([(
                    http::HeaderName::from_static("x-amz-target"),
                    "PinpointSMSVoiceV2.SendTextMessage".parse().unwrap(),
                )]),
                "ended before it was complete",
                Some(("x-amzn-errortype", "ValidationException")),
            ),
            (
                "http://localhost/sms?api-version=2026-01-23",
                HeaderMap::new(),
                "\"Body\"",
                None,
            ),
        ];

        for (uri, headers, expected, expected_header) in cases {
            let response = registry
                .render_incomplete_body(&Method::POST, &uri.parse::<Uri>().unwrap(), &headers)
                .expect("SMS request head should be recognized");
            assert_eq!(response.status(), StatusCode::BAD_REQUEST);
            if let Some((name, value)) = expected_header {
                assert_eq!(response.headers()[name], value);
            }
            assert!(body(response).await.contains(expected));
        }
    }

    #[tokio::test]
    async fn should_reject_invalid_sms_requests_without_persisting_them() {
        let registry = SmsAdapterRegistry::default();
        let store = store();

        let twilio = request(
            "POST",
            "http://localhost/2010-04-01/Accounts/AC00000000000000000000000000000001/Messages.json",
            &[("content-type", "application/x-www-form-urlencoded")],
            "To=%2B15550000002&From=%2B15550000001",
        )
        .await;
        let response = registry
            .route(store.clone(), auth(), twilio)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);

        let sns = request(
            "POST",
            "http://localhost/",
            &[],
            "Action=Publish&PhoneNumber=%2B15550000002&Message=hello",
        )
        .await;
        let response = registry
            .route(store.clone(), auth(), sns)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);

        let dry_run = request(
            "POST",
            "http://localhost/",
            &[("x-amz-target", "PinpointSMSVoiceV2.SendTextMessage")],
            r#"{"DestinationPhoneNumber":"+15550000002","MessageBody":"validate only","DryRun":true}"#,
        )
        .await;
        let response = registry
            .route(store.clone(), auth(), dry_run)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert!(store.list_conversations().unwrap().is_empty());
    }

    #[tokio::test]
    async fn should_support_sns_get_and_acs_per_recipient_results() {
        if std::env::var("SQRZL_ACS_CONNECTION_STRING").is_ok() {
            return;
        }
        let registry = SmsAdapterRegistry::default();
        let store = store();

        let sns = request(
            "GET",
            "http://localhost/?Action=Publish&Version=2010-03-31&PhoneNumber=%2B15550000002&Message=hello",
            &[],
            "",
        )
        .await;
        let response = registry
            .route(store.clone(), auth(), sns)
            .await
            .expect("SNS GET query should route")
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert!(body(response)
            .await
            .contains("xmlns=\"https://sns.amazonaws.com"));

        let acs = request(
            "POST",
            "http://localhost/sms?api-version=2026-01-23",
            &[("content-type", "application/json")],
            r#"{"from":"+15550000001","smsRecipients":[{"to":"+15550000003"},{"to":"invalid"}],"message":"mixed"}"#,
        )
        .await;
        let response = registry
            .route(store.clone(), auth(), acs)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(response.status(), StatusCode::ACCEPTED);
        let payload: serde_json::Value = serde_json::from_str(&body(response).await).unwrap();
        assert_eq!(payload["value"][0]["httpStatusCode"], 202);
        assert_eq!(payload["value"][1]["httpStatusCode"], 400);
        assert_eq!(
            store
                .list_messages("+15550000003", ListSmsParams::default())
                .unwrap()
                .messages
                .len(),
            1
        );
        assert!(store
            .list_messages("invalid", ListSmsParams::default())
            .unwrap()
            .messages
            .is_empty());

        let repeatable_body = r#"{"from":"+15550000001","smsRecipients":[{"to":"+15550000004","repeatabilityRequestId":"fda6d242-46aa-4247-8bf6-619a1206f9c3","repeatabilityFirstSent":"Mon, 01 Apr 2019 06:22:03 GMT"}],"message":"repeatable"}"#;
        for _ in 0..2 {
            let repeatable = request(
                "POST",
                "http://localhost/sms?api-version=2026-01-23",
                &[("content-type", "application/json")],
                repeatable_body,
            )
            .await;
            let response = registry
                .route(store.clone(), auth(), repeatable)
                .await
                .unwrap()
                .unwrap();
            let payload: serde_json::Value = serde_json::from_str(&body(response).await).unwrap();
            assert_eq!(payload["value"][0]["repeatabilityResult"], "accepted");
        }
        assert_eq!(
            store
                .list_messages("+15550000004", ListSmsParams::default())
                .unwrap()
                .messages
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn should_validate_acs_sms_options_and_repeatability_without_duplicate_capture() {
        if std::env::var("SQRZL_ACS_CONNECTION_STRING").is_ok() {
            return;
        }
        let registry = SmsAdapterRegistry::default();
        let store = store();
        let original = r#"{"from":"+15550000001","smsRecipients":[{"to":"+15550000040","repeatabilityRequestId":"fda6d242-46aa-4247-8bf6-619a1206f9c3","repeatabilityFirstSent":"Mon, 01 Apr 2019 06:22:03 GMT"}],"message":"repeatable","smsSendOptions":{"enableDeliveryReport":true,"tag":"qualification","deliveryReportTimeoutInSeconds":60}}"#;

        for _ in 0..2 {
            let repeat = request(
                "POST",
                "http://localhost/sms?api-version=2026-01-23",
                &[("content-type", "application/json")],
                original,
            )
            .await;
            let response = registry
                .route(store.clone(), auth(), repeat)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(response.status(), StatusCode::ACCEPTED);
            let payload: serde_json::Value = serde_json::from_str(&body(response).await).unwrap();
            assert_eq!(payload["value"][0]["repeatabilityResult"], "accepted");
        }

        let changed = request(
            "POST",
            "http://localhost/sms?api-version=2026-01-23",
            &[("content-type", "application/json")],
            r#"{"from":"+15550000001","smsRecipients":[{"to":"+15550000040","repeatabilityRequestId":"fda6d242-46aa-4247-8bf6-619a1206f9c3","repeatabilityFirstSent":"Mon, 01 Apr 2019 06:22:03 GMT"}],"message":"changed","smsSendOptions":{"enableDeliveryReport":true,"tag":"qualification","deliveryReportTimeoutInSeconds":60}}"#,
        )
        .await;
        let response = registry
            .route(store.clone(), auth(), changed)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(response.status(), StatusCode::ACCEPTED);
        let payload: serde_json::Value = serde_json::from_str(&body(response).await).unwrap();
        assert_eq!(payload["value"][0]["successful"], false);
        assert_eq!(payload["value"][0]["repeatabilityResult"], "rejected");

        let captured = store
            .list_messages("+15550000040", ListSmsParams::default())
            .unwrap()
            .messages;
        assert_eq!(captured.len(), 1);
        assert_eq!(
            captured[0].metadata["sms_send_options"]["tag"],
            "qualification"
        );

        for invalid_options in [
            r#"{"messagingConnect":{}}"#,
            r#"{"deliveryReportTimeoutInSeconds":59}"#,
        ] {
            let payload = format!(
                r#"{{"from":"+15550000001","smsRecipients":[{{"to":"+15550000041"}}],"message":"invalid","smsSendOptions":{invalid_options}}}"#
            );
            let invalid = request(
                "POST",
                "http://localhost/sms?api-version=2026-01-23",
                &[("content-type", "application/json")],
                &payload,
            )
            .await;
            let response = registry
                .route(store.clone(), auth(), invalid)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(response.status(), StatusCode::BAD_REQUEST);
            assert_eq!(
                response.headers()["content-type"],
                "application/problem+json"
            );
        }
        assert!(store
            .list_messages("+15550000041", ListSmsParams::default())
            .unwrap()
            .messages
            .is_empty());
    }
}
