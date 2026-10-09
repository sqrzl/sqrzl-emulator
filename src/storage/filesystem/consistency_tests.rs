use super::*;
use std::sync::mpsc;
use std::time::Duration;

const WAIT: Duration = Duration::from_secs(5);

async fn verify_request_read_path(
    path: &'static str,
    headers: &'static [(&'static str, &'static str)],
    range: bool,
    expected: http::StatusCode,
    forbidden_phase: TestPhase,
) -> bytes::Bytes {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let (base, storage, _, _) = paused_storage(TestPhase::BodyPublished);
    let full_reads = Arc::new(AtomicUsize::new(0));
    let observed_reads = full_reads.clone();
    *storage.test_hook.lock().unwrap() = Some(Arc::new(move |phase| {
        if phase == forbidden_phase {
            observed_reads.fetch_add(1, Ordering::SeqCst);
        }
    }));
    let (status, _, bytes) = wire_get(storage, path, headers, range).await;
    assert_eq!(status, expected);
    assert_eq!(
        full_reads.load(Ordering::SeqCst),
        0,
        "request entered an unnecessary payload read path"
    );
    fs::remove_dir_all(base).unwrap();
    bytes
}

#[tokio::test(flavor = "multi_thread")]
async fn should_keep_gcs_json_range_reads_bounded_to_the_requested_bytes() {
    // Arrange
    let path = "/storage/v1/b/coherent/o/lease?alt=media&ifGenerationMatch=1";

    // Act
    // Assert
    let bytes = verify_request_read_path(
        path,
        &[("range", "bytes=0-2")],
        false,
        http::StatusCode::PARTIAL_CONTENT,
        TestPhase::FullPayload,
    )
    .await;
    assert_eq!(bytes.as_ref(), b"{\"v");
}

#[tokio::test(flavor = "multi_thread")]
async fn should_reject_read_conditions_without_materializing_payloads() {
    // Arrange
    let surfaces: [(&str, &[(&str, &str)]); 2] = [
        (
            "/storage/v1/b/coherent/o/lease?alt=media&ifGenerationNotMatch=1",
            &[],
        ),
        (
            "/n/sqrzl-emulator/b/coherent/o/lease",
            &[("if-none-match", "\"6811fbc0e37e7eb14fdf61ff13ca76de\"")],
        ),
    ];

    // Act
    // Assert
    for (path, headers) in surfaces {
        for range in [false, true] {
            verify_request_read_path(
                path,
                headers,
                range,
                http::StatusCode::NOT_MODIFIED,
                TestPhase::ReadPayloadMetadata,
            )
            .await;
        }
    }
}

