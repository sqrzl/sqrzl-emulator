use super::*;
use crate::auth::AuthConfig;
use crate::providers::AdapterRegistry;
use crate::server::RequestExt;
use crate::storage::IndexedStorage;
use bytes::Bytes;
use http::StatusCode;
use http_body_util::{BodyExt, Full};
use std::sync::atomic::{AtomicUsize, Ordering};

const BUCKET: &str = "ordinary";
const CONTENT: &[u8] = b"ordinary-object-bytes";
const ROUTES: &[(&str, &str)] = &[
    ("localhost", "/ordinary/target"),
    ("storage.googleapis.com", "/ordinary/target"),
    ("localhost", "/storage/v1/b/ordinary/o/target?alt=media"),
    ("localhost", "/download/storage/v1/b/ordinary/o/target"),
];

fn object(key: &str) -> Object {
    let mut object = Object::new(key.to_string(), CONTENT.to_vec(), "text/plain".to_string());
    object
        .metadata
        .insert("__sqrzl_gcs_generation".to_string(), "1".to_string());
    object
        .metadata
        .insert("__sqrzl_gcs_metageneration".to_string(), "1".to_string());
    object
}

fn fixture() -> (PathBuf, Arc<FilesystemStorage>) {
    let root = std::env::temp_dir().join(format!("sqrzl-protection-cost-{}", Uuid::new_v4()));
    let storage = Arc::new(FilesystemStorage::open(&root).unwrap());
    storage.create_bucket(BUCKET.to_string()).unwrap();
    storage
        .put_object(BUCKET, "target".to_string(), object("target"))
        .unwrap();
    (root, storage)
}

async fn request(
    storage: Arc<dyn crate::storage::Storage>,
    method: &str,
    host: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: &[u8],
) -> (StatusCode, Bytes) {
    let mut builder = hyper::Request::builder()
        .method(method)
        .uri(format!("http://{host}{path}"))
        .header("host", host)
        .header("content-length", body.len());
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    let request = RequestExt::from_hyper(
        builder
            .body(Full::new(Bytes::copy_from_slice(body)))
            .unwrap(),
    )
    .await
    .unwrap();
    let response = AdapterRegistry::default()
        .handle(
            storage,
            Arc::new(AuthConfig {
                access_key_id: None,
                secret_access_key: None,
                enforce_auth: false,
                admin_auth_disabled: false,
                blobs_path: "./blobs".to_string(),
                lifecycle_interval: std::time::Duration::from_hours(1),
                api_port: 9000,
                ui_port: 9001,
                max_request_bytes: crate::config::DEFAULT_SQRZL_MAX_REQUEST_BYTES,
                smtp_port: crate::config::DEFAULT_SQRZL_SMTP_PORT,
                vendor_credentials: crate::config::VendorCredentials::default(),
                mcp: crate::config::McpConfig::default(),
            }),
            request,
        )
        .await
        .unwrap();
    (
        response.status(),
        response.into_body().collect().await.unwrap().to_bytes(),
    )
}

async fn request_read_costs(inner: Arc<FilesystemStorage>) -> Vec<String> {
    let storage: Arc<dyn crate::storage::Storage> = Arc::new(IndexedStorage::new(inner.clone()));
    let reads = Arc::new(AtomicUsize::new(0));
    let counter = reads.clone();
    let mut excessive = Vec::new();
    *inner.test_hook.lock().unwrap() = Some(Arc::new(move |phase| {
        if phase == TestPhase::ReadMetadata {
            counter.fetch_add(1, Ordering::SeqCst);
        }
    }));
    for (host, path) in ROUTES {
        for range in [false, true] {
            reads.store(0, Ordering::SeqCst);
            let headers = if range {
                vec![("range", "bytes=2-5")]
            } else {
                Vec::new()
            };
            let (status, bytes) = request(storage.clone(), "GET", host, path, &headers, b"").await;
            assert_eq!(
                status,
                if range {
                    StatusCode::PARTIAL_CONTENT
                } else {
                    StatusCode::OK
                },
                "{host}{path}"
            );
            assert_eq!(
                bytes.as_ref(),
                if range { &CONTENT[2..6] } else { CONTENT },
                "{host}{path}"
            );
            let count = reads.load(Ordering::SeqCst);
            if count > 8 {
                excessive.push(format!(
                    "{host}{path} range={range} read {count} metadata records"
                ));
            }
        }
    }
    *inner.test_hook.lock().unwrap() = None;
    excessive
}

