use super::*;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};

const BUCKET: &str = "crash-consistency";
const OLD: &[u8] = b"old-generation";
const NEW: &[u8] = b"new-generation-with-a-different-size";

fn generation(bytes: &[u8], label: &str) -> Object {
    let mut object = Object::new("item".to_string(), bytes.to_vec(), format!("text/{label}"));
    object
        .metadata
        .insert("generation".to_string(), label.to_string());
    object
}

#[test]
#[ignore = "subprocess worker, invoked by the crash boundary campaigns"]
fn publication_crash_worker() {
    let Ok(path) = std::env::var("SQRZL_TEST_CRASH_ROOT") else {
        return;
    };
    let storage = FilesystemStorage::open(&path).unwrap();
    let target =
        std::env::var("SQRZL_TEST_CRASH_PHASE").unwrap_or_else(|_| "BodyPublished".to_string());
    let hit = std::env::var("SQRZL_TEST_CRASH_HIT")
        .unwrap_or_else(|_| "1".to_string())
        .parse::<usize>()
        .unwrap();
    let seen = AtomicUsize::new(0);
    *storage.test_hook.lock().unwrap() = Some(Arc::new(move |phase| {
        if format!("{phase:?}") == target && seen.fetch_add(1, Ordering::SeqCst) + 1 == hit {
            // Immediate termination skips destructors and drops all mutexes;
            // reopen is the sole recovery mechanism.
            std::process::exit(86);
        }
    }));
    match std::env::var("SQRZL_TEST_CRASH_MODE")
        .unwrap_or_else(|_| "buffered".to_string())
        .as_str()
    {
        "streamed" => {
            let source = PathBuf::from(path).join("input-spool");
            fs::write(&source, NEW).unwrap();
            let mut object = generation(NEW, "new");
            object.data.clear();
            storage
                .put_object_streamed(BUCKET, "item".to_string(), object, &source)
                .unwrap();
        }
        "multipart" => {
            storage
                .complete_multipart_upload(BUCKET, &std::env::var("SQRZL_TEST_UPLOAD_ID").unwrap())
                .unwrap();
        }
        "delete" | "marker" | "suspended-marker" => {
            storage.delete_object(BUCKET, "item").unwrap();
        }
        "promote" | "version-delete" => {
            storage
                .delete_object_version(
                    BUCKET,
                    "item",
                    &std::env::var("SQRZL_TEST_VERSION_ID").unwrap(),
                )
                .unwrap();
        }
        _ => {
            storage
                .put_object(BUCKET, "item".to_string(), generation(NEW, "new"))
                .unwrap();
        }
    }
    panic!("the requested crash boundary was never reached");
}

fn crash_case(mode: &str, phase: TestPhase, hit: usize) {
    let root = std::env::temp_dir().join(format!("sqrzl-object-crash-{}", Uuid::new_v4()));
    let storage = FilesystemStorage::open(&root).unwrap();
    storage.create_bucket(BUCKET.to_string()).unwrap();
    if matches!(
        mode,
        "versioned" | "marker" | "promote" | "version-delete" | "suspended" | "suspended-marker"
    ) {
        storage.enable_versioning(BUCKET).unwrap();
    }
    if mode != "fresh" {
        storage
            .put_object(BUCKET, "item".to_string(), generation(OLD, "old"))
            .unwrap();
    }
    if matches!(mode, "suspended" | "suspended-marker") {
        storage.suspend_versioning(BUCKET).unwrap();
        storage
            .put_object(BUCKET, "item".to_string(), generation(OLD, "old"))
            .unwrap();
    }
    let first_id = storage
        .get_object_metadata(BUCKET, "item")
        .ok()
        .and_then(|object| object.version_id);
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args([
            "--exact",
            "storage::filesystem::crash_tests::publication_crash_worker",
            "--ignored",
        ])
        .env("SQRZL_TEST_CRASH_ROOT", &root)
        .env("SQRZL_TEST_CRASH_PHASE", format!("{phase:?}"))
        .env("SQRZL_TEST_CRASH_MODE", mode)
        .env("SQRZL_TEST_CRASH_HIT", hit.to_string())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if matches!(mode, "promote" | "version-delete") {
        storage
            .put_object(BUCKET, "item".to_string(), generation(NEW, "new"))
            .unwrap();
        let current_id = storage
            .get_object_metadata(BUCKET, "item")
            .unwrap()
            .version_id
            .unwrap();
        command.env(
            "SQRZL_TEST_VERSION_ID",
            if mode == "promote" {
                current_id
            } else {
                first_id.unwrap()
            },
        );
    }
    if mode == "multipart" {
        let upload = storage
            .create_multipart_upload_with_metadata(
                BUCKET,
                "item".to_string(),
                Some("text/new".to_string()),
                HashMap::from([("generation".to_string(), "new".to_string())]),
                HashMap::new(),
            )
            .unwrap();
        storage
            .upload_part(BUCKET, &upload.upload_id, 1, NEW.to_vec())
            .unwrap();
        command.env("SQRZL_TEST_UPLOAD_ID", &upload.upload_id);
    }
    drop(storage);
    assert_eq!(
        command.status().unwrap().code(),
        Some(86),
        "{mode} {phase:?} hit {hit}"
    );
    verify_recovered_store(&root, mode, phase);
    fs::remove_dir_all(root).unwrap();
}

