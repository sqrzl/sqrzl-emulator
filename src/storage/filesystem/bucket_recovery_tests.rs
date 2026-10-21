use super::*;
use crate::storage::StorageRootWriter;
use std::process::{Command, Stdio};

const INTERRUPTED: &str = "interrupted-creation";
const SURVIVORS: &[&str] = &["ordinary", "protected"];
const CONTENT: &[u8] = b"surviving-object-bytes";

#[test]
#[ignore = "subprocess worker, invoked by the bucket creation crash campaigns"]
fn should_exit_at_bucket_creation_boundary_given_worker_environment() {
    // Arrange
    let Ok(root) = std::env::var("SQRZL_TEST_BUCKET_CREATION_ROOT") else {
        return;
    };
    // The parent retains the root writer guard while this child is the sole
    // writer, then waits for termination before reopening the store.
    let storage = FilesystemStorage::open(&root).unwrap();
    let target = std::env::var("SQRZL_TEST_BUCKET_CREATION_PHASE").unwrap();
    *storage.test_hook.lock().unwrap() = Some(Arc::new(move |phase| {
        if format!("{phase:?}") == target {
            std::process::exit(86);
        }
    }));

    // Act
    storage.create_bucket(INTERRUPTED.to_string()).unwrap();

    // Assert
    panic!("the requested bucket creation crash boundary was never reached");
}

fn assert_creation_crash_recovery(phase: TestPhase, published: bool) {
    let root = std::env::temp_dir().join(format!("sqrzl-bucket-crash-{}", Uuid::new_v4()));
    let writer = StorageRootWriter::acquire(&root).unwrap();
    let storage = FilesystemStorage::open(&root).unwrap();
    for bucket in SURVIVORS {
        storage.create_bucket((*bucket).to_string()).unwrap();
        let mut object = Object::new(
            "target".to_string(),
            CONTENT.to_vec(),
            "text/plain".to_string(),
        );
        if *bucket == "protected" {
            object
                .provider_metadata
                .insert("azure_legal_hold".to_string(), "true".to_string());
        }
        storage
            .put_object(bucket, "target".to_string(), object)
            .unwrap();
    }
    let directory = storage.bucket_dir(INTERRUPTED);
    drop(storage);

    let status = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "storage::filesystem::bucket_recovery_tests::should_exit_at_bucket_creation_boundary_given_worker_environment",
            "--ignored",
        ])
        .env("SQRZL_TEST_BUCKET_CREATION_ROOT", &root)
        .env("SQRZL_TEST_BUCKET_CREATION_PHASE", format!("{phase:?}"))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .unwrap();
    assert_eq!(status.code(), Some(86), "{phase:?}");
    assert!(directory.is_dir());
    assert_eq!(directory.join(".bucket.name").exists(), published);
    if phase == TestPhase::BucketNamePrepared {
        let entries = fs::read_dir(&directory)
            .unwrap()
            .collect::<std::io::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(fs::read(entries[0].path()).unwrap(), INTERRUPTED.as_bytes());
    }

    // Repeat recovery before retrying creation to check that it is idempotent.
    for _ in 0..2 {
        let reopened = FilesystemStorage::open(&root).unwrap();
        for bucket in SURVIVORS {
            assert_eq!(reopened.get_object(bucket, "target").unwrap().data, CONTENT);
            assert_eq!(
                reopened.azure_object_protection_absent(bucket),
                *bucket == "ordinary"
            );
            assert_eq!(
                crate::providers::azure_object_protection_active(&reopened, bucket),
                *bucket == "protected"
            );
        }
        assert_eq!(reopened.bucket_exists(INTERRUPTED).unwrap(), published);
        assert_eq!(
            reopened.list_buckets().unwrap().len(),
            if published { 3 } else { 2 }
        );
    }

    let reopened = FilesystemStorage::open(&root).unwrap();
    if published {
        assert!(matches!(
            reopened.create_bucket(INTERRUPTED.to_string()),
            Err(Error::BucketAlreadyExists)
        ));
    } else {
        reopened.create_bucket(INTERRUPTED.to_string()).unwrap();
    }
    reopened
        .put_object(
            INTERRUPTED,
            "after-restart".to_string(),
            Object::new(
                "after-restart".to_string(),
                CONTENT.to_vec(),
                "text/plain".to_string(),
            ),
        )
        .unwrap();
    drop(reopened);
    let reopened = FilesystemStorage::open(&root).unwrap();
    assert_eq!(
        reopened
            .get_object(INTERRUPTED, "after-restart")
            .unwrap()
            .data,
        CONTENT
    );
    drop(reopened);
    drop(writer);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn should_recover_empty_uncommitted_bucket_given_crash_after_directory_creation() {
    // Arrange
    let phase = TestPhase::BucketDirectoryCreated;
    // Act
    // Assert
    assert_creation_crash_recovery(phase, false);
}

