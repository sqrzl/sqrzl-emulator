mod common;

use common::interop::{
    auth_disabled, body_bytes, body_text, call, extract_tag, request, temp_storage,
};
use hyper::StatusCode;

#[tokio::test(flavor = "multi_thread")]
async fn should_round_trip_bucket_and_object_operations_given_basic_s3_requests_when_using_crud_flows(
) {
    let storage = temp_storage();
    assert_eq!(
        call(
            storage.clone(),
            auth_disabled(),
            request("PUT", "http://localhost/interop-s3", &[], b""),
        )
        .await
        .status(),
        StatusCode::OK
    );

    assert_eq!(
        call(
            storage.clone(),
            auth_disabled(),
            request(
                "PUT",
                "http://localhost/interop-s3/hello.txt",
                &[("content-type", "text/plain")],
                b"s3 smoke",
            ),
        )
        .await
        .status(),
        StatusCode::OK
    );

    let body = body_bytes(
        call(
            storage.clone(),
            auth_disabled(),
            request("GET", "http://localhost/interop-s3/hello.txt", &[], b""),
        )
        .await,
    )
    .await;
    assert_eq!(body, b"s3 smoke");

    let ranged = body_bytes(
        call(
            storage.clone(),
            auth_disabled(),
            request(
                "GET",
                "http://localhost/interop-s3/hello.txt",
                &[("range", "bytes=0-1")],
                b"",
            ),
        )
        .await,
    )
    .await;
    assert_eq!(ranged, b"s3");

    let listing = body_text(
        call(
            storage,
            auth_disabled(),
            request("GET", "http://localhost/interop-s3?list-type=2", &[], b""),
        )
        .await,
    )
    .await;
    assert!(listing.contains("hello.txt"));
}

#[tokio::test(flavor = "multi_thread")]
async fn should_assemble_completed_object_given_uploaded_parts_when_finishing_s3_multipart_upload()
{
    let storage = temp_storage();
    call(
        storage.clone(),
        auth_disabled(),
        request("PUT", "http://localhost/interop-s3", &[], b""),
    )
    .await;

    let initiate = body_text(
        call(
            storage.clone(),
            auth_disabled(),
            request(
                "POST",
                "http://localhost/interop-s3/multipart.txt?uploads",
                &[],
                b"",
            ),
        )
        .await,
    )
    .await;
    let upload_id = extract_tag(&initiate, "UploadId").expect("upload id should exist");
    let part_one_body = vec![b'm'; 5 * 1024 * 1024];

    let part_one = call(
        storage.clone(),
        auth_disabled(),
        request(
            "PUT",
            &format!("http://localhost/interop-s3/multipart.txt?partNumber=2&uploadId={upload_id}"),
            &[],
            &part_one_body,
        ),
    )
    .await;
    let etag_one = part_one
        .headers()
        .get("etag")
        .and_then(|value| value.to_str().ok())
        .expect("etag one should exist")
        .to_string();

    let part_two = call(
        storage.clone(),
        auth_disabled(),
        request(
            "PUT",
            &format!("http://localhost/interop-s3/multipart.txt?partNumber=7&uploadId={upload_id}"),
            &[],
            b"part",
        ),
    )
    .await;
    let etag_two = part_two
        .headers()
        .get("etag")
        .and_then(|value| value.to_str().ok())
        .expect("etag two should exist")
        .to_string();

    let complete_xml = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?><CompleteMultipartUpload><Part><PartNumber>2</PartNumber><ETag>{etag_one}</ETag></Part><Part><PartNumber>7</PartNumber><ETag>{etag_two}</ETag></Part></CompleteMultipartUpload>"
    );
    assert_eq!(
        call(
            storage.clone(),
            auth_disabled(),
            request(
                "POST",
                &format!("http://localhost/interop-s3/multipart.txt?uploadId={upload_id}"),
                &[("content-type", "application/xml")],
                complete_xml.as_bytes(),
            ),
        )
        .await
        .status(),
        StatusCode::OK
    );

    let body = body_bytes(
        call(
            storage,
            auth_disabled(),
            request("GET", "http://localhost/interop-s3/multipart.txt", &[], b""),
        )
        .await,
    )
    .await;
    assert_eq!(body.len(), part_one_body.len() + b"part".len());
    assert_eq!(&body[..part_one_body.len()], part_one_body.as_slice());
    assert_eq!(&body[part_one_body.len()..], b"part");
}