async fn wire_get(
    storage: Arc<FilesystemStorage>,
    path: &'static str,
    headers: &'static [(&'static str, &'static str)],
    range: bool,
) -> (http::StatusCode, http::HeaderMap, bytes::Bytes) {
    use http_body_util::{BodyExt, Full};
    use hyper_util::client::legacy::Client;
    use hyper_util::rt::TokioExecutor;

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let config = Arc::new(crate::Config {
        access_key_id: None,
        secret_access_key: None,
        enforce_auth: false,
        admin_auth_disabled: false,
        blobs_path: storage.base_path.to_string_lossy().to_string(),
        lifecycle_interval: Duration::from_hours(1),
        api_port: address.port(),
        ui_port: 0,
        max_request_bytes: crate::config::DEFAULT_SQRZL_MAX_REQUEST_BYTES,
        smtp_port: 0,
        vendor_credentials: crate::config::VendorCredentials::default(),
    });
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let service = hyper::service::service_fn(move |request| {
            let storage = storage.clone();
            let config = config.clone();
            async move {
                let request = crate::server::RequestExt::from_hyper(request)
                    .await
                    .unwrap();
                let response = crate::providers::AdapterRegistry::default()
                    .handle(storage, config, request)
                    .await
                    .unwrap();
                Ok::<_, std::convert::Infallible>(response)
            }
        });
        crate::server::serve_h1_connection(stream, service)
            .await
            .unwrap();
    });
    let client = Client::builder(TokioExecutor::new()).build_http();
    let mut request = hyper::Request::builder()
        .method("GET")
        .uri(format!("http://{address}{path}"))
        .header("content-length", "0");
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    if range {
        request = request.header("range", "bytes=0-");
    }
    let response = client
        .request(request.body(Full::<bytes::Bytes>::default()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    server.abort();
    (status, headers, bytes)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn should_keep_held_http_reads_on_the_generation_used_for_response_conditions() {
    // Arrange
    let surfaces: [(&str, &[(&str, &str)]); 5] = [
        (
            "/coherent/lease",
            &[("if-match", "\"6811fbc0e37e7eb14fdf61ff13ca76de\"")],
        ),
        (
            "/devstoreaccount1/coherent/lease",
            &[("x-ms-version", "2023-11-03")],
        ),
        (
            "/storage/v1/b/coherent/o/lease?alt=media&ifGenerationMatch=1",
            &[],
        ),
        ("/coherent/lease", &[("host", "storage.googleapis.com")]),
        (
            "/n/sqrzl-emulator/b/coherent/o/lease",
            &[("if-match", "\"6811fbc0e37e7eb14fdf61ff13ca76de\"")],
        ),
    ];

    // Act
    // Assert
    for (path, headers) in surfaces {
        for range in [false, true] {
            let (base, storage, entered, resume) = paused_storage(TestPhase::ReadPayloadMetadata);
            let reader = tokio::spawn(wire_get(storage.clone(), path, headers, range));
            tokio::task::spawn_blocking(move || entered.recv_timeout(WAIT).unwrap())
                .await
                .unwrap();
            let writer_storage = storage.clone();
            let writer_base = base.clone();
            let (done_tx, done_rx) = mpsc::channel();
            let writer = std::thread::spawn(move || {
                replace(&writer_storage, &writer_base, object(b"{\"v\":2}\n"), true);
                let _ = done_tx.send(());
            });
            let wrote_during_read = tokio::task::spawn_blocking(move || {
                done_rx.recv_timeout(Duration::from_millis(100)).is_ok()
            })
            .await
            .unwrap();
            *storage.test_hook.lock().unwrap() = None;
            resume.send(()).unwrap();
            let (status, headers, bytes) =
                tokio::time::timeout(WAIT, reader).await.unwrap().unwrap();
            writer.join().unwrap();
            assert!(!wrote_during_read);
            assert_eq!(
                status,
                if range {
                    http::StatusCode::PARTIAL_CONTENT
                } else {
                    http::StatusCode::OK
                }
            );
            assert_eq!(bytes.as_ref(), b"{\"v\":1}\n", "{path} range={range}");
            assert_eq!(headers["content-length"], bytes.len().to_string());
            assert_eq!(
                headers["etag"].to_str().unwrap().trim_matches('"'),
                md5_hash(&bytes)
            );
            if range {
                assert_eq!(headers["content-range"], "bytes 0-7/8");
            }
            fs::remove_dir_all(base).unwrap();
        }
    }
}

fn object(data: &[u8]) -> Object {
    let mut object = Object::new(
        "lease".to_string(),
        data.to_vec(),
        "application/json".to_string(),
    );
    object.metadata.insert(
        "__sqrzl_gcs_generation".to_string(),
        if data == b"{\"v\":1}\n" { "1" } else { "2" }.to_string(),
    );
    object
}

fn paused_storage(
    phase: TestPhase,
) -> (
    PathBuf,
    Arc<FilesystemStorage>,
    mpsc::Receiver<()>,
    mpsc::Sender<()>,
) {
    let base = std::env::temp_dir().join(format!("sqrzl-consistency-{}", Uuid::new_v4()));
    let storage = Arc::new(FilesystemStorage::new(&base));
    storage.create_bucket("coherent".to_string()).unwrap();
    storage
        .put_object("coherent", "lease".to_string(), object(b"{\"v\":1}\n"))
        .unwrap();
    let (entered_tx, entered_rx) = mpsc::channel();
    let (resume_tx, resume_rx) = mpsc::channel();
    let resume_rx = Mutex::new(resume_rx);
    *storage.test_hook.lock().unwrap() = Some(Arc::new(move |observed| {
        if observed == phase {
            entered_tx.send(()).unwrap();
            resume_rx.lock().unwrap().recv_timeout(WAIT).unwrap();
        }
    }));
    (base, storage, entered_rx, resume_tx)
}

fn replace(storage: &FilesystemStorage, base: &Path, replacement: Object, streamed: bool) {
    if streamed {
        let path = base.join("replacement.tmp");
        fs::write(&path, &replacement.data).unwrap();
        let mut metadata = replacement;
        metadata.data.clear();
        storage
            .put_object_streamed("coherent", "lease".to_string(), metadata, &path)
            .unwrap();
    } else {
        storage
            .put_object("coherent", "lease".to_string(), replacement)
            .unwrap();
    }
}

fn verify_held_read(replacement: &[u8], streamed: bool, range: bool) {
    let (base, storage, entered, resume) = paused_storage(TestPhase::ReadPayloadMetadata);
    let reader_storage = storage.clone();
    let reader = std::thread::spawn(move || {
        if range {
            reader_storage.get_object_range("coherent", "lease", 0, None)
        } else {
            reader_storage
                .get_object("coherent", "lease")
                .map(|mut object| {
                    let bytes = std::mem::take(&mut object.data);
                    (object, bytes)
                })
        }
    });
    entered.recv_timeout(WAIT).unwrap();
    let writer_storage = storage.clone();
    let writer_base = base.clone();
    let replacement = object(replacement);
    let (done_tx, done_rx) = mpsc::channel();
    let writer = std::thread::spawn(move || {
        replace(&writer_storage, &writer_base, replacement, streamed);
        done_tx.send(()).unwrap();
    });
    let wrote_during_read = done_rx.recv_timeout(Duration::from_millis(100)).is_ok();
    *storage.test_hook.lock().unwrap() = None;
    resume.send(()).unwrap();
    let observed = reader.join().unwrap();
    writer.join().unwrap();
    let (metadata, bytes) = observed.unwrap();
    assert!(
        !wrote_during_read,
        "PUT published while GET was between metadata and bytes"
    );
    assert_eq!(bytes, b"{\"v\":1}\n");
    assert_eq!(metadata.size, bytes.len() as u64);
    assert_eq!(metadata.etag, md5_hash(&bytes));
    fs::remove_dir_all(base).unwrap();
}

#[test]
fn should_keep_held_payload_reads_on_one_generation() {
    // Arrange
    let replacements: [&[u8]; 3] = [b"{\"v\":100}\n", b"{}\n", b"{\"v\":2}\n"];

    // Act
    // Assert
    for replacement in replacements {
        for streamed in [false, true] {
            for range in [false, true] {
                verify_held_read(replacement, streamed, range);
            }
        }
    }
}

#[test]
fn should_hide_the_body_publication_window_from_all_object_readers() {
    // Arrange
    // Act
    // Assert
    for streamed in [false, true] {
        for read_kind in 0..3 {
            let (base, storage, entered, resume) = paused_storage(TestPhase::BodyPublished);
            let writer_storage = storage.clone();
            let writer_base = base.clone();
            let writer = std::thread::spawn(move || {
                replace(
                    &writer_storage,
                    &writer_base,
                    object(b"{\"v\":100}\n"),
                    streamed,
                );
            });
            entered.recv_timeout(WAIT).unwrap();
            let reader_storage = storage.clone();
            let (done_tx, done_rx) = mpsc::channel();
            let reader = std::thread::spawn(move || {
                let observed = match read_kind {
                    0 => reader_storage.get_object("coherent", "lease"),
                    1 => reader_storage.get_object_metadata("coherent", "lease"),
                    _ => reader_storage
                        .get_object_range("coherent", "lease", 0, None)
                        .map(|(mut object, bytes)| {
                            object.data = bytes;
                            object
                        }),
                };
                done_tx.send(()).unwrap();
                observed
            });
            let read_during_publication = done_rx.recv_timeout(Duration::from_millis(100)).is_ok();
            *storage.test_hook.lock().unwrap() = None;
            resume.send(()).unwrap();
            writer.join().unwrap();
            let observed = reader.join().unwrap().unwrap();
            assert!(
                !read_during_publication,
                "read escaped between body and metadata publication"
            );
            assert_eq!(observed.size, 10);
            assert_eq!(observed.etag, md5_hash(b"{\"v\":100}\n"));
            if read_kind != 1 {
                assert_eq!(observed.data, b"{\"v\":100}\n");
            }
            fs::remove_dir_all(base).unwrap();
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn should_admit_s3_replacements_and_subresources_without_loading_existing_payloads() {
    use http_body_util::Full;
    use std::sync::atomic::{AtomicUsize, Ordering};
    let (base, storage, _, _) = paused_storage(TestPhase::BodyPublished);
    let reads = Arc::new(AtomicUsize::new(0));
    let counter = reads.clone();
    *storage.test_hook.lock().unwrap() = Some(Arc::new(move |phase| {
        if phase == TestPhase::FullPayload {
            counter.fetch_add(1, Ordering::SeqCst);
        }
    }));
    storage
        .put_bucket_lifecycle(
            "coherent",
            crate::models::lifecycle::LifecycleConfiguration { rules: vec![] },
        )
        .unwrap();
    let config = Arc::new(crate::Config {
        access_key_id: None,
        secret_access_key: None,
        enforce_auth: false,
        admin_auth_disabled: false,
        blobs_path: base.to_string_lossy().to_string(),
        lifecycle_interval: Duration::from_hours(1),
        api_port: 0,
        ui_port: 0,
        max_request_bytes: crate::config::DEFAULT_SQRZL_MAX_REQUEST_BYTES,
        smtp_port: 0,
        vendor_credentials: crate::config::VendorCredentials::default(),
    });
    for (method, suffix, body, headers) in [
        ("PUT", "", b"replacement".as_slice(), vec![]),
        ("PUT", "?tagging", b"<Tagging><TagSet><Tag><Key>owner</Key><Value>contract</Value></Tag></TagSet></Tagging>".as_slice(), vec![]),
        ("PUT", "?acl", b"".as_slice(), vec![("x-amz-acl", "private")]),
        ("HEAD", "", b"".as_slice(), vec![]),
        ("GET", "?tagging", b"".as_slice(), vec![]),
        ("GET", "?acl", b"".as_slice(), vec![]),
        ("GET", "", b"".as_slice(), vec![("range", "bytes=0-2")]),
        ("DELETE", "?tagging", b"".as_slice(), vec![]),
        ("DELETE", "", b"".as_slice(), vec![]),
    ] {
        reads.store(0, Ordering::SeqCst);
        let mut builder = hyper::Request::builder().method(method).uri(format!("http://localhost/coherent/lease{suffix}"));
        for (name, value) in headers { builder = builder.header(name, value); }
        let req = crate::server::RequestExt::from_hyper(builder.body(Full::new(bytes::Bytes::copy_from_slice(body))).unwrap()).await.unwrap();
        let response = crate::providers::AdapterRegistry::default().handle(storage.clone(), config.clone(), req).await.unwrap();
        let expected = if method == "DELETE" { http::StatusCode::NO_CONTENT } else if method == "GET" && suffix.is_empty() { http::StatusCode::PARTIAL_CONTENT } else { http::StatusCode::OK };
        assert_eq!(response.status(), expected, "{method} {suffix}");
        assert_eq!(reads.load(Ordering::SeqCst), 0, "{method} {suffix} loaded the existing payload");
    }
    fs::remove_dir_all(base).unwrap();
}

#[tokio::test(flavor = "multi_thread")]
#[allow(clippy::too_many_lines)]
async fn should_bound_s3_current_and_historical_materialization_before_payload_reads() {
    use http_body_util::{BodyExt as _, Full};
    use std::sync::atomic::{AtomicUsize, Ordering};
    let (base, storage, _, _) = paused_storage(TestPhase::BodyPublished);
    *storage.test_hook.lock().unwrap() = None;
    storage.enable_versioning("coherent").unwrap();
    storage
        .put_object(
            "coherent",
            "large".to_string(),
            Object::new(
                "large".to_string(),
                b"seed".to_vec(),
                "text/plain".to_string(),
            ),
        )
        .unwrap();
    let object_id = FilesystemStorage::compute_object_id("coherent", "large");
    let metadata_path = storage.object_metadata_path("coherent", &object_id);
    let mut object = FilesystemStorage::read_object_metadata(&metadata_path).unwrap();
    let version = object.version_id.clone().unwrap();
    object.size = 64 * 1024 * 1024 + 1;
    std::fs::OpenOptions::new()
        .write(true)
        .open(storage.object_data_path("coherent", &object_id))
        .unwrap()
        .set_len(object.size)
        .unwrap();
    FilesystemStorage::atomic_write(&metadata_path, &serde_json::to_vec(&object).unwrap()).unwrap();
    let reads = Arc::new(AtomicUsize::new(0));
    let counter = reads.clone();
    *storage.test_hook.lock().unwrap() = Some(Arc::new(move |phase| {
        if matches!(
            phase,
            TestPhase::FullPayload | TestPhase::ReadPayloadMetadata
        ) {
            counter.fetch_add(1, Ordering::SeqCst);
        }
    }));
    let config = Arc::new(crate::Config {
        access_key_id: None,
        secret_access_key: None,
        enforce_auth: false,
        admin_auth_disabled: false,
        blobs_path: base.to_string_lossy().to_string(),
        lifecycle_interval: Duration::from_hours(1),
        api_port: 0,
        ui_port: 0,
        max_request_bytes: crate::config::DEFAULT_SQRZL_MAX_REQUEST_BYTES,
        smtp_port: 0,
        vendor_credentials: crate::config::VendorCredentials::default(),
    });
    let upload = storage
        .create_multipart_upload("coherent", "copy-part".to_string())
        .unwrap();
    for (method, path, headers, status) in [
        (
            "GET",
            "large".to_string(),
            vec![],
            http::StatusCode::NOT_IMPLEMENTED,
        ),
        (
            "GET",
            "large".to_string(),
            vec![("if-match", "\"wrong\"".to_string())],
            http::StatusCode::PRECONDITION_FAILED,
        ),
        (
            "GET",
            "large".to_string(),
            vec![("if-none-match", format!("\"{}\"", object.etag))],
            http::StatusCode::NOT_MODIFIED,
        ),
        (
            "PUT",
            "lease".to_string(),
            vec![
                ("x-amz-copy-source", "/coherent/large".to_string()),
                ("x-amz-copy-source-if-match", "\"wrong\"".to_string()),
            ],
            http::StatusCode::PRECONDITION_FAILED,
        ),
        (
            "GET",
            format!("large?versionId={version}"),
            vec![],
            http::StatusCode::NOT_IMPLEMENTED,
        ),
        (
            "HEAD",
            format!("large?versionId={version}"),
            vec![],
            http::StatusCode::OK,
        ),
        (
            "PUT",
            "lease".to_string(),
            vec![("x-amz-copy-source", "/coherent/large".to_string())],
            http::StatusCode::NOT_IMPLEMENTED,
        ),
        (
            "PUT",
            "lease".to_string(),
            vec![(
                "x-amz-copy-source",
                format!("/coherent/large?versionId={version}"),
            )],
            http::StatusCode::NOT_IMPLEMENTED,
        ),
        (
            "PUT",
            format!("copy-part?partNumber=1&uploadId={}", upload.upload_id),
            vec![("x-amz-copy-source", "/coherent/large".to_string())],
            http::StatusCode::NOT_IMPLEMENTED,
        ),
    ] {
        reads.store(0, Ordering::SeqCst);
        let mut builder = hyper::Request::builder()
            .method(method)
            .uri(format!("http://localhost/coherent/{path}"));
        for (name, value) in headers {
            builder = builder.header(name, value);
        }
        let req = crate::server::RequestExt::from_hyper(
            builder.body(Full::new(bytes::Bytes::new())).unwrap(),
        )
        .await
        .unwrap();
        let response = crate::providers::AdapterRegistry::default()
            .handle(storage.clone(), config.clone(), req)
            .await
            .unwrap();
        assert_eq!(response.status(), status, "{method} {path}");
        assert_eq!(
            reads.load(Ordering::SeqCst),
            0,
            "{method} {path} read a payload before rejecting its extent"
        );
    }
    assert_eq!(
        storage.get_object("coherent", "lease").unwrap().data,
        b"{\"v\":1}\n"
    );
    assert!(storage
        .list_parts("coherent", &upload.upload_id)
        .unwrap()
        .is_empty());
    // Both current and historical range requests may read a small selection of
    // a large object, while whole-object materialization remains unsupported.
    for suffix in [String::new(), format!("?versionId={version}")] {
        let req = crate::server::RequestExt::from_hyper(
            hyper::Request::builder()
                .method("GET")
                .uri(format!("http://localhost/coherent/large{suffix}"))
                .header("range", "bytes=0-2")
                .body(Full::new(bytes::Bytes::new()))
                .unwrap(),
        )
        .await
        .unwrap();
        let response = crate::providers::AdapterRegistry::default()
            .handle(storage.clone(), config.clone(), req)
            .await
            .unwrap();
        assert_eq!(response.status(), http::StatusCode::PARTIAL_CONTENT);
        assert_eq!(
            response.into_body().collect().await.unwrap().to_bytes(),
            b"see".as_slice()
        );
    }
    let mut bucket_metadata = storage.get_bucket("coherent").unwrap().metadata;
    bucket_metadata.insert("azure_versioning_enabled".to_string(), "true".to_string());
    storage
        .update_bucket_metadata("coherent", bucket_metadata)
        .unwrap();
    let lease_id = "00000000-0000-0000-0000-000000000001";
    for (method, suffix, headers, expected) in [
        (
            "GET",
            String::new(),
            vec![],
            http::StatusCode::NOT_IMPLEMENTED,
        ),
        (
            "HEAD",
            format!("?versionid={version}"),
            vec![],
            http::StatusCode::OK,
        ),
        (
            "PUT",
            "?comp=metadata".to_string(),
            vec![],
            http::StatusCode::NOT_IMPLEMENTED,
        ),
        (
            "PUT",
            "?comp=snapshot".to_string(),
            vec![],
            http::StatusCode::NOT_IMPLEMENTED,
        ),
        (
            "PUT",
            "?comp=lease".to_string(),
            vec![
                ("x-ms-lease-action", "acquire"),
                ("x-ms-proposed-lease-id", lease_id),
                ("x-ms-lease-duration", "-1"),
            ],
            http::StatusCode::CREATED,
        ),
        (
            "PUT",
            "?comp=lease".to_string(),
            vec![
                ("x-ms-lease-action", "release"),
                ("x-ms-lease-id", lease_id),
            ],
            http::StatusCode::OK,
        ),
        (
            "PUT",
            "?comp=legalhold".to_string(),
            vec![("x-ms-legal-hold", "true")],
            http::StatusCode::OK,
        ),
        (
            "PUT",
            "?comp=legalhold".to_string(),
            vec![("x-ms-legal-hold", "false")],
            http::StatusCode::OK,
        ),
    ] {
        reads.store(0, Ordering::SeqCst);
        let mut builder = hyper::Request::builder()
            .method(method)
            .uri(format!(
                "http://localhost/devstoreaccount1/coherent/large{suffix}"
            ))
            .header("content-length", "0")
            .header("x-ms-version", "2023-11-03");
        for (name, value) in headers {
            builder = builder.header(name, value);
        }
        let req = crate::server::RequestExt::from_hyper(
            builder.body(Full::new(bytes::Bytes::new())).unwrap(),
        )
        .await
        .unwrap();
        let response = crate::providers::AdapterRegistry::default()
            .handle(storage.clone(), config.clone(), req)
            .await
            .unwrap();
        assert_eq!(response.status(), expected, "Azure {method} {suffix}");
        assert_eq!(
            reads.load(Ordering::SeqCst),
            0,
            "Azure {method} {suffix} materialized a payload"
        );
    }
    let req = crate::server::RequestExt::from_hyper(
        hyper::Request::builder()
            .method("GET")
            .uri("http://localhost/devstoreaccount1/coherent/large")
            .header("x-ms-version", "2023-11-03")
            .header("x-ms-range", "bytes=0-2")
            .body(Full::new(bytes::Bytes::new()))
            .unwrap(),
    )
    .await
    .unwrap();
    let response = crate::providers::AdapterRegistry::default()
        .handle(storage.clone(), config.clone(), req)
        .await
        .unwrap();
    assert_eq!(response.status(), http::StatusCode::PARTIAL_CONTENT);
    assert_eq!(
        response.into_body().collect().await.unwrap().to_bytes(),
        b"see".as_slice()
    );
    *storage.test_hook.lock().unwrap() = None;
    storage
        .put_object(
            "coherent",
            "large".to_string(),
            Object::new(
                "large".to_string(),
                b"new".to_vec(),
                "text/plain".to_string(),
            ),
        )
        .unwrap();
    let historical = storage
        .get_object_version_metadata("coherent", "large", &version)
        .unwrap();
    assert_eq!(historical.data, Vec::<u8>::new());
    assert_eq!(historical.size, object.size);
    assert_eq!(
        storage
            .get_object_version_range("coherent", "large", &version, 0, Some(2))
            .unwrap()
            .1,
        b"see"
    );
    assert_eq!(
        storage
            .get_object_metadata("coherent", "large")
            .unwrap()
            .size,
        3
    );
    fs::remove_dir_all(base).unwrap();
}
