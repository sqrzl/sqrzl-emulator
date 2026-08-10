mod common;

use common::interop::{auth_disabled, body_bytes, body_text, call, request, temp_storage};
use hyper::StatusCode;

#[tokio::test(flavor = "multi_thread")]
async fn should_round_trip_namespace_bucket_and_object_operations_given_basic_oci_requests_when_using_core_flows(
) {
    let storage = temp_storage();
    let namespace = body_text(
        call(
            storage.clone(),
            auth_disabled(),
            request("GET", "http://localhost/n/", &[], b""),
        )
        .await,
    )
    .await;
    assert_eq!(namespace, "sqrzl-emulator");
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

#[tokio::test(flavor = "multi_thread")]
async fn should_paginate_oci_objects_with_next_start_with_body_token() {
    let storage = temp_storage();
    call(
        storage.clone(),
        auth_disabled(),
        request(
            "POST",
            "http://localhost/n/tenant/b",
            &[("content-type", "application/json")],
            br#"{"name":"page-oci","compartmentId":"ocid1.compartment.test"}"#,
        ),
    )
    .await;
    for key in ["a", "b", "c"] {
        call(
            storage.clone(),
            auth_disabled(),
            request(
                "PUT",
                &format!("http://localhost/n/tenant/b/page-oci/o/{key}"),
                &[],
                key.as_bytes(),
            ),
        )
        .await;
    }

    let first = call(
        storage.clone(),
        auth_disabled(),
        request(
            "GET",
            "http://localhost/n/tenant/b/page-oci/o?limit=2",
            &[],
            b"",
        ),
    )
    .await;
    assert!(!first.headers().contains_key("opc-next-page"));
    let first: serde_json::Value =
        serde_json::from_str(&body_text(first).await).expect("list response json");
    assert_eq!(first["objects"][0]["name"], "a");
    assert_eq!(first["objects"][1]["name"], "b");
    assert_eq!(first["nextStartWith"], "c");

    let second = call(
        storage,
        auth_disabled(),
        request(
            "GET",
            "http://localhost/n/tenant/b/page-oci/o?limit=2&start=c",
            &[],
            b"",
        ),
    )
    .await;
    let second: serde_json::Value =
        serde_json::from_str(&body_text(second).await).expect("list response json");
    assert_eq!(second["objects"].as_array().map(Vec::len), Some(1));
    assert_eq!(second["objects"][0]["name"], "c");
    assert!(second.get("nextStartWith").is_none());
}

#[tokio::test(flavor = "multi_thread")]
#[allow(clippy::too_many_lines)] // One CAS flow proves each failed condition preserves the same object.
async fn should_enforce_oci_object_conditions_without_mutating_on_failure() {
    let storage = temp_storage();
    call(
        storage.clone(),
        auth_disabled(),
        request(
            "POST",
            "http://localhost/n/tenant/b",
            &[("content-type", "application/json")],
            br#"{"name":"cas-oci","compartmentId":"ocid1.compartment.test"}"#,
        ),
    )
    .await;
    let created = call(
        storage.clone(),
        auth_disabled(),
        request(
            "PUT",
            "http://localhost/n/tenant/b/cas-oci/o/value",
            &[],
            b"first",
        ),
    )
    .await;
    let etag = created
        .headers()
        .get("etag")
        .and_then(|value| value.to_str().ok())
        .expect("etag")
        .to_string();

    let duplicate = call(
        storage.clone(),
        auth_disabled(),
        request(
            "PUT",
            "http://localhost/n/tenant/b/cas-oci/o/value",
            &[("if-none-match", "*")],
            b"second",
        ),
    )
    .await;
    assert_eq!(duplicate.status(), StatusCode::PRECONDITION_FAILED);
    let weak_update = call(
        storage.clone(),
        auth_disabled(),
        request(
            "PUT",
            "http://localhost/n/tenant/b/cas-oci/o/value",
            &[("if-match", &format!("W/\"{etag}\""))],
            b"weak",
        ),
    )
    .await;
    assert_eq!(weak_update.status(), StatusCode::PRECONDITION_FAILED);
    assert_eq!(
        body_bytes(
            call(
                storage.clone(),
                auth_disabled(),
                request(
                    "GET",
                    "http://localhost/n/tenant/b/cas-oci/o/value",
                    &[],
                    b""
                ),
            )
            .await
        )
        .await,
        b"first"
    );

    let stale_delete = call(
        storage.clone(),
        auth_disabled(),
        request(
            "DELETE",
            "http://localhost/n/tenant/b/cas-oci/o/value",
            &[("if-match", "stale")],
            b"",
        ),
    )
    .await;
    assert_eq!(stale_delete.status(), StatusCode::PRECONDITION_FAILED);
    let weak_delete = call(
        storage.clone(),
        auth_disabled(),
        request(
            "DELETE",
            "http://localhost/n/tenant/b/cas-oci/o/value",
            &[("if-match", &format!("W/\"{etag}\""))],
            b"",
        ),
    )
    .await;
    assert_eq!(weak_delete.status(), StatusCode::PRECONDITION_FAILED);

    let deleted = call(
        storage,
        auth_disabled(),
        request(
            "DELETE",
            "http://localhost/n/tenant/b/cas-oci/o/value",
            &[("if-match", &etag)],
            b"",
        ),
    )
    .await;
    assert_eq!(deleted.status(), StatusCode::NO_CONTENT);
}

#[tokio::test(flavor = "multi_thread")]
#[allow(clippy::too_many_lines)] // One protected bucket proves every object-committing path remains unchanged.
async fn should_reject_oci_mutations_on_gcs_retained_bucket_without_committing() {
    let storage = temp_storage();
    call(
        storage.clone(),
        auth_disabled(),
        request(
            "POST",
            "http://localhost/n/tenant/b",
            &[("content-type", "application/json")],
            br#"{"name":"protected-oci","compartmentId":"ignored"}"#,
        ),
    )
    .await;
    call(
        storage.clone(),
        auth_disabled(),
        request(
            "PUT",
            "http://localhost/n/tenant/b/protected-oci/o/current",
            &[],
            b"original",
        ),
    )
    .await;
    let init: serde_json::Value = serde_json::from_str(
        &body_text(
            call(
                storage.clone(),
                auth_disabled(),
                request(
                    "POST",
                    "http://localhost/n/tenant/b/protected-oci/u",
                    &[("content-type", "application/json")],
                    br#"{"object":"pending"}"#,
                ),
            )
            .await,
        )
        .await,
    )
    .expect("multipart init json");
    let upload_id = init["uploadId"].as_str().expect("upload id").to_string();
    let part = call(
        storage.clone(),
        auth_disabled(),
        request(
            "PUT",
            &format!(
                "http://localhost/n/tenant/b/protected-oci/u/pending?uploadId={upload_id}&uploadPartNum=1"
            ),
            &[],
            b"pending bytes",
        ),
    )
    .await;
    let part_etag = part
        .headers()
        .get("etag")
        .and_then(|value| value.to_str().ok())
        .expect("part etag")
        .to_string();
    let mut metadata = storage.get_bucket("protected-oci").unwrap().metadata;
    metadata.insert("gcs_retention_seconds".to_string(), "3600".to_string());
    storage
        .update_bucket_metadata("protected-oci", metadata)
        .unwrap();

    let overwrite = call(
        storage.clone(),
        auth_disabled(),
        request(
            "PUT",
            "http://localhost/n/tenant/b/protected-oci/o/current",
            &[],
            b"replacement",
        ),
    )
    .await;
    let delete = call(
        storage.clone(),
        auth_disabled(),
        request(
            "DELETE",
            "http://localhost/n/tenant/b/protected-oci/o/current",
            &[],
            b"",
        ),
    )
    .await;
    let commit_body = format!("{{\"partsToCommit\":[{{\"partNum\":1,\"etag\":\"{part_etag}\"}}]}}");
    let commit = call(
        storage.clone(),
        auth_disabled(),
        request(
            "POST",
            &format!("http://localhost/n/tenant/b/protected-oci/u/pending?uploadId={upload_id}"),
            &[("content-type", "application/json")],
            commit_body.as_bytes(),
        ),
    )
    .await;
    let bucket_delete = call(
        storage.clone(),
        auth_disabled(),
        request(
            "DELETE",
            "http://localhost/n/tenant/b/protected-oci",
            &[],
            b"",
        ),
    )
    .await;

    for response in [overwrite, delete, commit, bucket_delete] {
        assert_eq!(response.status(), StatusCode::CONFLICT);
        let body: serde_json::Value =
            serde_json::from_str(&body_text(response).await).expect("IncorrectState response json");
        assert_eq!(body["code"], "IncorrectState");
    }
    assert_eq!(
        storage.get_object("protected-oci", "current").unwrap().data,
        b"original"
    );
    assert!(storage.get_object("protected-oci", "pending").is_err());
    assert!(storage
        .get_multipart_upload("protected-oci", &upload_id)
        .is_ok());
}

#[tokio::test(flavor = "multi_thread")]
async fn should_validate_oci_md5_and_round_trip_object_response_metadata() {
    let storage = temp_storage();
    call(
        storage.clone(),
        auth_disabled(),
        request(
            "POST",
            "http://localhost/n/tenant/b",
            &[("content-type", "application/json")],
            br#"{"name":"metadata-oci","compartmentId":"ocid1.compartment.test"}"#,
        ),
    )
    .await;

    let mismatch = call(
        storage.clone(),
        auth_disabled(),
        request(
            "PUT",
            "http://localhost/n/tenant/b/metadata-oci/o/value",
            &[("content-md5", "AAAAAAAAAAAAAAAAAAAAAA==")],
            b"payload",
        ),
    )
    .await;
    assert_eq!(mismatch.status(), StatusCode::BAD_REQUEST);
    let mismatch_body: serde_json::Value =
        serde_json::from_str(&body_text(mismatch).await).expect("checksum error json");
    assert_eq!(mismatch_body["code"], "UnmatchedContentMD5");
    assert_eq!(
        mismatch_body["message"],
        "The computed MD5 of the request body (Mhw89IbtUJFk7eweGYH+yA==) does not match the Content-MD5 header (AAAAAAAAAAAAAAAAAAAAAA==)"
    );
    assert!(storage.get_object("metadata-oci", "value").is_err());

    let put = call(
        storage.clone(),
        auth_disabled(),
        request(
            "PUT",
            "http://localhost/n/tenant/b/metadata-oci/o/value",
            &[
                ("content-type", "text/plain"),
                ("content-language", "en-US"),
                ("content-encoding", "identity"),
                ("cache-control", "no-cache"),
                ("content-disposition", "attachment"),
                ("storage-tier", "InfrequentAccess"),
            ],
            b"payload",
        ),
    )
    .await;
    assert_eq!(put.status(), StatusCode::OK);
    let md5 = put
        .headers()
        .get("opc-content-md5")
        .and_then(|value| value.to_str().ok())
        .expect("server md5")
        .to_string();

    let head = call(
        storage,
        auth_disabled(),
        request(
            "HEAD",
            "http://localhost/n/tenant/b/metadata-oci/o/value",
            &[],
            b"",
        ),
    )
    .await;
    assert_eq!(head.headers().get("content-md5").unwrap(), md5.as_str());
    assert_eq!(head.headers().get("content-language").unwrap(), "en-US");
    assert_eq!(head.headers().get("content-encoding").unwrap(), "identity");
    assert_eq!(head.headers().get("cache-control").unwrap(), "no-cache");
    assert_eq!(
        head.headers().get("content-disposition").unwrap(),
        "attachment"
    );
    assert_eq!(
        head.headers().get("storage-tier").unwrap(),
        "InfrequentAccess"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn should_count_oci_prefixes_toward_limit_and_keep_start_inclusive() {
    // Arrange
    let storage = temp_storage();
    call(
        storage.clone(),
        auth_disabled(),
        request(
            "POST",
            "http://localhost/n/tenant/b",
            &[("content-type", "application/json")],
            br#"{"name":"delimiter-oci","compartmentId":"ignored"}"#,
        ),
    )
    .await;
    for key in ["a/one", "b/one", "z"] {
        call(
            storage.clone(),
            auth_disabled(),
            request(
                "PUT",
                &format!("http://localhost/n/tenant/b/delimiter-oci/o/{key}"),
                &[],
                b"data",
            ),
        )
        .await;
    }

    // Act
    let first: serde_json::Value = serde_json::from_str(
        &body_text(
            call(
                storage.clone(),
                auth_disabled(),
                request(
                    "GET",
                    "http://localhost/n/tenant/b/delimiter-oci/o?delimiter=/&limit=1",
                    &[],
                    b"",
                ),
            )
            .await,
        )
        .await,
    )
    .expect("first list response json");
    let second: serde_json::Value = serde_json::from_str(
        &body_text(
            call(
                storage,
                auth_disabled(),
                request(
                    "GET",
                    "http://localhost/n/tenant/b/delimiter-oci/o?delimiter=/&limit=1&start=b/",
                    &[],
                    b"",
                ),
            )
            .await,
        )
        .await,
    )
    .expect("second list response json");

    // Assert
    assert_eq!(first["prefixes"], serde_json::json!(["a/"]));
    assert_eq!(first["nextStartWith"], "b/");
    assert_eq!(second["prefixes"], serde_json::json!(["b/"]));
    assert_eq!(second["nextStartWith"], "z");
}

#[tokio::test(flavor = "multi_thread")]
async fn should_inherit_and_validate_oci_storage_tiers() {
    // Arrange
    let storage = temp_storage();
    let created = call(
        storage.clone(),
        auth_disabled(),
        request(
            "POST",
            "http://localhost/n/tenant/b",
            &[("content-type", "application/json")],
            br#"{"name":"archive-oci","compartmentId":"ignored","storageTier":"Archive"}"#,
        ),
    )
    .await;
    assert_eq!(created.status(), StatusCode::OK);
    let invalid_bucket = call(
        storage.clone(),
        auth_disabled(),
        request(
            "POST",
            "http://localhost/n/tenant/b",
            &[("content-type", "application/json")],
            br#"{"name":"invalid-tier-oci","compartmentId":"ignored","storageTier":"InfrequentAccess"}"#,
        ),
    )
    .await;

    // Act
    let inherited = call(
        storage.clone(),
        auth_disabled(),
        request(
            "PUT",
            "http://localhost/n/tenant/b/archive-oci/o/value",
            &[],
            b"payload",
        ),
    )
    .await;
    let invalid = call(
        storage.clone(),
        auth_disabled(),
        request(
            "PUT",
            "http://localhost/n/tenant/b/archive-oci/o/invalid",
            &[("storage-tier", "Glacier")],
            b"payload",
        ),
    )
    .await;
    let invalid_for_archive_bucket = call(
        storage.clone(),
        auth_disabled(),
        request(
            "PUT",
            "http://localhost/n/tenant/b/archive-oci/o/invalid-standard",
            &[("storage-tier", "Standard")],
            b"payload",
        ),
    )
    .await;
    let listing: serde_json::Value = serde_json::from_str(
        &body_text(
            call(
                storage.clone(),
                auth_disabled(),
                request(
                    "GET",
                    "http://localhost/n/tenant/b/archive-oci/o?fields=name,storageTier,archivalState",
                    &[],
                    b"",
                ),
            )
            .await,
        )
        .await,
    )
    .expect("list response json");

    // Assert
    assert_eq!(inherited.status(), StatusCode::OK);
    assert_eq!(invalid_bucket.status(), StatusCode::BAD_REQUEST);
    assert!(storage.get_bucket("invalid-tier-oci").is_err());
    assert_eq!(invalid.status(), StatusCode::BAD_REQUEST);
    assert_eq!(invalid_for_archive_bucket.status(), StatusCode::BAD_REQUEST);
    assert!(storage.get_object("archive-oci", "invalid").is_err());
    assert!(storage
        .get_object("archive-oci", "invalid-standard")
        .is_err());
    assert_eq!(listing["objects"][0]["storageTier"], "Archive");
    assert_eq!(listing["objects"][0]["archivalState"], "Archived");
}

#[tokio::test(flavor = "multi_thread")]
async fn should_echo_oci_client_request_id_and_use_http_dates() {
    // Arrange
    let storage = temp_storage();

    // Act
    let response = call(
        storage,
        auth_disabled(),
        request(
            "GET",
            "http://localhost/n/tenant",
            &[("opc-client-request-id", "client-correlation")],
            b"",
        ),
    )
    .await;

    // Assert
    assert_eq!(
        response
            .headers()
            .get("opc-client-request-id")
            .and_then(|value| value.to_str().ok()),
        Some("client-correlation")
    );
    assert!(response
        .headers()
        .get("date")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.ends_with(" GMT")));
}

#[tokio::test(flavor = "multi_thread")]
async fn should_return_oci_resource_specific_missing_errors() {
    // Arrange
    let storage = temp_storage();

    // Act
    let missing_bucket = call(
        storage.clone(),
        auth_disabled(),
        request(
            "GET",
            "http://localhost/n/tenant/b/missing/o",
            &[("opc-client-request-id", "missing-correlation")],
            b"",
        ),
    )
    .await;
    let missing_bucket_status = missing_bucket.status();
    let missing_bucket_request_id = missing_bucket
        .headers()
        .get("opc-client-request-id")
        .and_then(|value| value.to_str().ok())
        .map(str::to_string);
    let missing_bucket: serde_json::Value =
        serde_json::from_str(&body_text(missing_bucket).await).expect("missing bucket error json");

    call(
        storage.clone(),
        auth_disabled(),
        request(
            "POST",
            "http://localhost/n/tenant/b",
            &[("content-type", "application/json")],
            br#"{"name":"present","compartmentId":"ignored"}"#,
        ),
    )
    .await;
    let missing_object = call(
        storage,
        auth_disabled(),
        request(
            "GET",
            "http://localhost/n/tenant/b/present/o/missing",
            &[],
            b"",
        ),
    )
    .await;
    let missing_object_status = missing_object.status();
    let missing_object: serde_json::Value =
        serde_json::from_str(&body_text(missing_object).await).expect("missing object error json");

    // Assert
    assert_eq!(missing_bucket_status, StatusCode::NOT_FOUND);
    assert_eq!(missing_bucket["code"], "BucketNotFound");
    assert_eq!(
        missing_bucket_request_id.as_deref(),
        Some("missing-correlation")
    );
    assert_eq!(missing_object_status, StatusCode::NOT_FOUND);
    assert_eq!(missing_object["code"], "ObjectNotFound");
}
