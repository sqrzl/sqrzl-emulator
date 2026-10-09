mod common;

use common::interop::{auth_disabled, request};
use hyper::StatusCode;
use serde_json::json;
use sqrzl_emulator::capture::RepeatabilityRecord;
use sqrzl_emulator::error::Result;
use sqrzl_emulator::mail::providers::MailAdapterRegistry;
use sqrzl_emulator::mail::{
    DeliveryStatus, ListMessagesParams, ListMessagesResult, MailStore, MailboxInfo, Message,
    StoredMessage,
};
use sqrzl_emulator::server::RequestExt;
use sqrzl_emulator::sms::providers::SmsAdapterRegistry;
use sqrzl_emulator::sms::{FilesystemSmsStore, SmsStore};
use std::io::Write;
use std::sync::{Arc, Mutex};

#[derive(Default)]
struct CountBytes(u64);

impl Write for CountBytes {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0 += bytes.len() as u64;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

// This probe measures the exact JSON buffers prepared by FilesystemMailStore,
// without allocating a serialized Vec or a payload clone for every mailbox.
// Returning an empty accepted batch avoids disk writes and fan-out allocations.
#[derive(Default)]
struct CountingCaptureStore {
    admitted_bytes: Mutex<u64>,
}

impl MailStore for CountingCaptureStore {
    fn capture_batch(
        &self,
        messages: &[(String, Message)],
        _: &[RepeatabilityRecord],
    ) -> Result<Option<Vec<Vec<StoredMessage>>>> {
        let mut total = 0;
        let now = chrono::Utc::now();
        for (id, message) in messages {
            let mut targets = message
                .recipients()
                .into_iter()
                .map(sqrzl_emulator::mail::Address::mailbox_key)
                .collect::<Vec<_>>();
            targets.sort();
            targets.dedup();
            targets.push("_all".to_string());
            let mut stored = StoredMessage {
                message_id: id.clone(),
                mailbox: String::new(),
                message: message.clone(),
                delivery_status: DeliveryStatus::accepted(now),
                received_at: now,
            };
            stored.message.provider_metadata.insert(
                "__sqrzl_capture_transaction".to_string(),
                json!("11111111-1111-4111-8111-111111111111"),
            );
            for target in targets {
                stored.mailbox = target;
                let mut counter = CountBytes::default();
                serde_json::to_writer(&mut counter, &stored).unwrap();
                total += counter.0
                    + stored
                        .message
                        .raw_mime
                        .as_ref()
                        .map_or(0, |raw| raw.len() as u64);
            }
        }
        *self.admitted_bytes.lock().unwrap() = total;
        Ok(Some(vec![Vec::new(); messages.len()]))
    }