fn verify_recovered_store(root: &Path, mode: &str, phase: TestPhase) {
    let storage = FilesystemStorage::open(root).unwrap();
    let before_commit = matches!(
        phase,
        TestPhase::DirectoryCreated
            | TestPhase::PublicationStaged
            | TestPhase::VersionBodyPrepared
            | TestPhase::VersionBodyPublished
            | TestPhase::VersionMetadataPrepared
            | TestPhase::VersionMetadataPublished
            | TestPhase::VersionPublished
    );
    match storage.get_object(BUCKET, "item") {
        Ok(object) => {
            verify_generation(&object);
            let expected = match mode {
                "version-delete" => NEW,
                "promote" if before_commit => NEW,
                "promote" => OLD,
                "delete" | "marker" | "suspended-marker" if !before_commit => {
                    panic!("a committed deletion must complete on reopen: {mode} {phase:?}")
                }
                _ if before_commit => OLD,
                _ => NEW,
            };
            assert_eq!(object.data, expected, "{mode} {phase:?}");
        }
        Err(Error::KeyNotFound) => assert!(
            (mode == "fresh" && before_commit)
                || (matches!(mode, "delete" | "marker" | "suspended-marker") && !before_commit),
            "unexpected current-object loss: {mode} {phase:?}"
        ),
        Err(error) => panic!("{mode} {phase:?}: {error}"),
    }
    let versions = storage.list_object_versions(BUCKET, None).unwrap();
    let identities: std::collections::HashSet<_> = versions
        .iter()
        .map(|object| (&object.key, &object.version_id))
        .collect();
    assert_eq!(
        identities.len(),
        versions.len(),
        "duplicate historical version identity after recovery"
    );
    for metadata in versions {
        let object = storage
            .get_object_version(BUCKET, "item", metadata.version_id.as_deref().unwrap())
            .unwrap();
        if object
            .provider_metadata
            .get("s3_delete_marker")
            .is_some_and(|value| value == "true")
        {
            assert_eq!(object.data, [] as [u8; 0]);
            assert_eq!(object.size, 0);
        } else {
            verify_generation(&object);
        }
    }
    let first = storage
        .get_object_metadata(BUCKET, "item")
        .ok()
        .map(|object| (object.etag, object.size, object.version_id));
    drop(storage);
    let reopened = FilesystemStorage::open(root).unwrap();
    assert_eq!(
        reopened
            .get_object_metadata(BUCKET, "item")
            .ok()
            .map(|object| (object.etag, object.size, object.version_id)),
        first,
        "second recovery changed committed identity"
    );
    drop(reopened);
    assert_no_publication_artifacts(root);
}

fn assert_no_publication_artifacts(root: &Path) {
    for entry in fs::read_dir(root).unwrap() {
        let entry = entry.unwrap();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        assert!(
            name != ".publication.json"
                && !name.starts_with(".publication-stage-")
                && !name.starts_with(".published-"),
            "leftover publication artifact: {}",
            entry.path().display()
        );
        if entry.file_type().unwrap().is_dir() {
            assert_no_publication_artifacts(&entry.path());
        }
    }
}

fn verify_generation(object: &Object) {
    let label = object
        .metadata
        .get("generation")
        .map(String::as_str)
        .unwrap();
    let expected = match label {
        "old" => OLD,
        "new" => NEW,
        other => panic!("unknown generation {other}"),
    };
    assert_eq!(
        object.data, expected,
        "metadata and bytes belong to different generations"
    );
    assert_eq!(object.size, expected.len() as u64);
    assert_eq!(object.content_type, format!("text/{label}"));
    if !object.etag.contains('-') {
        assert_eq!(object.etag, crate::utils::headers::compute_etag(expected));
    }
}

