mod common;

use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use common::interop::{
    auth_disabled, body_bytes, body_text, call, extract_tag, request, AZURE_VERSION,
};
use hyper::StatusCode;
use sqrzl_emulator::storage::{FilesystemStorage, Storage};
use std::sync::Arc;

struct Fixture(std::path::PathBuf);

impl Fixture {
    fn new() -> Self {
        Self(std::env::temp_dir().join(format!("sqrzl-parity-{}", uuid::Uuid::new_v4())))
    }

    fn open(&self) -> Arc<dyn Storage> {
        Arc::new(FilesystemStorage::new(&self.0))
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn should_validate_s3_part_md5_before_replacing_an_acknowledged_part() {
    // Arrange
    let fixture = Fixture::new();
    let storage = fixture.open();
    storage.create_bucket("parts".to_string()).unwrap();
    let upload = body_text(
        call(
            storage.clone(),
            auth_disabled(),
            request("POST", "http://localhost/parts/key?uploads", &[], b""),
        )
        .await,
    )
    .await;
    let upload_id = extract_tag(&upload, "UploadId").unwrap();
    let uri = format!("http://localhost/parts/key?uploadId={upload_id}&partNumber=1");
    let digest = BASE64.encode(md5::compute(b"original").0);
    let original = call(
        storage.clone(),
        auth_disabled(),
        request("PUT", &uri, &[("content-md5", &digest)], b"original"),
    )
    .await;
    assert_eq!(original.status(), StatusCode::OK);
    let etag = original.headers()["etag"].to_str().unwrap().to_string();

    // Act
    // Assert
    for (digest, code) in [
        ("AAAAAA==", "InvalidDigest"),
        ("not-base64", "InvalidDigest"),
        ("AAAAAAAAAAAAAAAAAAAAAA==", "BadDigest"),
    ] {
        let response = call(
            storage.clone(),
            auth_disabled(),
            request("PUT", &uri, &[("content-md5", digest)], b"changed"),
        )
        .await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{digest}");
        assert_eq!(
            extract_tag(&body_text(response).await, "Code").as_deref(),
            Some(code)
        );
    }
    let response = call(
        storage.clone(),
        auth_disabled(),
        request(
            "PUT",
            &uri,
            &[("content-md5", &digest), ("content-md5", &digest)],
            b"changed",
        ),
    )
    .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        extract_tag(&body_text(response).await, "Code").as_deref(),
        Some("InvalidDigest")
    );
    drop(storage);
    let storage = fixture.open();
    let manifest = format!("<CompleteMultipartUpload><Part><PartNumber>1</PartNumber><ETag>{etag}</ETag></Part></CompleteMultipartUpload>");
    let response = call(
        storage.clone(),
        auth_disabled(),
        request(
            "POST",
            &format!("http://localhost/parts/key?uploadId={upload_id}"),
            &[],
            manifest.as_bytes(),
        ),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        storage.get_object("parts", "key").unwrap().data,
        b"original"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn should_validate_gcs_xml_integrity_headers_before_publishing_an_object() {
    // Arrange
    let fixture = Fixture::new();
    let storage = fixture.open();
    storage.create_bucket("checksums".to_string()).unwrap();
    let digest = BASE64.encode(md5::compute(b"original").0);
    let hashes = format!(
        "md5={digest},crc32c={}",
        BASE64.encode(crc32c::crc32c(b"original").to_be_bytes())
    );
    let original = call(
        storage.clone(),
        auth_disabled(),
        request(
            "PUT",
            "http://localhost/checksums/key",
            &[
                ("host", "storage.googleapis.com"),
                ("content-length", "8"),
                ("content-md5", &digest),
                ("x-goog-hash", &hashes),
            ],
            b"original",
        ),
    )
    .await;
    assert_eq!(original.status(), StatusCode::OK);
    let identity = storage.get_object_metadata("checksums", "key").unwrap();

    // Act
    // Assert
    for (name, digest, code) in [
        ("content-md5", "AAAAAA==", "InvalidDigest"),
        ("content-md5", "not-base64", "InvalidDigest"),
        ("content-md5", "\u{00e9}", "InvalidDigest"),
        ("content-md5", "AAAAAAAAAAAAAAAAAAAAAA==", "BadDigest"),
        ("x-goog-hash", "md5=AAAAAAAAAAAAAAAAAAAAAA==", "BadDigest"),
        ("x-goog-hash", "crc32c=AAAAAA==", "BadDigest"),
        ("x-goog-hash", "crc32c=not-base64", "InvalidHash"),
        ("x-goog-hash", "sha256=AAAAAA==", "InvalidHash"),
        ("x-goog-hash", "", "InvalidHash"),
        (
            "x-goog-hash",
            "md5=AAAAAAAAAAAAAAAAAAAAAA==,md5=AQEBAQEBAQEBAQEBAQEBAQ==",
            "InvalidHash",
        ),
    ] {
        let response = call(
            storage.clone(),
            auth_disabled(),
            request(
                "PUT",
                "http://localhost/checksums/key",
                &[
                    ("host", "storage.googleapis.com"),
                    ("content-length", "7"),
                    (name, digest),
                ],
                b"changed",
            ),
        )
        .await;
        assert_eq!(
            response.status(),
            StatusCode::BAD_REQUEST,
            "{name}: {digest}"
        );
        assert_eq!(
            extract_tag(&body_text(response).await, "Code").as_deref(),
            Some(code)
        );
        let unchanged = storage.get_object_metadata("checksums", "key").unwrap();
        assert_eq!(unchanged.etag, identity.etag);
        assert_eq!(unchanged.metadata, identity.metadata);
        assert_eq!(unchanged.last_modified, identity.last_modified);
    }
    drop(storage);
    let response = call(
        fixture.open(),
        auth_disabled(),
        request(
            "GET",
            "http://localhost/checksums/key",
            &[("host", "storage.googleapis.com"), ("content-length", "0")],
            b"",
        ),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(body_bytes(response).await, b"original");
}

#[tokio::test(flavor = "multi_thread")]
async fn should_enforce_azure_listing_include_contracts() {
    // Arrange
    let fixture = Fixture::new();
    let storage = fixture.open();
    storage.create_bucket("listing".to_string()).unwrap();
    let created = call(
        storage.clone(),
        auth_disabled(),
        request(
            "PUT",
            "http://localhost/devstoreaccount1/listing/key",
            &[
                ("x-ms-version", AZURE_VERSION),
                ("content-length", "8"),
                ("x-ms-blob-type", "BlockBlob"),
                ("x-ms-meta-owner", "a&b<c>"),
            ],
            b"original",
        ),
    )
    .await;
    assert_eq!(created.status(), StatusCode::CREATED);
    let uri = "http://localhost/devstoreaccount1/listing?restype=container&comp=list";

    // Act
    let response = call(
        storage.clone(),
        auth_disabled(),
        request(
            "GET",
            &format!("{uri}&include=metadata"),
            &[("x-ms-version", AZURE_VERSION)],
            b"",
        ),
    )
    .await;

    // Assert
    assert_eq!(response.status(), StatusCode::OK);
    assert!(body_text(response)
        .await
        .contains("<Metadata><owner>a&amp;b&lt;c&gt;</owner></Metadata>"));
    let response = call(
        storage.clone(),
        auth_disabled(),
        request("GET", uri, &[("x-ms-version", AZURE_VERSION)], b""),
    )
    .await;
    assert!(!body_text(response).await.contains("<Metadata>"));
    for include in [
        "snapshots",
        "versions",
        "deleted",
        "tags",
        "uncommittedblobs",
        "metadata,snapshots",
    ] {
        let response = call(
            storage.clone(),
            auth_disabled(),
            request(
                "GET",
                &format!("{uri}&include={include}"),
                &[("x-ms-version", AZURE_VERSION)],
                b"",
            ),
        )
        .await;
        assert_eq!(response.status(), StatusCode::NOT_IMPLEMENTED, "{include}");
        assert_eq!(response.headers()["x-ms-error-code"], "FeatureNotSupported");
    }
    let response = call(
        storage,
        auth_disabled(),
        request(
            "GET",
            &format!("{uri}&include=invalid"),
            &[("x-ms-version", AZURE_VERSION)],
            b"",
        ),
    )
    .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        response.headers()["x-ms-error-code"],
        "InvalidQueryParameterValue"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn should_reject_unrepresentable_azure_listing_metadata() {
    // Arrange
    let fixture = Fixture::new();
    let storage = fixture.open();
    storage.create_bucket("listing".to_string()).unwrap();
    let mut object = sqrzl_emulator::models::Object::new(
        "key".to_string(),
        b"original".to_vec(),
        "text/plain".to_string(),
    );
    object
        .metadata
        .insert("1invalid".to_string(), "foreign metadata".to_string());
    storage
        .put_object("listing", "key".to_string(), object)
        .unwrap();
    let uri = "http://localhost/devstoreaccount1/listing?restype=container&comp=list";

    // Act
    let response = call(
        storage.clone(),
        auth_disabled(),
        request(
            "GET",
            &format!("{uri}&include=metadata"),
            &[("x-ms-version", AZURE_VERSION)],
            b"",
        ),
    )
    .await;

    // Assert
    assert_eq!(response.status(), StatusCode::NOT_IMPLEMENTED);
    assert_eq!(response.headers()["x-ms-error-code"], "FeatureNotSupported");
    let response = call(
        storage.clone(),
        auth_disabled(),
        request("GET", uri, &[("x-ms-version", AZURE_VERSION)], b""),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        storage.get_object("listing", "key").unwrap().data,
        b"original"
    );
}
