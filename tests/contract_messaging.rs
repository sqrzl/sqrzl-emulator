mod common;

use common::interop::{auth_disabled, body_text, request};
use hyper::StatusCode;
use serde_json::{json, Value};
use sqrzl_emulator::mail::providers::MailAdapterRegistry;
use sqrzl_emulator::mail::{FilesystemMailStore, ListMessagesParams, MailStore};
use sqrzl_emulator::server::RequestExt;
use sqrzl_emulator::sms::providers::SmsAdapterRegistry;
use sqrzl_emulator::sms::{FilesystemSmsStore, ListSmsParams, SmsStore};
use std::sync::Arc;

#[tokio::test]
async fn should_enforce_sns_subject_character_boundaries_without_capture() {
    let root = std::env::temp_dir().join(format!("sqrzl-sns-contract-{}", uuid::Uuid::new_v4()));
    let store: Arc<dyn SmsStore> = Arc::new(FilesystemSmsStore::open(&root).unwrap());
    let registry = SmsAdapterRegistry::default();
    let mut invalid = vec!["x".repeat(100), String::new()];
    invalid.extend(
        (0_u8..=31)
            .chain(std::iter::once(127))
            .map(|byte| format!("a{}b", char::from(byte))),
    );
    for subject in invalid {
        let form = format!(
            "Action=Publish&Version=2010-03-31&PhoneNumber=%2B15550000020&Message=hello&Subject={}",
            urlencoding::encode(&subject)
        );
        let req = RequestExt::from_hyper(request(
            "POST",
            "http://localhost/",
            &[("content-type", "application/x-www-form-urlencoded")],
            form.as_bytes(),
        ))
        .await
        .unwrap();
        let response = registry
            .route(store.clone(), auth_disabled(), req)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            response.status(),
            StatusCode::BAD_REQUEST,
            "subject {subject:?}"
        );
        assert!(body_text(response).await.contains("InvalidParameter"));
        assert!(store
            .list_messages("+15550000020", ListSmsParams::default())
            .unwrap()
            .messages
            .is_empty());
    }
    for subject in ["x".repeat(99), "é".repeat(99)] {
        let form = format!(
            "Action=Publish&Version=2010-03-31&PhoneNumber=%2B15550000020&Message=hello&Subject={}",
            urlencoding::encode(&subject)
        );
        let req = RequestExt::from_hyper(request(
            "POST",
            "http://localhost/",
            &[("content-type", "application/x-www-form-urlencoded")],
            form.as_bytes(),
        ))
        .await
        .unwrap();
        assert_eq!(
            registry
                .route(store.clone(), auth_disabled(), req)
                .await
                .unwrap()
                .unwrap()
                .status(),
            StatusCode::OK
        );
    }
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn should_validate_sendgrid_attachment_fields_before_any_capture() {
    let root =
        std::env::temp_dir().join(format!("sqrzl-sendgrid-contract-{}", uuid::Uuid::new_v4()));
    let store: Arc<dyn MailStore> = Arc::new(FilesystemMailStore::open(&root).unwrap());
    let registry = MailAdapterRegistry::default();
    let mut invalid = Vec::new();
    for field in ["filename", "type", "content", "disposition", "content_id"] {
        for value in [Value::Null, json!(12), json!(false), json!([]), json!({})] {
            invalid.push((field, value));
        }
    }
    for value in ["", "a\r.txt", "a\n.txt", "a;b.txt", "a,b.txt"] {
        invalid.push(("filename", json!(value)));
    }
    for value in [
        "",
        "text/plain; charset=utf-8",
        "text/plain\r\nX: yes",
        "text/plain,other",
        "invalid",
        "text/",
        "/plain",
        "text/with space",
    ] {
        invalid.push(("type", json!(value)));
    }
    for value in ["", "id\r", "id\n", "id;part"] {
        invalid.push(("content_id", json!(value)));
    }
    for (field, value) in invalid {
        let mut attachment = json!({"filename":"note.txt", "content":"aGVsbG8="});
        attachment[field] = value;
        let payload = json!({"from":{"email":"sender@example.com"},"personalizations":[{"to":[{"email":"alice@example.com"}]},{"to":[{"email":"bob@example.com"}]}],"subject":"valid","content":[{"type":"text/plain","value":"hello"}],"attachments":[attachment]});
        let req = RequestExt::from_hyper(request(
            "POST",
            "http://localhost/v3/mail/send",
            &[("content-type", "application/json")],
            payload.to_string().as_bytes(),
        ))
        .await
        .unwrap();
        let response = registry
            .route(store.clone(), auth_disabled(), req)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            response.status(),
            StatusCode::BAD_REQUEST,
            "field {field}: {}",
            payload["attachments"][0]
        );
        assert!(
            serde_json::from_str::<Value>(&body_text(response).await).unwrap()["errors"].is_array()
        );
        for mailbox in ["alice@example.com", "bob@example.com", "_all"] {
            assert!(store
                .list_messages(mailbox, ListMessagesParams::default())
                .unwrap()
                .messages
                .is_empty());
        }
    }
    let payload = json!({"from":{"email":"sender@example.com"},"personalizations":[{"to":[{"email":"alice@example.com"}]}],"subject":"valid","content":[{"type":"text/plain","value":"hello"}],"attachments":[{"filename":"résumé.txt","content":"aGVsbG8="},{"filename":"other.bin","content":"aGVsbG8=","type":"application/vnd.example+json","content_id":"ordinary-id","disposition":"inline"}]});
    let req = RequestExt::from_hyper(request(
        "POST",
        "http://localhost/v3/mail/send",
        &[("content-type", "application/json")],
        payload.to_string().as_bytes(),
    ))
    .await
    .unwrap();
    assert_eq!(
        registry
            .route(store.clone(), auth_disabled(), req)
            .await
            .unwrap()
            .unwrap()
            .status(),
        StatusCode::ACCEPTED
    );
    assert_eq!(
        store
            .list_messages("alice@example.com", ListMessagesParams::default())
            .unwrap()
            .messages[0]
            .message
            .attachments[0]
            .content_type,
        "application/octet-stream"
    );
    std::fs::remove_dir_all(root).unwrap();
}

