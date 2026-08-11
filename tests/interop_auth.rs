mod common;

use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use common::interop::{auth_enabled, body_bytes, call, request, temp_storage, AZURE_VERSION};
use hmac::{Hmac, KeyInit, Mac};
use hyper::StatusCode;
use sha1::Sha1;
use sqrzl_emulator::models::Object;

#[tokio::test(flavor = "multi_thread")]
async fn should_authorize_gcs_v2_hmac_extension_headers_through_provider_registry() {
    // Arrange
    type HmacSha1 = Hmac<Sha1>;
    let storage = temp_storage();
    storage.create_bucket("private".to_string()).unwrap();
    storage
        .put_object(
            "private",
            "item.txt".to_string(),
            Object::new(
                "item.txt".to_string(),
                b"authenticated".to_vec(),
                "text/plain".to_string(),
            ),
        )
        .unwrap();
    let date = "Tue, 11 Aug 2026 12:00:00 GMT";
    let canonical = format!(
        "GET\n\n\n{date}\nx-goog-acl:public-read\nx-goog-meta-reviewer:jane,john\n/private/item.txt"
    );
    let mut mac = HmacSha1::new_from_slice(b"gcs-secret").expect("HMAC key should be valid");
    mac.update(canonical.as_bytes());
    let signature = BASE64.encode(mac.finalize().into_bytes());
    let authorization = format!("GOOG1 test-access:{signature}");

    // Act
    let response = call(
        storage,
        auth_enabled("test-access", "Z2NzLXNlY3JldA=="),
        request(
            "GET",
            "http://localhost/private/item.txt",
            &[
                ("date", date),
                ("authorization", &authorization),
                ("content-length", "0"),
                ("x-goog-meta-reviewer", "jane"),
                ("x-goog-acl", "public-read"),
                ("x-goog-meta-reviewer", "john"),
                ("x-goog-encryption-key", "sensitive-key"),
                ("x-goog-encryption-key-sha256", "sensitive-key-hash"),
            ],
            b"",
        ),
    )
    .await;

    // Assert
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(body_bytes(response).await, b"authenticated");
}

#[tokio::test(flavor = "multi_thread")]
async fn should_reject_unsigned_s3_request_given_auth_enforced_when_request_is_missing_signature() {
    let storage = temp_storage();
    let response = call(
        storage,
        auth_enabled("test", "test-secret"),
        request("GET", "http://localhost/", &[], b""),
    )
    .await;
    assert!(matches!(
        response.status(),
        StatusCode::FORBIDDEN | StatusCode::UNAUTHORIZED
    ));
}

#[tokio::test(flavor = "multi_thread")]
async fn should_reject_credential_only_s3_request_given_auth_enforced() {
    let storage = temp_storage();
    let response = call(
        storage,
        auth_enabled("test", "test-secret"),
        request(
            "GET",
            "http://localhost/?X-Amz-Credential=test%2F20260729%2Fus-east-1%2Fs3%2Faws4_request",
            &[],
            b"",
        ),
    )
    .await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
}

#[tokio::test(flavor = "multi_thread")]
async fn should_reject_unauthorized_azure_request_given_auth_enforced_when_listing_containers() {
    let storage = temp_storage();
    let response = call(
        storage,
        auth_enabled("azure-auth", "dG9wc2VjcmV0a2V5"),
        request(
            "GET",
            "http://localhost/devstoreaccount1?comp=list",
            &[("x-ms-version", AZURE_VERSION)],
            b"",
        ),
    )
    .await;
    assert!(matches!(
        response.status(),
        StatusCode::FORBIDDEN | StatusCode::UNAUTHORIZED
    ));
}

#[tokio::test(flavor = "multi_thread")]
async fn should_reject_invalid_signed_gcs_request_given_auth_enforced_when_signature_is_bad() {
    let storage = temp_storage();
    let response = call(
        storage,
        auth_enabled("test", "test-secret"),
        request(
            "GET",
            "http://localhost/missing?GoogleAccessId=wrong-access&Expires=4102444800&Signature=bad",
            &[("host", "storage.googleapis.com"), ("content-length", "0")],
            b"",
        ),
    )
    .await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
}

#[tokio::test(flavor = "multi_thread")]
async fn should_reject_unsigned_oci_request_given_auth_enforced_when_request_is_missing_signature()
{
    let storage = temp_storage();
    let response = call(
        storage,
        auth_enabled("oci-key", "oci-secret"),
        request("GET", "http://localhost/n/sqrzl-emulator", &[], b""),
    )
    .await;
    assert!(matches!(
        response.status(),
        StatusCode::FORBIDDEN | StatusCode::UNAUTHORIZED
    ));
}
