mod common;

use common::interop::{auth_disabled, temp_storage};
use http_body_util::Full;
use hyper::{Request, StatusCode};
use sqrzl_emulator::models::Object;
use sqrzl_emulator::providers::AdapterRegistry;
use sqrzl_emulator::server::RequestExt;
use sqrzl_emulator::storage::{IndexedStorage, Storage};
use std::sync::Arc;
use std::time::Duration;

#[tokio::test(flavor = "multi_thread")]
async fn should_reject_foreign_overwrite_given_stale_wrapper_when_inner_claims_azure_lease() {
    // Arrange
    let inner = temp_storage();
    inner.create_bucket("coordinated".to_string()).unwrap();
    let wrapper: Arc<dyn Storage> = Arc::new(IndexedStorage::new(inner.clone()));
    let registry = AdapterRegistry::default();
    for (uri, headers, body) in [
        (
            "http://localhost/devstoreaccount1/coordinated/target",
            vec![("x-ms-blob-type", "BlockBlob")],
            b"old".as_slice(),
        ),
        (
            "http://localhost/devstoreaccount1/coordinated/target?comp=lease",
            vec![
                ("x-ms-lease-action", "acquire"),
                ("x-ms-lease-duration", "-1"),
            ],
            b"".as_slice(),
        ),
    ] {
        let mut request = Request::builder()
            .method("PUT")
            .uri(uri)
            .header("x-ms-version", "2023-11-03")
            .header("content-length", body.len());
        for (name, value) in headers {
            request = request.header(name, value);
        }
        let request = RequestExt::from_hyper(
            request
                .body(Full::new(bytes::Bytes::copy_from_slice(body)))
                .unwrap(),
        )
        .await
        .unwrap();
        assert!(registry
            .handle(inner.clone(), auth_disabled(), request)
            .await
            .unwrap()
            .status()
            .is_success());
    }
    let request = Request::builder()
        .method("PUT")
        .uri("http://localhost/coordinated/target")
        .header("content-length", 3)
        .body(Full::new(bytes::Bytes::from_static(b"new")))
        .unwrap();

    // Act
    let response = registry
        .handle(
            wrapper,
            auth_disabled(),
            RequestExt::from_hyper(request).await.unwrap(),
        )
        .await
        .unwrap();

    // Assert
    assert_eq!(response.status(), StatusCode::CONFLICT);
    assert_eq!(
        inner.get_object("coordinated", "target").unwrap().data,
        b"old"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn should_serialize_copy_and_completion_given_active_owner_when_a_lease_is_committing() {
    // Arrange
    for complete in [false, true] {
        let storage = temp_storage();
        storage.create_bucket("coordinated".to_string()).unwrap();
        for key in ["target", "source"] {
            storage
                .put_object(
                    "coordinated",
                    key.to_string(),
                    Object::new(key.to_string(), b"old".to_vec(), "text/plain".to_string()),
                )
                .unwrap();
        }
        let upload = storage
            .create_multipart_upload("coordinated", "target".to_string())
            .unwrap();
        let etag = storage
            .upload_part("coordinated", &upload.upload_id, 1, b"part".to_vec())
            .unwrap();
        let body = if complete {
            format!("<CompleteMultipartUpload><Part><PartNumber>1</PartNumber><ETag>\"{etag}\"</ETag></Part></CompleteMultipartUpload>")
        } else {
            String::new()
        };
        let uri = if complete {
            format!(
                "http://localhost/coordinated/target?uploadId={}",
                upload.upload_id
            )
        } else {
            "http://localhost/coordinated/target".to_string()
        };
        let mut builder = Request::builder()
            .method(if complete { "POST" } else { "PUT" })
            .uri(uri)
            .header("host", "localhost")
            .header("content-length", body.len());
        if !complete {
            builder = builder.header("x-amz-copy-source", "/coordinated/source");
        }
        let request =
            RequestExt::from_hyper(builder.body(Full::new(bytes::Bytes::from(body))).unwrap())
                .await
                .unwrap();
        let gate = storage.operation_gate();
        let claim = gate.lock().await;
        let mut mutation = tokio::spawn({
            let storage = storage.clone();
            async move {
                AdapterRegistry::default()
                    .handle(storage, auth_disabled(), request)
                    .await
                    .unwrap()
            }
        });

        // Act
        assert!(
            tokio::time::timeout(Duration::from_millis(100), &mut mutation)
                .await
                .is_err()
        );
        let observed = storage
            .get_object_metadata("coordinated", "target")
            .unwrap();
        let mut held = observed.clone();
        held.provider_metadata
            .insert("azure_lease_status".to_string(), "locked".to_string());
        storage
            .replace_object_metadata_if_unchanged("coordinated", "target", &observed, &held)
            .unwrap();
        drop(claim);

        // Assert
        assert_eq!(mutation.await.unwrap().status(), StatusCode::CONFLICT);
        assert_eq!(
            storage.get_object("coordinated", "target").unwrap().data,
            b"old"
        );
        assert_eq!(
            storage
                .list_parts("coordinated", &upload.upload_id)
                .unwrap()
                .len(),
            1
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn should_preserve_active_uploads_given_concurrent_bucket_deletion_when_part_is_committing() {
    // Arrange
    let storage = temp_storage();
    storage.create_bucket("coordinated".to_string()).unwrap();
    let upload = storage
        .create_multipart_upload("coordinated", "target".to_string())
        .unwrap();
    let gate = storage.operation_gate();
    let claim = gate.lock().await;
    let request = Request::builder()
        .method("DELETE")
        .uri("http://localhost/coordinated")
        .header("host", "localhost")
        .body(Full::new(bytes::Bytes::new()))
        .unwrap();
    let request = RequestExt::from_hyper(request).await.unwrap();
    let mut deletion = tokio::spawn({
        let storage = storage.clone();
        async move {
            AdapterRegistry::default()
                .handle(storage, auth_disabled(), request)
                .await
                .unwrap()
        }
    });

    // Act
    assert!(
        tokio::time::timeout(Duration::from_millis(100), &mut deletion)
            .await
            .is_err()
    );
    let etag = storage
        .upload_part("coordinated", &upload.upload_id, 1, b"part".to_vec())
        .unwrap();
    drop(claim);

    // Assert
    assert_eq!(deletion.await.unwrap().status(), StatusCode::CONFLICT);
    assert_eq!(
        storage
            .list_parts("coordinated", &upload.upload_id)
            .unwrap()[0]
            .etag,
        etag
    );
    storage
        .abort_multipart_upload("coordinated", &upload.upload_id)
        .unwrap();
    storage.delete_bucket("coordinated").unwrap();
    storage.create_bucket("coordinated".to_string()).unwrap();
    assert!(matches!(
        storage.get_multipart_upload("coordinated", &upload.upload_id),
        Err(sqrzl_emulator::Error::NoSuchUpload)
    ));
    assert!(storage
        .list_objects("coordinated", None, None, None, None)
        .unwrap()
        .objects
        .is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn should_preserve_azure_owned_objects_given_foreign_mutations_when_a_lease_is_active() {
    // Arrange
    let mut failures = Vec::new();
    for (provider, path, host) in [
        ("s3", "/coordinated/target", "localhost"),
        ("gcs", "/coordinated/target", "storage.googleapis.com"),
        (
            "oci",
            "/n/sqrzl-emulator/b/coordinated/o/target",
            "localhost",
        ),
    ] {
        for method in ["DELETE", "PUT"] {
            let storage = temp_storage();
            storage.create_bucket("coordinated".to_string()).unwrap();
            let mut object = Object::new(
                "target".to_string(),
                b"old".to_vec(),
                "text/plain".to_string(),
            );
            object
                .provider_metadata
                .insert("azure_lease_status".to_string(), "locked".to_string());
            object.provider_metadata.insert(
                "azure_lease_id".to_string(),
                uuid::Uuid::new_v4().to_string(),
            );
            storage
                .put_object("coordinated", "target".to_string(), object)
                .unwrap();
            let request = Request::builder()
                .method(method)
                .uri(format!("http://localhost{path}"))
                .header("host", host)
                .header("content-length", "11")
                .body(Full::new(bytes::Bytes::from_static(b"replacement")))
                .unwrap();
            let request = RequestExt::from_hyper(request).await.unwrap();

            // Act
            let response = AdapterRegistry::default()
                .handle(storage.clone(), auth_disabled(), request)
                .await
                .unwrap();

            // Assert
            if response.status() != StatusCode::CONFLICT {
                failures.push(format!("{provider} {method}: {}", response.status()));
            }
            if !storage
                .get_object("coordinated", "target")
                .is_ok_and(|object| {
                    object.data == b"old"
                        && object
                            .provider_metadata
                            .get("azure_lease_status")
                            .is_some_and(|status| status == "locked")
                })
            {
                failures.push(format!("{provider} {method} changed leased generation"));
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("; "));
}

#[tokio::test(flavor = "multi_thread")]
async fn should_preserve_protected_data_given_admin_mutations_when_local_guards_are_active() {
    // Arrange
    let protections = [
        ("s3_object_lock_legal_hold", "ON"),
        ("azure_legal_hold", "true"),
        ("azure_lease_status", "locked"),
        ("azure_immutability_until", "2099-01-01T00:00:00Z"),
    ];
    let mut failures = Vec::new();
    for (protection, value) in protections {
        for method in ["DELETE", "PUT"] {
            let storage = temp_storage();
            storage.create_bucket("coordinated".to_string()).unwrap();
            let mut object = Object::new(
                "target".to_string(),
                b"old".to_vec(),
                "text/plain".to_string(),
            );
            object
                .provider_metadata
                .insert(protection.to_string(), value.to_string());
            storage
                .put_object("coordinated", "target".to_string(), object)
                .unwrap();
            let uri = if method == "PUT" {
                "/admin/v1/buckets/coordinated/objects/target/content"
            } else {
                "/admin/v1/buckets/coordinated/objects/target"
            };
            let request = Request::builder()
                .method(method)
                .uri(uri)
                .body(Full::new(bytes::Bytes::from_static(b"replacement")))
                .unwrap();

            // Act
            let response =
                sqrzl_emulator::api::admin::handle_request(storage.clone(), request).await;

            // Assert
            if !matches!(response, Err(sqrzl_emulator::Error::AccessDenied)) {
                failures.push(format!("{method} bypassed {protection}"));
            }
            if !storage
                .get_object("coordinated", "target")
                .is_ok_and(|object| object.data == b"old")
            {
                failures.push(format!(
                    "{method} destroyed protected bytes for {protection}"
                ));
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("; "));
}

#[tokio::test(flavor = "multi_thread")]
async fn should_serialize_lifecycle_given_active_owner_when_a_lease_is_committing() {
    // Arrange
    use sqrzl_emulator::models::lifecycle::{Expiration, LifecycleConfiguration, Rule, Status};
    let storage = temp_storage();
    storage.create_bucket("coordinated".to_string()).unwrap();
    let mut object = Object::new(
        "target".to_string(),
        b"old".to_vec(),
        "text/plain".to_string(),
    );
    object.last_modified = chrono::Utc::now() - chrono::TimeDelta::days(10);
    storage
        .put_object("coordinated", "target".to_string(), object)
        .unwrap();
    storage
        .put_bucket_lifecycle(
            "coordinated",
            LifecycleConfiguration {
                rules: vec![Rule {
                    id: Some("expire".to_string()),
                    status: Status::Enabled,
                    filter: None,
                    expiration: Some(Expiration {
                        days: Some(1),
                        date: None,
                        expired_object_delete_marker: None,
                    }),
                    noncurrent_version_expiration: None,
                    transitions: vec![],
                }],
            },
        )
        .unwrap();
    let gate = storage.operation_gate();
    let claim = gate.lock().await;
    let mut expiration = tokio::spawn({
        let storage = storage.clone();
        async move {
            sqrzl_emulator::LifecycleExecutor::new(storage, Duration::from_secs(1))
                .run_once()
                .await
                .unwrap();
        }
    });

    // Act
    assert!(
        tokio::time::timeout(Duration::from_millis(100), &mut expiration)
            .await
            .is_err()
    );
    let observed = storage
        .get_object_metadata("coordinated", "target")
        .unwrap();
    let mut held = observed.clone();
    held.provider_metadata
        .insert("azure_lease_status".to_string(), "locked".to_string());
    held.provider_metadata.insert(
        "azure_lease_id".to_string(),
        uuid::Uuid::new_v4().to_string(),
    );
    storage
        .replace_object_metadata_if_unchanged("coordinated", "target", &observed, &held)
        .unwrap();
    drop(claim);
    expiration.await.unwrap();

    // Assert
    assert_eq!(
        storage.get_object("coordinated", "target").unwrap().data,
        b"old"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn should_serialize_provider_decisions_given_active_owner_when_protection_is_committing() {
    // Arrange
    let storage = temp_storage();
    storage.create_bucket("coordinated".to_string()).unwrap();
    storage
        .put_object(
            "coordinated",
            "target".to_string(),
            Object::new(
                "target".to_string(),
                b"old".to_vec(),
                "text/plain".to_string(),
            ),
        )
        .unwrap();
    let gate = storage.operation_gate();
    let claim = gate.lock().await;
    let mut request = Request::builder()
        .method("DELETE")
        .uri("http://localhost/coordinated/target")
        .body(Full::new(bytes::Bytes::new()))
        .unwrap();
    request
        .headers_mut()
        .insert("host", "localhost".parse().unwrap());
    let request = RequestExt::from_hyper(request).await.unwrap();
    let mut deletion = tokio::spawn({
        let storage = storage.clone();
        async move {
            AdapterRegistry::default()
                .handle(storage, auth_disabled(), request)
                .await
                .unwrap()
        }
    });

    // Act: a claimed protection decision has not finished publishing its state.
    assert!(
        tokio::time::timeout(Duration::from_millis(100), &mut deletion)
            .await
            .is_err(),
        "provider mutation bypassed the active decision owner"
    );
    let observed = storage
        .get_object_metadata("coordinated", "target")
        .unwrap();
    let mut held = observed.clone();
    held.provider_metadata
        .insert("s3_object_lock_legal_hold".to_string(), "ON".to_string());
    assert!(storage
        .replace_object_metadata_if_unchanged("coordinated", "target", &observed, &held)
        .unwrap());
    drop(claim);

    // Assert: the waiting front door observes the committed protection.
    let response = deletion.await.unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert_eq!(
        storage.get_object("coordinated", "target").unwrap().data,
        b"old"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn should_serialize_admin_mutations_given_active_provider_when_storage_is_wrapped() {
    // Arrange
    let inner = temp_storage();
    inner.create_bucket("coordinated".to_string()).unwrap();
    let storage: Arc<dyn Storage> = Arc::new(IndexedStorage::new(inner.clone()));
    let inner_gate = inner.operation_gate();
    let gate = storage.operation_gate();
    assert!(Arc::ptr_eq(&gate, &inner_gate));
    let claim = gate.lock().await;
    let request = Request::builder()
        .method("DELETE")
        .uri("/admin/v1/buckets/coordinated")
        .body(Full::new(bytes::Bytes::new()))
        .unwrap();
    let mut deletion = tokio::spawn({
        let storage = storage.clone();
        async move {
            sqrzl_emulator::api::admin::handle_request(storage, request)
                .await
                .unwrap()
        }
    });

    // Act
    assert!(
        tokio::time::timeout(Duration::from_millis(100), &mut deletion)
            .await
            .is_err(),
        "admin mutation bypassed the active decision owner"
    );
    assert!(inner.bucket_exists("coordinated").unwrap());
    drop(claim);

    // Assert
    assert_eq!(deletion.await.unwrap().status(), StatusCode::NO_CONTENT);
    assert!(!inner.bucket_exists("coordinated").unwrap());
}
