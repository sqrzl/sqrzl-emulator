use super::*;
use crate::storage::MultipartCompletion;

fn checksum_upload(storage: &FilesystemStorage, payload: &[u8]) -> (String, Vec<(u32, String)>) {
    let upload = storage
        .create_multipart_upload_with_metadata(
            "bucket",
            "key".to_string(),
            None,
            HashMap::new(),
            HashMap::from([("s3_checksum_algorithm".to_string(), "CRC32C".to_string())]),
        )
        .unwrap();
    let etag = storage
        .upload_part("bucket", &upload.upload_id, 1, payload.to_vec())
        .unwrap();
    (upload.upload_id, vec![(1, etag)])
}

#[test]
fn should_reject_corrupted_staged_bytes_after_restart_without_publishing_an_object() {
    // Arrange
    let directory = std::env::temp_dir().join(format!("sqrzl-multipart-{}", Uuid::new_v4()));
    let storage = FilesystemStorage::new(&directory);
    storage.create_bucket("bucket".to_string()).unwrap();
    let (id, manifest) = checksum_upload(&storage, b"original");
    let part_path = storage.part_path("bucket", &id, 1);
    let checksum = crc32c::crc32c(b"original");
    drop(storage);
    fs::write(part_path, b"tampered").unwrap();
    let storage = FilesystemStorage::new(&directory);

    // Act
    let result = storage.complete_multipart_upload_checked(
        "bucket",
        &id,
        &manifest,
        &MultipartCompletion {
            crc32c: Some(checksum),
            ..MultipartCompletion::default()
        },
    );

    // Assert
    assert!(matches!(result, Err(Error::BadDigest)));
    assert!(!storage.object_exists("bucket", "key").unwrap());
    assert_eq!(
        storage.get_multipart_upload("bucket", &id).unwrap().parts[0].crc32c,
        Some(checksum)
    );
    assert_eq!(
        fs::read_dir(storage.bucket_dir("bucket").join(".spool")).map_or(0, Iterator::count),
        0
    );
    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn should_publish_one_checked_multipart_winner_given_concurrent_create_only_completions() {
    // Arrange
    let directory = std::env::temp_dir().join(format!("sqrzl-multipart-{}", Uuid::new_v4()));
    let storage = Arc::new(FilesystemStorage::new(&directory));
    storage.create_bucket("bucket".to_string()).unwrap();
    let first = checksum_upload(&storage, b"first");
    let second = checksum_upload(&storage, b"second");
    let barrier = Arc::new(std::sync::Barrier::new(2));
    let threads = [(first, b"first".as_slice()), (second, b"second".as_slice())]
        .into_iter()
        .map(|((id, manifest), payload)| {
            let storage = storage.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                let result = storage.complete_multipart_upload_checked(
                    "bucket",
                    &id,
                    &manifest,
                    &MultipartCompletion {
                        crc32c: Some(crc32c::crc32c(payload)),
                        size: Some(payload.len() as u64),
                        condition: Some(ObjectCondition::Missing),
                        ..MultipartCompletion::default()
                    },
                );
                (result, payload)
            })
        })
        .collect::<Vec<_>>();

    // Act
    let results = threads
        .into_iter()
        .map(|thread| thread.join().unwrap())
        .collect::<Vec<_>>();

    // Assert
    assert_eq!(
        results.iter().filter(|(result, _)| result.is_ok()).count(),
        1
    );
    assert_eq!(
        results
            .iter()
            .filter(|(result, _)| matches!(result, Err(Error::PreconditionFailed)))
            .count(),
        1
    );
    let expected = results.iter().find(|(result, _)| result.is_ok()).unwrap().1;
    assert_eq!(storage.get_object("bucket", "key").unwrap().data, expected);
    assert_eq!(storage.list_multipart_uploads("bucket").unwrap().len(), 1);
    fs::remove_dir_all(directory).unwrap();
}
