mod common;

use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use base64::Engine as _;
use bytes::Bytes;
use http_body_util::Full;
use hyper::{Request, StatusCode};
use serde_json::{json, Value};

use common::e2e::{auth_disabled, text_body, LiveServer};

async fn request(
    server: &LiveServer,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: Option<Value>,
) -> (StatusCode, String) {
    let mut builder = Request::builder()
        .method(method)
        .uri(format!("{}{path}", server.base_url));
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    let body = Full::new(body.map_or_else(Bytes::new, |body| Bytes::from(body.to_string())));
    let response = server.request(builder.body(body).unwrap()).await;
    let status = response.status();
    (status, text_body(response).await)
}

async fn aws(server: &LiveServer, operation: &str, parameters: Value) -> (StatusCode, Value) {
    aws_json(server, &format!("TrentService.{operation}"), parameters).await
}

async fn aws_secrets(
    server: &LiveServer,
    operation: &str,
    parameters: Value,
) -> (StatusCode, Value) {
    aws_json(server, &format!("secretsmanager.{operation}"), parameters).await
}

async fn aws_json(server: &LiveServer, target: &str, parameters: Value) -> (StatusCode, Value) {
    let (status, body) = request(
        server,
        "POST",
        "/",
        &[("x-amz-target", target)],
        Some(parameters),
    )
    .await;
    (status, serde_json::from_str(&body).unwrap())
}

