mod common;

use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use bytes::Bytes;
use common::interop::{auth_disabled, auth_enabled, body_text, request};
use hmac::KeyInit;
use hmac::{Hmac, Mac};
use http_body_util::Full;
use sha2::{Digest, Sha256};
use sqrzl_emulator::mail::filesystem::FilesystemMailStore;
use sqrzl_emulator::mail::model::ListMessagesParams;
use sqrzl_emulator::mail::providers::MailAdapterRegistry;
use sqrzl_emulator::mail::MailStore;
use sqrzl_emulator::server::RequestExt;
use sqrzl_emulator::{body::Body, Config};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;

fn temp_mail() -> Arc<dyn MailStore> {
    let dir = std::env::temp_dir().join(format!("sqrzl-interop-email-{}", uuid::Uuid::new_v4()));
    Arc::new(FilesystemMailStore::open(dir).expect("mail store should open"))
}

async fn call_adapter(
    adapter_registry: &MailAdapterRegistry,
    mail: Arc<dyn MailStore>,
    auth_config: Arc<Config>,
    request: hyper::Request<Full<Bytes>>,
) -> hyper::Response<Body> {
    let parsed = RequestExt::from_hyper(request)
        .await
        .expect("request should parse for adapter test");

    adapter_registry
        .route(mail, auth_config, parsed)
        .await
        .expect("adapter route should match one adapter")
        .expect("adapter should successfully handle request")
}

fn reserve_port() -> u16 {
    let listener =
        std::net::TcpListener::bind(("127.0.0.1", 0)).expect("smtp port reservation should bind");
    let port = listener
        .local_addr()
        .expect("smtp listener should report local address")
        .port();
    drop(listener);
    port
}

async fn send(writer: &mut tokio::net::tcp::OwnedWriteHalf, line: &str) {
    writer
        .write_all(format!("{line}\r\n").as_bytes())
        .await
        .expect("smtp command write should succeed");
}

