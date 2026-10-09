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