#[tokio::test(flavor = "multi_thread")]
async fn should_bound_foreign_object_request_metadata_reads_given_unrelated_objects_when_reopened()
{
    // Arrange
    let (root, storage) = fixture();
    assert_eq!(
        request_read_costs(storage.clone()).await,
        Vec::<String>::new()
    );
    for index in 0..64 {
        let key = format!("metadata-history/{index:04}");
        storage
            .put_object(BUCKET, key.clone(), object(&key))
            .unwrap();
    }

    // Act
    // Assert
    let live = request_read_costs(storage.clone()).await;
    drop(storage);
    let reopened = Arc::new(FilesystemStorage::open(&root).unwrap());
    let reopened = request_read_costs(reopened).await;
    fs::remove_dir_all(root).unwrap();
    assert!(
        live.is_empty() && reopened.is_empty(),
        "live: {live:#?}; reopened: {reopened:#?}"
    );
}

fn protected_owner() -> Object {
    let mut owner = object("owner");
    owner
        .provider_metadata
        .insert("azure_legal_hold".to_string(), "true".to_string());
    owner
}

async fn assert_foreign_reads(storage: Arc<dyn crate::storage::Storage>, expected: StatusCode) {
    for (host, path) in ROUTES {
        let (status, bytes) = request(storage.clone(), "GET", host, path, &[], b"").await;
        assert_eq!(
            status,
            expected,
            "{host}{path}: {}",
            String::from_utf8_lossy(&bytes)
        );
        if expected == StatusCode::OK {
            assert_eq!(bytes.as_ref(), CONTENT);
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn should_observe_real_azure_protection_changes_given_existing_wrappers() {
    // Arrange
    for mode in ["lease", "legalhold", "immutabilityPolicies"] {
        let (root, storage) = fixture();
        storage
            .put_object(BUCKET, "owner".to_string(), object("owner"))
            .unwrap();
        let wrapper: Arc<dyn crate::storage::Storage> = Arc::new(IndexedStorage::new(Arc::new(
            IndexedStorage::new(storage.clone()),
        )));
        assert!(wrapper.azure_object_protection_absent(BUCKET));
        let path = format!("/devstoreaccount1/ordinary/owner?comp={mode}");
        let until = crate::utils::headers::format_last_modified_at(
            &(chrono::Utc::now() + chrono::Duration::days(1)),
        );
        let lease_id = "11111111-2222-3333-4444-555555555555";
        let mut headers = vec![("x-ms-version", "2023-11-03")];
        match mode {
            "lease" => headers.extend([
                ("x-ms-lease-action", "acquire"),
                ("x-ms-lease-duration", "-1"),
                ("x-ms-proposed-lease-id", lease_id),
            ]),
            "legalhold" => headers.push(("x-ms-legal-hold", "true")),
            _ => headers.extend([
                ("x-ms-immutability-policy-until-date", until.as_str()),
                ("x-ms-immutability-policy-mode", "Unlocked"),
            ]),
        }

        // Act
        let (status, bytes) =
            request(storage.clone(), "PUT", "localhost", &path, &headers, b"").await;

        // Assert
        assert_eq!(
            status,
            if mode == "lease" {
                StatusCode::CREATED
            } else {
                StatusCode::OK
            },
            "{mode}: {}",
            String::from_utf8_lossy(&bytes)
        );
        assert!(!wrapper.azure_object_protection_absent(BUCKET));
        let bucket = storage.get_bucket(BUCKET).unwrap();
        assert!(!bucket.versioning_enabled && bucket.metadata.is_empty());
        assert_foreign_reads(wrapper.clone(), StatusCode::CONFLICT).await;
        let mut release_headers = vec![("x-ms-version", "2023-11-03")];
        match mode {
            "lease" => release_headers.extend([
                ("x-ms-lease-action", "release"),
                ("x-ms-lease-id", lease_id),
            ]),
            "legalhold" => release_headers.push(("x-ms-legal-hold", "false")),
            _ => {}
        }
        let (status, _) = request(
            storage.clone(),
            if mode == "immutabilityPolicies" {
                "DELETE"
            } else {
                "PUT"
            },
            "localhost",
            &path,
            &release_headers,
            b"",
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_foreign_reads(wrapper, StatusCode::OK).await;
        drop(storage);
        let reopened = Arc::new(FilesystemStorage::open(&root).unwrap());
        assert_foreign_reads(Arc::new(IndexedStorage::new(reopened)), StatusCode::OK).await;
        fs::remove_dir_all(root).unwrap();
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn should_preserve_azure_metadata_decisions_given_expired_released_or_malformed_values_when_reopened(
) {
    // Arrange
    for (key, value, active) in [
        ("azure_legal_hold", "true", true),
        ("azure_legal_hold", "false", false),
        ("azure_legal_hold", "malformed", true),
        ("azure_immutability_until", "2999-01-01T00:00:00Z", true),
        ("azure_immutability_until", "2000-01-01T00:00:00Z", false),
        ("azure_immutability_until", "malformed", true),
        ("azure_immutability_mode", "Unlocked", true),
        ("azure_lease_status", "locked", true),
        ("azure_lease_status", "unlocked", false),
        ("azure_lease_status", "malformed", true),
    ] {
        let (root, storage) = fixture();
        let mut owner = object("owner");
        owner
            .provider_metadata
            .insert(key.to_string(), value.to_string());
        storage
            .put_object(BUCKET, "owner".to_string(), owner)
            .unwrap();

        // Act
        // Assert
        let expected = if active {
            StatusCode::CONFLICT
        } else {
            StatusCode::OK
        };
        assert_foreign_reads(Arc::new(IndexedStorage::new(storage.clone())), expected).await;
        drop(storage);
        let reopened = Arc::new(FilesystemStorage::open(&root).unwrap());
        assert!(!reopened.azure_object_protection_absent(BUCKET));
        assert_foreign_reads(Arc::new(IndexedStorage::new(reopened)), expected).await;
        fs::remove_dir_all(root).unwrap();
    }
}

fn hold_publication_metadata(storage: &Arc<FilesystemStorage>, key: &str) -> PathBuf {
    let directory =
        storage.object_id_dir(BUCKET, &FilesystemStorage::compute_object_id(BUCKET, key));
    let hook_directory = directory.clone();
    let weak = Arc::downgrade(storage);
    *storage.test_hook.lock().unwrap() = Some(Arc::new(move |phase| {
        if phase == TestPhase::DirectoryCreated {
            assert!(!weak
                .upgrade()
                .unwrap()
                .azure_object_protection_absent(BUCKET));
        }
        if phase == TestPhase::BodyPublished {
            let journal: serde_json::Value = serde_json::from_slice(
                &fs::read(hook_directory.join(".publication.json")).unwrap(),
            )
            .unwrap();
            let stage = hook_directory.join(journal["stage"].as_str().unwrap());
            fs::rename(stage.join("object.meta.json"), stage.join("held-metadata")).unwrap();
        }
    }));
    directory
}

fn restore_publication_metadata(storage: &FilesystemStorage, directory: &Path) {
    *storage.test_hook.lock().unwrap() = None;
    let journal: serde_json::Value =
        serde_json::from_slice(&fs::read(directory.join(".publication.json")).unwrap()).unwrap();
    let stage = directory.join(journal["stage"].as_str().unwrap());
    fs::rename(stage.join("held-metadata"), stage.join("object.meta.json")).unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn should_reject_unrelated_foreign_reads_after_ambiguous_protected_publications() {
    // Arrange
    for mode in [
        "buffered",
        "conditional",
        "streamed",
        "conditional-streamed",
        "multipart",
    ] {
        let (root, storage) = fixture();
        let wrapper: Arc<dyn crate::storage::Storage> =
            Arc::new(IndexedStorage::new(storage.clone()));
        assert!(wrapper.azure_object_protection_absent(BUCKET));
        let directory = hold_publication_metadata(&storage, "owner");
        let owner = protected_owner();
        let source = root.join("input-spool");
        fs::write(&source, CONTENT).unwrap();

        // Act
        let outcome = match mode {
            "conditional" => storage
                .put_object_if(
                    BUCKET,
                    "owner".to_string(),
                    owner,
                    &ObjectCondition::Missing,
                )
                .map(|_| ()),
            "streamed" => storage.put_object_streamed(BUCKET, "owner".to_string(), owner, &source),
            "conditional-streamed" => storage
                .put_object_streamed_if(
                    BUCKET,
                    "owner".to_string(),
                    owner,
                    &source,
                    &ObjectCondition::Missing,
                )
                .map(|_| ()),
            "multipart" => {
                let upload = storage
                    .create_multipart_upload_with_metadata(
                        BUCKET,
                        "owner".to_string(),
                        Some("text/plain".to_string()),
                        HashMap::new(),
                        owner.provider_metadata,
                    )
                    .unwrap();
                storage
                    .upload_part(BUCKET, &upload.upload_id, 1, CONTENT.to_vec())
                    .unwrap();
                storage
                    .complete_multipart_upload(BUCKET, &upload.upload_id)
                    .map(|_| ())
            }
            _ => storage.put_object(BUCKET, "owner".to_string(), owner),
        };

        // Assert
        assert!(
            outcome.is_err(),
            "{mode} must leave an ambiguous protected commit"
        );
        *storage.test_hook.lock().unwrap() = None;
        assert!(!wrapper.azure_object_protection_absent(BUCKET));
        assert_foreign_reads(wrapper, StatusCode::CONFLICT).await;
        assert!(FilesystemStorage::open(&root).is_err());
        restore_publication_metadata(&storage, &directory);
        drop(storage);
        let reopened = Arc::new(FilesystemStorage::open(&root).unwrap());
        assert_foreign_reads(
            Arc::new(IndexedStorage::new(reopened)),
            StatusCode::CONFLICT,
        )
        .await;
        fs::remove_dir_all(root).unwrap();
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn should_invalidate_absence_before_failed_protected_metadata_cas() {
    // Arrange
    let (root, storage) = fixture();
    storage
        .put_object(BUCKET, "owner".to_string(), object("owner"))
        .unwrap();
    let observed = storage.get_object_metadata(BUCKET, "owner").unwrap();
    let mut updated = observed.clone();
    updated.provider_metadata = protected_owner().provider_metadata;
    let metadata = storage.object_metadata_path(
        BUCKET,
        &FilesystemStorage::compute_object_id(BUCKET, "owner"),
    );
    let held = metadata.with_file_name("held-metadata");
    let hook_metadata = metadata.clone();
    let hook_held = held.clone();
    let weak = Arc::downgrade(&storage);
    *storage.test_hook.lock().unwrap() = Some(Arc::new(move |phase| {
        if phase == TestPhase::MetadataReplacementPrepared {
            assert!(!weak
                .upgrade()
                .unwrap()
                .azure_object_protection_absent(BUCKET));
            fs::rename(&hook_metadata, &hook_held).unwrap();
            fs::create_dir(&hook_metadata).unwrap();
        }
    }));

    // Act
    let outcome =
        storage.replace_object_metadata_if_unchanged(BUCKET, "owner", &observed, &updated);

    // Assert
    assert!(outcome.is_err());
    *storage.test_hook.lock().unwrap() = None;
    assert!(!storage.azure_object_protection_absent(BUCKET));
    assert_foreign_reads(
        Arc::new(IndexedStorage::new(storage.clone())),
        StatusCode::CONFLICT,
    )
    .await;
    fs::remove_dir(&metadata).unwrap();
    fs::rename(&held, &metadata).unwrap();
    assert_foreign_reads(
        Arc::new(IndexedStorage::new(storage.clone())),
        StatusCode::OK,
    )
    .await;
    drop(storage);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn should_reject_incomplete_startup_instead_of_proving_azure_protection_absent() {
    // Arrange
    for fault in [
        "invalid-json",
        "missing-metadata",
        "missing-payload",
        "missing-bucket-identity",
        "wrong-bucket-identity",
        "wrong-key-identity",
    ] {
        let (root, storage) = fixture();
        storage
            .put_object(BUCKET, "owner".to_string(), protected_owner())
            .unwrap();
        let directory = storage.object_id_dir(
            BUCKET,
            &FilesystemStorage::compute_object_id(BUCKET, "owner"),
        );
        drop(storage);
        match fault {
            "invalid-json" => fs::write(directory.join("object.meta.json"), b"invalid").unwrap(),
            "missing-metadata" => fs::remove_file(directory.join("object.meta.json")).unwrap(),
            "missing-bucket-identity" => {
                fs::remove_file(directory.parent().unwrap().join(".bucket.name")).unwrap();
            }
            "wrong-bucket-identity" => fs::write(
                directory.parent().unwrap().join(".bucket.name"),
                b"another-bucket",
            )
            .unwrap(),
            "wrong-key-identity" => {
                let mut owner = protected_owner();
                owner.key = "another-key".to_string();
                FilesystemStorage::write_object_metadata(
                    &directory.join("object.meta.json"),
                    &owner,
                )
                .unwrap();
            }
            _ => fs::remove_file(directory.join("object.blob")).unwrap(),
        }

        // Act
        let outcome = FilesystemStorage::open(&root);

        // Assert
        assert!(
            outcome.is_err(),
            "{fault} must fail startup before publishing an incomplete index"
        );
        assert!(directory.exists());
        fs::remove_dir_all(root).unwrap();
    }
}

#[test]
fn should_preserve_positive_admission_when_empty_bucket_initialization_finishes_late() {
    // Arrange
    let root = std::env::temp_dir().join(format!(
        "sqrzl-protection-initialization-{}",
        Uuid::new_v4()
    ));
    let storage = Arc::new(FilesystemStorage::open(&root).unwrap());
    let (reached, observed) = std::sync::mpsc::channel();
    let release = Arc::new(std::sync::Barrier::new(2));
    let hook_release = release.clone();
    *storage.test_hook.lock().unwrap() = Some(Arc::new(move |phase| {
        if phase == TestPhase::BucketCreated {
            reached.send(()).unwrap();
            hook_release.wait();
        }
    }));
    let creating = storage.clone();
    let creator = std::thread::spawn(move || creating.create_bucket(BUCKET.to_string()).unwrap());
    observed
        .recv_timeout(std::time::Duration::from_secs(5))
        .unwrap();

    // Act
    let outcome = storage.put_object(BUCKET, "owner".to_string(), protected_owner());
    release.wait();
    creator.join().unwrap();

    // Assert
    outcome.unwrap();
    assert!(!storage.azure_object_protection_absent(BUCKET));
    let wrapper = IndexedStorage::new(storage.clone());
    assert!(crate::providers::azure_object_protection_active(
        &wrapper, BUCKET
    ));
    drop(storage);
    fs::remove_dir_all(root).unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn should_invalidate_reopened_absence_before_protected_history_promotion_even_if_commit_fails(
) {
    // Arrange
    for ambiguous in [false, true] {
        let (root, storage) = fixture();
        storage.enable_versioning(BUCKET).unwrap();
        storage
            .put_object(BUCKET, "owner".to_string(), protected_owner())
            .unwrap();
        let protected_version = storage
            .get_object_metadata(BUCKET, "owner")
            .unwrap()
            .version_id;
        storage
            .put_object(BUCKET, "owner".to_string(), object("owner"))
            .unwrap();
        let current_version = storage
            .get_object_metadata(BUCKET, "owner")
            .unwrap()
            .version_id
            .unwrap();
        drop(storage);
        let reopened = Arc::new(FilesystemStorage::open(&root).unwrap());
        assert!(reopened.azure_object_protection_absent(BUCKET));
        assert!(!crate::providers::azure_object_protection_active(
            reopened.as_ref(),
            BUCKET
        ));
        let directory = if ambiguous {
            Some(hold_publication_metadata(&reopened, "owner"))
        } else {
            None
        };

        // Act
        let outcome = reopened.delete_object_version(BUCKET, "owner", &current_version);

        // Assert
        assert_eq!(outcome.is_err(), ambiguous);
        *reopened.test_hook.lock().unwrap() = None;
        assert!(!reopened.azure_object_protection_absent(BUCKET));
        assert_foreign_reads(
            Arc::new(IndexedStorage::new(reopened.clone())),
            StatusCode::CONFLICT,
        )
        .await;
        if let Some(directory) = directory {
            assert!(FilesystemStorage::open(&root).is_err());
            restore_publication_metadata(&reopened, &directory);
        }
        let promoted = reopened.get_object_metadata(BUCKET, "owner").unwrap();
        assert_eq!(promoted.version_id, protected_version);
        assert_eq!(promoted.provider_metadata["azure_legal_hold"], "true");
        drop(reopened);
        let recovered = Arc::new(FilesystemStorage::open(&root).unwrap());
        assert_foreign_reads(
            Arc::new(IndexedStorage::new(recovered)),
            StatusCode::CONFLICT,
        )
        .await;
        fs::remove_dir_all(root).unwrap();
    }
}

#[test]
#[ignore = "subprocess worker, invoked by the protection recovery campaign"]
fn should_exit_at_protected_publication_boundary_given_worker_environment() {
    // Arrange
    let Ok(root) = std::env::var("SQRZL_TEST_PROTECTION_ROOT") else {
        return;
    };
    let storage = FilesystemStorage::open(root).unwrap();
    let target = std::env::var("SQRZL_TEST_PROTECTION_PHASE").unwrap();
    *storage.test_hook.lock().unwrap() = Some(Arc::new(move |phase| {
        if format!("{phase:?}") == target {
            std::process::exit(86);
        }
    }));
    // Act
    storage
        .put_object(BUCKET, "owner".to_string(), protected_owner())
        .unwrap();
    // Assert
    panic!("the requested protection crash boundary was never reached");
}

#[tokio::test(flavor = "multi_thread")]
async fn should_reconstruct_protection_only_from_recovered_current_metadata_after_process_crash() {
    // Arrange
    for (phase, committed) in [
        (TestPhase::DirectoryCreated, false),
        (TestPhase::PublicationStaged, false),
        (TestPhase::PublicationCommitted, true),
        (TestPhase::BodyPublished, true),
        (TestPhase::MetadataPublished, true),
        (TestPhase::PublicationCleaned, true),
    ] {
        let (root, storage) = fixture();
        drop(storage);

        // Act
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "storage::filesystem::protection_tests::should_exit_at_protected_publication_boundary_given_worker_environment",
                "--ignored",
            ])
            .env("SQRZL_TEST_PROTECTION_ROOT", &root)
            .env("SQRZL_TEST_PROTECTION_PHASE", format!("{phase:?}"))
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap();

        // Assert
        assert_eq!(status.code(), Some(86), "{phase:?}");
        for _ in 0..2 {
            let reopened = Arc::new(FilesystemStorage::open(&root).unwrap());
            assert_eq!(
                reopened.azure_object_protection_absent(BUCKET),
                !committed,
                "{phase:?}"
            );
            assert_foreign_reads(
                Arc::new(IndexedStorage::new(reopened)),
                if committed {
                    StatusCode::CONFLICT
                } else {
                    StatusCode::OK
                },
            )
            .await;
        }
        fs::remove_dir_all(root).unwrap();
    }
}
