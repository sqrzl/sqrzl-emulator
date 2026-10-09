mod common;

use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use chrono::{TimeDelta, Utc};
use common::interop::{auth_enabled, body_text, temp_storage};
use hmac::{Hmac, KeyInit, Mac};
use http_body_util::Full;
use hyper::{Request, Response, StatusCode};
use sha2::{Digest, Sha256};
use sqrzl_emulator::mail::providers::MailAdapterRegistry;
use sqrzl_emulator::mail::{FilesystemMailStore, MailStore};
use sqrzl_emulator::providers::AdapterRegistry;
use sqrzl_emulator::server::RequestExt;
use sqrzl_emulator::sms::providers::SmsAdapterRegistry;
use sqrzl_emulator::sms::{FilesystemSmsStore, SmsStore};
use sqrzl_emulator::storage::Storage;
use std::fmt::Write as _;
use std::sync::Arc;

type RequestBody = Full<bytes::Bytes>;

struct Fixture {
    storage: Arc<dyn Storage>,
    mail: Arc<dyn MailStore>,
    sms: Arc<dyn SmsStore>,
    root: std::path::PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let root =
            std::env::temp_dir().join(format!("sqrzl-auth-contract-{}", uuid::Uuid::new_v4()));
        Self {
            storage: temp_storage(),
            mail: Arc::new(FilesystemMailStore::open(&root).unwrap()),
            sms: Arc::new(FilesystemSmsStore::open(&root).unwrap()),
            root,
        }
    }

    async fn call(&self, request: Request<RequestBody>) -> Response<sqrzl_emulator::body::Body> {
        let request = RequestExt::from_hyper(request).await.unwrap();
        let auth = auth_enabled("audit-access", "audit-secret");
        if request.path().starts_with("/v2/") || request.path().starts_with("/emails:") {
            return MailAdapterRegistry::default()
                .route(self.mail.clone(), auth, request)
                .await
                .unwrap()
                .unwrap();
        }
        if request.method() == "POST" {
            return SmsAdapterRegistry::default()
                .route(self.sms.clone(), auth, request)
                .await
                .unwrap()
                .unwrap();
        }
        AdapterRegistry::default()
            .handle(self.storage.clone(), auth, request)
            .await
            .unwrap()
    }

    fn assert_no_capture(&self) {
        assert!(self.mail.list_mailboxes().unwrap().is_empty());
        assert!(self.sms.list_conversations().unwrap().is_empty());
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

struct Operation {
    service: &'static str,
    path: &'static str,
    body: &'static str,
    content_type: &'static str,
    target: Option<&'static str>,
}

fn operations() -> [Operation; 4] {
    [
        Operation {
            service: "s3",
            path: "/auth-contract",
            body: "",
            content_type: "application/octet-stream",
            target: None,
        },
        Operation {
            service: "sns",
            path: "/",
            body: "Action=Publish&Version=2010-03-31&PhoneNumber=%2B15551234567&Message=audit",
            content_type: "application/x-www-form-urlencoded",
            target: None,
        },
        Operation {
            service: "ses",
            path: "/v2/email/outbound-emails",
            body: r#"{"FromEmailAddress":"sender@example.com","Destination":{"ToAddresses":["recipient@example.com"]},"Content":{"Simple":{"Subject":{"Data":"audit"},"Body":{"Text":{"Data":"body"}}}}}"#,
            content_type: "application/json",
            target: None,
        },
        Operation {
            service: "sms-voice",
            path: "/",
            body: r#"{"DestinationPhoneNumber":"+15551234567","MessageBody":"audit"}"#,
            content_type: "application/x-amz-json-1.0",
            target: Some("PinpointSMSVoiceV2.SendTextMessage"),
        },
    ]
}

fn digest(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

fn mac(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut mac = Hmac::<Sha256>::new_from_slice(key).unwrap();
    mac.update(data);
    mac.finalize().into_bytes().to_vec()
}

fn presigned_request(
    path: &str,
    date: chrono::DateTime<Utc>,
    expires: i64,
    signed_names: &str,
    headers: &[(&str, &str)],
    body: &str,
) -> Request<RequestBody> {
    // Independent signer: query authentication always canonicalizes UNSIGNED-PAYLOAD.
    let stamp = date.format("%Y%m%d").to_string();
    let date = date.format("%Y%m%dT%H%M%SZ").to_string();
    let scope = format!("{stamp}/us-east-1/s3/aws4_request");
    let mut params = [
        ("X-Amz-Algorithm", "AWS4-HMAC-SHA256".to_string()),
        ("X-Amz-Credential", format!("audit-access/{scope}")),
        ("X-Amz-Date", date.clone()),
        ("X-Amz-Expires", expires.to_string()),
        ("X-Amz-SignedHeaders", signed_names.to_string()),
    ];
    params.sort();
    let query = params
        .iter()
        .map(|(name, value)| format!("{name}={}", urlencoding::encode(value)))
        .collect::<Vec<_>>()
        .join("&");
    let names = signed_names
        .split(';')
        .map(|name| name.trim().to_ascii_lowercase())
        .filter(|name| !name.is_empty())
        .collect::<Vec<_>>();
    let canonical_headers = names.iter().fold(String::new(), |mut output, name| {
        let value = if name == "host" {
            "localhost"
        } else {
            headers
                .iter()
                .find(|(header, _)| *header == name)
                .unwrap()
                .1
        };
        writeln!(output, "{name}:{value}").unwrap();
        output
    });
    let canonical = format!(
        "PUT\n{path}\n{query}\n{canonical_headers}\n{}\nUNSIGNED-PAYLOAD",
        names.join(";")
    );
    let key = mac(b"AWS4audit-secret", stamp.as_bytes());
    let key = mac(&key, b"us-east-1");
    let key = mac(&key, b"s3");
    let key = mac(&key, b"aws4_request");
    let signature = hex::encode(mac(
        &key,
        format!(
            "AWS4-HMAC-SHA256\n{date}\n{scope}\n{}",
            digest(canonical.as_bytes())
        )
        .as_bytes(),
    ));
    let mut request = Request::builder()
        .method("PUT")
        .uri(format!(
            "http://localhost{path}?{query}&X-Amz-Signature={signature}"
        ))
        .header("host", "localhost");
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    request.body(RequestBody::from(body.to_string())).unwrap()
}

async fn assert_s3_failure(
    response: Response<sqrzl_emulator::body::Body>,
    status: StatusCode,
    code: &str,
) {
    assert_eq!(response.status(), status);
    let body = body_text(response).await;
    assert!(body.contains(&format!("<Code>{code}</Code>")), "{body}");
}

async fn assert_aws_auth_failure(
    response: Response<sqrzl_emulator::body::Body>,
    service: &str,
    s3_status: StatusCode,
    s3_code: &str,
) {
    match service {
        "s3" => assert_s3_failure(response, s3_status, s3_code).await,
        "sns" => assert_s3_failure(response, StatusCode::FORBIDDEN, "AuthorizationError").await,
        "ses" | "sms-voice" => {
            let (status, code) = if service == "ses" {
                (StatusCode::FORBIDDEN, "MissingAuthenticationTokenException")
            } else {
                (StatusCode::BAD_REQUEST, "AccessDeniedException")
            };
            assert_eq!(response.status(), status);
            assert_eq!(response.headers().get("x-amzn-errortype").unwrap(), code);
            let body: serde_json::Value = serde_json::from_str(&body_text(response).await).unwrap();
            assert!(body["message"]
                .as_str()
                .is_some_and(|message| !message.is_empty()));
        }
        _ => panic!("Unexpected service: {service}"),
    }
}

fn with_signed_names(
    mut request: Request<RequestBody>,
    raw_names: &str,
    service: &str,
) -> Request<RequestBody> {
    // Calculate a valid HMAC for the old permissive parser, independently of production code.
    let mut names = raw_names
        .split(';')
        .map(|name| name.trim().to_ascii_lowercase())
        .filter(|name| !name.is_empty())
        .collect::<Vec<_>>();
    names.sort();
    let canonical_headers = names.iter().fold(String::new(), |mut output, name| {
        writeln!(
            output,
            "{name}:{}",
            request.headers().get(name).unwrap().to_str().unwrap()
        )
        .unwrap();
        output
    });
    let date = request
        .headers()
        .get("x-amz-date")
        .unwrap()
        .to_str()
        .unwrap();
    let stamp = &date[..8];
    let scope = format!("{stamp}/us-east-1/{service}/aws4_request");
    let hash = request
        .headers()
        .get("x-amz-content-sha256")
        .unwrap()
        .to_str()
        .unwrap();
    let canonical = format!(
        "{}\n{}\n\n{canonical_headers}\n{}\n{hash}",
        request.method(),
        request.uri().path(),
        names.join(";")
    );
    let key = mac(b"AWS4audit-secret", stamp.as_bytes());
    let key = mac(&key, b"us-east-1");
    let key = mac(&key, service.as_bytes());
    let key = mac(&key, b"aws4_request");
    let signature = hex::encode(mac(
        &key,
        format!(
            "AWS4-HMAC-SHA256\n{date}\n{scope}\n{}",
            digest(canonical.as_bytes())
        )
        .as_bytes(),
    ));
    request.headers_mut().insert("authorization",
        format!("AWS4-HMAC-SHA256 Credential=audit-access/{scope}, SignedHeaders={raw_names}, Signature={signature}").parse().unwrap());
    request
}

#[tokio::test(flavor = "multi_thread")]
async fn should_reject_malformed_signed_names_given_all_aws_front_doors_when_hmac_matches() {
    // Arrange
    let date = Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
    for operation in operations() {
        let target = if operation.target.is_some() {
            ";x-amz-target"
        } else {
            ""
        };
        for names in [
            format!("Host;x-amz-content-sha256;x-amz-date{target}"),
            format!("host;x-amz-content-sha256;x-amz-date{target};"),
            format!("host;host;x-amz-content-sha256;x-amz-date{target}"),
            format!("x-amz-content-sha256;host;x-amz-date{target}"),
        ] {
            let fixture = Fixture::new();
            let request = with_signed_names(
                aws_request(
                    &operation,
                    &date,
                    &digest(operation.body.as_bytes()),
                    None,
                    false,
                ),
                &names,
                operation.service,
            );
            // Act
            let response = fixture.call(request).await;
            // Assert
            assert_aws_auth_failure(
                response,
                operation.service,
                StatusCode::FORBIDDEN,
                "SignatureDoesNotMatch",
            )
            .await;
            fixture.assert_no_capture();
            assert!(fixture.storage.list_buckets().unwrap().is_empty());
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn should_reject_future_presigns_given_bucket_and_object_when_valid_hmac_is_supplied() {
    // Arrange
    for path in ["/auth-contract", "/auth-contract/object"] {
        let fixture = Fixture::new();
        if path.ends_with("/object") {
            fixture
                .storage
                .create_bucket("auth-contract".into())
                .unwrap();
        }
        let request = presigned_request(
            path,
            Utc::now() + TimeDelta::hours(1),
            604_800,
            "host",
            &[],
            "future",
        );
        // Act
        let response = fixture.call(request).await;
        // Assert
        assert_s3_failure(response, StatusCode::FORBIDDEN, "RequestTimeTooSkewed").await;
        assert!(fixture
            .storage
            .get_object("auth-contract", "object")
            .is_err());
        assert_eq!(
            fixture.storage.list_buckets().unwrap().len(),
            usize::from(path.ends_with("/object"))
        );
        fixture.assert_no_capture();
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn should_require_signed_amz_headers_given_presign_when_native_request_headers_are_supplied()
{
    // Arrange
    for names in [
        "host",
        "Host;x-amz-meta-color",
        "host;x-amz-meta-color;",
        "host;host;x-amz-meta-color",
        "x-amz-meta-color;host",
    ] {
        let fixture = Fixture::new();
        fixture
            .storage
            .create_bucket("auth-contract".into())
            .unwrap();
        let request = presigned_request(
            "/auth-contract/object",
            Utc::now(),
            300,
            names,
            &[("x-amz-meta-color", "red")],
            "unsigned metadata",
        );
        // Act
        let response = fixture.call(request).await;
        // Assert
        assert_s3_failure(response, StatusCode::FORBIDDEN, "SignatureDoesNotMatch").await;
        assert!(fixture
            .storage
            .get_object("auth-contract", "object")
            .is_err());
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn should_reject_conflicting_signed_duplicates_given_bucket_and_object_presigns_when_hmac_matches(
) {
    // Arrange
    for path in ["/auth-contract", "/auth-contract/object"] {
        let fixture = Fixture::new();
        if path.ends_with("/object") {
            fixture
                .storage
                .create_bucket("auth-contract".into())
                .unwrap();
        }
        let date = Utc::now();
        let header_date = (date + TimeDelta::minutes(1))
            .format("%Y%m%dT%H%M%SZ")
            .to_string();
        let request = presigned_request(
            path,
            date,
            300,
            "host;x-amz-date",
            &[("x-amz-date", &header_date)],
            "conflicting date",
        );
        // Act
        let response = fixture.call(request).await;
        // Assert
        assert_s3_failure(response, StatusCode::BAD_REQUEST, "InvalidRequest").await;
        assert!(fixture
            .storage
            .get_object("auth-contract", "object")
            .is_err());
        assert_eq!(
            fixture.storage.list_buckets().unwrap().len(),
            usize::from(path.ends_with("/object"))
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn should_accept_matching_signed_duplicates_and_long_lived_presigns_given_valid_native_requests(
) {
    // Arrange
    for date in [Utc::now(), Utc::now() - TimeDelta::hours(2)] {
        let fixture = Fixture::new();
        fixture
            .storage
            .create_bucket("auth-contract".into())
            .unwrap();
        let header_date = date.format("%Y%m%dT%H%M%SZ").to_string();
        let hash = digest(b"signed metadata");
        let request = presigned_request(
            "/auth-contract/object",
            date,
            86_400,
            "host;x-amz-content-sha256;x-amz-date;x-amz-meta-color",
            &[
                ("x-amz-content-sha256", &hash),
                ("x-amz-date", &header_date),
                ("x-amz-meta-color", "red"),
            ],
            "signed metadata",
        );
        // Act
        let response = fixture.call(request).await;
        // Assert
        assert_eq!(
            response.status(),
            StatusCode::OK,
            "{}",
            body_text(response).await
        );
        assert!(fixture
            .storage
            .get_object("auth-contract", "object")
            .is_ok());
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn should_distinguish_malformed_sha_from_unsupported_modes_given_signed_s3_when_no_mutation_is_allowed(
) {
    // Arrange
    let date = Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
    for (hash, status, code) in [
        ("broken", StatusCode::BAD_REQUEST, "InvalidArgument"),
        ("", StatusCode::BAD_REQUEST, "InvalidArgument"),
        (
            "STREAMING-AWS4-HMAC-SHA256-PAYLOAD",
            StatusCode::NOT_IMPLEMENTED,
            "NotImplemented",
        ),
        (
            "STREAMING-AWS4-HMAC-SHA256-PAYLOAD-TRAILER",
            StatusCode::NOT_IMPLEMENTED,
            "NotImplemented",
        ),
        (
            "STREAMING-UNSIGNED-PAYLOAD-TRAILER",
            StatusCode::NOT_IMPLEMENTED,
            "NotImplemented",
        ),
    ] {
        let fixture = Fixture::new();
        let operation = &operations()[0];
        // Act
        let response = fixture
            .call(aws_request(operation, &date, hash, None, false))
            .await;
        // Assert
        assert_s3_failure(response, status, code).await;
        assert!(fixture.storage.list_buckets().unwrap().is_empty());
    }
}

fn aws_request(
    operation: &Operation,
    date: &str,
    payload_hash: &str,
    scope_date: Option<&str>,
    omit_date_signature: bool,
) -> Request<RequestBody> {
    aws_request_with_date_header(
        operation,
        date,
        payload_hash,
        scope_date,
        omit_date_signature,
        "x-amz-date",
    )
}

fn aws_request_with_date_header(
    operation: &Operation,
    date: &str,
    payload_hash: &str,
    scope_date: Option<&str>,
    omit_date_signature: bool,
    date_header: &str,
) -> Request<RequestBody> {
    let method = if operation.service == "s3" {
        "PUT"
    } else {
        "POST"
    };
    let mut headers = vec![
        ("host", "localhost"),
        ("x-amz-content-sha256", payload_hash),
    ];
    if !omit_date_signature {
        headers.push((date_header, date));
    }
    if let Some(target) = operation.target {
        headers.push(("x-amz-target", target));
    }
    headers.sort_unstable();
    let names = headers
        .iter()
        .map(|(name, _)| *name)
        .collect::<Vec<_>>()
        .join(";");
    let canonical_headers = headers
        .iter()
        .fold(String::new(), |mut output, (name, value)| {
            writeln!(output, "{name}:{value}").unwrap();
            output
        });
    let canonical = format!(
        "{method}\n{}\n\n{canonical_headers}\n{names}\n{payload_hash}",
        operation.path
    );
    let stamp = scope_date.unwrap_or_else(|| date.get(..8).unwrap_or("20200101"));
    let scope = format!("{stamp}/us-east-1/{}/aws4_request", operation.service);
    let key = mac(b"AWS4audit-secret", stamp.as_bytes());
    let key = mac(&key, b"us-east-1");
    let key = mac(&key, operation.service.as_bytes());
    let key = mac(&key, b"aws4_request");
    let signature = hex::encode(mac(
        &key,
        format!(
            "AWS4-HMAC-SHA256\n{date}\n{scope}\n{}",
            digest(canonical.as_bytes())
        )
        .as_bytes(),
    ));
    let mut builder = Request::builder()
        .method(method)
        .uri(format!("http://localhost{}", operation.path))
        .header("host", "localhost")
        .header("content-type", operation.content_type)
        .header(date_header, date)
        .header("x-amz-content-sha256", payload_hash)
        .header("authorization", format!("AWS4-HMAC-SHA256 Credential=audit-access/{scope}, SignedHeaders={names}, Signature={signature}"));
    if let Some(target) = operation.target {
        builder = builder.header("x-amz-target", target);
    }
    builder
        .body(RequestBody::from(operation.body.to_string()))
        .unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn should_accept_date_only_iso8601_signatures_given_aws_front_doors_when_x_amz_date_is_absent(
) {
    // Arrange
    let date = Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
    for operation in operations() {
        let fixture = Fixture::new();

        // Act
        let response = fixture
            .call(aws_request_with_date_header(
                &operation,
                &date,
                &digest(operation.body.as_bytes()),
                None,
                false,
                "date",
            ))
            .await;

        // Assert
        assert!(
            response.status().is_success(),
            "{} rejects native Date-only signature: {}",
            operation.service,
            response.status()
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn should_reject_invalid_signed_dates_given_aws_front_doors_when_auth_is_enforced() {
    // Arrange
    let now = Utc::now();
    let dates = [
        (now - TimeDelta::hours(1))
            .format("%Y%m%dT%H%M%SZ")
            .to_string(),
        (now + TimeDelta::hours(1))
            .format("%Y%m%dT%H%M%SZ")
            .to_string(),
        "not-a-date".to_string(),
    ];
    for operation in operations() {
        for date in &dates {
            for date_header in ["x-amz-date", "date"] {
                let fixture = Fixture::new();
                // Act
                let response = fixture
                    .call(aws_request_with_date_header(
                        &operation,
                        date,
                        &digest(operation.body.as_bytes()),
                        None,
                        false,
                        date_header,
                    ))
                    .await;
                // Assert
                let (status, code) = if date == "not-a-date" {
                    (StatusCode::BAD_REQUEST, "InvalidRequest")
                } else {
                    (StatusCode::FORBIDDEN, "RequestTimeTooSkewed")
                };
                assert_aws_auth_failure(response, operation.service, status, code).await;
                assert!(fixture.storage.list_buckets().unwrap().is_empty());
                fixture.assert_no_capture();
            }
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn should_reject_payload_hash_mismatch_given_aws_front_doors_when_signed_body_differs() {
    // Arrange
    let date = Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
    for operation in operations() {
        let fixture = Fixture::new();
        // Act
        let response = fixture
            .call(aws_request(&operation, &date, &"0".repeat(64), None, false))
            .await;
        // Assert
        assert_aws_auth_failure(
            response,
            operation.service,
            StatusCode::BAD_REQUEST,
            "XAmzContentSHA256Mismatch",
        )
        .await;
        assert!(fixture.storage.list_buckets().unwrap().is_empty());
        fixture.assert_no_capture();
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn should_reject_scope_date_and_unsigned_timestamp_given_aws_front_doors_when_hmac_is_valid()
{
    // Arrange
    let date = Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
    for operation in operations() {
        for (scope_date, omit_date) in [(Some("20200101"), false), (None, true)] {
            let fixture = Fixture::new();
            // Act
            let response = fixture
                .call(aws_request(
                    &operation,
                    &date,
                    &digest(operation.body.as_bytes()),
                    scope_date,
                    omit_date,
                ))
                .await;
            // Assert
            assert_aws_auth_failure(
                response,
                operation.service,
                StatusCode::FORBIDDEN,
                "SignatureDoesNotMatch",
            )
            .await;
            assert!(fixture.storage.list_buckets().unwrap().is_empty());
            fixture.assert_no_capture();
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn should_preserve_valid_signed_workflows_given_aws_front_doors_when_date_and_body_match() {
    // Arrange
    let fixture = Fixture::new();
    let date = Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
    for operation in operations() {
        // Act
        let response = fixture
            .call(aws_request(
                &operation,
                &date,
                &digest(operation.body.as_bytes()),
                None,
                false,
            ))
            .await;
        // Assert
        assert!(
            response.status().is_success(),
            "{}: {}",
            operation.service,
            body_text(response).await
        );
    }
    assert_eq!(fixture.storage.list_buckets().unwrap().len(), 1);
    assert_eq!(fixture.mail.list_mailboxes().unwrap().len(), 1);
    assert_eq!(fixture.sms.list_conversations().unwrap().len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn should_reject_invalid_acs_hmac_dates_given_email_and_sms_when_auth_is_configured() {
    // Arrange
    let previous = std::env::var("SQRZL_ACS_CONNECTION_STRING").ok();
    let key = BASE64.encode(b"audit-secret");
    std::env::set_var(
        "SQRZL_ACS_CONNECTION_STRING",
        format!("endpoint=http://localhost;accesskey={key}"),
    );
    let now = Utc::now();
    let mut failures = Vec::new();
    for (path, body) in [
        (
            "/emails:send?api-version=2023-03-31",
            r#"{"senderAddress":"sender@example.com","recipients":{"to":[{"address":"recipient@example.com"}]},"content":{"subject":"audit","plainText":"body"}}"#,
        ),
        (
            "/sms?api-version=2021-03-07",
            r#"{"from":"+15550000001","smsRecipients":[{"to":"+15551234567"}],"message":"audit"}"#,
        ),
    ] {
        for date in [
            (now - TimeDelta::hours(1))
                .format("%a, %d %b %Y %H:%M:%S GMT")
                .to_string(),
            (now + TimeDelta::hours(1))
                .format("%a, %d %b %Y %H:%M:%S GMT")
                .to_string(),
            "not-a-date".to_string(),
        ] {
            let fixture = Fixture::new();
            let hash = BASE64.encode(Sha256::digest(body.as_bytes()));
            let canonical = format!("POST\n{path}\n{date};localhost;{hash}");
            let signature = BASE64.encode(mac(b"audit-secret", canonical.as_bytes()));
            let request=Request::builder().method("POST").uri(format!("http://localhost{path}")).header("host","localhost").header("content-type","application/json").header("x-ms-date",&date).header("x-ms-content-sha256",hash).header("authorization",format!("HMAC-SHA256 SignedHeaders=x-ms-date;host;x-ms-content-sha256&Signature={signature}")).body(RequestBody::from(body)).unwrap();
            // Act
            let response = fixture.call(request).await;
            // Assert
            if response.status().is_success() {
                failures.push(format!("{path} {date}"));
            } else {
                assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
                let error: serde_json::Value =
                    serde_json::from_str(&body_text(response).await).unwrap();
                assert_eq!(error["error"]["code"], "Unauthorized");
                fixture.assert_no_capture();
            }
        }
    }
    if let Some(previous) = previous {
        std::env::set_var("SQRZL_ACS_CONNECTION_STRING", previous);
    } else {
        std::env::remove_var("SQRZL_ACS_CONNECTION_STRING");
    }
    assert!(
        failures.is_empty(),
        "accepted invalid ACS dates: {failures:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn should_preserve_streamed_object_given_authenticated_s3_when_body_hash_does_not_match() {
    // Arrange
    let server = common::e2e::LiveServer::start_s3(common::e2e::auth_enabled(
        "audit-access",
        "audit-secret",
    ))
    .await;
    let date = Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
    let mut create = aws_request(&operations()[0], &date, &digest(b""), None, false);
    *create.uri_mut() = format!("{}/auth-contract", server.base_url)
        .parse()
        .unwrap();
    assert_eq!(server.request(create).await.status(), StatusCode::OK);
    let operation = Operation {
        service: "s3",
        path: "/auth-contract/object",
        body: "original payload",
        content_type: "application/octet-stream",
        target: None,
    };
    let mut put = aws_request(
        &operation,
        &date,
        &digest(operation.body.as_bytes()),
        None,
        false,
    );
    *put.uri_mut() = format!("{}{}", server.base_url, operation.path)
        .parse()
        .unwrap();
    assert_eq!(server.request(put).await.status(), StatusCode::OK);
    let operation = Operation {
        body: "replacement payload",
        ..operation
    };
    let mut replace = aws_request(&operation, &date, &"0".repeat(64), None, false);
    *replace.uri_mut() = format!("{}{}", server.base_url, operation.path)
        .parse()
        .unwrap();
    // Act
    let denied = server.request(replace).await;
    // Assert
    assert_eq!(denied.status(), StatusCode::BAD_REQUEST);
    assert!(common::e2e::text_body(denied)
        .await
        .contains("XAmzContentSHA256Mismatch"));
    let url = sqrzl_emulator::auth::PresignedUrl::generate_get_url(
        "auth-contract",
        "object",
        300,
        &server.base_url,
        &sqrzl_emulator::auth::PresignedUrlConfig {
            access_key: "audit-access".into(),
            secret_key: "audit-secret".into(),
        },
    );
    let fetched = server
        .request(
            Request::builder()
                .uri(url)
                .body(RequestBody::default())
                .unwrap(),
        )
        .await;
    assert_eq!(fetched.status(), StatusCode::OK);
    assert_eq!(common::e2e::text_body(fetched).await, "original payload");
}

#[tokio::test(flavor = "multi_thread")]
async fn should_reject_provided_hash_mismatch_given_presigned_s3_when_payload_mode_is_unsigned() {
    // Arrange
    let fixture = Fixture::new();
    fixture
        .storage
        .create_bucket("auth-contract".into())
        .unwrap();
    let request = presigned_request(
        "/auth-contract/object",
        Utc::now(),
        300,
        "host;x-amz-content-sha256",
        &[("x-amz-content-sha256", &"0".repeat(64))],
        "changed payload",
    );
    // Act
    let denied = fixture.call(request).await;
    // Assert
    assert_eq!(denied.status(), StatusCode::BAD_REQUEST);
    assert!(body_text(denied)
        .await
        .contains("XAmzContentSHA256Mismatch"));
    assert!(fixture
        .storage
        .get_object("auth-contract", "object")
        .is_err());
}

#[tokio::test(flavor = "multi_thread")]
async fn should_reject_unsigned_payload_given_aws_messaging_when_no_unsigned_mode_is_supported() {
    // Arrange
    let date = Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
    for operation in operations().into_iter().skip(1) {
        let fixture = Fixture::new();
        // Act
        let response = fixture
            .call(aws_request(
                &operation,
                &date,
                "UNSIGNED-PAYLOAD",
                None,
                false,
            ))
            .await;
        // Assert
        assert_aws_auth_failure(
            response,
            operation.service,
            StatusCode::NOT_IMPLEMENTED,
            "NotImplemented",
        )
        .await;
        fixture.assert_no_capture();
    }
}