async fn acs_email(
    store: Arc<dyn MailStore>,
    request_id: &str,
    first_sent: &str,
    operation_id: Option<&str>,
    payload: &Value,
) -> (StatusCode, Value) {
    let mut headers = vec![
        ("content-type", "application/json"),
        ("repeatability-request-id", request_id),
        ("repeatability-first-sent", first_sent),
    ];
    if let Some(id) = operation_id {
        headers.push(("operation-id", id));
    }
    let req = RequestExt::from_hyper(request(
        "POST",
        "http://localhost/emails:send?api-version=2023-03-31",
        &headers,
        payload.to_string().as_bytes(),
    ))
    .await
    .unwrap();
    let response = MailAdapterRegistry::default()
        .route(store, auth_disabled(), req)
        .await
        .unwrap()
        .unwrap();
    (
        response.status(),
        serde_json::from_str(&body_text(response).await).unwrap(),
    )
}

fn email_payload() -> Value {
    json!({"senderAddress":"sender@example.com","recipients":{"to":[{"address":"alice@example.com"},{"address":"bob@example.com"}]},"content":{"subject":"repeatable","plainText":"hello"}})
}

async fn acs_sms(store: Arc<dyn SmsStore>, payload: &Value) -> (StatusCode, Value) {
    let req = RequestExt::from_hyper(request(
        "POST",
        "http://localhost/sms?api-version=2021-03-07",
        &[("content-type", "application/json")],
        payload.to_string().as_bytes(),
    ))
    .await
    .unwrap();
    let response = SmsAdapterRegistry::default()
        .route(store, auth_disabled(), req)
        .await
        .unwrap()
        .unwrap();
    (
        response.status(),
        serde_json::from_str(&body_text(response).await).unwrap(),
    )
}

