mod common;

use common::interop::{auth_enabled, call, request, temp_storage, AZURE_VERSION};
use hyper::StatusCode;

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
            &[("host", "storage.googleapis.com")],
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
        request("GET", "http://localhost/n/tenant", &[], b""),
    )
    .await;
    assert!(matches!(
        response.status(),
        StatusCode::FORBIDDEN | StatusCode::UNAUTHORIZED
    ));
}