#[test]
fn should_recover_one_generation_after_interrupted_pair_publication() {
    // Arrange: crash_case creates an isolated known generation for every case.
    // Act: terminate the child process at the selected publication boundary.
    // Assert: crash_case verifies the recovered generation, catalog and second reopen.
    crash_case("buffered", TestPhase::BodyPublished, 1);
}

#[test]
fn should_recover_object_publication_at_every_process_boundary() {
    // Arrange: crash_case creates an isolated known generation for every case.
    // Act: terminate the child process at the selected publication boundary.
    // Assert: crash_case verifies the recovered generation, catalog and second reopen.
    for mode in ["buffered", "streamed", "fresh", "multipart"] {
        for phase in [
            TestPhase::DirectoryCreated,
            TestPhase::PublicationStaged,
            TestPhase::PublicationCommitted,
            TestPhase::BodyPrepared,
            TestPhase::BodyPublished,
            TestPhase::MetadataPrepared,
            TestPhase::MetadataPublished,
            TestPhase::PublicationCleaned,
        ] {
            crash_case(mode, phase, 1);
        }
    }
    for phase in [
        TestPhase::UploadCleanup,
        TestPhase::UploadRecordRetired,
        TestPhase::UploadCleanupDone,
    ] {
        crash_case("multipart", phase, 1);
    }
}

#[test]
fn should_recover_version_history_at_every_process_boundary() {
    // Arrange: crash_case creates an isolated known generation for every case.
    // Act: terminate the child process at the selected publication boundary.
    // Assert: crash_case verifies the recovered generation, catalog and second reopen.
    for phase in [
        TestPhase::VersionBodyPrepared,
        TestPhase::VersionBodyPublished,
        TestPhase::VersionMetadataPrepared,
        TestPhase::VersionMetadataPublished,
        TestPhase::VersionPublished,
    ] {
        crash_case("versioned", phase, 1);
    }
    for phase in [
        TestPhase::PublicationStaged,
        TestPhase::PublicationCommitted,
        TestPhase::BodyPublished,
        TestPhase::MetadataPublished,
        TestPhase::PublicationCleaned,
    ] {
        let hit = if matches!(
            phase,
            TestPhase::PublicationStaged
                | TestPhase::PublicationCommitted
                | TestPhase::PublicationCleaned
        ) {
            2
        } else {
            1
        };
        crash_case("versioned", phase, hit);
    }
    for mode in ["delete", "marker", "promote", "version-delete"] {
        for phase in [
            TestPhase::PublicationStaged,
            TestPhase::PublicationCommitted,
            TestPhase::PublicationCleaned,
        ] {
            crash_case(mode, phase, if mode == "marker" { 2 } else { 1 });
        }
    }
    for phase in [TestPhase::CurrentBodyRemoved, TestPhase::CurrentRemoved] {
        crash_case("delete", phase, 1);
        crash_case("marker", phase, 1);
    }
    for phase in [
        TestPhase::MarkerBodyPublished,
        TestPhase::MarkerMetadataPublished,
    ] {
        crash_case("marker", phase, 1);
    }
    for phase in [TestPhase::VersionRetiring, TestPhase::VersionDeleted] {
        crash_case("promote", phase, 1);
        crash_case("version-delete", phase, 1);
    }
    for phase in [
        TestPhase::BodyPrepared,
        TestPhase::BodyPublished,
        TestPhase::MetadataPrepared,
        TestPhase::MetadataPublished,
    ] {
        crash_case("promote", phase, 1);
    }
    for mode in ["suspended", "suspended-marker"] {
        for phase in [
            TestPhase::PublicationStaged,
            TestPhase::PublicationCommitted,
            TestPhase::PublicationCleaned,
        ] {
            crash_case(mode, phase, 1);
        }
    }
    for phase in [
        TestPhase::BodyPrepared,
        TestPhase::BodyPublished,
        TestPhase::MetadataPrepared,
        TestPhase::MetadataPublished,
    ] {
        crash_case("suspended", phase, 1);
    }
    for phase in [
        TestPhase::MarkerBodyPublished,
        TestPhase::MarkerMetadataPublished,
        TestPhase::CurrentBodyRemoved,
        TestPhase::CurrentRemoved,
    ] {
        crash_case("suspended-marker", phase, 1);
    }
}

