mod common;

use common::interop::{auth_disabled, body_bytes, body_text, call, request, temp_storage};

#[tokio::test(flavor = "multi_thread")]
async fn should_round_trip_namespace_bucket_and_object_operations_given_basic_oci_requests_when_using_core_flows(
) {
    let storage = temp_storage();
    let namespace = body_text(
        call(
            storage.clone(),
            auth_disabled(),
            request("GET", "http://localhost/n/tenant", &[], b""),
        )
        .await,
    )
    .await;
    assert_eq!(namespace, "tenant");
    call(
        storage.clone(),
        auth_disabled(),
        request(
            "POST",
            "http://localhost/n/tenant/b",
            &[("content-type", "application/json")],
            br#"{"name":"interop-oci","compartmentId":"ignored"}"#,
        ),
    )
    .await;
    call(
        storage.clone(),
        auth_disabled(),
        request(
            "PUT",
            "http://localhost/n/tenant/b/interop-oci/o/hello.txt",
            &[("content-type", "text/plain")],
            b"oci smoke",
        ),
    )
    .await;
    let body = body_bytes(
        call(
            storage,
            auth_disabled(),
            request(
                "GET",
                "http://localhost/n/tenant/b/interop-oci/o/hello.txt",
                &[],
                b"",
            ),
        )
        .await,
    )
    .await;
    assert_eq!(body, b"oci smoke");
}

#[tokio::test(flavor = "multi_thread")]
async fn should_return_custom_metadata_given_oci_metadata_headers_when_requesting_object_head() {
    let storage = temp_storage();
    call(
        storage.clone(),
        auth_disabled(),
        request(
            "POST",
            "http://localhost/n/tenant/b",
            &[("content-type", "application/json")],
            br#"{"name":"interop-oci","compartmentId":"ignored"}"#,
        ),
    )
    .await;
    call(
        storage.clone(),
        auth_disabled(),
        request(
            "PUT",
            "http://localhost/n/tenant/b/interop-oci/o/hello.txt",
            &[("content-type", "text/plain"), ("opc-meta-owner", "sdk")],
            b"oci smoke",
        ),
    )
    .await;
    let response = call(
        storage,
        auth_disabled(),
        request(
            "HEAD",
            "http://localhost/n/tenant/b/interop-oci/o/hello.txt",
            &[],
            b"",
        ),
    )
    .await;
    assert_eq!(
        response
            .headers()
            .get("opc-meta-owner")
            .and_then(|value| value.to_str().ok()),
        Some("sdk")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn should_list_prefixed_objects_given_nested_keys_when_querying_oci_bucket_contents() {
    let storage = temp_storage();
    call(
        storage.clone(),
        auth_disabled(),
        request(
            "POST",
            "http://localhost/n/tenant/b",
            &[("content-type", "application/json")],
            br#"{"name":"interop-oci","compartmentId":"ignored"}"#,
        ),
    )
    .await;
    call(
        storage.clone(),
        auth_disabled(),
        request(
            "PUT",
            "http://localhost/n/tenant/b/interop-oci/o/folder/hello.txt",
            &[("content-type", "text/plain")],
            b"oci smoke",
        ),
    )
    .await;
    let listing = body_text(
        call(
            storage,
            auth_disabled(),
            request(
                "GET",
                "http://localhost/n/tenant/b/interop-oci/o?prefix=folder/",
                &[],
                b"",
            ),
        )
        .await,
    )
    .await;
    assert!(listing.contains("folder/hello.txt"));
}

#[tokio::test(flavor = "multi_thread")]
async fn should_commit_multipart_object_given_uploaded_parts_when_finalizing_oci_upload() {
    let storage = temp_storage();
    call(
        storage.clone(),
        auth_disabled(),
        request(
            "POST",
            "http://localhost/n/tenant/b",
            &[("content-type", "application/json")],
            br#"{"name":"interop-oci","compartmentId":"ignored"}"#,
        ),
    )
    .await;
    let init = body_text(
        call(
            storage.clone(),
            auth_disabled(),
            request(
                "POST",
                "http://localhost/n/tenant/b/interop-oci/u",
                &[("content-type", "application/json")],
                br#"{"object":"multi.txt","contentType":"text/plain","metadata":{"owner":"sdk"},"storageTier":"InfrequentAccess"}"#,
            ),
        )
        .await,
    )
    .await;
    let init_json: serde_json::Value = serde_json::from_str(&init).expect("json should parse");
    let upload_id = init_json
        .get("uploadId")
        .and_then(|value| value.as_str())
        .expect("upload id should exist");

    let part_one = call(
        storage.clone(),
        auth_disabled(),
        request(
            "PUT",
            &format!(
                "http://localhost/n/tenant/b/interop-oci/u/multi.txt?uploadId={upload_id}&uploadPartNum=1"
            ),
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
            &format!(
                "http://localhost/n/tenant/b/interop-oci/u/multi.txt?uploadId={upload_id}&uploadPartNum=2"
            ),
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

    let commit = format!(
        "{{\"partsToCommit\":[{{\"partNum\":1,\"etag\":\"{etag_one}\"}},{{\"partNum\":2,\"etag\":\"{etag_two}\"}}]}}"
    );
    call(
        storage.clone(),
        auth_disabled(),
        request(
            "POST",
            &format!("http://localhost/n/tenant/b/interop-oci/u/multi.txt?uploadId={upload_id}"),
            &[("content-type", "application/json")],
            commit.as_bytes(),
        ),
    )
    .await;
    let body = body_bytes(
        call(
            storage,
            auth_disabled(),
            request(
                "GET",
                "http://localhost/n/tenant/b/interop-oci/o/multi.txt",
                &[],
                b"",
            ),
        )
        .await,
    )
    .await;
    assert_eq!(body, b"multipart");
}
