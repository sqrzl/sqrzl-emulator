mod common;

use common::interop::{
    auth_disabled, body_text, call, extract_tag, request, temp_storage, AZURE_VERSION,
};
use http_body_util::{BodyExt, Full};
use hyper::StatusCode;
use sqrzl_emulator::body::Body as SqrzlBody;
use sqrzl_emulator::providers::AdapterRegistry;
use sqrzl_emulator::server::{RequestExt, RequestParseError};
use sqrzl_emulator::storage::{FilesystemStorage, Storage};
use std::sync::Arc;

#[derive(Clone, Copy, Debug)]
enum StorageFrontDoor {
    S3,
    Azure,
    GcsJson,
    GcsXml,
    Oci,
}

impl StorageFrontDoor {
    const fn label(self) -> &'static str {
        match self {
            Self::S3 => "S3",
            Self::Azure => "Azure Blob",
            Self::GcsJson => "GCS JSON",
            Self::GcsXml => "GCS XML",
            Self::Oci => "OCI Object Storage",
        }
    }

    const fn error_content_type(self) -> &'static str {
        match self {
            Self::GcsJson | Self::Oci => "application/json",
            Self::S3 | Self::Azure | Self::GcsXml => "application/xml",
        }
    }

    const fn throttle_status(self) -> StatusCode {
        match self {
            Self::S3 | Self::Azure => StatusCode::SERVICE_UNAVAILABLE,
            Self::GcsJson | Self::GcsXml | Self::Oci => StatusCode::TOO_MANY_REQUESTS,
        }
    }

    const fn throttle_marker(self) -> &'static str {
        match self {
            Self::S3 => "SlowDown",
            Self::Azure => "ServerBusy",
            Self::GcsJson => "Sqrzl deterministic failpoint",
            Self::GcsXml | Self::Oci => "TooManyRequests",
        }
    }

    const fn incomplete_body_marker(self) -> &'static str {
        match self {
            Self::S3 | Self::GcsXml => "IncompleteBody",
            Self::Azure => "InvalidHeaderValue",
            Self::GcsJson => "invalidArgument",
            Self::Oci => "InvalidParameter",
        }
    }

    fn transient_marker(self, failpoint: &str) -> &'static str {
        match self {
            Self::S3 => match failpoint {
                "transient-503" => "ServiceUnavailable",
                "transient-504" => "RequestTimeout",
                _ => "InternalError",
            },
            Self::Azure => match failpoint {
                "transient-503" => "ServerBusy",
                "transient-504" => "OperationTimedOut",
                _ => "InternalError",
            },
            Self::GcsJson => "Sqrzl deterministic failpoint",
            Self::GcsXml => "InternalError",
            Self::Oci => match failpoint {
                "transient-502" => "BadGateway",
                "transient-503" => "ServiceUnavailable",
                "transient-504" => "GatewayTimeout",
                _ => "InternalServerError",
            },
        }
    }

    const fn put_success(self) -> StatusCode {
        match self {
            Self::Azure => StatusCode::CREATED,
            Self::S3 | Self::GcsJson | Self::GcsXml | Self::Oci => StatusCode::OK,
        }
    }

    const fn delete_success(self) -> StatusCode {
        match self {
            Self::Azure => StatusCode::ACCEPTED,
            Self::S3 | Self::GcsJson | Self::GcsXml | Self::Oci => StatusCode::NO_CONTENT,
        }
    }
}

struct MutationSurface {
    front_door: StorageFrontDoor,
    bucket: &'static str,
    key: &'static str,
    method: &'static str,
    uri: &'static str,
    headers: &'static [(&'static str, &'static str)],
    success: StatusCode,
}

#[derive(Clone, Copy)]
struct ReadSurface {
    front_door: StorageFrontDoor,
    bucket: &'static str,
    uri: &'static str,
    headers: &'static [(&'static str, &'static str)],
}

