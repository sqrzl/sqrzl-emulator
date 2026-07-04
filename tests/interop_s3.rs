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

    let part_one = call(
        storage.clone(),
        auth_disabled(),
        request(
            "PUT",
            &format!("http://localhost/interop-s3/multipart.txt?partNumber=1&uploadId={upload_id}"),
            &[],
            b"multi",
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
            &format!("http://localhost/interop-s3/multipart.txt?partNumber=2&uploadId={upload_id}"),
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
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?><CompleteMultipartUpload><Part><PartNumber>1</PartNumber><ETag>{etag_one}</ETag></Part><Part><PartNumber>2</PartNumber><ETag>{etag_two}</ETag></Part></CompleteMultipartUpload>"
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
    assert_eq!(body, b"multipart");
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