fn sms_payload(to: &str, request_id: &str) -> Value {
    json!({"from":"+15550000001", "message":"repeatable", "smsRecipients":[{"to":to,"repeatabilityRequestId":request_id,"repeatabilityFirstSent":"Mon, 01 Apr 2019 06:22:03 GMT"}]})
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn should_return_one_acs_email_operation_for_concurrent_retries_and_restart() {
    let root =
        std::env::temp_dir().join(format!("sqrzl-acs-email-claims-{}", uuid::Uuid::new_v4()));
    let store: Arc<dyn MailStore> = Arc::new(FilesystemMailStore::open(&root).unwrap());
    let request_id = uuid::Uuid::new_v4().to_string();
    let first_sent = chrono::Utc::now()
        .format("%a, %d %b %Y %H:%M:%S GMT")
        .to_string();
    let barrier = Arc::new(tokio::sync::Barrier::new(33));
    let mut jobs = tokio::task::JoinSet::new();
    for _ in 0..32 {
        let (store, id, date, barrier) = (
            store.clone(),
            request_id.clone(),
            first_sent.clone(),
            barrier.clone(),
        );
        jobs.spawn(async move {
            barrier.wait().await;
            acs_email(store, &id, &date, None, &email_payload()).await
        });
    }
    barrier.wait().await;
    let mut ids = std::collections::HashSet::new();
    while let Some(result) = jobs.join_next().await {
        let (status, body) = result.unwrap();
        assert_eq!(status, StatusCode::ACCEPTED);
        ids.insert(body["id"].as_str().unwrap().to_string());
    }
    assert_eq!(ids.len(), 1);
    let operation_id = ids.into_iter().next().unwrap();
    for mailbox in ["alice@example.com", "bob@example.com", "_all"] {
        assert_eq!(
            store
                .list_messages(mailbox, ListMessagesParams::default())
                .unwrap()
                .messages
                .len(),
            1
        );
        store.delete_message(mailbox, &operation_id).unwrap();
    }
    // Replay records retain operation identity independently of admin capture deletion.
    drop(store);
    let store: Arc<dyn MailStore> = Arc::new(FilesystemMailStore::open(&root).unwrap());
    let (status, body) = acs_email(
        store.clone(),
        &request_id,
        &first_sent,
        None,
        &email_payload(),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert_eq!(body["id"], operation_id);
    assert_eq!(
        acs_email(
            store.clone(),
            &request_id.to_ascii_uppercase(),
            &first_sent,
            None,
            &email_payload()
        )
        .await
        .1["id"],
        operation_id
    );
    assert!(store
        .list_messages("_all", ListMessagesParams::default())
        .unwrap()
        .messages
        .is_empty());
    let (status, _) = acs_email(
        store.clone(),
        &uuid::Uuid::new_v4().to_string(),
        &first_sent,
        Some(&operation_id),
        &email_payload(),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn should_replay_acs_email_client_errors_and_reject_conflicting_bodies_after_restart() {
    let root =
        std::env::temp_dir().join(format!("sqrzl-acs-email-errors-{}", uuid::Uuid::new_v4()));
    let store: Arc<dyn MailStore> = Arc::new(FilesystemMailStore::open(&root).unwrap());
    let id = uuid::Uuid::new_v4().to_string();
    let date = chrono::Utc::now()
        .format("%a, %d %b %Y %H:%M:%S GMT")
        .to_string();
    let original = acs_email(store.clone(), &id, &date, None, &json!({})).await;
    assert_eq!(original.0, StatusCode::BAD_REQUEST);
    drop(store);
    let store: Arc<dyn MailStore> = Arc::new(FilesystemMailStore::open(&root).unwrap());
    assert_eq!(
        acs_email(store.clone(), &id, &date, None, &json!({})).await,
        original
    );
    let (status, body) = acs_email(store.clone(), &id, &date, None, &email_payload()).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(
        body["error"]["message"],
        "Repeated request does not match the original request"
    );
    assert!(store
        .list_messages("_all", ListMessagesParams::default())
        .unwrap()
        .messages
        .is_empty());
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn should_scope_acs_sms_repeats_per_recipient_and_replay_after_capture_deletion() {
    let root = std::env::temp_dir().join(format!("sqrzl-acs-sms-claims-{}", uuid::Uuid::new_v4()));
    let store: Arc<dyn SmsStore> = Arc::new(FilesystemSmsStore::open(&root).unwrap());
    let id = uuid::Uuid::new_v4().to_string();
    let barrier = Arc::new(tokio::sync::Barrier::new(33));
    let mut jobs = tokio::task::JoinSet::new();
    for _ in 0..32 {
        let (store, id, barrier) = (store.clone(), id.clone(), barrier.clone());
        jobs.spawn(async move {
            barrier.wait().await;
            acs_sms(store, &sms_payload("+15550000002", &id)).await
        });
    }
    barrier.wait().await;
    let mut ids = std::collections::HashSet::new();
    while let Some(result) = jobs.join_next().await {
        let (status, body) = result.unwrap();
        assert_eq!(status, StatusCode::ACCEPTED);
        ids.insert(body["value"][0]["messageId"].as_str().unwrap().to_string());
    }
    assert_eq!(ids.len(), 1);
    let provider_id = ids.into_iter().next().unwrap();
    let mut per_recipient = sms_payload("+15550000003", &id);
    let duplicate = per_recipient["smsRecipients"][0].clone();
    per_recipient["smsRecipients"]
        .as_array_mut()
        .unwrap()
        .push(duplicate);
    let (_, second) = acs_sms(store.clone(), &per_recipient).await;
    assert_ne!(second["value"][0]["messageId"], provider_id);
    assert_eq!(
        second["value"][0]["messageId"],
        second["value"][1]["messageId"]
    );
    assert_eq!(
        store
            .list_messages("+15550000003", ListSmsParams::default())
            .unwrap()
            .messages
            .len(),
        1
    );
    let message = store.get_message_by_provider_id(&provider_id).unwrap();
    store
        .delete_message(&message.peer, &message.message_id)
        .unwrap();
    drop(store);
    let store: Arc<dyn SmsStore> = Arc::new(FilesystemSmsStore::open(&root).unwrap());
    let (_, repeated) = acs_sms(store.clone(), &sms_payload("+15550000002", &id)).await;
    assert_eq!(repeated["value"][0]["messageId"], provider_id);
    assert_eq!(
        acs_sms(
            store.clone(),
            &sms_payload("+15550000002", &id.to_ascii_uppercase())
        )
        .await
        .1["value"][0]["messageId"],
        provider_id
    );
    let mut changed_sender = sms_payload("+15550000002", &id);
    changed_sender["from"] = json!("+15550000099");
    assert_eq!(
        acs_sms(store.clone(), &changed_sender).await.1["value"][0]["repeatabilityResult"],
        "rejected"
    );
    assert!(store
        .list_messages("+15550000002", ListSmsParams::default())
        .unwrap()
        .messages
        .is_empty());
    let mut conflicting = sms_payload("+15550000002", &id);
    conflicting["message"] = json!("changed");
    let (_, rejected) = acs_sms(store.clone(), &conflicting).await;
    assert_eq!(rejected["value"][0]["repeatabilityResult"], "rejected");
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn should_replay_acs_sms_error_results_without_capturing_after_restart() {
    let root = std::env::temp_dir().join(format!("sqrzl-acs-sms-errors-{}", uuid::Uuid::new_v4()));
    let store: Arc<dyn SmsStore> = Arc::new(FilesystemSmsStore::open(&root).unwrap());
    let id = uuid::Uuid::new_v4().to_string();
    let payload = sms_payload("bad", &id);
    let original = acs_sms(store.clone(), &payload).await;
    assert_eq!(original.1["value"][0]["httpStatusCode"], 400);
    drop(store);
    let store: Arc<dyn SmsStore> = Arc::new(FilesystemSmsStore::open(&root).unwrap());
    assert_eq!(acs_sms(store.clone(), &payload).await, original);
    let mut changed = payload;
    changed["message"] = json!("different invalid request");
    let (_, result) = acs_sms(store.clone(), &changed).await;
    assert_eq!(result["value"][0]["repeatabilityResult"], "rejected");
    assert!(store.list_conversations().unwrap().is_empty());
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn should_retry_acs_requests_after_failed_persistence_without_extra_captures() {
    let root = std::env::temp_dir().join(format!("sqrzl-acs-persistence-{}", uuid::Uuid::new_v4()));
    let mail: Arc<dyn MailStore> = Arc::new(FilesystemMailStore::open(&root).unwrap());
    let sms: Arc<dyn SmsStore> = Arc::new(FilesystemSmsStore::open(&root).unwrap());
    let id = uuid::Uuid::new_v4().to_string();
    let date = chrono::Utc::now()
        .format("%a, %d %b %Y %H:%M:%S GMT")
        .to_string();
    for directory in ["_mail", "_sms"] {
        std::fs::write(
            root.join(directory).join(".repeatability"),
            b"injected blocker",
        )
        .unwrap();
    }
    let (status, body) = acs_email(mail.clone(), &id, &date, None, &email_payload()).await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(body["error"]["code"], "InternalError");
    let payload = sms_payload("+15550000002", &id);
    let (status, body) = acs_sms(sms.clone(), &payload).await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(body["error"]["code"], "InternalError");
    assert!(mail
        .list_messages("_all", ListMessagesParams::default())
        .unwrap()
        .messages
        .is_empty());
    assert!(sms
        .list_messages("+15550000002", ListSmsParams::default())
        .unwrap()
        .messages
        .is_empty());
    for directory in ["_mail", "_sms"] {
        std::fs::remove_file(root.join(directory).join(".repeatability")).unwrap();
    }
    assert_eq!(
        acs_email(mail.clone(), &id, &date, None, &email_payload())
            .await
            .0,
        StatusCode::ACCEPTED
    );
    assert_eq!(acs_sms(sms.clone(), &payload).await.0, StatusCode::ACCEPTED);
    assert_eq!(
        mail.list_messages("_all", ListMessagesParams::default())
            .unwrap()
            .messages
            .len(),
        1
    );
    assert_eq!(
        sms.list_messages("+15550000002", ListSmsParams::default())
            .unwrap()
            .messages
            .len(),
        1
    );
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn should_claim_an_acs_operation_id_once_for_competing_repeatability_keys() {
    let root =
        std::env::temp_dir().join(format!("sqrzl-acs-operation-id-{}", uuid::Uuid::new_v4()));
    let store: Arc<dyn MailStore> = Arc::new(FilesystemMailStore::open(&root).unwrap());
    let operation_id = uuid::Uuid::new_v4().to_string();
    let first_sent = chrono::Utc::now()
        .format("%a, %d %b %Y %H:%M:%S GMT")
        .to_string();
    let barrier = Arc::new(tokio::sync::Barrier::new(17));
    let mut jobs = tokio::task::JoinSet::new();
    for _ in 0..16 {
        let (store, id, date, barrier) = (
            store.clone(),
            operation_id.clone(),
            first_sent.clone(),
            barrier.clone(),
        );
        jobs.spawn(async move {
            barrier.wait().await;
            acs_email(
                store,
                &uuid::Uuid::new_v4().to_string(),
                &date,
                Some(&id),
                &email_payload(),
            )
            .await
        });
    }
    barrier.wait().await;
    let mut accepted = 0;
    while let Some(result) = jobs.join_next().await {
        let (status, _) = result.unwrap();
        match status {
            StatusCode::ACCEPTED => accepted += 1,
            StatusCode::BAD_REQUEST => {}
            other => panic!("unexpected status {other}"),
        }
    }
    assert_eq!(accepted, 1);
    assert_eq!(
        store
            .list_messages("_all", ListMessagesParams::default())
            .unwrap()
            .messages
            .len(),
        1
    );
    std::fs::remove_dir_all(root).unwrap();
}