#[tokio::test(flavor = "multi_thread")]
async fn should_read_current_and_previous_aws_secret_values_after_putting_a_binary_version() {
    // Arrange
    let server = LiveServer::start_api(auth_disabled()).await;
    let (created_status, created) = aws_secrets(
        &server,
        "CreateSecret",
        json!({"Name": "fixture/password", "SecretString": "original", "Description": "fixture"}),
    )
    .await;
    assert_eq!(created_status, StatusCode::OK);
    let arn = &created["ARN"];
    let binary = STANDARD.encode(b"\x00\xfffixture");

    // Act
    let (put_status, put) = aws_secrets(
        &server,
        "PutSecretValue",
        json!({"SecretId": arn, "SecretBinary": binary}),
    )
    .await;
    let (current_status, current) =
        aws_secrets(&server, "GetSecretValue", json!({"SecretId": arn})).await;
    let (previous_status, previous) = aws_secrets(
        &server,
        "GetSecretValue",
        json!({"SecretId": "fixture/password", "VersionStage": "AWSPREVIOUS"}),
    )
    .await;
    let (describe_status, metadata) =
        aws_secrets(&server, "DescribeSecret", json!({"SecretId": arn})).await;

    // Assert
    assert_eq!(put_status, StatusCode::OK);
    assert_eq!(current_status, StatusCode::OK);
    assert_eq!(previous_status, StatusCode::OK);
    assert_eq!(describe_status, StatusCode::OK);
    assert_eq!(current["VersionId"], put["VersionId"]);
    assert_eq!(current["VersionStages"], json!(["AWSCURRENT"]));
    assert_eq!(current["SecretBinary"], binary);
    assert_eq!(previous["VersionId"], created["VersionId"]);
    assert_eq!(previous["SecretString"], "original");
    assert_eq!(metadata["Description"], "fixture");
    assert_eq!(
        metadata["VersionIdsToStages"][created["VersionId"].as_str().unwrap()],
        json!(["AWSPREVIOUS"])
    );
    assert!(metadata.get("SecretString").is_none());
    assert!(metadata.get("SecretBinary").is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn should_reject_aws_secret_value_changes_given_a_reused_version_token() {
    // Arrange
    let server = LiveServer::start_api(auth_disabled()).await;
    let (created, _) = aws_secrets(&server, "CreateSecret", json!({"Name": "idempotent"})).await;
    assert_eq!(created, StatusCode::OK);
    let token = uuid::Uuid::new_v4().to_string();
    let fixture =
        json!({"SecretId": "idempotent", "ClientRequestToken": token, "SecretString": "first"});
    let (written, _) = aws_secrets(&server, "PutSecretValue", fixture.clone()).await;
    assert_eq!(written, StatusCode::OK);

    // Act
    let (retry_status, retry) = aws_secrets(&server, "PutSecretValue", fixture).await;
    let (conflict_status, conflict) = aws_secrets(
        &server,
        "PutSecretValue",
        json!({"SecretId": "idempotent", "ClientRequestToken": token, "SecretString": "changed"}),
    )
    .await;
    let (_, value) = aws_secrets(
        &server,
        "GetSecretValue",
        json!({"SecretId": "idempotent", "VersionId": token}),
    )
    .await;

    // Assert
    assert_eq!(retry_status, StatusCode::OK);
    assert_eq!(retry["VersionId"], token);
    assert_eq!(conflict_status, StatusCode::BAD_REQUEST);
    assert_eq!(conflict["__type"], "ResourceExistsException");
    assert_eq!(value["SecretString"], "first");
}

#[tokio::test(flavor = "multi_thread")]
async fn should_bind_aws_ciphertext_to_its_key_and_context_given_envelope_encryption() {
    // Arrange
    let server = LiveServer::start_api(auth_disabled()).await;
    let (status, response) = aws(&server, "CreateKey", json!({})).await;
    assert_eq!(status, StatusCode::OK);
    let key = &response["KeyMetadata"]["Arn"];
    let (_, data) = aws(
        &server,
        "GenerateDataKey",
        json!({
            "KeyId": key, "KeySpec": "AES_256", "EncryptionContext": {"tenant": "fixture"}
        }),
    )
    .await;

    // Act
    let (status, decrypted) = aws(
        &server,
        "Decrypt",
        json!({
            "CiphertextBlob": data["CiphertextBlob"], "EncryptionContext": {"tenant": "fixture"}
        }),
    )
    .await;
    let (_, rejected) = aws(
        &server,
        "Decrypt",
        json!({
            "CiphertextBlob": data["CiphertextBlob"], "EncryptionContext": {"tenant": "other"}
        }),
    )
    .await;

    // Assert
    assert_eq!(status, StatusCode::OK);
    assert_eq!(decrypted["Plaintext"], data["Plaintext"]);
    assert_eq!(
        STANDARD
            .decode(data["Plaintext"].as_str().unwrap())
            .unwrap()
            .len(),
        32
    );
    assert_eq!(rejected["__type"], "InvalidCiphertextException");
}

#[tokio::test(flavor = "multi_thread")]
async fn should_read_azure_secret_versions_given_multiple_set_operations() {
    // Arrange
    let server = LiveServer::start_api(auth_disabled()).await;
    let headers = [("x-sqrzl-vault", "azure-key-vault")];
    let path = "/secrets/example?api-version=7.6";
    let (_, first) = request(
        &server,
        "PUT",
        path,
        &headers,
        Some(json!({"value": "first"})),
    )
    .await;
    let first: Value = serde_json::from_str(&first).unwrap();
    let version = first["id"].as_str().unwrap().rsplit('/').next().unwrap();

    // Act
    let (status, _) = request(
        &server,
        "PUT",
        path,
        &headers,
        Some(json!({"value": "second"})),
    )
    .await;
    let (_, latest) = request(&server, "GET", path, &headers, None).await;
    let (_, original) = request(
        &server,
        "GET",
        &format!("/secrets/example/{version}?api-version=7.6"),
        &headers,
        None,
    )
    .await;

    // Assert
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        serde_json::from_str::<Value>(&latest).unwrap()["value"],
        "second"
    );
    assert_eq!(
        serde_json::from_str::<Value>(&original).unwrap()["value"],
        "first"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn should_round_trip_azure_wrapped_material_given_rsa_key() {
    // Arrange
    let server = LiveServer::start_api(auth_disabled()).await;
    let headers = [("x-sqrzl-vault", "azure-key-vault")];
    let (status, created) = request(
        &server,
        "POST",
        "/keys/example/create?api-version=7.6",
        &headers,
        Some(json!({"kty": "RSA"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let created: Value = serde_json::from_str(&created).unwrap();
    let version = created["key"]["kid"]
        .as_str()
        .unwrap()
        .rsplit('/')
        .next()
        .unwrap();
    let material = URL_SAFE_NO_PAD.encode(b"fixture material");

    // Act
    let (wrapped_status, wrapped) = request(
        &server,
        "POST",
        &format!("/keys/example/{version}/wrapkey?api-version=7.6"),
        &headers,
        Some(json!({"alg": "RSA-OAEP-256", "value": material})),
    )
    .await;
    let wrapped: Value = serde_json::from_str(&wrapped).unwrap();
    let (unwrapped_status, unwrapped) = request(
        &server,
        "POST",
        &format!("/keys/example/{version}/unwrapkey?api-version=7.6"),
        &headers,
        Some(json!({"alg": "RSA-OAEP-256", "value": wrapped["value"]})),
    )
    .await;

    // Assert
    assert_eq!(wrapped_status, StatusCode::OK);
    assert_eq!(unwrapped_status, StatusCode::OK);
    assert_eq!(
        serde_json::from_str::<Value>(&unwrapped).unwrap()["value"],
        material
    );
    assert!(created["key"].get("d").is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn should_route_google_cloud_kms_round_trips_given_a_service_discriminator() {
    // Arrange
    let server = LiveServer::start_api(auth_disabled()).await;
    let prefix = "/v1/projects/fixture/locations/global/keyRings";
    let headers = [("x-sqrzl-vault", "google-kms")];
    let (created, _) = request(
        &server,
        "POST",
        &format!("{prefix}?keyRingId=ring"),
        &headers,
        Some(json!({})),
    )
    .await;
    assert_eq!(created, StatusCode::OK);
    let key_collection = format!("{prefix}/ring/cryptoKeys");
    let (created, _) = request(
        &server,
        "POST",
        &format!("{key_collection}?cryptoKeyId=key"),
        &headers,
        Some(json!({"purpose": "ENCRYPT_DECRYPT"})),
    )
    .await;
    assert_eq!(created, StatusCode::OK);
    let plaintext = STANDARD.encode(b"google fixture");

    // Act
    let (encrypted, ciphertext) = request(
        &server,
        "POST",
        &format!("{key_collection}/key:encrypt"),
        &headers,
        Some(json!({"plaintext": plaintext})),
    )
    .await;
    let ciphertext: Value = serde_json::from_str(&ciphertext).unwrap();
    let (decrypted, response) = request(&server, "POST", &format!("{key_collection}/key:decrypt"), &headers, Some(json!({"ciphertext": ciphertext["ciphertext"], "ciphertextCrc32c": ciphertext["ciphertextCrc32c"]}))).await;
    let (_, storage) = request(&server, "GET", &format!("{key_collection}/key"), &[], None).await;

    // Assert
    assert_eq!(encrypted, StatusCode::OK);
    assert_eq!(decrypted, StatusCode::OK);
    assert_eq!(
        serde_json::from_str::<Value>(&response).unwrap()["plaintext"],
        plaintext
    );
    assert!(storage.contains("NoSuchBucket"));
}

#[tokio::test(flavor = "multi_thread")]
async fn should_keep_versioned_query_on_s3_given_a_vault_shaped_object_path() {
    // Arrange
    let server = LiveServer::start_api(auth_disabled()).await;
    let (status, _) = request(&server, "PUT", "/keys", &[], None).await;
    assert_eq!(status, StatusCode::OK);
    let fixture = json!({ "object": "S3 data" });
    let (status, _) = request(&server, "PUT", "/keys/report", &[], Some(fixture.clone())).await;
    assert_eq!(status, StatusCode::OK);

    // Act
    let (status, response) =
        request(&server, "GET", "/keys/report?api-version=7.6", &[], None).await;

    // Assert
    assert_eq!(status, StatusCode::OK);
    assert_eq!(serde_json::from_str::<Value>(&response).unwrap(), fixture);
}

#[tokio::test(flavor = "multi_thread")]
async fn should_reject_get_given_a_disabled_azure_secret_version() {
    // Arrange
    let server = LiveServer::start_api(auth_disabled()).await;
    let path = "/secrets/example?api-version=7.6";
    let headers = [("x-sqrzl-vault", "azure-key-vault")];
    let (status, created) = request(
        &server,
        "PUT",
        path,
        &headers,
        Some(json!({"value": "disabled fixture", "attributes": {"enabled": false}})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let created: Value = serde_json::from_str(&created).unwrap();
    let version = created["id"].as_str().unwrap().rsplit('/').next().unwrap();

    // Act
    let (status, response) = request(
        &server,
        "GET",
        &format!("/secrets/example/{version}?api-version=7.6"),
        &headers,
        None,
    )
    .await;

    // Assert
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(serde_json::from_str::<Value>(&response)
        .unwrap()
        .get("value")
        .is_none());
}