    fn store_message(&self, _: &str, _: &str, _: Message) -> Result<StoredMessage> {
        panic!("unexpected fallback capture")
    }
    fn get_message(&self, _: &str, _: &str) -> Result<StoredMessage> {
        panic!("unexpected message read")
    }
    fn list_messages(&self, _: &str, _: ListMessagesParams) -> Result<ListMessagesResult> {
        panic!("unexpected message listing")
    }
    fn delete_message(&self, _: &str, _: &str) -> Result<()> {
        panic!("unexpected message delete")
    }
    fn delete_mailbox(&self, _: &str) -> Result<()> {
        panic!("unexpected mailbox delete")
    }
    fn update_delivery_status(&self, _: &str, _: &str, _: DeliveryStatus) -> Result<()> {
        panic!("unexpected delivery update")
    }
    fn list_mailboxes(&self) -> Result<Vec<MailboxInfo>> {
        panic!("unexpected mailbox listing")
    }
    fn ensure_mailbox(&self, _: &str) -> Result<()> {
        panic!("unexpected mailbox creation")
    }
}

#[tokio::test]
async fn should_reject_acs_fanout_above_64_mib_before_capture() {
    let recipients = (0..50)
        .map(|n| json!({"address":format!("recipient{n}@example.com")}))
        .collect::<Vec<_>>();
    let payload = json!({"senderAddress":"sender@example.com", "recipients":{"to":recipients}, "content":{"subject":"capture admission", "plainText":"x".repeat(1536 * 1024)}}).to_string();
    verify_admission(
        "http://localhost/emails:send?api-version=2023-03-31",
        &payload,
    )
    .await;
}

#[tokio::test]
async fn should_reject_sendgrid_fanout_above_64_mib_before_capture() {
    let recipients = (0..50)
        .map(|n| json!({"email":format!("recipient{n}@example.com")}))
        .collect::<Vec<_>>();
    let payload = json!({"from":{"email":"sender@example.com"}, "personalizations":[{"to":recipients}], "subject":"capture admission", "content":[{"type":"text/plain", "value":"x".repeat(1536 * 1024)}]}).to_string();
    verify_admission("http://localhost/v3/mail/send", &payload).await;
}

#[tokio::test]
async fn should_reject_ses_fanout_above_64_mib_before_capture() {
    let recipients = (0..50)
        .map(|n| format!("recipient{n}@example.com"))
        .collect::<Vec<_>>();
    let payload = json!({"FromEmailAddress":"sender@example.com", "Destination":{"ToAddresses":recipients}, "Content":{"Simple":{"Subject":{"Data":"capture admission"}, "Body":{"Text":{"Data":"x".repeat(1536 * 1024)}}}}}).to_string();
    verify_admission("http://localhost/v2/email/outbound-emails", &payload).await;
}

#[tokio::test]
async fn should_reject_sendgrid_personalization_amplification_before_capture() {
    let personalizations = (0..40)
        .map(|n| json!({"to":[{"email":format!("recipient{n}@example.com")}]}))
        .collect::<Vec<_>>();
    let payload = json!({"from":{"email":"sender@example.com"}, "personalizations":personalizations, "subject":"capture admission", "content":[{"type":"text/plain", "value":"x".repeat(1024 * 1024)}]}).to_string();
    verify_admission("http://localhost/v3/mail/send", &payload).await;
}

#[tokio::test]
async fn should_admit_acs_fanout_near_the_local_capture_budget() {
    let recipients = (0..50)
        .map(|n| json!({"address":format!("recipient{n}@example.com")}))
        .collect::<Vec<_>>();
    let payload = json!({"senderAddress":"sender@example.com", "recipients":{"to":recipients}, "content":{"subject":"near capture limit", "plainText":"x".repeat(620 * 1024)}}).to_string();
    let store = Arc::new(CountingCaptureStore::default());
    let req = RequestExt::from_hyper(request(
        "POST",
        "http://localhost/emails:send?api-version=2023-03-31",
        &[("content-type", "application/json")],
        payload.as_bytes(),
    ))
    .await
    .unwrap();
    let response = MailAdapterRegistry::default()
        .route(store.clone(), auth_disabled(), req)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    assert!(*store.admitted_bytes.lock().unwrap() > 30 * 1024 * 1024);
}

#[tokio::test]
async fn should_charge_retained_results_when_mailbox_json_buffers_fit_the_budget() {
    let recipients = (0..50)
        .map(|n| json!({"address":format!("recipient{n}@example.com")}))
        .collect::<Vec<_>>();
    let payload = json!({"senderAddress":"sender@example.com", "recipients":{"to":recipients}, "content":{"subject":"retained result admission", "plainText":"x".repeat(700 * 1024)}}).to_string();
    // The fifty-one mailbox JSON payloads occupy about 35 MiB. Retaining fifty
    // full result messages plus the incoming/prepared copies exceeds 64 MiB.
    verify_admission(
        "http://localhost/emails:send?api-version=2023-03-31",
        &payload,
    )
    .await;
}

#[tokio::test]
async fn should_reject_large_fanout_without_claiming_an_acs_repeatability_id() {
    let root = std::env::temp_dir().join(format!(
        "capture-resource-no-mutation-{}",
        uuid::Uuid::new_v4()
    ));
    let store = Arc::new(sqrzl_emulator::mail::FilesystemMailStore::open(&root).unwrap());
    let request_id = uuid::Uuid::new_v4().to_string();
    let operation_id = uuid::Uuid::new_v4().to_string();
    let first_sent = chrono::Utc::now()
        .format("%a, %d %b %Y %H:%M:%S GMT")
        .to_string();
    let recipients = (0..50)
        .map(|n| json!({"address":format!("recipient{n}@example.com")}))
        .collect::<Vec<_>>();
    let mut payload = json!({"senderAddress":"sender@example.com", "recipients":{"to":recipients}, "content":{"subject":"capture admission", "plainText":"x".repeat(1536 * 1024)}});
    for expected in [StatusCode::PAYLOAD_TOO_LARGE, StatusCode::ACCEPTED] {
        let req = RequestExt::from_hyper(request(
            "POST",
            "http://localhost/emails:send?api-version=2023-03-31",
            &[
                ("content-type", "application/json"),
                ("repeatability-request-id", &request_id),
                ("repeatability-first-sent", &first_sent),
                ("operation-id", &operation_id),
            ],
            payload.to_string().as_bytes(),
        ))
        .await
        .unwrap();
        let response = MailAdapterRegistry::default()
            .route(store.clone(), auth_disabled(), req)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(response.status(), expected);
        if expected == StatusCode::PAYLOAD_TOO_LARGE {
            assert_eq!(
                response.headers().get("x-ms-error-code").unwrap(),
                "RequestBodyTooLarge"
            );
            assert!(common::interop::body_text(response)
                .await
                .contains("local 64 MiB aggregate limit"));
            assert!(store.list_mailboxes().unwrap().is_empty());
            assert!(store
                .get_repeatability_record(&format!("acs-email/{request_id}"))
                .unwrap()
                .is_none());
            assert!(store
                .get_repeatability_record(&format!("acs-email-operation/{operation_id}"))
                .unwrap()
                .is_none());
            assert_eq!(std::fs::read_dir(root.join("_mail")).unwrap().count(), 1);
            assert_eq!(
                std::fs::read_dir(root.join("_mail/.capture-transactions"))
                    .unwrap()
                    .count(),
                0
            );
            payload["content"]["plainText"] = json!("retry after rejected admission");
        }
    }
    assert_eq!(store.list_mailboxes().unwrap().len(), 50);
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn should_bound_acs_sms_metadata_fanout_before_claims_or_capture() {
    let root = std::env::temp_dir().join(format!("capture-sms-resource-{}", uuid::Uuid::new_v4()));
    let store = Arc::new(FilesystemSmsStore::open(&root).unwrap());
    let first_sent = chrono::Utc::now()
        .format("%a, %d %b %Y %H:%M:%S GMT")
        .to_string();
    let recipients = (0..100).map(|n| json!({"to":format!("+15550000{n:03}"), "repeatabilityRequestId":uuid::Uuid::new_v4().to_string(), "repeatabilityFirstSent":first_sent})).collect::<Vec<_>>();
    let mut payload = json!({"from":"+15550000001", "message":"small", "smsRecipients":recipients, "smsSendOptions":{"tag":"x".repeat(800 * 1024)}});
    for expected in [StatusCode::PAYLOAD_TOO_LARGE, StatusCode::ACCEPTED] {
        let req = RequestExt::from_hyper(request(
            "POST",
            "http://localhost/sms?api-version=2021-03-07",
            &[("content-type", "application/json")],
            payload.to_string().as_bytes(),
        ))
        .await
        .unwrap();
        let response = SmsAdapterRegistry::default()
            .route(store.clone(), auth_disabled(), req)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(response.status(), expected);
        if expected == StatusCode::PAYLOAD_TOO_LARGE {
            for child in [
                "messages",
                "conversations",
                "media",
                ".capture-transactions",
            ] {
                assert_eq!(
                    std::fs::read_dir(root.join("_sms").join(child))
                        .unwrap()
                        .count(),
                    0
                );
            }
            for recipient in &payload["smsRecipients"].as_array().unwrap()[..] {
                let key = format!(
                    "acs-sms/{}/{}",
                    recipient["to"].as_str().unwrap(),
                    recipient["repeatabilityRequestId"].as_str().unwrap()
                );
                assert!(store.get_repeatability_record(&key).unwrap().is_none());
            }
            assert!(common::interop::body_text(response)
                .await
                .contains("local 64 MiB aggregate limit"));
            payload["smsSendOptions"]["tag"] = json!("small");
        } else {
            let body: serde_json::Value =
                serde_json::from_str(&common::interop::body_text(response).await).unwrap();
            assert_eq!(body["value"].as_array().unwrap().len(), 100);
            assert!(body["value"]
                .as_array()
                .unwrap()
                .iter()
                .all(|result| result["successful"] == true));
        }
    }
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn should_bound_acs_invalid_recipient_replay_before_result_clones_or_claims() {
    // Arrange a native invalid recipient whose record key and echoed result
    // each contain ten MiB. Three record representations fit under 64 MiB,
    // while retaining the caller's result at publication exceeds the limit.
    let root =
        std::env::temp_dir().join(format!("capture-invalid-replay-{}", uuid::Uuid::new_v4()));
    let store = Arc::new(FilesystemSmsStore::open(&root).unwrap());
    let request_id = uuid::Uuid::new_v4().to_string();
    let first_sent = chrono::Utc::now()
        .format("%a, %d %b %Y %H:%M:%S GMT")
        .to_string();
    let recipient = "x".repeat(10 * 1024 * 1024);
    let replay_key = format!("acs-sms/{recipient}/{request_id}");
    let mut payload = json!({"from":"+15550000001", "message":"small", "smsRecipients":[{"to":recipient,"repeatabilityRequestId":request_id,"repeatabilityFirstSent":first_sent}]});

    // Act through the native ACS Send route, then retry a small invalid recipient.
    let response = native_sms_request(store.clone(), &payload).await;

    // Assert rejection precedes every capture, journal and repeatability write.
    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(
        response.headers().get("x-ms-error-code").unwrap(),
        "RequestBodyTooLarge"
    );
    assert!(store
        .get_repeatability_record(&replay_key)
        .unwrap()
        .is_none());
    assert_sms_capture_is_empty(&root);
    payload["smsRecipients"][0]["to"] = json!("invalid");
    let response = native_sms_request(store.clone(), &payload).await;
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    let body: serde_json::Value =
        serde_json::from_str(&common::interop::body_text(response).await).unwrap();
    assert_eq!(body["value"][0]["successful"], false);
    assert_eq!(body["value"][0]["httpStatusCode"], 400);
    assert_eq!(body["value"][0]["to"], "invalid");
    let record = store
        .get_repeatability_record(&format!("acs-sms/invalid/{request_id}"))
        .unwrap()
        .unwrap();
    assert_eq!(record.status, 400);
    let replay = native_sms_request(store.clone(), &payload).await;
    assert_eq!(replay.status(), StatusCode::ACCEPTED);
    assert_eq!(common::interop::body_text(replay).await, body.to_string());
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn should_bound_acs_invalid_recipient_results_without_repeatability_records() {
    // Arrange an echoed result whose three response representations alone
    // exceed the aggregate limit, although no messages or replay records exist.
    let root =
        std::env::temp_dir().join(format!("capture-invalid-result-{}", uuid::Uuid::new_v4()));
    let store = Arc::new(FilesystemSmsStore::open(&root).unwrap());
    let mut payload = json!({"from":"+15550000001", "message":"small", "smsRecipients":[{"to":"x".repeat(24 * 1024 * 1024)}]});

    // Act through the native route with no repeatability GUID.
    let response = native_sms_request(store.clone(), &payload).await;

    // Assert the result-only path is bounded and preserves a later normal error.
    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    assert_sms_capture_is_empty(&root);
    payload["smsRecipients"][0]["to"] = json!("invalid");
    let response = native_sms_request(store, &payload).await;
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    let body: serde_json::Value =
        serde_json::from_str(&common::interop::body_text(response).await).unwrap();
    assert_eq!(body["value"][0]["successful"], false);
    assert_eq!(body["value"][0]["httpStatusCode"], 400);
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn should_bound_acs_recipient_source_fields_before_cloning_invalid_repeatability() {
    // Arrange a small echoed recipient with a large invalid header field. Raw
    // input and parsed JSON fit, but cloning the recipient field exceeds 64 MiB.
    let root =
        std::env::temp_dir().join(format!("capture-invalid-source-{}", uuid::Uuid::new_v4()));
    let store = Arc::new(FilesystemSmsStore::open(&root).unwrap());
    let request_id = uuid::Uuid::new_v4().to_string();
    let mut payload = json!({"from":"+15550000001", "message":"small", "smsRecipients":[{"to":"invalid","repeatabilityRequestId":request_id,"repeatabilityFirstSent":"x".repeat(24 * 1024 * 1024)}]});

    // Act through native ACS recipient parsing.
    let response = native_sms_request(store.clone(), &payload).await;

    // Assert the rejected plan makes no reservation; ordinary invalid metadata
    // retains its native per-recipient 202 response after a smaller retry.
    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    assert_sms_capture_is_empty(&root);
    payload["smsRecipients"][0]["repeatabilityFirstSent"] = json!("invalid-date");
    let response = native_sms_request(store.clone(), &payload).await;
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    let body: serde_json::Value =
        serde_json::from_str(&common::interop::body_text(response).await).unwrap();
    assert_eq!(body["value"][0]["repeatabilityResult"], "rejected");
    assert!(store
        .get_repeatability_record(&format!("acs-sms/invalid/{request_id}"))
        .unwrap()
        .is_none());
    std::fs::remove_dir_all(root).unwrap();
}

async fn native_sms_request(
    store: Arc<FilesystemSmsStore>,
    payload: &serde_json::Value,
) -> hyper::Response<sqrzl_emulator::body::Body> {
    let req = RequestExt::from_hyper(request(
        "POST",
        "http://localhost/sms?api-version=2021-03-07",
        &[("content-type", "application/json")],
        payload.to_string().as_bytes(),
    ))
    .await
    .unwrap();
    SmsAdapterRegistry::default()
        .route(store, auth_disabled(), req)
        .await
        .unwrap()
        .unwrap()
}

fn assert_sms_capture_is_empty(root: &std::path::Path) {
    for child in [
        "messages",
        "conversations",
        "media",
        ".capture-transactions",
        ".repeatability",
    ] {
        let count = match std::fs::read_dir(root.join("_sms").join(child)) {
            Ok(entries) => entries.count(),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => 0,
            Err(error) => panic!("failed to inspect {child}: {error}"),
        };
        assert_eq!(count, 0, "{child}");
    }
}

async fn verify_admission(uri: &str, payload: &str) {
    let store = Arc::new(CountingCaptureStore::default());
    let request = RequestExt::from_hyper(request(
        "POST",
        uri,
        &[("content-type", "application/json")],
        payload.as_bytes(),
    ))
    .await
    .unwrap();
    let response = MailAdapterRegistry::default()
        .route(store.clone(), auth_disabled(), request)
        .await
        .unwrap()
        .unwrap();
    let prepared_bytes = *store.admitted_bytes.lock().unwrap();
    eprintln!(
        "request_bytes={} projected_json_and_raw_buffers={prepared_bytes} response_status={}",
        payload.len(),
        response.status()
    );
    assert!(payload.len() < 10 * 1024 * 1024);
    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(
        prepared_bytes, 0,
        "reject before cloning or calling capture_batch"
    );
}