#[tokio::test(flavor = "multi_thread")]
#[allow(clippy::too_many_lines)] // Failure, retained upload, retry, and final bytes form one multipart invariant.
async fn should_return_entity_too_small_without_consuming_s3_multipart_upload() {
    let storage = temp_storage();
    call(
        storage.clone(),
        auth_disabled(),
        request("PUT", "http://localhost/interop-s3", &[], b""),
    )
    .await;

    let initiate = body_text(
        call(
            storage.clone(),
            auth_disabled(),
            request(
                "POST",
                "http://localhost/interop-s3/undersized.txt?uploads",
                &[],
                b"",
            ),
        )
        .await,
    )
    .await;
    let upload_id = extract_tag(&initiate, "UploadId").expect("upload id should exist");

    let small_part = call(
        storage.clone(),
        auth_disabled(),
        request(
            "PUT",
            &format!(
                "http://localhost/interop-s3/undersized.txt?partNumber=1&uploadId={upload_id}"
            ),
            &[],
            b"too-small",
        ),
    )
    .await;
    let small_etag = small_part
        .headers()
        .get("etag")
        .and_then(|value| value.to_str().ok())
        .expect("small part ETag should exist")
        .to_string();

    let final_part = call(
        storage.clone(),
        auth_disabled(),
        request(
            "PUT",
            &format!(
                "http://localhost/interop-s3/undersized.txt?partNumber=2&uploadId={upload_id}"
            ),
            &[],
            b"final",
        ),
    )
    .await;
    let final_etag = final_part
        .headers()
        .get("etag")
        .and_then(|value| value.to_str().ok())
        .expect("final part ETag should exist")
        .to_string();

    let completion = |first_etag: &str| {
        format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?><CompleteMultipartUpload><Part><PartNumber>1</PartNumber><ETag>{first_etag}</ETag></Part><Part><PartNumber>2</PartNumber><ETag>{final_etag}</ETag></Part></CompleteMultipartUpload>"
        )
    };
    let rejected = call(
        storage.clone(),
        auth_disabled(),
        request(
            "POST",
            &format!("http://localhost/interop-s3/undersized.txt?uploadId={upload_id}"),
            &[("content-type", "application/xml")],
            completion(&small_etag).as_bytes(),
        ),
    )
    .await;
    assert_eq!(rejected.status(), StatusCode::BAD_REQUEST);
    assert!(body_text(rejected)
        .await
        .contains("<Code>EntityTooSmall</Code>"));

    let replacement_body = vec![b'r'; 5 * 1024 * 1024];
    let replacement = call(
        storage.clone(),
        auth_disabled(),
        request(
            "PUT",
            &format!(
                "http://localhost/interop-s3/undersized.txt?partNumber=1&uploadId={upload_id}"
            ),
            &[],
            &replacement_body,
        ),
    )
    .await;
    let replacement_etag = replacement
        .headers()
        .get("etag")
        .and_then(|value| value.to_str().ok())
        .expect("replacement part ETag should exist")
        .to_string();

    assert_eq!(
        call(
            storage,
            auth_disabled(),
            request(
                "POST",
                &format!("http://localhost/interop-s3/undersized.txt?uploadId={upload_id}"),
                &[("content-type", "application/xml")],
                completion(&replacement_etag).as_bytes(),
            ),
        )
        .await
        .status(),
        StatusCode::OK
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn should_list_multiple_versions_given_versioning_enabled_when_object_is_overwritten() {
    let storage = temp_storage();
    call(
        storage.clone(),
        auth_disabled(),
        request("PUT", "http://localhost/interop-s3", &[], b""),
    )
    .await;
    let versioning_xml = br#"<?xml version="1.0" encoding="UTF-8"?><VersioningConfiguration xmlns="http://s3.amazonaws.com/doc/2006-03-01/"><Status>Enabled</Status></VersioningConfiguration>"#;
    call(
        storage.clone(),
        auth_disabled(),
        request(
            "PUT",
            "http://localhost/interop-s3?versioning",
            &[("content-type", "application/xml")],
            versioning_xml,
        ),
    )
    .await;

    call(
        storage.clone(),
        auth_disabled(),
        request(
            "PUT",
            "http://localhost/interop-s3/versioned.txt",
            &[],
            b"v1",
        ),
    )
    .await;
    call(
        storage.clone(),
        auth_disabled(),
        request(
            "PUT",
            "http://localhost/interop-s3/versioned.txt",
            &[],
            b"v2",
        ),
    )
    .await;

    let versions = body_text(
        call(
            storage,
            auth_disabled(),
            request(
                "GET",
                "http://localhost/interop-s3?versions&prefix=versioned.txt",
                &[],
                b"",
            ),
        )
        .await,
    )
    .await;
    assert!(versions.matches("<Version>").count() >= 2);
}

#[tokio::test(flavor = "multi_thread")]
#[allow(clippy::too_many_lines)] // Enabled, suspended, null replacement, and reveal checks share one version timeline.
async fn should_use_one_null_version_and_preserve_non_null_history_when_versioning_is_suspended() {
    let storage = temp_storage();
    call(
        storage.clone(),
        auth_disabled(),
        request("PUT", "http://localhost/interop-s3", &[], b""),
    )
    .await;
    call(
        storage.clone(),
        auth_disabled(),
        request(
            "PUT",
            "http://localhost/interop-s3/suspended.txt",
            &[],
            b"pre-versioning",
        ),
    )
    .await;
    let enabled = br#"<?xml version="1.0" encoding="UTF-8"?><VersioningConfiguration xmlns="http://s3.amazonaws.com/doc/2006-03-01/"><Status>Enabled</Status></VersioningConfiguration>"#;
    assert_eq!(
        call(
            storage.clone(),
            auth_disabled(),
            request(
                "PUT",
                "http://localhost/interop-s3?versioning",
                &[("content-type", "application/xml")],
                enabled,
            ),
        )
        .await
        .status(),
        StatusCode::OK
    );
    let preexisting = call(
        storage.clone(),
        auth_disabled(),
        request("GET", "http://localhost/interop-s3/suspended.txt", &[], b""),
    )
    .await;
    assert_eq!(
        preexisting
            .headers()
            .get("x-amz-version-id")
            .and_then(|value| value.to_str().ok()),
        Some("null")
    );
    assert_eq!(body_bytes(preexisting).await, b"pre-versioning");

    let versioned = call(
        storage.clone(),
        auth_disabled(),
        request(
            "PUT",
            "http://localhost/interop-s3/suspended.txt",
            &[],
            b"versioned",
        ),
    )
    .await;
    let versioned_id = versioned
        .headers()
        .get("x-amz-version-id")
        .and_then(|value| value.to_str().ok())
        .expect("enabled write should return a version id")
        .to_string();
    assert_ne!(versioned_id, "null");

    let suspended = br#"<?xml version="1.0" encoding="UTF-8"?><VersioningConfiguration xmlns="http://s3.amazonaws.com/doc/2006-03-01/"><Status>Suspended</Status></VersioningConfiguration>"#;
    assert_eq!(
        call(
            storage.clone(),
            auth_disabled(),
            request(
                "PUT",
                "http://localhost/interop-s3?versioning",
                &[("content-type", "application/xml")],
                suspended,
            ),
        )
        .await
        .status(),
        StatusCode::OK
    );

    for body in [b"first-null".as_slice(), b"replacement-null".as_slice()] {
        let response = call(
            storage.clone(),
            auth_disabled(),
            request(
                "PUT",
                "http://localhost/interop-s3/suspended.txt",
                &[],
                body,
            ),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response
                .headers()
                .get("x-amz-version-id")
                .and_then(|value| value.to_str().ok()),
            Some("null")
        );
    }

    let versions = body_text(
        call(
            storage.clone(),
            auth_disabled(),
            request(
                "GET",
                "http://localhost/interop-s3?versions&prefix=suspended.txt",
                &[],
                b"",
            ),
        )
        .await,
    )
    .await;
    assert_eq!(versions.matches("<Version>").count(), 2);
    assert!(versions.contains(&format!("<VersionId>{versioned_id}</VersionId>")));
    assert!(versions.contains("<VersionId>null</VersionId>"));

    let deleted = call(
        storage.clone(),
        auth_disabled(),
        request(
            "DELETE",
            "http://localhost/interop-s3/suspended.txt",
            &[],
            b"",
        ),
    )
    .await;
    assert_eq!(deleted.status(), StatusCode::NO_CONTENT);
    assert_eq!(
        deleted
            .headers()
            .get("x-amz-version-id")
            .and_then(|value| value.to_str().ok()),
        Some("null")
    );
    assert_eq!(
        deleted
            .headers()
            .get("x-amz-delete-marker")
            .and_then(|value| value.to_str().ok()),
        Some("true")
    );

    let hidden = call(
        storage.clone(),
        auth_disabled(),
        request("GET", "http://localhost/interop-s3/suspended.txt", &[], b""),
    )
    .await;
    assert_eq!(hidden.status(), StatusCode::NOT_FOUND);
    assert_eq!(
        hidden
            .headers()
            .get("x-amz-delete-marker")
            .and_then(|value| value.to_str().ok()),
        Some("true")
    );

    let remove_marker = call(
        storage.clone(),
        auth_disabled(),
        request(
            "DELETE",
            "http://localhost/interop-s3/suspended.txt?versionId=null",
            &[],
            b"",
        ),
    )
    .await;
    assert_eq!(remove_marker.status(), StatusCode::NO_CONTENT);
    assert_eq!(
        remove_marker
            .headers()
            .get("x-amz-delete-marker")
            .and_then(|value| value.to_str().ok()),
        Some("true")
    );
    assert_eq!(
        body_bytes(
            call(
                storage,
                auth_disabled(),
                request("GET", "http://localhost/interop-s3/suspended.txt", &[], b"",),
            )
            .await,
        )
        .await,
        b"versioned"
    );
}
