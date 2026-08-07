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
        let request = request(
            "POST",
            "http://localhost/2010-04-01/Accounts/ACtest/Messages.json",
            &[("content-type", "application/x-www-form-urlencoded")],
            "To=%2B15550000002&From=%2B15550000001&Body=hello+world&MediaUrl=https%3A%2F%2Fexample.com%2Fa.jpg&MediaUrl=https%3A%2F%2Fexample.com%2Fb.jpg",
        )
        .await;
        let response = SmsAdapterRegistry::default()
            .route(store.clone(), auth(), request)
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
            "Action=Publish&PhoneNumber=5550000002&Message=hello",
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
                "http://localhost/2010-04-01/Accounts/ACtest/Messages.json",
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
}