#[test]
fn should_recover_bucket_name_temp_file_given_crash_before_identity_rename() {
    // Arrange
    let phase = TestPhase::BucketNamePrepared;
    // Act
    // Assert
    assert_creation_crash_recovery(phase, false);
}

#[test]
fn should_preserve_published_empty_bucket_given_crash_after_identity_rename() {
    // Arrange
    let phases = [TestPhase::BucketNamePublished, TestPhase::BucketCreated];
    // Act
    // Assert
    for phase in phases {
        assert_creation_crash_recovery(phase, true);
    }
}

#[test]
fn should_reject_missing_bucket_name_given_committed_or_unrecognized_entries() {
    // Arrange
    for entry in [
        ".bucket.identity.json",
        ".bucket.meta.json",
        "object.blob",
        "object/versions/history/object.blob",
        ".multipart/upload/part",
        ".spool/payload",
        "unknown",
        "..bucket.name.not-a-uuid.tmp",
        "..bucket.name.01234567-89ab-cdef-0123-456789abcdef.tmp.extra",
    ] {
        let root =
            std::env::temp_dir().join(format!("sqrzl-missing-bucket-name-{}", Uuid::new_v4()));
        let writer = StorageRootWriter::acquire(&root).unwrap();
        let storage = FilesystemStorage::open(&root).unwrap();
        let directory = storage.bucket_dir(INTERRUPTED);
        fs::create_dir(&directory).unwrap();
        let temporary = directory.join(format!("..bucket.name.{}.tmp", Uuid::new_v4()));
        fs::write(&temporary, INTERRUPTED).unwrap();
        let preserved = directory.join(entry);
        fs::create_dir_all(preserved.parent().unwrap()).unwrap();
        fs::write(&preserved, CONTENT).unwrap();
        drop(storage);

        // Act
        let outcome = FilesystemStorage::open(&root);

        // Assert
        assert!(
            outcome.is_err(),
            "must preserve missing-identity bucket with {entry}"
        );
        assert_eq!(fs::read(&preserved).unwrap(), CONTENT);
        assert_eq!(fs::read(&temporary).unwrap(), INTERRUPTED.as_bytes());
        drop(writer);
        fs::remove_dir_all(root).unwrap();
    }
}

#[test]
fn should_preserve_unrecognized_bucket_directories_given_missing_identity() {
    // Arrange
    for malformed_namespace in [false, true] {
        let root = std::env::temp_dir().join(format!("sqrzl-bucket-remnant-{}", Uuid::new_v4()));
        let writer = StorageRootWriter::acquire(&root).unwrap();
        let storage = FilesystemStorage::open(&root).unwrap();
        let directory = if malformed_namespace {
            root.join("b-unrecognized")
        } else {
            storage.bucket_dir(INTERRUPTED)
        };
        fs::create_dir(&directory).unwrap();
        let temporary = directory.join(format!("..bucket.name.{}.tmp", Uuid::new_v4()));
        if !malformed_namespace {
            fs::create_dir(&temporary).unwrap();
        }
        drop(storage);

        // Act
        let outcome = FilesystemStorage::open(&root);

        // Assert
        assert!(outcome.is_err());
        assert!(directory.is_dir());
        if !malformed_namespace {
            assert!(temporary.is_dir());
        }
        drop(writer);
        fs::remove_dir_all(root).unwrap();
    }
}

#[cfg(unix)]
#[test]
fn should_preserve_bucket_name_temp_symlinks_given_missing_identity() {
    // Arrange
    for dangling in [false, true] {
        let root = std::env::temp_dir().join(format!("sqrzl-bucket-symlink-{}", Uuid::new_v4()));
        let writer = StorageRootWriter::acquire(&root).unwrap();
        let storage = FilesystemStorage::open(&root).unwrap();
        let directory = storage.bucket_dir(INTERRUPTED);
        fs::create_dir(&directory).unwrap();
        let target = root.join("external-file");
        if !dangling {
            fs::write(&target, CONTENT).unwrap();
        }
        let temporary = directory.join(format!("..bucket.name.{}.tmp", Uuid::new_v4()));
        std::os::unix::fs::symlink(&target, &temporary).unwrap();
        drop(storage);

        // Act
        let outcome = FilesystemStorage::open(&root);

        // Assert
        assert!(outcome.is_err());
        assert!(fs::symlink_metadata(&temporary)
            .unwrap()
            .file_type()
            .is_symlink());
        if !dangling {
            assert_eq!(fs::read(&target).unwrap(), CONTENT);
        }
        drop(writer);
        fs::remove_dir_all(root).unwrap();
    }
}