#[derive(Clone, Copy)]
enum PaginationBody {
    Json(&'static str),
    Xml(&'static str),
}

#[derive(Clone, Copy)]
struct PaginationSurface {
    front_door: StorageFrontDoor,
    bucket: &'static str,
    first_uri: &'static str,
    token_parameter: &'static str,
    headers: &'static [(&'static str, &'static str)],
    body: PaginationBody,
}

fn mutation_surfaces() -> [MutationSurface; 5] {
    [
        MutationSurface {
            front_door: StorageFrontDoor::S3,
            bucket: "fault-s3-all",
            key: "object",
            method: "PUT",
            uri: "http://localhost/fault-s3-all/object",
            headers: &[],
            success: StatusCode::OK,
        },
        MutationSurface {
            front_door: StorageFrontDoor::Azure,
            bucket: "fault-azure-all",
            key: "object",
            method: "PUT",
            uri: "http://localhost/devstoreaccount1/fault-azure-all/object",
            headers: &[
                ("x-ms-version", AZURE_VERSION),
                ("x-ms-blob-type", "BlockBlob"),
            ],
            success: StatusCode::CREATED,
        },
        MutationSurface {
            front_door: StorageFrontDoor::GcsJson,
            bucket: "fault-gcs-all",
            key: "object",
            method: "POST",
            uri:
                "http://localhost/upload/storage/v1/b/fault-gcs-all/o?uploadType=media&name=object",
            headers: &[],
            success: StatusCode::OK,
        },
        MutationSurface {
            front_door: StorageFrontDoor::GcsXml,
            bucket: "fault-gcs-xml-all",
            key: "object",
            method: "PUT",
            uri: "http://localhost/fault-gcs-xml-all/object",
            headers: &[("host", "storage.googleapis.com")],
            success: StatusCode::OK,
        },
        MutationSurface {
            front_door: StorageFrontDoor::Oci,
            bucket: "fault-oci-all",
            key: "object",
            method: "PUT",
            uri: "http://localhost/n/sqrzl-emulator/b/fault-oci-all/o/object",
            headers: &[],
            success: StatusCode::OK,
        },
    ]
}

fn read_surfaces() -> [ReadSurface; 5] {
    [
        ReadSurface {
            front_door: StorageFrontDoor::S3,
            bucket: "truncate-s3",
            uri: "http://localhost/truncate-s3/object",
            headers: &[],
        },
        ReadSurface {
            front_door: StorageFrontDoor::Azure,
            bucket: "truncate-azure",
            uri: "http://localhost/devstoreaccount1/truncate-azure/object",
            headers: &[("x-ms-version", AZURE_VERSION)],
        },
        ReadSurface {
            front_door: StorageFrontDoor::GcsJson,
            bucket: "truncate-gcs-json",
            uri: "http://localhost/storage/v1/b/truncate-gcs-json/o/object?alt=media",
            headers: &[],
        },
        ReadSurface {
            front_door: StorageFrontDoor::GcsXml,
            bucket: "truncate-gcs-xml",
            uri: "http://localhost/truncate-gcs-xml/object",
            headers: &[("host", "storage.googleapis.com"), ("content-length", "0")],
        },
        ReadSurface {
            front_door: StorageFrontDoor::Oci,
            bucket: "truncate-oci",
            uri: "http://localhost/n/sqrzl-emulator/b/truncate-oci/o/object",
            headers: &[],
        },
    ]
}

fn pagination_surfaces() -> [PaginationSurface; 5] {
    [
        PaginationSurface {
            front_door: StorageFrontDoor::S3,
            bucket: "paging-s3",
            first_uri: "http://localhost/paging-s3?list-type=2&max-keys=1",
            token_parameter: "continuation-token",
            headers: &[],
            body: PaginationBody::Xml("NextContinuationToken"),
        },
        PaginationSurface {
            front_door: StorageFrontDoor::Azure,
            bucket: "paging-azure",
            first_uri: "http://localhost/devstoreaccount1/paging-azure?restype=container&comp=list&maxresults=1",
            token_parameter: "marker",
            headers: &[("x-ms-version", AZURE_VERSION)],
            body: PaginationBody::Xml("NextMarker"),
        },
        PaginationSurface {
            front_door: StorageFrontDoor::GcsJson,
            bucket: "paging-gcs-json",
            first_uri: "http://localhost/storage/v1/b/paging-gcs-json/o?maxResults=1",
            token_parameter: "pageToken",
            headers: &[],
            body: PaginationBody::Json("nextPageToken"),
        },
        PaginationSurface {
            front_door: StorageFrontDoor::GcsXml,
            bucket: "paging-gcs-xml",
            first_uri: "http://localhost/paging-gcs-xml?max-keys=1",
            token_parameter: "marker",
            headers: &[
                ("host", "storage.googleapis.com"),
                ("content-length", "0"),
            ],
            body: PaginationBody::Xml("NextMarker"),
        },
        PaginationSurface {
            front_door: StorageFrontDoor::Oci,
            bucket: "paging-oci",
            first_uri: "http://localhost/n/sqrzl-emulator/b/paging-oci/o?limit=1",
            token_parameter: "start",
            headers: &[],
            body: PaginationBody::Json("nextStartWith"),
        },
    ]
}

fn pagination_token(body: &str, format: PaginationBody) -> Option<String> {
    match format {
        PaginationBody::Json(field) => serde_json::from_str::<serde_json::Value>(body)
            .ok()?
            .get(field)?
            .as_str()
            .map(str::to_string),
        PaginationBody::Xml(tag) => extract_tag(body, tag),
    }
}

fn sdk_like_has_next_page(surface: PaginationSurface, body: &str) -> bool {
    let has_token = pagination_token(body, surface.body).is_some_and(|token| !token.is_empty());
    match surface.front_door {
        StorageFrontDoor::S3 | StorageFrontDoor::GcsXml => {
            has_token && extract_tag(body, "IsTruncated").as_deref() == Some("true")
        }
        StorageFrontDoor::Azure | StorageFrontDoor::GcsJson | StorageFrontDoor::Oci => has_token,
    }
}

fn assert_gcs_json_error(body: &str, expected_reason: &str) {
    let error: serde_json::Value = serde_json::from_str(body).unwrap();
    assert_eq!(error["error"]["errors"][0]["reason"], expected_reason);
    assert!(error["error"]["code"].is_number());
    assert!(error["error"]["message"].is_string());
}

fn assert_provider_fault_headers(front_door: StorageFrontDoor, headers: &hyper::HeaderMap) {
    match front_door {
        StorageFrontDoor::S3 => {
            assert!(headers.contains_key("x-amz-request-id"));
            assert!(headers.contains_key("x-amz-id-2"));
        }
        StorageFrontDoor::Azure => {
            assert!(headers.contains_key("x-ms-request-id"));
            assert_eq!(headers["x-ms-version"], AZURE_VERSION);
            assert!(headers.contains_key("x-ms-error-code"));
        }
        StorageFrontDoor::Oci => assert!(headers.contains_key("opc-request-id")),
        StorageFrontDoor::GcsJson | StorageFrontDoor::GcsXml => {}
    }
}

async fn assert_s3_conditional_conflict_response(response: hyper::Response<SqrzlBody>) {
    assert_eq!(response.status(), StatusCode::CONFLICT);
    assert_eq!(
        response.headers()["x-sqrzl-failpoint-applied"],
        "conditional-request-conflict"
    );
    let request_id = response.headers()["x-amz-request-id"]
        .to_str()
        .unwrap()
        .to_string();
    let host_id = response.headers()["x-amz-id-2"]
        .to_str()
        .unwrap()
        .to_string();
    let response_body = body_text(response).await;
    assert!(response_body.contains("<Code>ConditionalRequestConflict</Code>"));
    assert!(response_body.contains(&format!("<RequestId>{request_id}</RequestId>")));
    assert!(response_body.contains(&format!("<HostId>{host_id}</HostId>")));
}

fn create_mutation_buckets(storage: &Arc<dyn Storage>) {
    for surface in mutation_surfaces() {
        storage
            .create_bucket(surface.bucket.to_string())
            .expect("test bucket should be created");
    }
}

async fn create_gcs_bucket(storage: &Arc<dyn Storage>, payload: &[u8]) {
    let response = call(
        storage.clone(),
        auth_disabled(),
        request(
            "POST",
            "http://localhost/storage/v1/b?project=local",
            &[("content-type", "application/json")],
            payload,
        ),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
}

async fn upload_gcs_object(storage: &Arc<dyn Storage>, bucket: &str, key: &str, data: &[u8]) {
    let content_length = data.len().to_string();
    let response = call(
        storage.clone(),
        auth_disabled(),
        request(
            "POST",
            &format!("http://localhost/upload/storage/v1/b/{bucket}/o?uploadType=media&name={key}"),
            &[("content-length", content_length.as_str())],
            data,
        ),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
}

fn failpoint_headers(
    surface: &MutationSurface,
    name: &'static str,
    delay: Option<&'static str>,
) -> Vec<(&'static str, &'static str)> {
    let mut headers = surface.headers.to_vec();
    headers.push(("x-sqrzl-failpoint", name));
    if let Some(delay) = delay {
        headers.push(("x-sqrzl-failpoint-delay-ms", delay));
    }
    headers
}

fn framed_mutation_request(
    surface: &MutationSurface,
    headers: &[(&str, &str)],
    payload: &[u8],
) -> hyper::Request<Full<bytes::Bytes>> {
    let mut builder = hyper::Request::builder()
        .method(surface.method)
        .uri(surface.uri);
    let has_explicit_framing = headers.iter().any(|(name, _)| {
        name.eq_ignore_ascii_case("content-length")
            || name.eq_ignore_ascii_case("transfer-encoding")
    });
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    if !has_explicit_framing {
        builder = builder.header("content-length", payload.len().to_string());
    }
    builder
        .body(Full::new(bytes::Bytes::copy_from_slice(payload)))
        .unwrap()
}

async fn lifecycle_put(
    storage: Arc<dyn Storage>,
    front_door: StorageFrontDoor,
    bucket: &str,
    key: &str,
    payload: &[u8],
) -> hyper::Response<SqrzlBody> {
    let uri = match front_door {
        StorageFrontDoor::S3 | StorageFrontDoor::GcsXml => {
            format!("http://localhost/{bucket}/{key}")
        }
        StorageFrontDoor::Azure => {
            format!("http://localhost/devstoreaccount1/{bucket}/{key}")
        }
        StorageFrontDoor::GcsJson => {
            format!("http://localhost/upload/storage/v1/b/{bucket}/o?uploadType=media&name={key}")
        }
        StorageFrontDoor::Oci => {
            format!("http://localhost/n/sqrzl-emulator/b/{bucket}/o/{key}")
        }
    };
    let content_length = payload.len().to_string();
    let headers = match front_door {
        StorageFrontDoor::S3 | StorageFrontDoor::GcsJson | StorageFrontDoor::Oci => {
            vec![("content-length", content_length.as_str())]
        }
        StorageFrontDoor::GcsXml => vec![
            ("host", "storage.googleapis.com"),
            ("content-length", content_length.as_str()),
        ],
        StorageFrontDoor::Azure => vec![
            ("x-ms-version", AZURE_VERSION),
            ("x-ms-blob-type", "BlockBlob"),
            ("content-length", content_length.as_str()),
        ],
    };
    call(
        storage,
        auth_disabled(),
        request(
            if matches!(front_door, StorageFrontDoor::GcsJson) {
                "POST"
            } else {
                "PUT"
            },
            &uri,
            &headers,
            payload,
        ),
    )
    .await
}

async fn lifecycle_get(
    storage: Arc<dyn Storage>,
    front_door: StorageFrontDoor,
    bucket: &str,
    key: &str,
) -> hyper::Response<SqrzlBody> {
    let uri = match front_door {
        StorageFrontDoor::S3 | StorageFrontDoor::GcsXml => {
            format!("http://localhost/{bucket}/{key}")
        }
        StorageFrontDoor::Azure => {
            format!("http://localhost/devstoreaccount1/{bucket}/{key}")
        }
        StorageFrontDoor::GcsJson => {
            format!("http://localhost/storage/v1/b/{bucket}/o/{key}?alt=media")
        }
        StorageFrontDoor::Oci => {
            format!("http://localhost/n/sqrzl-emulator/b/{bucket}/o/{key}")
        }
    };
    let headers = match front_door {
        StorageFrontDoor::Azure => vec![("x-ms-version", AZURE_VERSION)],
        StorageFrontDoor::GcsXml => {
            vec![("host", "storage.googleapis.com"), ("content-length", "0")]
        }
        StorageFrontDoor::S3 | StorageFrontDoor::GcsJson | StorageFrontDoor::Oci => Vec::new(),
    };
    call(
        storage,
        auth_disabled(),
        request("GET", &uri, &headers, b""),
    )
    .await
}

async fn lifecycle_delete(
    storage: Arc<dyn Storage>,
    front_door: StorageFrontDoor,
    bucket: &str,
    key: &str,
) -> hyper::Response<SqrzlBody> {
    let uri = match front_door {
        StorageFrontDoor::S3 | StorageFrontDoor::GcsXml => {
            format!("http://localhost/{bucket}/{key}")
        }
        StorageFrontDoor::Azure => {
            format!("http://localhost/devstoreaccount1/{bucket}/{key}")
        }
        StorageFrontDoor::GcsJson => {
            format!("http://localhost/storage/v1/b/{bucket}/o/{key}")
        }
        StorageFrontDoor::Oci => {
            format!("http://localhost/n/sqrzl-emulator/b/{bucket}/o/{key}")
        }
    };
    let headers = match front_door {
        StorageFrontDoor::Azure => vec![("x-ms-version", AZURE_VERSION)],
        StorageFrontDoor::GcsXml => {
            vec![("host", "storage.googleapis.com"), ("content-length", "0")]
        }
        StorageFrontDoor::S3 | StorageFrontDoor::GcsJson | StorageFrontDoor::Oci => Vec::new(),
    };
    call(
        storage,
        auth_disabled(),
        request("DELETE", &uri, &headers, b""),
    )
    .await
}

#[tokio::test(flavor = "multi_thread")]
async fn should_preserve_protocol_durability_lifecycle_across_cache_loss_on_every_front_door() {
    // Arrange
    let root =
        std::env::temp_dir().join(format!("sqrzl-provider-lifecycle-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&root).unwrap();
    let surfaces = [
        (StorageFrontDoor::S3, "lifecycle-s3"),
        (StorageFrontDoor::Azure, "lifecycle-azure"),
        (StorageFrontDoor::GcsJson, "lifecycle-gcs-json"),
        (StorageFrontDoor::GcsXml, "lifecycle-gcs-xml"),
        (StorageFrontDoor::Oci, "lifecycle-oci"),
    ];
    let storage: Arc<dyn Storage> = Arc::new(FilesystemStorage::new(&root));
    for (_, bucket) in surfaces {
        storage.create_bucket(bucket.to_string()).unwrap();
    }
    for (front_door, bucket) in surfaces {
        for (key, payload) in [
            ("wal-0001", b"wal-records".as_slice()),
            ("sst-0001", b"sstable-data".as_slice()),
            ("catalog-0001", b"catalog-v1".as_slice()),
        ] {
            let response = lifecycle_put(storage.clone(), front_door, bucket, key, payload).await;
            assert_eq!(
                response.status(),
                front_door.put_success(),
                "initial {key} upload through {}",
                front_door.label()
            );
        }
    }

    // Act
    drop(storage);
    let reopened: Arc<dyn Storage> = Arc::new(FilesystemStorage::new(&root));
    for (front_door, bucket) in surfaces {
        for (key, payload) in [
            ("wal-0001", b"wal-records".as_slice()),
            ("sst-0001", b"sstable-data".as_slice()),
            ("catalog-0001", b"catalog-v1".as_slice()),
        ] {
            let response = lifecycle_get(reopened.clone(), front_door, bucket, key).await;
            assert_eq!(
                response.status(),
                StatusCode::OK,
                "reopened {key} read through {}",
                front_door.label()
            );
            assert_eq!(body_text(response).await.as_bytes(), payload);
        }
        let published = lifecycle_put(
            reopened.clone(),
            front_door,
            bucket,
            "catalog-0002",
            b"catalog-v2",
        )
        .await;
        assert_eq!(published.status(), front_door.put_success());
        for retired in ["catalog-0001", "wal-0001"] {
            let deleted = lifecycle_delete(reopened.clone(), front_door, bucket, retired).await;
            assert_eq!(
                deleted.status(),
                front_door.delete_success(),
                "retire {retired} through {}",
                front_door.label()
            );
        }
    }

    // Assert
    for (front_door, bucket) in surfaces {
        for retired in ["catalog-0001", "wal-0001"] {
            let missing = lifecycle_get(reopened.clone(), front_door, bucket, retired).await;
            assert_eq!(
                missing.status(),
                StatusCode::NOT_FOUND,
                "retired {retired} through {}",
                front_door.label()
            );
        }
        for (survivor, payload) in [
            ("sst-0001", b"sstable-data".as_slice()),
            ("catalog-0002", b"catalog-v2".as_slice()),
        ] {
            let present = lifecycle_get(reopened.clone(), front_door, bucket, survivor).await;
            assert_eq!(
                present.status(),
                StatusCode::OK,
                "surviving {survivor} through {}",
                front_door.label()
            );
            assert_eq!(body_text(present).await.as_bytes(), payload);
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn should_require_explicit_zero_length_for_s3_object_put() {
    // Arrange
    let storage = temp_storage();
    call(
        storage.clone(),
        auth_disabled(),
        request("PUT", "http://localhost/framing-s3", &[], b""),
    )
    .await;

    // Act
    let missing_length = call(
        storage.clone(),
        auth_disabled(),
        request("PUT", "http://localhost/framing-s3/empty", &[], b""),
    )
    .await;
    let explicit = call(
        storage.clone(),
        auth_disabled(),
        request(
            "PUT",
            "http://localhost/framing-s3/empty",
            &[("content-length", "0")],
            b"",
        ),
    )
    .await;
    let get = call(
        storage.clone(),
        auth_disabled(),
        request("GET", "http://localhost/framing-s3/empty", &[], b""),
    )
    .await;
    let head = call(
        storage.clone(),
        auth_disabled(),
        request("HEAD", "http://localhost/framing-s3/empty", &[], b""),
    )
    .await;
    let delete = call(
        storage.clone(),
        auth_disabled(),
        request("DELETE", "http://localhost/framing-s3/empty", &[], b""),
    )
    .await;
    let missing_object = call(
        storage,
        auth_disabled(),
        request("GET", "http://localhost/framing-s3/empty", &[], b""),
    )
    .await;

    // Assert
    assert_eq!(missing_length.status(), StatusCode::LENGTH_REQUIRED);
    assert!(body_text(missing_length)
        .await
        .contains("MissingContentLength"));
    assert_eq!(explicit.status(), StatusCode::OK);
    assert_eq!(get.status(), StatusCode::OK);
    assert_eq!(get.headers()["content-length"], "0");
    assert!(body_text(get).await.is_empty());
    assert_eq!(head.status(), StatusCode::OK);
    assert_eq!(head.headers()["content-length"], "0");
    assert!(body_text(head).await.is_empty());
    assert_eq!(delete.status(), StatusCode::NO_CONTENT);
    assert_eq!(missing_object.status(), StatusCode::NOT_FOUND);
}

#[tokio::test(flavor = "multi_thread")]
async fn should_reject_declared_length_mismatch_with_provider_shape() {
    // Arrange
    let storage = temp_storage();
    call(
        storage.clone(),
        auth_disabled(),
        request("PUT", "http://localhost/framing-mismatch", &[], b""),
    )
    .await;

    // Act
    let response = call(
        storage,
        auth_disabled(),
        request(
            "PUT",
            "http://localhost/framing-mismatch/object",
            &[("content-length", "5")],
            b"abc",
        ),
    )
    .await;

    // Assert
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert!(body_text(response).await.contains("IncompleteBody"));
}

#[tokio::test(flavor = "multi_thread")]
async fn should_require_provider_specific_content_length_for_azure_and_gcs_uploads() {
    // Arrange
    let storage = temp_storage();
    for bucket in ["length-azure", "length-gcs-json", "length-gcs-xml"] {
        storage.create_bucket(bucket.to_string()).unwrap();
    }

    // Act
    let azure = call(
        storage.clone(),
        auth_disabled(),
        request(
            "PUT",
            "http://localhost/devstoreaccount1/length-azure/object",
            &[
                ("x-ms-version", AZURE_VERSION),
                ("x-ms-blob-type", "BlockBlob"),
            ],
            b"",
        ),
    )
    .await;
    let gcs_json = call(
        storage.clone(),
        auth_disabled(),
        request(
            "POST",
            "http://localhost/upload/storage/v1/b/length-gcs-json/o?uploadType=media&name=object",
            &[],
            b"",
        ),
    )
    .await;
    let gcs_xml = call(
        storage.clone(),
        auth_disabled(),
        request(
            "PUT",
            "http://localhost/length-gcs-xml/object",
            &[("host", "storage.googleapis.com")],
            b"",
        ),
    )
    .await;

    // Assert
    assert_eq!(azure.status(), StatusCode::LENGTH_REQUIRED);
    assert_eq!(
        azure.headers()["x-ms-error-code"],
        "MissingContentLengthHeader"
    );
    assert!(body_text(azure)
        .await
        .contains("MissingContentLengthHeader"));
    assert_eq!(gcs_json.status(), StatusCode::LENGTH_REQUIRED);
    assert!(body_text(gcs_json).await.is_empty());
    assert_eq!(gcs_xml.status(), StatusCode::LENGTH_REQUIRED);
    assert!(gcs_xml.headers()["content-type"]
        .to_str()
        .unwrap()
        .starts_with("application/xml"));
    assert!(body_text(gcs_xml)
        .await
        .contains("<Code>MissingContentLength</Code>"));
    for bucket in ["length-azure", "length-gcs-json", "length-gcs-xml"] {
        assert!(storage.get_object(bucket, "object").is_err());
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn should_allow_chunked_gcs_uploads_without_content_length() {
    // Arrange
    let storage = temp_storage();
    for bucket in ["chunked-gcs-json", "chunked-gcs-xml"] {
        storage.create_bucket(bucket.to_string()).unwrap();
    }

    // Act
    let json = call(
        storage.clone(),
        auth_disabled(),
        request(
            "POST",
            "http://localhost/upload/storage/v1/b/chunked-gcs-json/o?uploadType=media&name=object",
            &[("transfer-encoding", "chunked")],
            b"",
        ),
    )
    .await;
    let xml = call(
        storage.clone(),
        auth_disabled(),
        request(
            "PUT",
            "http://localhost/chunked-gcs-xml/object",
            &[
                ("host", "storage.googleapis.com"),
                ("transfer-encoding", "chunked"),
            ],
            b"",
        ),
    )
    .await;

    // Assert
    assert_eq!(json.status(), StatusCode::OK);
    assert_eq!(xml.status(), StatusCode::OK);
    for bucket in ["chunked-gcs-json", "chunked-gcs-xml"] {
        let object = storage.get_object(bucket, "object").unwrap();
        assert_eq!(object.size, 0);
        assert!(object.data.is_empty());
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn should_preserve_azure_empty_mutation_statuses() {
    // Arrange
    let storage = temp_storage();
    let headers = [("x-ms-version", AZURE_VERSION)];
    let created = call(
        storage.clone(),
        auth_disabled(),
        request(
            "PUT",
            "http://localhost/devstoreaccount1/empty-azure?restype=container",
            &headers,
            b"",
        ),
    )
    .await;
    assert_eq!(created.status(), StatusCode::CREATED);

    // Act
    let put = call(
        storage.clone(),
        auth_disabled(),
        request(
            "PUT",
            "http://localhost/devstoreaccount1/empty-azure/blob",
            &[
                ("x-ms-version", AZURE_VERSION),
                ("x-ms-blob-type", "BlockBlob"),
                ("content-length", "0"),
            ],
            b"",
        ),
    )
    .await;
    let get = call(
        storage.clone(),
        auth_disabled(),
        request(
            "GET",
            "http://localhost/devstoreaccount1/empty-azure/blob",
            &headers,
            b"",
        ),
    )
    .await;
    let head = call(
        storage.clone(),
        auth_disabled(),
        request(
            "HEAD",
            "http://localhost/devstoreaccount1/empty-azure/blob",
            &headers,
            b"",
        ),
    )
    .await;
    let delete = call(
        storage.clone(),
        auth_disabled(),
        request(
            "DELETE",
            "http://localhost/devstoreaccount1/empty-azure/blob",
            &headers,
            b"",
        ),
    )
    .await;
    let missing = call(
        storage,
        auth_disabled(),
        request(
            "GET",
            "http://localhost/devstoreaccount1/empty-azure/blob",
            &headers,
            b"",
        ),
    )
    .await;

    // Assert
    assert_eq!(put.status(), StatusCode::CREATED);
    assert!(put.headers()["etag"].to_str().unwrap().starts_with('"'));
    assert_eq!(get.status(), StatusCode::OK);
    assert_eq!(get.headers()["content-length"], "0");
    assert!(body_text(get).await.is_empty());
    assert_eq!(head.status(), StatusCode::OK);
    assert_eq!(head.headers()["content-length"], "0");
    assert!(body_text(head).await.is_empty());
    assert_eq!(delete.status(), StatusCode::ACCEPTED);
    assert_eq!(missing.status(), StatusCode::NOT_FOUND);
}

#[tokio::test(flavor = "multi_thread")]
async fn should_preserve_gcs_xml_empty_mutation_statuses() {
    // Arrange
    let storage = temp_storage();
    let host = [("host", "storage.googleapis.com"), ("content-length", "0")];
    call(
        storage.clone(),
        auth_disabled(),
        request("PUT", "http://localhost/empty-gcs-xml", &host, b""),
    )
    .await;

    // Act
    let put = call(
        storage.clone(),
        auth_disabled(),
        request(
            "PUT",
            "http://localhost/empty-gcs-xml/object",
            &[("host", "storage.googleapis.com"), ("content-length", "0")],
            b"",
        ),
    )
    .await;
    let get = call(
        storage.clone(),
        auth_disabled(),
        request("GET", "http://localhost/empty-gcs-xml/object", &host, b""),
    )
    .await;
    let head = call(
        storage.clone(),
        auth_disabled(),
        request("HEAD", "http://localhost/empty-gcs-xml/object", &host, b""),
    )
    .await;
    let delete = call(
        storage.clone(),
        auth_disabled(),
        request(
            "DELETE",
            "http://localhost/empty-gcs-xml/object",
            &host,
            b"",
        ),
    )
    .await;
    let missing = call(
        storage,
        auth_disabled(),
        request("GET", "http://localhost/empty-gcs-xml/object", &host, b""),
    )
    .await;

    // Assert
    assert_eq!(put.status(), StatusCode::OK);
    assert!(put.headers()["etag"].to_str().unwrap().starts_with('"'));
    assert_eq!(get.status(), StatusCode::OK);
    assert_eq!(get.headers()["content-length"], "0");
    assert!(body_text(get).await.is_empty());
    assert_eq!(head.status(), StatusCode::OK);
    assert_eq!(head.headers()["content-length"], "0");
    assert!(body_text(head).await.is_empty());
    assert_eq!(delete.status(), StatusCode::NO_CONTENT);
    assert_eq!(missing.status(), StatusCode::NOT_FOUND);
}

#[tokio::test(flavor = "multi_thread")]
async fn should_enforce_gcs_xml_generation_preconditions_on_create_and_update() {
    // Arrange
    let storage = temp_storage();
    let gcs_host = [("host", "storage.googleapis.com"), ("content-length", "0")];
    call(
        storage.clone(),
        auth_disabled(),
        request(
            "PUT",
            "http://localhost/conditional-gcs-xml",
            &gcs_host,
            b"",
        ),
    )
    .await;

    // Act
    let created = call(
        storage.clone(),
        auth_disabled(),
        request(
            "PUT",
            "http://localhost/conditional-gcs-xml/object",
            &[
                ("host", "storage.googleapis.com"),
                ("content-length", "2"),
                ("x-goog-if-generation-match", "0"),
            ],
            b"v1",
        ),
    )
    .await;
    let generation = created.headers()["x-goog-generation"]
        .to_str()
        .unwrap()
        .to_string();
    let create_conflict = call(
        storage.clone(),
        auth_disabled(),
        request(
            "PUT",
            "http://localhost/conditional-gcs-xml/object",
            &[
                ("host", "storage.googleapis.com"),
                ("content-length", "2"),
                ("x-goog-if-generation-match", "0"),
            ],
            b"v2",
        ),
    )
    .await;
    let updated = call(
        storage.clone(),
        auth_disabled(),
        request(
            "PUT",
            "http://localhost/conditional-gcs-xml/object",
            &[
                ("host", "storage.googleapis.com"),
                ("content-length", "2"),
                ("x-goog-if-generation-match", &generation),
                ("x-goog-if-metageneration-match", "1"),
            ],
            b"v3",
        ),
    )
    .await;

    // Assert
    assert_eq!(created.status(), StatusCode::OK);
    assert_eq!(create_conflict.status(), StatusCode::PRECONDITION_FAILED);
    assert!(body_text(create_conflict)
        .await
        .contains("PreconditionFailed"));
    assert_eq!(updated.status(), StatusCode::OK);
    assert_ne!(updated.headers()["x-goog-generation"], generation);
    assert_eq!(
        storage
            .get_object("conditional-gcs-xml", "object")
            .unwrap()
            .data,
        b"v3"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn should_enforce_gcs_xml_generation_precondition_on_delete() {
    // Arrange
    let storage = temp_storage();
    let gcs_host = [("host", "storage.googleapis.com"), ("content-length", "0")];
    call(
        storage.clone(),
        auth_disabled(),
        request("PUT", "http://localhost/delete-gcs-xml", &gcs_host, b""),
    )
    .await;
    let created = call(
        storage.clone(),
        auth_disabled(),
        request(
            "PUT",
            "http://localhost/delete-gcs-xml/object",
            &[("host", "storage.googleapis.com"), ("content-length", "4")],
            b"kept",
        ),
    )
    .await;
    let generation = created.headers()["x-goog-generation"].to_str().unwrap();

    // Act
    let rejected = call(
        storage.clone(),
        auth_disabled(),
        request(
            "DELETE",
            "http://localhost/delete-gcs-xml/object",
            &[
                ("host", "storage.googleapis.com"),
                ("content-length", "0"),
                ("x-goog-if-generation-match", "1"),
            ],
            b"",
        ),
    )
    .await;
    let deleted = call(
        storage.clone(),
        auth_disabled(),
        request(
            "DELETE",
            "http://localhost/delete-gcs-xml/object",
            &[
                ("host", "storage.googleapis.com"),
                ("content-length", "0"),
                ("x-goog-if-generation-match", generation),
                ("x-goog-if-metageneration-match", "1"),
            ],
            b"",
        ),
    )
    .await;

    // Assert
    assert_eq!(rejected.status(), StatusCode::PRECONDITION_FAILED);
    assert_eq!(deleted.status(), StatusCode::NO_CONTENT);
    assert!(storage.get_object("delete-gcs-xml", "object").is_err());
}

#[tokio::test(flavor = "multi_thread")]
async fn should_reject_gcs_json_upload_metageneration_precondition_without_mutation() {
    // Arrange
    let storage = temp_storage();
    create_gcs_bucket(&storage, br#"{"name":"upload-meta-gcs"}"#).await;

    // Act
    let response = call(
        storage.clone(),
        auth_disabled(),
        request(
            "POST",
            "http://localhost/upload/storage/v1/b/upload-meta-gcs/o?uploadType=media&name=object&ifMetagenerationMatch=1",
            &[("content-length", "7")],
            b"blocked",
        ),
    )
    .await;

    // Assert
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_gcs_json_error(&body_text(response).await, "invalidParameter");
    assert!(storage.get_object("upload-meta-gcs", "object").is_err());
}

#[tokio::test(flavor = "multi_thread")]
async fn should_reject_malformed_gcs_json_mutation_bodies_without_mutation() {
    // Arrange
    let storage = temp_storage();
    create_gcs_bucket(&storage, br#"{"name":"malformed-gcs"}"#).await;
    upload_gcs_object(&storage, "malformed-gcs", "object", b"original").await;
    let original = storage.get_object("malformed-gcs", "object").unwrap();

    // Act
    let create = call(
        storage.clone(),
        auth_disabled(),
        request(
            "POST",
            "http://localhost/storage/v1/b?project=local",
            &[("content-type", "application/json")],
            b"{",
        ),
    )
    .await;
    let patch_bucket = call(
        storage.clone(),
        auth_disabled(),
        request(
            "PATCH",
            "http://localhost/storage/v1/b/malformed-gcs",
            &[("content-type", "application/json")],
            b"{",
        ),
    )
    .await;
    let patch_object = call(
        storage.clone(),
        auth_disabled(),
        request(
            "PATCH",
            "http://localhost/storage/v1/b/malformed-gcs/o/object",
            &[("content-type", "application/json")],
            b"{",
        ),
    )
    .await;

    // Assert
    for response in [create, patch_bucket, patch_object] {
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert!(response.headers()["content-type"]
            .to_str()
            .unwrap()
            .starts_with("application/json"));
        assert_gcs_json_error(&body_text(response).await, "parseError");
    }
    let unchanged = storage.get_object("malformed-gcs", "object").unwrap();
    assert_eq!(unchanged.data, original.data);
    assert_eq!(unchanged.metadata, original.metadata);
    assert_eq!(unchanged.provider_metadata, original.provider_metadata);
    assert_eq!(storage.list_buckets().unwrap().len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn should_return_gcs_json_not_found_for_missing_bucket_operations() {
    // Arrange
    let storage = temp_storage();

    // Act
    let get = call(
        storage.clone(),
        auth_disabled(),
        request(
            "GET",
            "http://localhost/storage/v1/b/missing-gcs-bucket",
            &[],
            b"",
        ),
    )
    .await;
    let delete = call(
        storage.clone(),
        auth_disabled(),
        request(
            "DELETE",
            "http://localhost/storage/v1/b/missing-gcs-bucket",
            &[],
            b"",
        ),
    )
    .await;
    let patch = call(
        storage.clone(),
        auth_disabled(),
        request(
            "PATCH",
            "http://localhost/storage/v1/b/missing-gcs-bucket",
            &[("content-type", "application/json")],
            b"{}",
        ),
    )
    .await;

    // Assert
    for response in [get, delete, patch] {
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert!(response.headers()["content-type"]
            .to_str()
            .unwrap()
            .starts_with("application/json"));
        assert_gcs_json_error(&body_text(response).await, "notFound");
    }
    assert!(storage.list_buckets().unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn should_return_gcs_xml_no_such_bucket_across_bucket_and_object_operations() {
    // Arrange
    let storage = temp_storage();
    let cases = [
        (
            "GET",
            "http://localhost/missing-gcs-xml-bucket",
            b"".as_slice(),
            true,
        ),
        (
            "DELETE",
            "http://localhost/missing-gcs-xml-bucket",
            b"".as_slice(),
            true,
        ),
        (
            "PUT",
            "http://localhost/missing-gcs-xml-bucket/object",
            b"value".as_slice(),
            true,
        ),
        (
            "GET",
            "http://localhost/missing-gcs-xml-bucket/object",
            b"".as_slice(),
            true,
        ),
        (
            "HEAD",
            "http://localhost/missing-gcs-xml-bucket/object",
            b"".as_slice(),
            false,
        ),
        (
            "DELETE",
            "http://localhost/missing-gcs-xml-bucket/object",
            b"".as_slice(),
            true,
        ),
    ];

    // Act
    // Assert
    for (method, uri, payload, has_xml_body) in cases {
        let content_length = payload.len().to_string();
        let headers = vec![
            ("host", "storage.googleapis.com"),
            ("content-length", content_length.as_str()),
        ];
        let response = call(
            storage.clone(),
            auth_disabled(),
            request(method, uri, &headers, payload),
        )
        .await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND, "{method} {uri}");
        let body = body_text(response).await;
        if has_xml_body {
            assert!(body.contains("<Code>NoSuchBucket</Code>"), "{method} {uri}");
        } else {
            assert!(body.is_empty(), "HEAD must not include an error document");
        }
        assert!(storage.list_buckets().unwrap().is_empty());
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn should_preserve_gcs_json_empty_mutation_statuses() {
    // Arrange
    let storage = temp_storage();
    storage.create_bucket("empty-gcs-json".to_string()).unwrap();

    // Act
    let put = call(
        storage.clone(),
        auth_disabled(),
        request(
            "POST",
            "http://localhost/upload/storage/v1/b/empty-gcs-json/o?uploadType=media&name=object",
            &[("content-length", "0")],
            b"",
        ),
    )
    .await;
    let metadata_get = call(
        storage.clone(),
        auth_disabled(),
        request(
            "GET",
            "http://localhost/storage/v1/b/empty-gcs-json/o/object",
            &[],
            b"",
        ),
    )
    .await;
    let media_get = call(
        storage.clone(),
        auth_disabled(),
        request(
            "GET",
            "http://localhost/storage/v1/b/empty-gcs-json/o/object?alt=media",
            &[],
            b"",
        ),
    )
    .await;
    let head = call(
        storage.clone(),
        auth_disabled(),
        request(
            "HEAD",
            "http://localhost/storage/v1/b/empty-gcs-json/o/object",
            &[],
            b"",
        ),
    )
    .await;
    let delete = call(
        storage.clone(),
        auth_disabled(),
        request(
            "DELETE",
            "http://localhost/storage/v1/b/empty-gcs-json/o/object",
            &[],
            b"",
        ),
    )
    .await;
    let missing = call(
        storage,
        auth_disabled(),
        request(
            "GET",
            "http://localhost/storage/v1/b/empty-gcs-json/o/object",
            &[],
            b"",
        ),
    )
    .await;

    // Assert
    assert_eq!(put.status(), StatusCode::OK);
    let metadata: serde_json::Value = serde_json::from_str(&body_text(put).await).unwrap();
    assert_eq!(metadata["size"], "0");
    assert_eq!(metadata_get.status(), StatusCode::OK);
    let metadata: serde_json::Value = serde_json::from_str(&body_text(metadata_get).await).unwrap();
    assert_eq!(metadata["size"], "0");
    assert_eq!(media_get.status(), StatusCode::OK);
    assert_eq!(media_get.headers()["content-length"], "0");
    assert!(body_text(media_get).await.is_empty());
    assert_eq!(head.status(), StatusCode::METHOD_NOT_ALLOWED);
    assert!(head.headers()["content-type"]
        .to_str()
        .unwrap()
        .starts_with("application/json"));
    assert_eq!(delete.status(), StatusCode::NO_CONTENT);
    assert_eq!(missing.status(), StatusCode::NOT_FOUND);
}

#[tokio::test(flavor = "multi_thread")]
async fn should_reject_content_length_mismatch_on_every_storage_front_door() {
    // Arrange
    let storage = temp_storage();
    create_mutation_buckets(&storage);

    // Act
    // Assert
    for surface in mutation_surfaces() {
        let mut headers = surface.headers.to_vec();
        headers.push(("content-length", "10"));
        let response = call(
            storage.clone(),
            auth_disabled(),
            request(surface.method, surface.uri, &headers, b"short"),
        )
        .await;
        assert_eq!(
            response.status(),
            StatusCode::BAD_REQUEST,
            "wrong framing status for {}",
            surface.front_door.label()
        );
        assert!(
            response.headers()["content-type"]
                .to_str()
                .unwrap()
                .starts_with(surface.front_door.error_content_type()),
            "wrong framing content type for {}",
            surface.front_door.label()
        );
        assert!(
            body_text(response)
                .await
                .contains(surface.front_door.incomplete_body_marker()),
            "wrong framing body for {}",
            surface.front_door.label()
        );
        assert!(storage.get_object(surface.bucket, surface.key).is_err());
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn should_keep_gcs_json_resumable_sessions_retriable_after_framing_failures() {
    // Arrange
    let storage = temp_storage();
    storage
        .create_bucket("framing-gcs-resumable".to_string())
        .unwrap();
    let mut locations = Vec::new();
    for object in ["mismatch", "truncated"] {
        let initiated = call(
            storage.clone(),
            auth_disabled(),
            request(
                "POST",
                &format!(
                    "http://localhost/upload/storage/v1/b/framing-gcs-resumable/o?uploadType=resumable&name={object}"
                ),
                &[
                    ("content-type", "application/json"),
                    ("content-length", "2"),
                ],
                b"{}",
            ),
        )
        .await;
        assert_eq!(initiated.status(), StatusCode::OK);
        locations.push(
            initiated.headers()["location"]
                .to_str()
                .unwrap()
                .to_string(),
        );
    }

    // Act
    let mismatched = call(
        storage.clone(),
        auth_disabled(),
        request("PUT", &locations[0], &[("content-length", "10")], b"short"),
    )
    .await;
    let truncated_request = hyper::Request::builder()
        .method("PUT")
        .uri(&locations[1])
        .header("content-length", "10")
        .body(SqrzlBody::truncated(
            bytes::Bytes::from_static(b"short"),
            10,
        ))
        .unwrap();
    let truncated = match RequestExt::from_hyper_with_max_body(truncated_request, None).await {
        Err(RequestParseError::BodyRead {
            method,
            uri,
            headers,
            ..
        }) => AdapterRegistry::default().render_incomplete_body(&method, &uri, &headers),
        _ => panic!("truncated request should fail body parsing"),
    };

    // Assert
    for response in [mismatched, truncated] {
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert!(response.headers()["content-type"]
            .to_str()
            .unwrap()
            .starts_with("application/json"));
        assert_gcs_json_error(&body_text(response).await, "invalidArgument");
    }
    assert!(storage
        .get_object("framing-gcs-resumable", "mismatch")
        .is_err());
    assert!(storage
        .get_object("framing-gcs-resumable", "truncated")
        .is_err());
    for (location, payload) in locations
        .iter()
        .zip([b"retry-one".as_slice(), b"retry-two"])
    {
        let content_length = payload.len().to_string();
        let retry = call(
            storage.clone(),
            auth_disabled(),
            request(
                "PUT",
                location,
                &[("content-length", content_length.as_str())],
                payload,
            ),
        )
        .await;
        assert_eq!(retry.status(), StatusCode::OK);
    }
    assert_eq!(
        storage
            .get_object("framing-gcs-resumable", "mismatch")
            .unwrap()
            .data,
        b"retry-one"
    );
    assert_eq!(
        storage
            .get_object("framing-gcs-resumable", "truncated")
            .unwrap()
            .data,
        b"retry-two"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn should_route_non_upload_gcs_methods_before_enforcing_upload_framing() {
    // Arrange
    let storage = temp_storage();
    storage
        .create_bucket("method-gcs-json".to_string())
        .unwrap();
    let initiated = call(
        storage.clone(),
        auth_disabled(),
        request(
            "POST",
            "http://localhost/upload/storage/v1/b/method-gcs-json/o?uploadType=resumable&name=session-object",
            &[
                ("content-type", "application/json"),
                ("content-length", "2"),
            ],
            b"{}",
        ),
    )
    .await;
    assert_eq!(initiated.status(), StatusCode::OK);
    let location = initiated.headers()["location"]
        .to_str()
        .unwrap()
        .to_string();

    // Act
    let media_get = call(
        storage.clone(),
        auth_disabled(),
        request(
            "GET",
            "http://localhost/upload/storage/v1/b/method-gcs-json/o?uploadType=media&name=get-object",
            &[],
            b"",
        ),
    )
    .await;
    let session_delete = call(
        storage.clone(),
        auth_disabled(),
        request("DELETE", &location, &[], b""),
    )
    .await;

    // Assert
    for response in [media_get, session_delete] {
        assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
        assert_gcs_json_error(&body_text(response).await, "methodNotAllowed");
    }
    assert!(storage.get_object("method-gcs-json", "get-object").is_err());
    assert!(storage
        .get_object("method-gcs-json", "session-object")
        .is_err());
    let completed = call(
        storage.clone(),
        auth_disabled(),
        request("PUT", &location, &[("content-length", "7")], b"payload"),
    )
    .await;
    assert_eq!(completed.status(), StatusCode::OK);
    assert_eq!(
        storage
            .get_object("method-gcs-json", "session-object")
            .unwrap()
            .data,
        b"payload"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn should_render_gcs_json_payload_too_large_as_json_without_mutation() {
    // Arrange
    let storage = temp_storage();
    storage
        .create_bucket("oversized-gcs-json".to_string())
        .unwrap();
    let oversized = request(
        "POST",
        "http://localhost/upload/storage/v1/b/oversized-gcs-json/o?uploadType=media&name=object",
        &[("content-type", "application/octet-stream")],
        b"large",
    );

    // Act
    let response = match RequestExt::from_hyper_with_max_body(oversized, Some(4)).await {
        Err(RequestParseError::BodyTooLarge {
            max_request_bytes,
            method,
            uri,
            headers,
        }) => AdapterRegistry::default().render_payload_too_large(
            &method,
            &uri,
            &headers,
            max_request_bytes,
        ),
        _ => panic!("oversized request should fail body parsing"),
    };

    // Assert
    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    assert!(response.headers()["content-type"]
        .to_str()
        .unwrap()
        .starts_with("application/json"));
    assert_gcs_json_error(&body_text(response).await, "uploadTooLarge");
    assert!(storage.get_object("oversized-gcs-json", "object").is_err());
}

#[tokio::test(flavor = "multi_thread")]
async fn should_preserve_oci_empty_mutation_statuses() {
    // Arrange
    let storage = temp_storage();
    let created = call(
        storage.clone(),
        auth_disabled(),
        request(
            "POST",
            "http://localhost/n/sqrzl-emulator/b",
            &[("content-type", "application/json")],
            br#"{"name":"empty-oci","compartmentId":"ocid1.compartment.local"}"#,
        ),
    )
    .await;
    assert_eq!(created.status(), StatusCode::OK);

    // Act
    let put = call(
        storage.clone(),
        auth_disabled(),
        request(
            "PUT",
            "http://localhost/n/sqrzl-emulator/b/empty-oci/o/object",
            &[("content-length", "0")],
            b"",
        ),
    )
    .await;
    let get = call(
        storage.clone(),
        auth_disabled(),
        request(
            "GET",
            "http://localhost/n/sqrzl-emulator/b/empty-oci/o/object",
            &[],
            b"",
        ),
    )
    .await;
    let head = call(
        storage.clone(),
        auth_disabled(),
        request(
            "HEAD",
            "http://localhost/n/sqrzl-emulator/b/empty-oci/o/object",
            &[],
            b"",
        ),
    )
    .await;
    let delete = call(
        storage.clone(),
        auth_disabled(),
        request(
            "DELETE",
            "http://localhost/n/sqrzl-emulator/b/empty-oci/o/object",
            &[],
            b"",
        ),
    )
    .await;
    let missing = call(
        storage,
        auth_disabled(),
        request(
            "GET",
            "http://localhost/n/sqrzl-emulator/b/empty-oci/o/object",
            &[],
            b"",
        ),
    )
    .await;

    // Assert
    assert_eq!(put.status(), StatusCode::OK);
    assert!(put.headers().contains_key("etag"));
    assert_eq!(put.headers()["opc-content-md5"], "1B2M2Y8AsgTpgAmY7PhCfg==");
    assert!(body_text(put).await.is_empty());
    assert_eq!(get.status(), StatusCode::OK);
    assert_eq!(get.headers()["content-length"], "0");
    assert!(body_text(get).await.is_empty());
    assert_eq!(head.status(), StatusCode::OK);
    assert_eq!(head.headers()["content-length"], "0");
    assert!(body_text(head).await.is_empty());
    assert_eq!(delete.status(), StatusCode::NO_CONTENT);
    assert_eq!(missing.status(), StatusCode::NOT_FOUND);
}

#[tokio::test(flavor = "multi_thread")]
async fn should_report_gcs_json_metadata_document_length_separately_from_object_size() {
    // Arrange
    let storage = temp_storage();
    call(
        storage.clone(),
        auth_disabled(),
        request(
            "POST",
            "http://localhost/storage/v1/b?project=local",
            &[("content-type", "application/json")],
            br#"{"name":"metadata-gcs"}"#,
        ),
    )
    .await;
    call(
        storage.clone(),
        auth_disabled(),
        request(
            "POST",
            "http://localhost/upload/storage/v1/b/metadata-gcs/o?uploadType=media&name=empty",
            &[("content-length", "0")],
            b"",
        ),
    )
    .await;

    // Act
    let response = call(
        storage,
        auth_disabled(),
        request(
            "GET",
            "http://localhost/storage/v1/b/metadata-gcs/o/empty",
            &[],
            b"",
        ),
    )
    .await;
    let declared = response.headers()["content-length"]
        .to_str()
        .unwrap()
        .parse::<usize>()
        .unwrap();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();

    // Assert
    assert_eq!(json["size"], "0");
    assert_eq!(declared, body.len());
    assert_ne!(declared, 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn should_reject_gcs_json_etag_mutation_headers_without_applying_delete() {
    // Arrange
    let storage = temp_storage();
    call(
        storage.clone(),
        auth_disabled(),
        request(
            "POST",
            "http://localhost/storage/v1/b?project=local",
            &[("content-type", "application/json")],
            br#"{"name":"etag-gcs"}"#,
        ),
    )
    .await;
    call(
        storage.clone(),
        auth_disabled(),
        request(
            "POST",
            "http://localhost/upload/storage/v1/b/etag-gcs/o?uploadType=media&name=object",
            &[("content-length", "5")],
            b"value",
        ),
    )
    .await;

    // Act
    let rejected = call(
        storage.clone(),
        auth_disabled(),
        request(
            "DELETE",
            "http://localhost/storage/v1/b/etag-gcs/o/object",
            &[("if-match", "anything")],
            b"",
        ),
    )
    .await;
    let still_present = call(
        storage,
        auth_disabled(),
        request(
            "GET",
            "http://localhost/storage/v1/b/etag-gcs/o/object",
            &[],
            b"",
        ),
    )
    .await;

    // Assert
    assert_eq!(rejected.status(), StatusCode::BAD_REQUEST);
    assert_eq!(still_present.status(), StatusCode::OK);
}

#[tokio::test(flavor = "multi_thread")]
async fn should_not_apply_failed_s3_conditional_delete() {
    // Arrange
    let storage = temp_storage();
    call(
        storage.clone(),
        auth_disabled(),
        request("PUT", "http://localhost/conditional-s3", &[], b""),
    )
    .await;
    call(
        storage.clone(),
        auth_disabled(),
        request(
            "PUT",
            "http://localhost/conditional-s3/object",
            &[],
            b"value",
        ),
    )
    .await;

    // Act
    let rejected = call(
        storage.clone(),
        auth_disabled(),
        request(
            "DELETE",
            "http://localhost/conditional-s3/object",
            &[("if-match", "\"stale\"")],
            b"",
        ),
    )
    .await;

    // Assert
    assert_eq!(rejected.status(), StatusCode::PRECONDITION_FAILED);
    assert_eq!(
        storage.get_object("conditional-s3", "object").unwrap().data,
        b"value"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn should_not_treat_if_none_match_as_an_s3_delete_precondition() {
    // Arrange
    let storage = temp_storage();
    call(
        storage.clone(),
        auth_disabled(),
        request("PUT", "http://localhost/delete-header-s3", &[], b""),
    )
    .await;
    call(
        storage.clone(),
        auth_disabled(),
        request(
            "PUT",
            "http://localhost/delete-header-s3/object",
            &[],
            b"value",
        ),
    )
    .await;

    // Act
    let response = call(
        storage.clone(),
        auth_disabled(),
        request(
            "DELETE",
            "http://localhost/delete-header-s3/object",
            &[("if-none-match", "*")],
            b"",
        ),
    )
    .await;

    // Assert
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    assert!(storage.get_object("delete-header-s3", "object").is_err());
}

#[tokio::test(flavor = "multi_thread")]
async fn should_return_s3_not_found_for_if_match_put_without_current_object() {
    // Arrange
    let storage = temp_storage();
    call(
        storage.clone(),
        auth_disabled(),
        request("PUT", "http://localhost/missing-match-s3", &[], b""),
    )
    .await;

    // Act
    let response = call(
        storage.clone(),
        auth_disabled(),
        request(
            "PUT",
            "http://localhost/missing-match-s3/object",
            &[("if-match", "\"missing\"")],
            b"value",
        ),
    )
    .await;

    // Assert
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert!(body_text(response).await.contains("NoSuchKey"));
    assert!(storage.get_object("missing-match-s3", "object").is_err());
}

#[tokio::test(flavor = "multi_thread")]
async fn should_reject_non_wildcard_s3_if_none_match_put() {
    // Arrange
    let storage = temp_storage();
    call(
        storage.clone(),
        auth_disabled(),
        request("PUT", "http://localhost/invalid-none-match-s3", &[], b""),
    )
    .await;

    // Act
    let response = call(
        storage.clone(),
        auth_disabled(),
        request(
            "PUT",
            "http://localhost/invalid-none-match-s3/object",
            &[("if-none-match", "\"some-etag\"")],
            b"value",
        ),
    )
    .await;

    // Assert
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert!(body_text(response).await.contains("InvalidArgument"));
    assert!(storage
        .get_object("invalid-none-match-s3", "object")
        .is_err());
}

#[tokio::test(flavor = "multi_thread")]
async fn should_enforce_azure_if_none_match_etag_on_put() {
    // Arrange
    let storage = temp_storage();
    let headers = [("x-ms-version", AZURE_VERSION)];
    call(
        storage.clone(),
        auth_disabled(),
        request(
            "PUT",
            "http://localhost/devstoreaccount1/conditional-azure?restype=container",
            &headers,
            b"",
        ),
    )
    .await;
    let created = call(
        storage.clone(),
        auth_disabled(),
        request(
            "PUT",
            "http://localhost/devstoreaccount1/conditional-azure/blob",
            &[
                ("x-ms-version", AZURE_VERSION),
                ("x-ms-blob-type", "BlockBlob"),
                ("content-length", "5"),
            ],
            b"first",
        ),
    )
    .await;
    let etag = created.headers()["etag"].to_str().unwrap().to_string();

    // Act
    let rejected = call(
        storage.clone(),
        auth_disabled(),
        request(
            "PUT",
            "http://localhost/devstoreaccount1/conditional-azure/blob",
            &[
                ("x-ms-version", AZURE_VERSION),
                ("x-ms-blob-type", "BlockBlob"),
                ("if-none-match", &etag),
                ("content-length", "6"),
            ],
            b"second",
        ),
    )
    .await;

    // Assert
    assert_eq!(rejected.status(), StatusCode::PRECONDITION_FAILED);
    assert_eq!(
        storage
            .get_object("conditional-azure", "blob")
            .unwrap()
            .data,
        b"first"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn should_not_apply_failed_gcs_generation_not_match_upload() {
    // Arrange
    let storage = temp_storage();
    call(
        storage.clone(),
        auth_disabled(),
        request(
            "POST",
            "http://localhost/storage/v1/b?project=local",
            &[("content-type", "application/json")],
            br#"{"name":"not-match-gcs"}"#,
        ),
    )
    .await;
    let created = call(
        storage.clone(),
        auth_disabled(),
        request(
            "POST",
            "http://localhost/upload/storage/v1/b/not-match-gcs/o?uploadType=media&name=object",
            &[("content-length", "5")],
            b"first",
        ),
    )
    .await;
    let created_json: serde_json::Value = serde_json::from_str(&body_text(created).await).unwrap();
    let generation = created_json["generation"].as_str().unwrap();

    // Act
    let rejected = call(
        storage.clone(),
        auth_disabled(),
        request(
            "POST",
            &format!("http://localhost/upload/storage/v1/b/not-match-gcs/o?uploadType=media&name=object&ifGenerationNotMatch={generation}"),
            &[("content-length", "6")],
            b"second",
        ),
    )
    .await;

    // Assert
    assert_eq!(rejected.status(), StatusCode::NOT_MODIFIED);
    assert!(body_text(rejected).await.is_empty());
    assert_eq!(
        storage.get_object("not-match-gcs", "object").unwrap().data,
        b"first"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn should_not_commit_precommit_redirect_or_throttle_failpoints() {
    // Arrange
    let storage = temp_storage();
    call(
        storage.clone(),
        auth_disabled(),
        request("PUT", "http://localhost/fault-s3", &[], b""),
    )
    .await;

    // Act
    let redirect = call(
        storage.clone(),
        auth_disabled(),
        request(
            "PUT",
            "http://localhost/fault-s3/redirected",
            &[("x-sqrzl-failpoint", "redirect-307")],
            b"value",
        ),
    )
    .await;
    let throttle = call(
        storage.clone(),
        auth_disabled(),
        request(
            "PUT",
            "http://localhost/fault-s3/throttled",
            &[("x-sqrzl-failpoint", "throttle")],
            b"value",
        ),
    )
    .await;

    // Assert
    assert_eq!(redirect.status(), StatusCode::TEMPORARY_REDIRECT);
    assert_eq!(throttle.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert!(storage.get_object("fault-s3", "redirected").is_err());
    assert!(storage.get_object("fault-s3", "throttled").is_err());
}

#[tokio::test(flavor = "multi_thread")]
async fn should_expose_s3_conditional_request_conflict_without_mutation() {
    // Arrange
    let storage = temp_storage();
    storage
        .create_bucket("conditional-conflict".to_string())
        .unwrap();
    storage
        .put_object(
            "conditional-conflict",
            "object".to_string(),
            sqrzl_emulator::models::Object::new(
                "object".to_string(),
                b"winner".to_vec(),
                "application/octet-stream".to_string(),
            ),
        )
        .unwrap();
    let etag = storage
        .get_object("conditional-conflict", "object")
        .unwrap()
        .etag;
    let multipart = storage
        .create_multipart_upload("conditional-conflict", "multipart-object".to_string())
        .unwrap();

    // Act
    let put_response = call(
        storage.clone(),
        auth_disabled(),
        request(
            "PUT",
            "http://localhost/conditional-conflict/object",
            &[
                ("if-match", etag.as_str()),
                ("x-sqrzl-failpoint", "conditional-request-conflict"),
            ],
            b"loser",
        ),
    )
    .await;
    let complete_response = call(
        storage.clone(),
        auth_disabled(),
        request(
            "POST",
            &format!(
                "http://localhost/conditional-conflict/multipart-object?uploadId={}",
                multipart.upload_id
            ),
            &[
                ("if-none-match", "*"),
                ("x-sqrzl-failpoint", "conditional-request-conflict"),
            ],
            b"<CompleteMultipartUpload/>",
        ),
    )
    .await;

    // Assert
    assert_s3_conditional_conflict_response(put_response).await;
    assert_s3_conditional_conflict_response(complete_response).await;
    assert_eq!(
        storage
            .get_object("conditional-conflict", "object")
            .unwrap()
            .data,
        b"winner"
    );
    assert_eq!(
        storage
            .get_multipart_upload("conditional-conflict", &multipart.upload_id)
            .unwrap()
            .key,
        "multipart-object"
    );
    assert!(storage
        .get_object("conditional-conflict", "multipart-object")
        .is_err());
}

#[tokio::test(flavor = "multi_thread")]
async fn should_render_gcs_json_throttle_as_json_429() {
    // Arrange
    let storage = temp_storage();

    // Act
    let response = call(
        storage,
        auth_disabled(),
        request(
            "GET",
            "http://localhost/storage/v1/b?project=local",
            &[("x-sqrzl-failpoint", "throttle")],
            b"",
        ),
    )
    .await;

    // Assert
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    assert!(response.headers()["content-type"]
        .to_str()
        .unwrap()
        .starts_with("application/json"));
}

#[tokio::test(flavor = "multi_thread")]
async fn should_render_gcs_xml_throttle_as_xml_429() {
    // Arrange
    let storage = temp_storage();

    // Act
    let response = call(
        storage,
        auth_disabled(),
        request(
            "GET",
            "http://localhost/gcs-xml-bucket",
            &[
                ("host", "storage.googleapis.com"),
                ("content-length", "0"),
                ("x-sqrzl-failpoint", "throttle"),
            ],
            b"",
        ),
    )
    .await;

    // Assert
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    assert!(response.headers()["content-type"]
        .to_str()
        .unwrap()
        .starts_with("application/xml"));
}

#[tokio::test(flavor = "multi_thread")]
async fn should_commit_before_postcommit_response_loss_failpoint() {
    // Arrange
    let storage = temp_storage();
    call(
        storage.clone(),
        auth_disabled(),
        request("PUT", "http://localhost/ambiguous-s3", &[], b""),
    )
    .await;

    // Act
    let response = call(
        storage.clone(),
        auth_disabled(),
        request(
            "PUT",
            "http://localhost/ambiguous-s3/object",
            &[("x-sqrzl-failpoint", "response-loss-after-commit")],
            b"committed",
        ),
    )
    .await;

    // Assert
    assert_eq!(response.headers()["content-length"], "1");
    assert_eq!(
        storage.get_object("ambiguous-s3", "object").unwrap().data,
        b"committed"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn should_not_report_response_loss_after_a_rejected_mutation() {
    // Arrange
    let storage = temp_storage();
    call(
        storage.clone(),
        auth_disabled(),
        request("PUT", "http://localhost/rejected-ambiguity-s3", &[], b""),
    )
    .await;

    // Act
    let response = call(
        storage.clone(),
        auth_disabled(),
        request(
            "PUT",
            "http://localhost/rejected-ambiguity-s3/object",
            &[
                ("if-match", "\"missing\""),
                ("x-sqrzl-failpoint", "response-loss-after-commit"),
            ],
            b"must-not-commit",
        ),
    )
    .await;

    // Assert
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert!(!response.headers().contains_key("x-sqrzl-failpoint-applied"));
    assert!(body_text(response).await.contains("NoSuchKey"));
    assert!(storage
        .get_object("rejected-ambiguity-s3", "object")
        .is_err());
}

#[tokio::test(flavor = "multi_thread")]
async fn should_surface_truncated_response_as_body_error_on_every_storage_front_door() {
    // Arrange
    let storage = temp_storage();
    for surface in read_surfaces() {
        storage.create_bucket(surface.bucket.to_string()).unwrap();
        storage
            .put_object(
                surface.bucket,
                "object".to_string(),
                sqrzl_emulator::models::Object::new(
                    "object".to_string(),
                    b"abcdefgh".to_vec(),
                    "application/octet-stream".to_string(),
                ),
            )
            .unwrap();
    }

    // Act
    // Assert
    for surface in read_surfaces() {
        let mut headers = surface.headers.to_vec();
        headers.push(("x-sqrzl-failpoint", "truncate-response"));
        let response = call(
            storage.clone(),
            auth_disabled(),
            request("GET", surface.uri, &headers, b""),
        )
        .await;
        assert_eq!(
            response.status(),
            StatusCode::OK,
            "wrong read status for {}",
            surface.front_door.label()
        );
        assert_eq!(response.headers()["content-length"], "8");
        assert_eq!(
            response.headers()["x-sqrzl-failpoint-applied"],
            "truncate-response"
        );
        let collected = response.into_body().collect().await;
        assert!(
            collected.is_err(),
            "{} truncated body must fail collection",
            surface.front_door.label()
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn should_reject_malformed_gcs_page_token() {
    // Arrange
    let storage = temp_storage();
    call(
        storage.clone(),
        auth_disabled(),
        request(
            "POST",
            "http://localhost/storage/v1/b?project=local",
            &[("content-type", "application/json")],
            br#"{"name":"pagination-gcs"}"#,
        ),
    )
    .await;

    // Act
    let response = call(
        storage,
        auth_disabled(),
        request(
            "GET",
            "http://localhost/storage/v1/b/pagination-gcs/o?pageToken=%%%",
            &[],
            b"",
        ),
    )
    .await;

    // Assert
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert!(body_text(response).await.contains("Invalid pageToken"));
}

#[tokio::test(flavor = "multi_thread")]
#[allow(clippy::too_many_lines)] // Every front door must prove both malformed and repeated token behavior in one matrix.
async fn should_rewrite_malformed_and_repeated_pagination_tokens_on_every_storage_front_door() {
    // Arrange
    let storage = temp_storage();
    for surface in pagination_surfaces() {
        storage.create_bucket(surface.bucket.to_string()).unwrap();
        for key in ["a", "b"] {
            storage
                .put_object(
                    surface.bucket,
                    key.to_string(),
                    sqrzl_emulator::models::Object::new(
                        key.to_string(),
                        key.as_bytes().to_vec(),
                        "application/octet-stream".to_string(),
                    ),
                )
                .unwrap();
        }
    }

    // Act
    // Assert
    for surface in pagination_surfaces() {
        let first = call(
            storage.clone(),
            auth_disabled(),
            request("GET", surface.first_uri, surface.headers, b""),
        )
        .await;
        assert_eq!(
            first.status(),
            StatusCode::OK,
            "first page failed for {}",
            surface.front_door.label()
        );
        let first_token =
            pagination_token(&body_text(first).await, surface.body).unwrap_or_else(|| {
                panic!(
                    "first page omitted token for {}",
                    surface.front_door.label()
                )
            });

        let next_uri = format!(
            "{}&{}={}",
            surface.first_uri,
            surface.token_parameter,
            urlencoding::encode(&first_token)
        );
        let mut repeated_headers = surface.headers.to_vec();
        repeated_headers.push(("x-sqrzl-failpoint", "repeated-pagination-token"));
        let repeated = call(
            storage.clone(),
            auth_disabled(),
            request("GET", &next_uri, &repeated_headers, b""),
        )
        .await;
        assert_eq!(
            repeated.status(),
            StatusCode::OK,
            "repeated-token page failed for {}",
            surface.front_door.label()
        );
        assert_eq!(
            repeated.headers()["x-sqrzl-failpoint-applied"],
            "repeated-pagination-token"
        );
        let repeated_body = body_text(repeated).await;
        let repeated_token = pagination_token(&repeated_body, surface.body).unwrap_or_else(|| {
            panic!(
                "repeated page omitted token for {}",
                surface.front_door.label()
            )
        });
        assert_eq!(
            repeated_token,
            first_token,
            "token was not repeated for {}",
            surface.front_door.label()
        );
        assert!(
            sdk_like_has_next_page(surface, &repeated_body),
            "{} SDK-like paginator would not follow the repeated token",
            surface.front_door.label()
        );
        let followed_uri = format!(
            "{}&{}={}",
            surface.first_uri,
            surface.token_parameter,
            urlencoding::encode(&repeated_token)
        );
        let followed = call(
            storage.clone(),
            auth_disabled(),
            request("GET", &followed_uri, &repeated_headers, b""),
        )
        .await;
        assert_eq!(followed.status(), StatusCode::OK);
        let followed_body = body_text(followed).await;
        assert!(sdk_like_has_next_page(surface, &followed_body));
        assert_eq!(
            pagination_token(&followed_body, surface.body).as_deref(),
            Some(repeated_token.as_str()),
            "{} SDK-like paginator did not observe the repeated cursor twice",
            surface.front_door.label()
        );

        let mut malformed_headers = surface.headers.to_vec();
        malformed_headers.push(("x-sqrzl-failpoint", "malformed-pagination-token"));
        let malformed = call(
            storage.clone(),
            auth_disabled(),
            request("GET", surface.first_uri, &malformed_headers, b""),
        )
        .await;
        assert_eq!(
            malformed.status(),
            StatusCode::OK,
            "malformed-token page failed for {}",
            surface.front_door.label()
        );
        assert_eq!(
            malformed.headers()["x-sqrzl-failpoint-applied"],
            "malformed-pagination-token"
        );
        assert_eq!(
            pagination_token(&body_text(malformed).await, surface.body).as_deref(),
            Some("%%%not-a-valid-token%%%"),
            "wrong malformed token for {}",
            surface.front_door.label()
        );
        assert_eq!(storage.get_object(surface.bucket, "a").unwrap().data, b"a");
        assert_eq!(storage.get_object(surface.bucket, "b").unwrap().data, b"b");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn should_retain_gcs_soft_deleted_object_outside_current_listing() {
    // Arrange
    let storage = temp_storage();
    call(
        storage.clone(),
        auth_disabled(),
        request(
            "POST",
            "http://localhost/storage/v1/b?project=local",
            &[("content-type", "application/json")],
            br#"{"name":"soft-delete-gcs","softDeletePolicy":{"retentionDurationSeconds":"604800"}}"#,
        ),
    )
    .await;
    call(
        storage.clone(),
        auth_disabled(),
        request(
            "POST",
            "http://localhost/upload/storage/v1/b/soft-delete-gcs/o?uploadType=media&name=object",
            &[("content-length", "8")],
            b"retained",
        ),
    )
    .await;

    // Act
    let deleted = call(
        storage.clone(),
        auth_disabled(),
        request(
            "DELETE",
            "http://localhost/storage/v1/b/soft-delete-gcs/o/object",
            &[],
            b"",
        ),
    )
    .await;

    // Assert
    assert_eq!(deleted.status(), StatusCode::NO_CONTENT);
    assert!(storage.get_object("soft-delete-gcs", "object").is_err());
    assert!(!storage
        .list_object_versions_for_key("soft-delete-gcs", "object")
        .unwrap()
        .is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn should_not_report_gcs_soft_delete_as_s3_versioning_or_history() {
    // Arrange
    let storage = temp_storage();
    create_gcs_bucket(
        &storage,
        br#"{"name":"isolated-gcs-history","softDeletePolicy":{"retentionDurationSeconds":"604800"}}"#,
    )
    .await;
    upload_gcs_object(&storage, "isolated-gcs-history", "object", b"retained").await;
    let deleted = call(
        storage.clone(),
        auth_disabled(),
        request(
            "DELETE",
            "http://localhost/storage/v1/b/isolated-gcs-history/o/object",
            &[],
            b"",
        ),
    )
    .await;
    assert_eq!(deleted.status(), StatusCode::NO_CONTENT);

    // Act
    let versioning = call(
        storage.clone(),
        auth_disabled(),
        request(
            "GET",
            "http://localhost/isolated-gcs-history?versioning",
            &[],
            b"",
        ),
    )
    .await;
    let versions = call(
        storage,
        auth_disabled(),
        request(
            "GET",
            "http://localhost/isolated-gcs-history?versions",
            &[],
            b"",
        ),
    )
    .await;

    // Assert
    assert_eq!(versioning.status(), StatusCode::OK);
    let versioning_body = body_text(versioning).await;
    assert!(!versioning_body.contains("<Status>Enabled</Status>"));
    assert!(!versioning_body.contains("<Status>Suspended</Status>"));
    assert_eq!(versions.status(), StatusCode::CONFLICT);
    assert!(body_text(versions).await.contains("InvalidBucketState"));
}

#[tokio::test(flavor = "multi_thread")]
async fn should_reject_s3_object_access_while_gcs_owns_retained_history() {
    // Arrange
    let storage = temp_storage();
    create_gcs_bucket(
        &storage,
        br#"{"name":"isolated-gcs-object","softDeletePolicy":{"retentionDurationSeconds":"604800"}}"#,
    )
    .await;
    upload_gcs_object(&storage, "isolated-gcs-object", "object", b"original").await;

    // Act
    let get = call(
        storage.clone(),
        auth_disabled(),
        request(
            "GET",
            "http://localhost/isolated-gcs-object/object",
            &[],
            b"",
        ),
    )
    .await;
    let head = call(
        storage.clone(),
        auth_disabled(),
        request(
            "HEAD",
            "http://localhost/isolated-gcs-object/object",
            &[],
            b"",
        ),
    )
    .await;
    let put = call(
        storage.clone(),
        auth_disabled(),
        request(
            "PUT",
            "http://localhost/isolated-gcs-object/object",
            &[("content-length", "7")],
            b"changed",
        ),
    )
    .await;
    let delete = call(
        storage.clone(),
        auth_disabled(),
        request(
            "DELETE",
            "http://localhost/isolated-gcs-object/object",
            &[],
            b"",
        ),
    )
    .await;

    // Assert
    for response in [&get, &head, &put, &delete] {
        assert_eq!(response.status(), StatusCode::CONFLICT);
        assert!(response.headers().get("x-amz-version-id").is_none());
        assert!(response.headers().get("x-amz-delete-marker").is_none());
    }
    assert_eq!(
        storage
            .get_object("isolated-gcs-object", "object")
            .unwrap()
            .data,
        b"original"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn should_reject_s3_mutation_while_gcs_retention_owns_the_bucket() {
    // Arrange
    let storage = temp_storage();
    create_gcs_bucket(
        &storage,
        br#"{"name":"isolated-gcs-retention","retentionPolicy":{"retentionPeriod":"3600"}}"#,
    )
    .await;
    upload_gcs_object(&storage, "isolated-gcs-retention", "object", b"original").await;

    // Act
    let put = call(
        storage.clone(),
        auth_disabled(),
        request(
            "PUT",
            "http://localhost/isolated-gcs-retention/object",
            &[("content-length", "7")],
            b"changed",
        ),
    )
    .await;
    let delete = call(
        storage.clone(),
        auth_disabled(),
        request(
            "DELETE",
            "http://localhost/isolated-gcs-retention/object",
            &[],
            b"",
        ),
    )
    .await;

    // Assert
    assert_eq!(put.status(), StatusCode::CONFLICT);
    assert_eq!(delete.status(), StatusCode::CONFLICT);
    assert_eq!(
        storage
            .get_object("isolated-gcs-retention", "object")
            .unwrap()
            .data,
        b"original"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn should_reject_s3_mutation_while_azure_versioning_owns_the_bucket() {
    // Arrange
    let storage = temp_storage();
    let created = call(
        storage.clone(),
        auth_disabled(),
        request(
            "PUT",
            "http://localhost/devstoreaccount1/isolated-azure?restype=container",
            &[
                ("x-ms-version", AZURE_VERSION),
                ("x-sqrzl-azure-versioning-enabled", "true"),
            ],
            b"",
        ),
    )
    .await;
    assert_eq!(created.status(), StatusCode::CREATED);

    // Act
    let put = call(
        storage.clone(),
        auth_disabled(),
        request(
            "PUT",
            "http://localhost/isolated-azure/object",
            &[("content-length", "7")],
            b"blocked",
        ),
    )
    .await;

    // Assert
    assert_eq!(put.status(), StatusCode::CONFLICT);
    assert!(body_text(put).await.contains("InvalidBucketState"));
    assert!(storage.get_object("isolated-azure", "object").is_err());
}

#[tokio::test(flavor = "multi_thread")]
async fn should_reject_s3_versioning_changes_on_foreign_protected_bucket() {
    // Arrange
    let storage = temp_storage();
    create_gcs_bucket(
        &storage,
        br#"{"name":"isolated-enable-s3","softDeletePolicy":{"retentionDurationSeconds":"604800"}}"#,
    )
    .await;
    let suspension_body = br#"<VersioningConfiguration xmlns="http://s3.amazonaws.com/doc/2006-03-01/"><Status>Suspended</Status></VersioningConfiguration>"#;
    let enablement_body = br#"<VersioningConfiguration xmlns="http://s3.amazonaws.com/doc/2006-03-01/"><Status>Enabled</Status></VersioningConfiguration>"#;

    // Act
    let suspended = call(
        storage.clone(),
        auth_disabled(),
        request(
            "PUT",
            "http://localhost/isolated-enable-s3?versioning",
            &[],
            suspension_body,
        ),
    )
    .await;
    let enabled = call(
        storage.clone(),
        auth_disabled(),
        request(
            "PUT",
            "http://localhost/isolated-enable-s3?versioning",
            &[],
            enablement_body,
        ),
    )
    .await;

    // Assert
    assert_eq!(suspended.status(), StatusCode::CONFLICT);
    assert_eq!(enabled.status(), StatusCode::CONFLICT);
    assert!(!storage
        .get_bucket("isolated-enable-s3")
        .unwrap()
        .metadata
        .contains_key("s3_versioning_status"));
}

#[tokio::test(flavor = "multi_thread")]
async fn should_reject_gcs_data_protection_activation_on_s3_versioned_bucket() {
    // Arrange
    let storage = temp_storage();
    let created = call(
        storage.clone(),
        auth_disabled(),
        request("PUT", "http://localhost/isolated-s3-first", &[], b""),
    )
    .await;
    assert_eq!(created.status(), StatusCode::OK);
    let enabled = call(
        storage.clone(),
        auth_disabled(),
        request(
            "PUT",
            "http://localhost/isolated-s3-first?versioning",
            &[],
            br#"<VersioningConfiguration xmlns="http://s3.amazonaws.com/doc/2006-03-01/"><Status>Enabled</Status></VersioningConfiguration>"#,
        ),
    )
    .await;
    assert_eq!(enabled.status(), StatusCode::OK);

    // Act
    let gcs = call(
        storage.clone(),
        auth_disabled(),
        request(
            "PATCH",
            "http://localhost/storage/v1/b/isolated-s3-first",
            &[("content-type", "application/json")],
            br#"{"softDeletePolicy":{"retentionDurationSeconds":"604800"}}"#,
        ),
    )
    .await;

    // Assert
    assert_eq!(gcs.status(), StatusCode::CONFLICT);
    assert_gcs_json_error(&body_text(gcs).await, "conflict");
    let bucket = storage.get_bucket("isolated-s3-first").unwrap();
    assert_eq!(
        bucket
            .metadata
            .get("s3_versioning_status")
            .map(String::as_str),
        Some("Enabled")
    );
    assert!(!bucket.metadata.contains_key("gcs_soft_delete_seconds"));
}

#[tokio::test(flavor = "multi_thread")]
async fn should_reject_s3_history_access_for_legacy_mixed_provider_state() {
    // Arrange
    let storage = temp_storage();
    call(
        storage.clone(),
        auth_disabled(),
        request("PUT", "http://localhost/legacy-mixed", &[], b""),
    )
    .await;
    call(
        storage.clone(),
        auth_disabled(),
        request(
            "PUT",
            "http://localhost/legacy-mixed?versioning",
            &[],
            br#"<VersioningConfiguration xmlns="http://s3.amazonaws.com/doc/2006-03-01/"><Status>Enabled</Status></VersioningConfiguration>"#,
        ),
    )
    .await;
    let mut metadata = storage.get_bucket("legacy-mixed").unwrap().metadata;
    metadata.insert("gcs_soft_delete_seconds".to_string(), "604800".to_string());
    storage
        .update_bucket_metadata("legacy-mixed", metadata)
        .unwrap();

    // Act
    let put = call(
        storage.clone(),
        auth_disabled(),
        request(
            "PUT",
            "http://localhost/legacy-mixed/object",
            &[("content-length", "7")],
            b"blocked",
        ),
    )
    .await;
    let versions = call(
        storage.clone(),
        auth_disabled(),
        request("GET", "http://localhost/legacy-mixed?versions", &[], b""),
    )
    .await;

    // Assert
    assert_eq!(put.status(), StatusCode::CONFLICT);
    assert_eq!(versions.status(), StatusCode::CONFLICT);
    assert!(storage.get_object("legacy-mixed", "object").is_err());
}

#[tokio::test(flavor = "multi_thread")]
async fn should_reject_gcs_json_history_operations_on_azure_versioned_bucket() {
    // Arrange
    let storage = temp_storage();
    let azure_headers = [
        ("x-ms-version", AZURE_VERSION),
        ("x-sqrzl-azure-versioning-enabled", "true"),
    ];
    call(
        storage.clone(),
        auth_disabled(),
        request(
            "PUT",
            "http://localhost/devstoreaccount1/gcs-vs-azure?restype=container",
            &azure_headers,
            b"",
        ),
    )
    .await;
    call(
        storage.clone(),
        auth_disabled(),
        request(
            "PUT",
            "http://localhost/devstoreaccount1/gcs-vs-azure/object",
            &[
                ("x-ms-version", AZURE_VERSION),
                ("x-ms-blob-type", "BlockBlob"),
                ("content-length", "8"),
            ],
            b"original",
        ),
    )
    .await;

    // Act
    let get = call(
        storage.clone(),
        auth_disabled(),
        request(
            "GET",
            "http://localhost/storage/v1/b/gcs-vs-azure/o/object",
            &[],
            b"",
        ),
    )
    .await;
    let list = call(
        storage.clone(),
        auth_disabled(),
        request(
            "GET",
            "http://localhost/storage/v1/b/gcs-vs-azure/o",
            &[],
            b"",
        ),
    )
    .await;
    let put = call(
        storage.clone(),
        auth_disabled(),
        request(
            "POST",
            "http://localhost/upload/storage/v1/b/gcs-vs-azure/o?uploadType=media&name=object",
            &[("content-length", "7")],
            b"changed",
        ),
    )
    .await;
    let delete = call(
        storage.clone(),
        auth_disabled(),
        request(
            "DELETE",
            "http://localhost/storage/v1/b/gcs-vs-azure/o/object",
            &[],
            b"",
        ),
    )
    .await;

    // Assert
    for response in [&get, &list, &put, &delete] {
        assert_eq!(response.status(), StatusCode::CONFLICT);
        assert!(response.headers()["content-type"]
            .to_str()
            .unwrap()
            .starts_with("application/json"));
    }
    assert_gcs_json_error(&body_text(get).await, "conflict");
    assert_eq!(
        storage.get_object("gcs-vs-azure", "object").unwrap().data,
        b"original"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn should_reject_gcs_xml_history_operations_on_s3_versioned_bucket() {
    // Arrange
    let storage = temp_storage();
    call(
        storage.clone(),
        auth_disabled(),
        request("PUT", "http://localhost/gcs-vs-s3", &[], b""),
    )
    .await;
    call(
        storage.clone(),
        auth_disabled(),
        request(
            "PUT",
            "http://localhost/gcs-vs-s3?versioning",
            &[],
            br#"<VersioningConfiguration xmlns="http://s3.amazonaws.com/doc/2006-03-01/"><Status>Enabled</Status></VersioningConfiguration>"#,
        ),
    )
    .await;
    call(
        storage.clone(),
        auth_disabled(),
        request(
            "PUT",
            "http://localhost/gcs-vs-s3/object",
            &[("content-length", "8")],
            b"original",
        ),
    )
    .await;
    let gcs_host = [("host", "storage.googleapis.com"), ("content-length", "0")];

    // Act
    let get = call(
        storage.clone(),
        auth_disabled(),
        request("GET", "http://localhost/gcs-vs-s3/object", &gcs_host, b""),
    )
    .await;
    let list = call(
        storage.clone(),
        auth_disabled(),
        request("GET", "http://localhost/gcs-vs-s3", &gcs_host, b""),
    )
    .await;
    let put = call(
        storage.clone(),
        auth_disabled(),
        request(
            "PUT",
            "http://localhost/gcs-vs-s3/object",
            &[("host", "storage.googleapis.com"), ("content-length", "7")],
            b"changed",
        ),
    )
    .await;
    let delete = call(
        storage.clone(),
        auth_disabled(),
        request(
            "DELETE",
            "http://localhost/gcs-vs-s3/object",
            &gcs_host,
            b"",
        ),
    )
    .await;

    // Assert
    for response in [&get, &list, &put, &delete] {
        assert_eq!(response.status(), StatusCode::CONFLICT);
        assert!(response.headers()["content-type"]
            .to_str()
            .unwrap()
            .starts_with("application/xml"));
    }
    assert!(body_text(get)
        .await
        .contains("<Code>InvalidBucketState</Code>"));
    assert_eq!(
        storage.get_object("gcs-vs-s3", "object").unwrap().data,
        b"original"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn should_return_provider_specific_missing_object_responses() {
    // Arrange
    let storage = temp_storage();
    for bucket in [
        "missing-s3",
        "missing-azure",
        "missing-gcs-json",
        "missing-gcs-xml",
        "missing-oci",
    ] {
        storage.create_bucket(bucket.to_string()).unwrap();
    }

    // Act
    let s3 = call(
        storage.clone(),
        auth_disabled(),
        request("GET", "http://localhost/missing-s3/object", &[], b""),
    )
    .await;
    let gcs_json = call(
        storage.clone(),
        auth_disabled(),
        request(
            "GET",
            "http://localhost/storage/v1/b/missing-gcs-json/o/object",
            &[],
            b"",
        ),
    )
    .await;
    let gcs_xml = call(
        storage.clone(),
        auth_disabled(),
        request(
            "GET",
            "http://localhost/missing-gcs-xml/object",
            &[("host", "storage.googleapis.com"), ("content-length", "0")],
            b"",
        ),
    )
    .await;
    let azure = call(
        storage.clone(),
        auth_disabled(),
        request(
            "GET",
            "http://localhost/devstoreaccount1/missing-azure/object",
            &[("x-ms-version", AZURE_VERSION)],
            b"",
        ),
    )
    .await;
    let oci = call(
        storage.clone(),
        auth_disabled(),
        request(
            "GET",
            "http://localhost/n/sqrzl-emulator/b/missing-oci/o/object",
            &[],
            b"",
        ),
    )
    .await;

    // Assert
    assert_eq!(s3.status(), StatusCode::NOT_FOUND);
    assert!(s3.headers()["content-type"]
        .to_str()
        .unwrap()
        .starts_with("application/xml"));
    assert!(body_text(s3).await.contains("NoSuchKey"));
    assert_eq!(gcs_json.status(), StatusCode::NOT_FOUND);
    assert!(gcs_json.headers()["content-type"]
        .to_str()
        .unwrap()
        .starts_with("application/json"));
    assert!(body_text(gcs_json).await.contains("No such object"));
    assert_eq!(gcs_xml.status(), StatusCode::NOT_FOUND);
    assert!(gcs_xml.headers()["content-type"]
        .to_str()
        .unwrap()
        .starts_with("application/xml"));
    assert!(body_text(gcs_xml).await.contains("NoSuchKey"));
    assert_eq!(azure.status(), StatusCode::NOT_FOUND);
    assert_eq!(azure.headers()["x-ms-error-code"], "BlobNotFound");
    assert!(body_text(azure).await.contains("BlobNotFound"));
    assert_eq!(oci.status(), StatusCode::NOT_FOUND);
    assert!(body_text(oci).await.contains("ObjectNotFound"));
    for bucket in [
        "missing-s3",
        "missing-azure",
        "missing-gcs-json",
        "missing-gcs-xml",
        "missing-oci",
    ] {
        assert!(storage.get_object(bucket, "object").is_err());
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn should_return_every_redirect_before_mutation_on_every_storage_front_door() {
    // Arrange
    let storage = temp_storage();
    create_mutation_buckets(&storage);
    let cases = [
        ("redirect-301", StatusCode::MOVED_PERMANENTLY),
        ("redirect-302", StatusCode::FOUND),
        ("redirect-303", StatusCode::SEE_OTHER),
        ("redirect-307", StatusCode::TEMPORARY_REDIRECT),
        ("redirect-308", StatusCode::PERMANENT_REDIRECT),
    ];

    // Act
    // Assert
    for surface in mutation_surfaces() {
        for (failpoint, expected) in cases {
            let headers = failpoint_headers(&surface, failpoint, None);
            let response = call(
                storage.clone(),
                auth_disabled(),
                framed_mutation_request(&surface, &headers, b"must-not-commit"),
            )
            .await;
            assert_eq!(
                response.status(),
                expected,
                "wrong {failpoint} status for {}",
                surface.front_door.label()
            );
            assert_eq!(
                response.headers()["location"],
                "http://127.0.0.1:1/sqrzl-redirect-target"
            );
            assert_eq!(response.headers()["x-sqrzl-failpoint-applied"], failpoint);
            assert!(storage.get_object(surface.bucket, surface.key).is_err());
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn should_keep_redirects_precommit_on_every_storage_front_door() {
    // Arrange
    let storage = temp_storage();
    create_mutation_buckets(&storage);

    // Act
    // Assert
    for surface in mutation_surfaces() {
        let mut headers = failpoint_headers(&surface, "redirect-307", None);
        headers.push((
            "x-sqrzl-redirect-location",
            "https://invalid.example/sqrzl-test-only",
        ));
        let response = call(
            storage.clone(),
            auth_disabled(),
            framed_mutation_request(&surface, &headers, b"must-not-commit"),
        )
        .await;
        assert_eq!(response.status(), StatusCode::TEMPORARY_REDIRECT);
        assert_eq!(
            response.headers()["location"],
            "https://invalid.example/sqrzl-test-only"
        );
        assert!(storage.get_object(surface.bucket, surface.key).is_err());
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn should_distinguish_timeout_before_and_after_commit_on_every_front_door() {
    // Arrange
    let storage = temp_storage();
    create_mutation_buckets(&storage);

    // Act
    // Assert
    for surface in mutation_surfaces() {
        let before_headers = failpoint_headers(&surface, "timeout-before-commit", Some("100"));
        let before = tokio::time::timeout(
            std::time::Duration::from_millis(10),
            call(
                storage.clone(),
                auth_disabled(),
                framed_mutation_request(&surface, &before_headers, b"before"),
            ),
        )
        .await;
        assert!(before.is_err());
        assert!(storage.get_object(surface.bucket, surface.key).is_err());

        let after_headers = failpoint_headers(&surface, "timeout-after-commit", Some("100"));
        let after = tokio::time::timeout(
            std::time::Duration::from_millis(10),
            call(
                storage.clone(),
                auth_disabled(),
                framed_mutation_request(&surface, &after_headers, b"after"),
            ),
        )
        .await;
        assert!(after.is_err());
        assert_eq!(
            storage
                .get_object(surface.bucket, surface.key)
                .unwrap()
                .data,
            b"after"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn should_commit_before_response_loss_on_every_storage_front_door() {
    // Arrange
    let storage = temp_storage();
    create_mutation_buckets(&storage);

    // Act
    // Assert
    for surface in mutation_surfaces() {
        let headers = failpoint_headers(&surface, "response-loss-after-commit", None);
        let response = call(
            storage.clone(),
            auth_disabled(),
            framed_mutation_request(&surface, &headers, b"committed"),
        )
        .await;
        assert_eq!(response.status(), surface.success);
        assert_eq!(response.headers()["content-length"], "1");
        assert_eq!(
            response.headers()["x-sqrzl-failpoint-applied"],
            "response-loss-after-commit"
        );
        assert_eq!(
            storage
                .get_object(surface.bucket, surface.key)
                .unwrap()
                .data,
            b"committed"
        );
        assert!(
            response.into_body().collect().await.is_err(),
            "{} response loss must surface as a body error",
            surface.front_door.label()
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn should_return_provider_shaped_throttling_without_mutation() {
    // Arrange
    let storage = temp_storage();
    create_mutation_buckets(&storage);

    // Act
    // Assert
    for surface in mutation_surfaces() {
        let headers = failpoint_headers(&surface, "throttle", None);
        let response = call(
            storage.clone(),
            auth_disabled(),
            framed_mutation_request(&surface, &headers, b"must-not-commit"),
        )
        .await;
        assert_eq!(response.status(), surface.front_door.throttle_status());
        assert_eq!(response.headers()["retry-after"], "1");
        assert!(response.headers()["content-type"]
            .to_str()
            .unwrap()
            .starts_with(surface.front_door.error_content_type()));
        assert_provider_fault_headers(surface.front_door, response.headers());
        assert!(
            body_text(response)
                .await
                .contains(surface.front_door.throttle_marker()),
            "wrong throttle body for {}",
            surface.front_door.label()
        );
        assert!(storage.get_object(surface.bucket, surface.key).is_err());
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn should_expose_each_transient_status_before_commit_on_every_storage_front_door() {
    // Arrange
    let storage = temp_storage();
    create_mutation_buckets(&storage);
    let cases = [
        ("transient-500", StatusCode::INTERNAL_SERVER_ERROR),
        ("transient-502", StatusCode::BAD_GATEWAY),
        ("transient-503", StatusCode::SERVICE_UNAVAILABLE),
        ("transient-504", StatusCode::GATEWAY_TIMEOUT),
    ];

    // Act
    // Assert
    for surface in mutation_surfaces() {
        for (failpoint, expected) in cases {
            let headers = failpoint_headers(&surface, failpoint, None);
            let response = call(
                storage.clone(),
                auth_disabled(),
                framed_mutation_request(&surface, &headers, b"must-not-commit"),
            )
            .await;
            assert_eq!(
                response.status(),
                expected,
                "wrong {failpoint} status for {}",
                surface.front_door.label()
            );
            assert_eq!(response.headers()["retry-after"], "1");
            assert_provider_fault_headers(surface.front_door, response.headers());
            if matches!(surface.front_door, StorageFrontDoor::GcsJson)
                && (expected == StatusCode::BAD_GATEWAY || expected == StatusCode::GATEWAY_TIMEOUT)
            {
                assert!(!response.headers().contains_key("content-type"));
                assert!(body_text(response).await.is_empty());
            } else {
                assert!(response.headers()["content-type"]
                    .to_str()
                    .unwrap()
                    .starts_with(surface.front_door.error_content_type()));
                assert!(
                    body_text(response)
                        .await
                        .contains(surface.front_door.transient_marker(failpoint)),
                    "wrong {failpoint} body for {}",
                    surface.front_door.label()
                );
            }
            assert!(storage.get_object(surface.bucket, surface.key).is_err());
        }
    }
}