async fn expect_reply(
    reader: &mut BufReader<tokio::net::tcp::OwnedReadHalf>,
    expected_prefix: &str,
) {
    let mut response = String::new();
    reader
        .read_line(&mut response)
        .await
        .expect("smtp reply should read");
    assert!(
        response.starts_with(expected_prefix),
        "expected reply prefix {expected_prefix}, got {response:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn should_capture_message_when_sending_over_a_smtp_session() {
    let mail = temp_mail();
    let smtp_port = reserve_port();
    let mail_for_server = mail.clone();

    tokio::spawn(async move {
        let _ = sqrzl_emulator::mail::SmtpServer::new(mail_for_server, smtp_port)
            .start()
            .await;
    });

    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    let stream = TcpStream::connect(("127.0.0.1", smtp_port))
        .await
        .expect("smtp test client should connect");

    let (reader, mut writer) = stream.into_split();
    let mut reader = BufReader::new(reader);

    let mut line = String::new();
    reader
        .read_line(&mut line)
        .await
        .expect("greeting should be readable");
    assert!(line.starts_with("220"));

    send(&mut writer, "EHLO localhost").await;
    expect_reply(&mut reader, "250").await;

    send(&mut writer, "MAIL FROM:<sender@example.com>").await;
    expect_reply(&mut reader, "250").await;

    send(&mut writer, "RCPT TO:<alice@example.com>").await;
    expect_reply(&mut reader, "250").await;

    send(&mut writer, "DATA").await;
    expect_reply(&mut reader, "354").await;

    send(&mut writer, "Subject: smtp interop").await;
    send(&mut writer, "").await;
    send(&mut writer, "hello from the interop smtp path").await;
    send(&mut writer, ".").await;
    expect_reply(&mut reader, "250").await;

    send(&mut writer, "QUIT").await;
    expect_reply(&mut reader, "221").await;

    let messages = mail
        .list_messages("alice@example.com", ListMessagesParams::default())
        .expect("list messages should succeed");
    assert_eq!(messages.messages.len(), 1);
    assert_eq!(messages.messages[0].message.subject, "smtp interop");
    assert_eq!(
        messages.messages[0].message.body_text,
        Some("hello from the interop smtp path".to_string())
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn should_capture_sendgrid_send_and_fan_out_recipients() {
    let mail = temp_mail();
    let adapters = MailAdapterRegistry::default();

    let payload = serde_json::json!({
        "personalizations": [{
            "to": [{"email": "alice@example.com"}, {"email": "bob@example.com"}],
            "cc": [{"email": "carol@example.com"}],
            "subject": "sendgrid blackbox",
            "headers": {"x-trace": "personalized"},
        }, {
            "to": [{"email": "dave@example.com"}],
            "subject": "sendgrid personalized blackbox",
        }],
        "from": {"email": "no-reply@example.com", "name": "Email Emulator"},
        "reply_to_list": [{"email": "support@example.com", "name": "Support"}],
        "subject": "sendgrid blackbox",
        "headers": {"X-Trace": "global", "X-Request": "qualification"},
        "content": [
            {"type": "text/plain", "value": "hello from sendgrid"},
            {"type": "text/html", "value": "<p>hello from sendgrid</p>"},
        ],
        "attachments": [{
            "filename": "notes.txt",
            "type": "text/plain",
            "content": BASE64.encode("hello"),
            "disposition": "inline",
            "content_id": "notes",
        }],
    });

    let send_request = request(
        "POST",
        "http://localhost/v3/mail/send",
        &[("content-type", "application/json")],
        payload.to_string().as_bytes(),
    );
    let response = call_adapter(&adapters, mail.clone(), auth_disabled(), send_request).await;

    assert_eq!(response.status(), hyper::StatusCode::ACCEPTED);
    assert!(response.headers().contains_key("x-message-id"));
    let response_body = body_text(response).await;
    assert!(response_body.trim().is_empty());

    let alice = mail
        .list_messages("alice@example.com", ListMessagesParams::default())
        .expect("list alice messages should succeed");
    let bob = mail
        .list_messages("bob@example.com", ListMessagesParams::default())
        .expect("list bob messages should succeed");
    let carol = mail
        .list_messages("carol@example.com", ListMessagesParams::default())
        .expect("list carol messages should succeed");
    let dave = mail
        .list_messages("dave@example.com", ListMessagesParams::default())
        .expect("list dave messages should succeed");
    assert_eq!(alice.messages.len(), 1);
    assert_eq!(bob.messages.len(), 1);
    assert_eq!(carol.messages.len(), 1);
    assert_eq!(dave.messages.len(), 1);
    assert_eq!(alice.messages[0].message_id, bob.messages[0].message_id);
    assert_eq!(alice.messages[0].message_id, carol.messages[0].message_id);
    assert_eq!(alice.messages[0].message.subject, "sendgrid blackbox");
    assert_eq!(
        alice.messages[0].message.body_text,
        Some("hello from sendgrid".into())
    );
    assert_eq!(
        alice.messages[0].message.body_html,
        Some("<p>hello from sendgrid</p>".into())
    );
    assert_eq!(alice.messages[0].message.attachments.len(), 1);
    assert_eq!(
        alice.messages[0].message.reply_to[0].email,
        "support@example.com"
    );
    assert_eq!(alice.messages[0].message.headers["x-trace"], "personalized");
    assert_eq!(
        alice.messages[0].message.headers["X-Request"],
        "qualification"
    );
    assert_eq!(
        alice.messages[0].message.attachments[0]
            .disposition
            .as_deref(),
        Some("inline")
    );
    assert_eq!(
        alice.messages[0].message.attachments[0]
            .content_id
            .as_deref(),
        Some("notes")
    );
    assert_eq!(
        dave.messages[0].message.subject,
        "sendgrid personalized blackbox"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn should_reject_unsupported_mail_shapes_before_capture() {
    let mail = temp_mail();
    let adapters = MailAdapterRegistry::default();

    let sendgrid = request(
        "POST",
        "http://localhost/v3/mail/send",
        &[("content-type", "application/json")],
        br#"{"personalizations":[{"to":[{"email":"alice@example.com"}],"subject":"bad attachment"}],"from":{"email":"sender@example.com"},"content":[{"type":"text/plain","value":"hello"}],"attachments":[{"filename":"bad.bin","content":"%%%"}]}"#,
    );
    let response = call_adapter(&adapters, mail.clone(), auth_disabled(), sendgrid).await;
    assert_eq!(response.status(), hyper::StatusCode::BAD_REQUEST);

    let duplicate_sendgrid = request(
        "POST",
        "http://localhost/v3/mail/send",
        &[("content-type", "application/json")],
        br#"{"personalizations":[{"to":[{"email":"alice@example.com"}]},{"to":[{"email":"ALICE@example.com"}]}],"from":{"email":"sender@example.com"},"subject":"duplicate","content":[{"type":"text/plain","value":"hello"}]}"#,
    );
    let response = call_adapter(&adapters, mail.clone(), auth_disabled(), duplicate_sendgrid).await;
    assert_eq!(response.status(), hyper::StatusCode::BAD_REQUEST);

    let ses = request(
        "POST",
        "http://localhost/v2/email/outbound-emails",
        &[("content-type", "application/json")],
        br#"{"FromEmailAddress":"sender@example.com","Destination":{"ToAddresses":["alice@example.com"]},"Content":{"Raw":{"Data":"aGVsbG8="}}}"#,
    );
    let response = call_adapter(&adapters, mail.clone(), auth_disabled(), ses).await;
    assert_eq!(response.status(), hyper::StatusCode::BAD_REQUEST);
    assert_eq!(
        response.headers()["x-amzn-errortype"],
        "BadRequestException"
    );

    let reserved_ses_header = request(
        "POST",
        "http://localhost/v2/email/outbound-emails",
        &[("content-type", "application/json")],
        br#"{"FromEmailAddress":"sender@example.com","Destination":{"ToAddresses":["alice@example.com"]},"Content":{"Simple":{"Subject":{"Data":"reserved"},"Body":{"Text":{"Data":"hello"}},"Headers":[{"Name":"Subject","Value":"override"}]}}}"#,
    );
    let response = call_adapter(
        &adapters,
        mail.clone(),
        auth_disabled(),
        reserved_ses_header,
    )
    .await;
    assert_eq!(response.status(), hyper::StatusCode::BAD_REQUEST);

    let acs = request(
        "POST",
        "http://localhost/emails:send?api-version=2025-09-01",
        &[("content-type", "application/json")],
        br#"{"senderAddress":"sender@example.com","recipients":{"to":[{"address":"alice@example.com"}]},"content":{"subject":"bad attachment","plainText":"hello"},"attachments":[{"name":"bad.bin","contentType":"application/octet-stream","contentInBase64":"%%%"}]}"#,
    );
    let response = call_adapter(&adapters, mail.clone(), auth_disabled(), acs).await;
    assert_eq!(response.status(), hyper::StatusCode::BAD_REQUEST);

    assert!(mail
        .list_messages("alice@example.com", ListMessagesParams::default())
        .unwrap()
        .messages
        .is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn should_capture_acs_send_with_recipients_and_attachments() {
    let mail = temp_mail();
    let adapters = MailAdapterRegistry::default();
    let payload = serde_json::json!({
        "senderAddress": "sender@example.com",
        "recipients": {
            "to": [{"address": "alice@example.com"}],
            "cc": [{"address": "bob@example.com"}],
            "bcc": [{"address": "carol@example.com"}],
        },
        "content": {
            "subject": "acs blackbox",
            "plainText": "hello from acs",
            "html": "<p>hello from acs</p>",
        },
        "attachments": [{
            "name": "inline.txt",
            "contentType": "text/plain",
            "contentInBase64": BASE64.encode("hello"),
            "contentId": "inline-note",
        }],
        "headers": {"X-Request": "qualification"},
        "replyTo": [{"address": "support@example.com", "displayName": "Support"}],
        "userEngagementTrackingDisabled": true,
    });

    let send_request = request(
        "POST",
        "http://localhost/emails:send?api-version=2023-03-01",
        &[("content-type", "application/json")],
        payload.to_string().as_bytes(),
    );
    let response = call_adapter(&adapters, mail.clone(), auth_disabled(), send_request).await;

    assert_eq!(response.status(), hyper::StatusCode::ACCEPTED);
    assert!(response.headers().contains_key("operation-location"));
    let response_body = body_text(response).await;
    let response_json: serde_json::Value =
        serde_json::from_str(&response_body).expect("acs response body should be valid json");
    assert_eq!(response_json["status"], "Running");

    let alice = mail
        .list_messages("alice@example.com", ListMessagesParams::default())
        .expect("list alice messages should succeed");
    let bob = mail
        .list_messages("bob@example.com", ListMessagesParams::default())
        .expect("list bob messages should succeed");
    let carol = mail
        .list_messages("carol@example.com", ListMessagesParams::default())
        .expect("list carol messages should succeed");
    assert_eq!(alice.messages.len(), 1);
    assert_eq!(bob.messages.len(), 1);
    assert_eq!(carol.messages.len(), 1);
    assert_eq!(alice.messages[0].message.subject, "acs blackbox");
    assert_eq!(
        alice.messages[0].message.body_text,
        Some("hello from acs".into())
    );
    assert_eq!(
        alice.messages[0].message.body_html,
        Some("<p>hello from acs</p>".into())
    );
    assert_eq!(alice.messages[0].message.attachments.len(), 1);
    assert_eq!(
        alice.messages[0].message.headers["X-Request"],
        "qualification"
    );
    assert_eq!(
        alice.messages[0].message.reply_to[0].email,
        "support@example.com"
    );
    assert_eq!(
        alice.messages[0].message.user_engagement_tracking_disabled,
        Some(true)
    );
    assert_eq!(
        alice.messages[0].message.attachments[0]
            .content_id
            .as_deref(),
        Some("inline-note")
    );

    let bcc_only = request(
        "POST",
        "http://localhost/emails:send?api-version=2025-09-01",
        &[("content-type", "application/json")],
        br#"{"senderAddress":"sender@example.com","recipients":{"bcc":[{"address":"dave@example.com"}]},"content":{"subject":"bcc only","plainText":"accepted"}}"#,
    );
    let response = call_adapter(&adapters, mail.clone(), auth_disabled(), bcc_only).await;
    assert_eq!(response.status(), hyper::StatusCode::ACCEPTED);
    assert_eq!(
        mail.list_messages("dave@example.com", ListMessagesParams::default())
            .unwrap()
            .messages
            .len(),
        1
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn should_capture_ses_send_with_signature_authorization() {
    let mail = temp_mail();
    let adapters = MailAdapterRegistry::default();

    let payload = serde_json::json!({
        "FromEmailAddress": "\"Sqrzl Sender\" <sender@example.com>",
        "Destination": {
            "ToAddresses": ["alice@example.com", "bob@example.com"],
            "CcAddresses": ["carol@example.com"],
        },
        "ReplyToAddresses": ["Support <support@example.com>"],
        "Content": {
            "Simple": {
                "Subject": {"Data": "ses blackbox", "Charset": "UTF-8"},
                "Body": {
                    "Text": {"Data": "hello from ses", "Charset": "UTF-8"},
                },
                "Headers": [{"Name": "X-Request", "Value": "qualification"}],
            }
        },
    });
    let request_body = payload.to_string();
    let request = signed_ses_request(&request_body, "test-key", "test-secret").await;
    let response = call_adapter(
        &adapters,
        mail.clone(),
        auth_enabled("test-key", "test-secret"),
        request,
    )
    .await;

    assert_eq!(response.status(), hyper::StatusCode::OK);
    let body = body_text(response).await;
    let response_json: serde_json::Value =
        serde_json::from_str(&body).expect("ses response body should be valid json");
    assert!(response_json["MessageId"]
        .as_str()
        .is_some_and(|id| !id.is_empty()));

    let alice = mail
        .list_messages("alice@example.com", ListMessagesParams::default())
        .expect("list alice should succeed");
    let bob = mail
        .list_messages("bob@example.com", ListMessagesParams::default())
        .expect("list bob should succeed");
    let carol = mail
        .list_messages("carol@example.com", ListMessagesParams::default())
        .expect("list carol should succeed");
    assert_eq!(alice.messages.len(), 1);
    assert_eq!(bob.messages.len(), 1);
    assert_eq!(carol.messages.len(), 1);
    assert_eq!(alice.messages[0].message.subject, "ses blackbox");
    assert_eq!(
        alice.messages[0].message.from.name.as_deref(),
        Some("Sqrzl Sender")
    );
    assert_eq!(
        alice.messages[0].message.reply_to[0].email,
        "support@example.com"
    );
    assert_eq!(
        alice.messages[0].message.headers["X-Request"],
        "qualification"
    );
    assert_eq!(
        alice.messages[0].message.provider_metadata["subject_charset"],
        "UTF-8"
    );
    assert_eq!(
        alice.messages[0].message.provider_metadata["text_charset"],
        "UTF-8"
    );
    assert_eq!(
        alice.messages[0].message.body_text,
        Some("hello from ses".into())
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn should_apply_acs_email_repeatability_without_overwriting_or_duplicate_capture() {
    let mail = temp_mail();
    let adapters = MailAdapterRegistry::default();
    let request_id = "fda6d242-46aa-4247-8bf6-619a1206f9c3";
    let operation_id = "8540c0de-899f-5cce-acb5-3ec493af3800";
    let first_sent = chrono::Utc::now()
        .format("%a, %d %b %Y %H:%M:%S GMT")
        .to_string();
    let original = br#"{"senderAddress":"sender@example.com","recipients":{"to":[{"address":"alice@example.com"}]},"content":{"subject":"repeatable","plainText":"hello"}}"#;

    let mut accepted_ids = Vec::new();
    for _ in 0..2 {
        let repeat = request(
            "POST",
            "http://localhost/emails:send?api-version=2025-09-01",
            &[
                ("content-type", "application/json"),
                ("operation-id", operation_id),
                ("repeatability-request-id", request_id),
                ("repeatability-first-sent", &first_sent),
            ],
            original,
        );
        let response = call_adapter(&adapters, mail.clone(), auth_disabled(), repeat).await;
        assert_eq!(response.status(), hyper::StatusCode::ACCEPTED);
        let payload: serde_json::Value = serde_json::from_str(&body_text(response).await).unwrap();
        accepted_ids.push(payload["id"].as_str().unwrap().to_string());
    }
    assert_eq!(accepted_ids, [operation_id, operation_id]);
    assert_eq!(
        mail.list_messages("alice@example.com", ListMessagesParams::default())
            .unwrap()
            .messages
            .len(),
        1
    );

    let changed = request(
        "POST",
        "http://localhost/emails:send?api-version=2025-09-01",
        &[
            ("content-type", "application/json"),
            ("operation-id", operation_id),
            ("repeatability-request-id", request_id),
            ("repeatability-first-sent", &first_sent),
        ],
        br#"{"senderAddress":"sender@example.com","recipients":{"to":[{"address":"alice@example.com"}]},"content":{"subject":"changed","plainText":"hello"}}"#,
    );
    let response = call_adapter(&adapters, mail.clone(), auth_disabled(), changed).await;
    assert_eq!(response.status(), hyper::StatusCode::BAD_REQUEST);
    assert_eq!(response.headers()["x-ms-error-code"], "InvalidRequest");
    assert_eq!(
        mail.list_messages("alice@example.com", ListMessagesParams::default())
            .unwrap()
            .messages
            .len(),
        1
    );

    let stale = request(
        "POST",
        "http://localhost/emails:send?api-version=2025-09-01",
        &[
            ("content-type", "application/json"),
            (
                "repeatability-request-id",
                "58b62dc6-f646-4d6c-bb99-cd6df75108fe",
            ),
            ("repeatability-first-sent", "Mon, 01 Apr 2019 06:22:03 GMT"),
        ],
        original,
    );
    let response = call_adapter(&adapters, mail, auth_disabled(), stale).await;
    assert_eq!(response.status(), hyper::StatusCode::PRECONDITION_FAILED);
    assert_eq!(response.headers()["x-ms-error-code"], "PreconditionFailed");
}

fn uri_encode(value: &str, encode_slash: bool) -> String {
    let mut encoded = String::new();
    for byte in value.bytes() {
        let ch = byte as char;
        let keep = ch.is_ascii_alphanumeric()
            || matches!(ch, '-' | '_' | '.' | '~')
            || (!encode_slash && ch == '/');
        if keep {
            encoded.push(ch);
        } else {
            let _ = std::fmt::Write::write_fmt(&mut encoded, format_args!("%{byte:02X}"));
        }
    }
    encoded
}

fn canonical_query_string(query: Option<&str>) -> String {
    let Some(query) = query else {
        return String::new();
    };

    let mut params: Vec<(String, String)> = query
        .split('&')
        .filter(|part| !part.is_empty())
        .map(|part| {
            let (raw_key, raw_value) = part.split_once('=').unwrap_or((part, ""));
            let key = urlencoding::decode(raw_key)
                .map_or_else(|_| raw_key.to_string(), std::borrow::Cow::into_owned);
            let value = urlencoding::decode(raw_value)
                .map_or_else(|_| raw_value.to_string(), std::borrow::Cow::into_owned);
            (uri_encode(&key, true), uri_encode(&value, true))
        })
        .collect();
    params.sort();
    params
        .into_iter()
        .map(|(key, value)| format!("{key}={value}"))
        .collect::<Vec<_>>()
        .join("&")
}

fn sha256_hex(value: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(value);
    hex::encode(hasher.finalize())
}

async fn canonical_request(body: &[u8], signed_headers: &[&str]) -> String {
    let parsed = RequestExt::from_hyper(
        hyper::Request::builder()
            .method("POST")
            .uri("http://localhost/v2/email/outbound-emails")
            .header("host", "localhost:9000")
            .header("x-amz-date", "20260101T120000Z")
            .header("x-amz-content-sha256", "UNSIGNED-PAYLOAD")
            .header("content-type", "application/json")
            .body(Body::from(body.to_vec()))
            .expect("SES signature request should build"),
    )
    .await
    .expect("request should parse");

    let canonical_uri = parsed.path();
    let canonical_query = canonical_query_string(parsed.uri.query());
    let mut canonical_headers: Vec<String> = signed_headers
        .iter()
        .map(|name| {
            let value = parsed
                .header(name)
                .map(str::trim)
                .unwrap_or_default()
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ");
            format!("{name}:{value}")
        })
        .collect();
    canonical_headers.sort_unstable();

    let canonical_headers = canonical_headers.join("\n");
    let mut signed_header_names = signed_headers.to_vec();
    signed_header_names.sort_unstable();
    let signed_headers = signed_header_names.join(";");

    let payload_hash = parsed
        .header("x-amz-content-sha256")
        .unwrap_or("UNSIGNED-PAYLOAD");

    format!(
        "POST\n{canonical_uri}\n{canonical_query}\n{canonical_headers}\n\n{signed_headers}\n{payload_hash}"
    )
}

fn signature_key(secret: &str, date: &str, region: &str, service: &str) -> Vec<u8> {
    type HmacSha256 = Hmac<Sha256>;
    let mut date_mac = HmacSha256::new_from_slice(format!("AWS4{secret}").as_bytes())
        .expect("sigv4 secret should be valid");
    date_mac.update(date.as_bytes());
    let date_key = date_mac.finalize().into_bytes().to_vec();

    let mut region_mac =
        HmacSha256::new_from_slice(&date_key).expect("sigv4 region key should be valid");
    region_mac.update(region.as_bytes());
    let region_key = region_mac.finalize().into_bytes().to_vec();

    let mut service_mac =
        HmacSha256::new_from_slice(&region_key).expect("sigv4 service key should be valid");
    service_mac.update(service.as_bytes());
    let service_key = service_mac.finalize().into_bytes().to_vec();

    let mut signing_mac =
        HmacSha256::new_from_slice(&service_key).expect("sigv4 signing key should be valid");
    signing_mac.update(b"aws4_request");
    signing_mac.finalize().into_bytes().to_vec()
}

fn sign_signature(secret: &str, canonical_request: &str, date: &str, amz_date: &str) -> String {
    type HmacSha256 = Hmac<Sha256>;
    let key = signature_key(secret, date, "us-east-1", "ses");
    let string_to_sign = format!(
        "AWS4-HMAC-SHA256\n{amz_date}\n{date}/us-east-1/ses/aws4_request\n{}",
        sha256_hex(canonical_request.as_bytes())
    );
    let mut mac = HmacSha256::new_from_slice(&key).expect("signature key should be valid");
    mac.update(string_to_sign.as_bytes());
    hex::encode(mac.finalize().into_bytes())
}

async fn signed_ses_request(
    body: &str,
    access_key: &str,
    secret_key: &str,
) -> hyper::Request<Full<Bytes>> {
    let body = body.as_bytes().to_vec();
    let signed_headers = ["host", "x-amz-content-sha256", "x-amz-date"];
    let canonical = canonical_request(&body, &signed_headers).await;
    let signature = sign_signature(secret_key, &canonical, "20260101", "20260101T120000Z");
    let auth_header = format!(
        "AWS4-HMAC-SHA256 Credential={access_key}/20260101/us-east-1/ses/aws4_request, SignedHeaders=host;x-amz-content-sha256;x-amz-date, Signature={signature}"
    );

    request(
        "POST",
        "http://localhost/v2/email/outbound-emails",
        &[
            ("authorization", auth_header.as_str()),
            ("host", "localhost:9000"),
            ("x-amz-date", "20260101T120000Z"),
            ("x-amz-content-sha256", "UNSIGNED-PAYLOAD"),
            ("content-type", "application/json"),
        ],
        &body,
    )
}