#[test]
fn should_preserve_old_object_when_staging_fails_before_commit() {
    // Arrange
    let root = std::env::temp_dir().join(format!("sqrzl-object-stage-error-{}", Uuid::new_v4()));
    let storage = FilesystemStorage::open(&root).unwrap();
    storage.create_bucket(BUCKET.to_string()).unwrap();
    storage
        .put_object(BUCKET, "item".to_string(), generation(OLD, "old"))
        .unwrap();
    let mut invalid = generation(NEW, "new");
    invalid.size += 1;
    // Act
    // Assert
    assert!(storage
        .put_object(BUCKET, "item".to_string(), invalid)
        .is_err());
    assert_eq!(storage.get_object(BUCKET, "item").unwrap().data, OLD);
    drop(storage);
    let storage = FilesystemStorage::open(&root).unwrap();
    assert_eq!(storage.get_object(BUCKET, "item").unwrap().data, OLD);
    drop(storage);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn should_fail_closed_until_pending_commit_can_be_recovered() {
    // Arrange
    let root = std::env::temp_dir().join(format!("sqrzl-object-ambiguous-{}", Uuid::new_v4()));
    let storage = FilesystemStorage::open(&root).unwrap();
    storage.create_bucket(BUCKET.to_string()).unwrap();
    storage
        .put_object(BUCKET, "item".to_string(), generation(OLD, "old"))
        .unwrap();
    let directory = storage.object_id_dir(
        BUCKET,
        &FilesystemStorage::compute_object_id(BUCKET, "item"),
    );
    let hook_directory = directory.clone();
    *storage.test_hook.lock().unwrap() = Some(Arc::new(move |phase| {
        if phase == TestPhase::BodyPublished {
            let journal: serde_json::Value = serde_json::from_slice(
                &fs::read(hook_directory.join(".publication.json")).unwrap(),
            )
            .unwrap();
            let stage = hook_directory.join(journal["stage"].as_str().unwrap());
            fs::rename(stage.join("object.meta.json"), stage.join("held-metadata")).unwrap();
        }
    }));
    // Act
    // Assert
    assert!(storage
        .put_object(BUCKET, "item".to_string(), generation(NEW, "new"))
        .is_err());
    *storage.test_hook.lock().unwrap() = None;
    // The public body is already new, so every live read must stop at recovery
    // rather than expose the old public metadata alongside those bytes.
    assert!(storage.get_object(BUCKET, "item").is_err());
    assert!(storage.get_object_metadata(BUCKET, "item").is_err());
    assert!(storage.get_object_acl(BUCKET, "item").is_err());
    assert!(storage.get_object_tags(BUCKET, "item").is_err());
    assert!(storage.object_exists(BUCKET, "item").is_err());
    assert!(storage
        .list_objects(BUCKET, None, None, None, None)
        .is_err());
    assert!(storage
        .list_objects(BUCKET, None, Some("/"), None, None)
        .is_err());
    assert!(storage.list_object_versions(BUCKET, None).is_err());
    assert!(storage
        .put_object_tags(BUCKET, "item", HashMap::new())
        .is_err());
    assert!(storage
        .put_object_acl(BUCKET, "item", Acl::default())
        .is_err());
    assert!(storage
        .get_object_range(BUCKET, "item", 0, Some(2))
        .is_err());
    assert!(FilesystemStorage::open(&root).is_err());
    let journal: serde_json::Value =
        serde_json::from_slice(&fs::read(directory.join(".publication.json")).unwrap()).unwrap();
    let stage = directory.join(journal["stage"].as_str().unwrap());
    fs::rename(stage.join("held-metadata"), stage.join("object.meta.json")).unwrap();
    verify_generation(&storage.get_object(BUCKET, "item").unwrap());
    assert_eq!(storage.get_object(BUCKET, "item").unwrap().data, NEW);
    assert!(!directory.join(".publication.json").exists());
    drop(storage);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn should_reject_invalid_committed_journal_before_mutating_public_files() {
    // Arrange
    let root =
        std::env::temp_dir().join(format!("sqrzl-object-invalid-journal-{}", Uuid::new_v4()));
    let storage = FilesystemStorage::open(&root).unwrap();
    storage.create_bucket(BUCKET.to_string()).unwrap();
    storage
        .put_object(BUCKET, "item".to_string(), generation(OLD, "old"))
        .unwrap();
    let directory = storage.object_id_dir(
        BUCKET,
        &FilesystemStorage::compute_object_id(BUCKET, "item"),
    );
    let metadata = fs::read(directory.join("object.meta.json")).unwrap();
    fs::write(directory.join(".publication.json"), br#"{"stage":"../escape","publish":true,"clear_current":false,"remove_versions":[],"marker":null}"#).unwrap();
    // Act
    // Assert
    assert!(FilesystemStorage::open(&root).is_err());
    assert_eq!(fs::read(directory.join("object.blob")).unwrap(), OLD);
    assert_eq!(
        fs::read(directory.join("object.meta.json")).unwrap(),
        metadata
    );
    drop(storage);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn should_fail_closed_on_pending_publications_before_selected_version_reads() {
    // Arrange
    let root = std::env::temp_dir().join(format!("sqrzl-version-recovery-{}", Uuid::new_v4()));
    let storage = FilesystemStorage::open(&root).unwrap();
    storage.create_bucket(BUCKET.to_string()).unwrap();
    storage.enable_versioning(BUCKET).unwrap();
    storage
        .put_object(BUCKET, "item".to_string(), generation(OLD, "old"))
        .unwrap();
    let version = storage
        .get_object_metadata(BUCKET, "item")
        .unwrap()
        .version_id
        .unwrap();
    storage
        .put_object(BUCKET, "item".to_string(), generation(NEW, "new"))
        .unwrap();
    let object_id = FilesystemStorage::compute_object_id(BUCKET, "item");
    // Act
    // Assert
    for directory in [
        storage.object_id_dir(BUCKET, &object_id),
        storage.version_dir(BUCKET, &object_id, &version),
    ] {
        let journal = directory.join(".publication.json");
        fs::write(&journal, br#"{"stage":"../escape","publish":true,"clear_current":false,"remove_versions":[],"marker":null}"#).unwrap();
        assert!(storage
            .get_object_version_metadata(BUCKET, "item", &version)
            .is_err());
        assert!(storage
            .get_object_version_range(BUCKET, "item", &version, 0, Some(2))
            .is_err());
        fs::remove_file(journal).unwrap();
    }
    assert_eq!(
        storage
            .get_object_version_range(BUCKET, "item", &version, 0, Some(2))
            .unwrap()
            .1,
        b"old"
    );
    drop(storage);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn should_copy_payload_through_a_private_synced_path_without_changing_source() {
    // Arrange
    let root = std::env::temp_dir().join(format!("sqrzl-object-copy-{}", Uuid::new_v4()));
    fs::create_dir_all(&root).unwrap();
    let source = root.join("source");
    let destination = root.join("destination");
    fs::write(&source, NEW).unwrap();
    fs::write(&destination, OLD).unwrap();
    // Act
    FilesystemStorage::atomic_copy(&source, &destination).unwrap();
    // Assert
    assert_eq!(fs::read(&source).unwrap(), NEW);
    assert_eq!(fs::read(&destination).unwrap(), NEW);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn should_keep_new_ambiguous_publications_in_listing_until_recovery() {
    // Arrange
    let root = std::env::temp_dir().join(format!("sqrzl-object-new-ambiguous-{}", Uuid::new_v4()));
    let storage = FilesystemStorage::open(&root).unwrap();
    storage.create_bucket(BUCKET.to_string()).unwrap();
    let directory = storage.object_id_dir(
        BUCKET,
        &FilesystemStorage::compute_object_id(BUCKET, "item"),
    );
    let hook_directory = directory.clone();
    *storage.test_hook.lock().unwrap() = Some(Arc::new(move |phase| {
        if phase == TestPhase::BodyPublished {
            let journal: serde_json::Value = serde_json::from_slice(
                &fs::read(hook_directory.join(".publication.json")).unwrap(),
            )
            .unwrap();
            let stage = hook_directory.join(journal["stage"].as_str().unwrap());
            fs::rename(stage.join("object.meta.json"), stage.join("held-metadata")).unwrap();
        }
    }));
    // Act
    // Assert
    assert!(storage
        .put_object(BUCKET, "item".to_string(), generation(NEW, "new"))
        .is_err());
    *storage.test_hook.lock().unwrap() = None;
    assert!(storage
        .list_objects(BUCKET, None, None, None, None)
        .is_err());
    let journal: serde_json::Value =
        serde_json::from_slice(&fs::read(directory.join(".publication.json")).unwrap()).unwrap();
    let stage = directory.join(journal["stage"].as_str().unwrap());
    fs::rename(stage.join("held-metadata"), stage.join("object.meta.json")).unwrap();
    let listed = storage
        .list_objects(BUCKET, None, None, None, None)
        .unwrap();
    assert_eq!(listed.objects.len(), 1);
    assert_eq!(listed.objects[0].metadata["generation"], "new");
    verify_generation(&storage.get_object(BUCKET, "item").unwrap());
    drop(storage);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn should_clean_only_owned_uncommitted_publication_files_on_reopen() {
    // Arrange
    let root = std::env::temp_dir().join(format!("sqrzl-object-orphans-{}", Uuid::new_v4()));
    let storage = FilesystemStorage::open(&root).unwrap();
    storage.create_bucket(BUCKET.to_string()).unwrap();
    storage
        .put_object(BUCKET, "item".to_string(), generation(OLD, "old"))
        .unwrap();
    let directory = storage.object_id_dir(
        BUCKET,
        &FilesystemStorage::compute_object_id(BUCKET, "item"),
    );
    let stage = directory.join(format!(".publication-stage-{}", Uuid::new_v4()));
    fs::create_dir(&stage).unwrap();
    fs::write(stage.join("object.blob"), NEW).unwrap();
    let mut owned = vec![stage];
    for prefix in [
        ".published-",
        ".copy-",
        ".object.blob.",
        ".object.meta.json.",
        "..publication.json.",
    ] {
        let file = directory.join(format!("{prefix}{}.tmp", Uuid::new_v4()));
        fs::write(&file, NEW).unwrap();
        owned.push(file);
    }
    let unrelated = directory.join(".published-user-owned.tmp");
    fs::write(&unrelated, b"preserve me").unwrap();
    drop(storage);
    // Act
    let storage = FilesystemStorage::open(&root).unwrap();
    // Assert
    assert_eq!(storage.get_object(BUCKET, "item").unwrap().data, OLD);
    assert!(owned.iter().all(|path| !path.exists()));
    assert_eq!(fs::read(&unrelated).unwrap(), b"preserve me");
    drop(storage);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn should_preserve_memory_bounds_for_version_history_operations() {
    const SIZE: u64 = 192 * 1024 * 1024;
    // Arrange
    let root = std::env::temp_dir().join(format!("sqrzl-object-sparse-history-{}", Uuid::new_v4()));
    let storage = FilesystemStorage::open(&root).unwrap();
    storage.create_bucket(BUCKET.to_string()).unwrap();
    storage.enable_versioning(BUCKET).unwrap();
    let reads = Arc::new(AtomicUsize::new(0));
    let observed = reads.clone();
    *storage.test_hook.lock().unwrap() = Some(Arc::new(move |phase| {
        if phase == TestPhase::FullPayload {
            observed.fetch_add(1, Ordering::SeqCst);
        }
    }));
    // Act
    for (label, marker) in [("old", b'o'), ("new", b'n')] {
        let source = root.join(format!("{label}-spool"));
        let mut file = fs::File::create(&source).unwrap();
        file.set_len(SIZE).unwrap();
        std::io::Write::write_all(&mut file, &[marker; 3]).unwrap();
        std::io::Seek::seek(&mut file, std::io::SeekFrom::Start(SIZE - 3)).unwrap();
        std::io::Write::write_all(&mut file, &[marker; 3]).unwrap();
        let mut object = generation(&[], label);
        object.size = SIZE;
        object.etag = format!("{label}-sparse-etag");
        storage
            .put_object_streamed(BUCKET, "item".to_string(), object, &source)
            .unwrap();
    }
    let current = storage.get_object_metadata(BUCKET, "item").unwrap();
    storage
        .delete_object_version(BUCKET, "item", current.version_id.as_deref().unwrap())
        .unwrap();
    // Assert
    assert_eq!(
        reads.load(Ordering::SeqCst),
        0,
        "history snapshot or promotion materialized a full payload"
    );
    let (metadata, first) = storage
        .get_object_range(BUCKET, "item", 0, Some(2))
        .unwrap();
    assert_eq!(metadata.size, SIZE);
    assert_eq!(metadata.metadata["generation"], "old");
    assert_eq!(first, b"ooo");
    assert_eq!(
        storage
            .get_object_range(BUCKET, "item", SIZE - 3, Some(SIZE - 1))
            .unwrap()
            .1,
        b"ooo"
    );
    drop(storage);
    let storage = FilesystemStorage::open(&root).unwrap();
    assert_eq!(
        storage
            .get_object_range(BUCKET, "item", 0, Some(2))
            .unwrap()
            .1,
        b"ooo"
    );
    drop(storage);
    fs::remove_dir_all(root).unwrap();
}
