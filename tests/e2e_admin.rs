mod common;

use bytes::Bytes;
use common::e2e::{
    auth_disabled, auth_enabled, auth_enabled_with_admin_bypass, text_body, LiveServer,
};
use http_body_util::Full;
type Body = Full<Bytes>;
use hyper::{body::Incoming, Request, Response, StatusCode};
use serde::de::DeserializeOwned;
use serde::Deserialize;

#[derive(Debug, Deserialize)]
struct BucketDetails {
    name: String,
    versioning_enabled: bool,
}

#[derive(Debug, Deserialize)]
struct VersioningStatus {
    enabled: bool,
}

#[derive(Debug, Deserialize)]
struct ObjectMetadata {
    key: String,
    content_type: Option<String>,
    metadata: std::collections::HashMap<String, String>,
}

#[derive(Debug, Deserialize)]
struct TagsResponse {
    tags: std::collections::HashMap<String, String>,
}

#[derive(Debug, Deserialize)]
struct BucketInfo {
    name: String,
}

#[derive(Debug, Deserialize)]
struct ListBucketsResponse {
    items: Vec<BucketInfo>,
    next: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ObjectInfo {
    key: String,
}

#[derive(Debug, Deserialize)]
struct ListObjectsResponse {
    items: Vec<ObjectInfo>,
    next: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ObjectVersionInfo {
    version_id: String,
    is_latest: bool,
}

#[derive(Debug, Deserialize)]
struct ListVersionsResponse {
    items: Vec<ObjectVersionInfo>,
    next: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ErrorResponse {
    error: String,
    code: String,
    details: Option<String>,
}

#[derive(Debug, Deserialize)]
struct AdminSessionResponse {
    mode: String,
    username: Option<String>,
}

async fn json_body<T: DeserializeOwned>(response: Response<Incoming>) -> T {
    let body = text_body(response).await;
    serde_json::from_str(&body).expect("response body should deserialize")
}

#[tokio::test(flavor = "multi_thread")]
async fn should_round_trip_admin_bucket_and_object_given_live_server_when_using_admin_api() {
    let server = LiveServer::start_admin(auth_disabled()).await;

    let create_bucket = Request::builder()
        .method("POST")
        .uri(format!("{}/admin/v1/buckets", server.base_url))
        .header("content-type", "application/json")
        .body(Body::from(r#"{"name":"e2e-admin"}"#))
        .expect("bucket create request should build");
    let create_bucket_response = server.request(create_bucket).await;
    assert_eq!(create_bucket_response.status(), StatusCode::CREATED);
    let created_bucket: BucketDetails = json_body(create_bucket_response).await;
    assert_eq!(created_bucket.name, "e2e-admin");
    assert!(!created_bucket.versioning_enabled);

    let enable_versioning = Request::builder()
        .method("PUT")
        .uri(format!(
            "{}/admin/v1/buckets/e2e-admin/versioning",
            server.base_url
        ))
        .header("content-type", "application/json")
        .body(Body::from(r#"{"enabled":true}"#))
        .expect("versioning request should build");
    let enable_versioning_response = server.request(enable_versioning).await;
    assert_eq!(enable_versioning_response.status(), StatusCode::OK);
    let versioning: VersioningStatus = json_body(enable_versioning_response).await;
    assert!(versioning.enabled);

    let put_object = Request::builder()
        .method("PUT")
        .uri(format!(
            "{}/admin/v1/buckets/e2e-admin/objects/hello.txt/content",
            server.base_url
        ))
        .header("content-type", "text/plain")
        .header("x-amz-meta-owner", "alice")
        .body(Body::from("hello over admin tcp"))
        .expect("object put request should build");
    let put_object_response = server.request(put_object).await;
    assert_eq!(put_object_response.status(), StatusCode::CREATED);
    let uploaded: ObjectMetadata = json_body(put_object_response).await;
    assert_eq!(uploaded.key, "hello.txt");

    let get_metadata = Request::builder()
        .method("GET")
        .uri(format!(
            "{}/admin/v1/buckets/e2e-admin/objects/hello.txt",
            server.base_url
        ))
        .body(Body::default())
        .expect("metadata request should build");
    let get_metadata_response = server.request(get_metadata).await;
    assert_eq!(get_metadata_response.status(), StatusCode::OK);
    let metadata: ObjectMetadata = json_body(get_metadata_response).await;
    assert_eq!(metadata.content_type.as_deref(), Some("text/plain"));
    assert_eq!(metadata.metadata.get("owner"), Some(&"alice".to_string()));

    let put_tags = Request::builder()
        .method("PUT")
        .uri(format!(
            "{}/admin/v1/buckets/e2e-admin/objects/hello.txt/tags",
            server.base_url
        ))
        .header("content-type", "application/json")
        .body(Body::from(r#"{"tags":{"env":"dev"}}"#))
        .expect("tag put request should build");
    let put_tags_response = server.request(put_tags).await;
    assert_eq!(put_tags_response.status(), StatusCode::OK);
    let tags: TagsResponse = json_body(put_tags_response).await;
    assert_eq!(tags.tags.get("env"), Some(&"dev".to_string()));

    let get_object = Request::builder()
        .method("GET")
        .uri(format!(
            "{}/admin/v1/buckets/e2e-admin/objects/hello.txt/content",
            server.base_url
        ))
        .body(Body::default())
        .expect("object get request should build");
    let get_object_response = server.request(get_object).await;
    assert_eq!(get_object_response.status(), StatusCode::OK);
    assert_eq!(text_body(get_object_response).await, "hello over admin tcp");

    let put_nested_object = Request::builder()
        .method("PUT")
        .uri(format!(
            "{}/admin/v1/buckets/e2e-admin/objects/docs%2Freadme.txt/content",
            server.base_url
        ))
        .header("content-type", "text/plain")
        .body(Body::from("nested object"))
        .expect("nested object put request should build");
    let put_nested_object_response = server.request(put_nested_object).await;
    assert_eq!(put_nested_object_response.status(), StatusCode::CREATED);
    let nested_upload: ObjectMetadata = json_body(put_nested_object_response).await;
    assert_eq!(nested_upload.key, "docs/readme.txt");

    let get_nested_metadata = Request::builder()
        .method("GET")
        .uri(format!(
            "{}/admin/v1/buckets/e2e-admin/objects/docs%2Freadme.txt",
            server.base_url
        ))
        .body(Body::default())
        .expect("nested metadata request should build");
    let get_nested_metadata_response = server.request(get_nested_metadata).await;
    assert_eq!(get_nested_metadata_response.status(), StatusCode::OK);
    let nested_metadata: ObjectMetadata = json_body(get_nested_metadata_response).await;
    assert_eq!(nested_metadata.key, "docs/readme.txt");

    let delete_object = Request::builder()
        .method("DELETE")
        .uri(format!(
            "{}/admin/v1/buckets/e2e-admin/objects/hello.txt",
            server.base_url
        ))
        .body(Body::default())
        .expect("object delete request should build");
    let delete_object_response = server.request(delete_object).await;
    assert_eq!(delete_object_response.status(), StatusCode::NO_CONTENT);

    let delete_nested_object = Request::builder()
        .method("DELETE")
        .uri(format!(
            "{}/admin/v1/buckets/e2e-admin/objects/docs%2Freadme.txt",
            server.base_url
        ))
        .body(Body::default())
        .expect("nested object delete request should build");
    let delete_nested_object_response = server.request(delete_nested_object).await;
    assert_eq!(
        delete_nested_object_response.status(),
        StatusCode::NO_CONTENT
    );

    let delete_bucket = Request::builder()
        .method("DELETE")
        .uri(format!("{}/admin/v1/buckets/e2e-admin", server.base_url))
        .body(Body::default())
        .expect("bucket delete request should build");
    let delete_bucket_response = server.request(delete_bucket).await;
    assert_eq!(delete_bucket_response.status(), StatusCode::NO_CONTENT);
}

#[tokio::test(flavor = "multi_thread")]
async fn should_reject_legacy_api_surface_given_live_server_when_mutating_storage() {
    let server = LiveServer::start_admin(auth_disabled()).await;

    let create_bucket = Request::builder()
        .method("POST")
        .uri(format!("{}/api/buckets", server.base_url))
        .header("content-type", "application/json")
        .body(Body::from(r#"{"name":"legacy"}"#))
        .expect("legacy api request should build");
    let create_bucket_response = server.request_without_default_auth(create_bucket).await;
    assert_eq!(create_bucket_response.status(), StatusCode::NOT_FOUND);
    let _ = text_body(create_bucket_response).await;

    let list_buckets = Request::builder()
        .method("GET")
        .uri(format!("{}/admin/v1/buckets", server.base_url))
        .body(Body::default())
        .expect("admin list request should build");
    let list_buckets_response = server.request(list_buckets).await;
    assert_eq!(list_buckets_response.status(), StatusCode::OK);
    let buckets: ListBucketsResponse = json_body(list_buckets_response).await;
    assert!(buckets.items.is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn should_page_and_search_admin_collections_given_live_server_when_listing_resources() {
    let server = LiveServer::start_admin(auth_disabled()).await;

    for bucket in ["alpha", "beta", "gamma"] {
        let create_bucket = Request::builder()
            .method("POST")
            .uri(format!("{}/admin/v1/buckets", server.base_url))
            .header("content-type", "application/json")
            .body(Body::from(format!(r#"{{"name":"{}"}}"#, bucket)))
            .expect("bucket create request should build");
        let response = server.request(create_bucket).await;
        assert_eq!(response.status(), StatusCode::CREATED);
    }

    let list_buckets = Request::builder()
        .method("GET")
        .uri(format!(
            "{}/admin/v1/buckets?limit=1&search=a",
            server.base_url
        ))
        .body(Body::default())
        .expect("bucket list request should build");
    let list_buckets_response = server.request(list_buckets).await;
    assert_eq!(list_buckets_response.status(), StatusCode::OK);
    let buckets: ListBucketsResponse = json_body(list_buckets_response).await;
    assert_eq!(buckets.items.len(), 1);
    assert!(buckets.items[0].name.contains('a'));
    let next = buckets
        .next
        .clone()
        .expect("bucket list should have next token");
    assert!(next.parse::<usize>().is_err());

    let list_buckets_next = Request::builder()
        .method("GET")
        .uri(format!(
            "{}/admin/v1/buckets?limit=1&search=a&next={}",
            server.base_url, next
        ))
        .body(Body::default())
        .expect("bucket list next request should build");
    let list_buckets_next_response = server.request(list_buckets_next).await;
    assert_eq!(list_buckets_next_response.status(), StatusCode::OK);
    let next_buckets: ListBucketsResponse = json_body(list_buckets_next_response).await;
    assert_eq!(next_buckets.items.len(), 1);

    let invalid_next = Request::builder()
        .method("GET")
        .uri(format!(
            "{}/admin/v1/buckets?next=not-a-valid-token",
            server.base_url
        ))
        .body(Body::default())
        .expect("invalid next request should build");
    let invalid_next_response = server.request(invalid_next).await;
    assert_eq!(invalid_next_response.status(), StatusCode::BAD_REQUEST);
    let invalid_next_error: ErrorResponse = json_body(invalid_next_response).await;
    assert_eq!(invalid_next_error.code, "InvalidRequest");
    assert_eq!(invalid_next_error.error, "Invalid request");
    assert_eq!(
        invalid_next_error.details.as_deref(),
        Some("invalid next token")
    );

    let invalid_limit = Request::builder()
        .method("GET")
        .uri(format!("{}/admin/v1/buckets?limit=0", server.base_url))
        .body(Body::default())
        .expect("invalid limit request should build");
    let invalid_limit_response = server.request(invalid_limit).await;
    assert_eq!(invalid_limit_response.status(), StatusCode::BAD_REQUEST);
    let invalid_limit_error: ErrorResponse = json_body(invalid_limit_response).await;
    assert_eq!(invalid_limit_error.code, "InvalidRequest");
    assert_eq!(
        invalid_limit_error.details.as_deref(),
        Some("limit must be between 1 and 500")
    );

    let create_demo_bucket = Request::builder()
        .method("POST")
        .uri(format!("{}/admin/v1/buckets", server.base_url))
        .header("content-type", "application/json")
        .body(Body::from(r#"{"name":"demo"}"#))
        .expect("demo bucket create request should build");
    let response = server.request(create_demo_bucket).await;
    assert_eq!(response.status(), StatusCode::CREATED);

    let enable_versioning = Request::builder()
        .method("PUT")
        .uri(format!(
            "{}/admin/v1/buckets/demo/versioning",
            server.base_url
        ))
        .header("content-type", "application/json")
        .body(Body::from(r#"{"enabled":true}"#))
        .expect("versioning request should build");
    let response = server.request(enable_versioning).await;
    assert_eq!(response.status(), StatusCode::OK);

    for key in ["alpha.txt", "beta.txt", "gamma.bin"] {
        let put_object = Request::builder()
            .method("PUT")
            .uri(format!(
                "{}/admin/v1/buckets/demo/objects/{}/content",
                server.base_url, key
            ))
            .header("content-type", "text/plain")
            .body(Body::from(key.to_string()))
            .expect("object put request should build");
        let response = server.request(put_object).await;
        assert!(matches!(
            response.status(),
            StatusCode::CREATED | StatusCode::OK
        ));
    }

    for body in ["v1", "v2"] {
        let put_version = Request::builder()
            .method("PUT")
            .uri(format!(
                "{}/admin/v1/buckets/demo/objects/versioned.txt/content",
                server.base_url
            ))
            .header("content-type", "text/plain")
            .body(Body::from(body))
            .expect("version put request should build");
        let response = server.request(put_version).await;
        assert!(matches!(
            response.status(),
            StatusCode::CREATED | StatusCode::OK
        ));
    }

    let list_objects = Request::builder()
        .method("GET")
        .uri(format!(
            "{}/admin/v1/buckets/demo/objects?limit=1&search=.txt",
            server.base_url
        ))
        .body(Body::default())
        .expect("object list request should build");
    let list_objects_response = server.request(list_objects).await;
    assert_eq!(list_objects_response.status(), StatusCode::OK);
    let objects: ListObjectsResponse = json_body(list_objects_response).await;
    assert_eq!(objects.items.len(), 1);
    assert!(objects.items[0].key.ends_with(".txt"));
    let next = objects
        .next
        .clone()
        .expect("object list should have next token");
    assert!(next.parse::<usize>().is_err());

    let list_objects_next = Request::builder()
        .method("GET")
        .uri(format!(
            "{}/admin/v1/buckets/demo/objects?limit=1&search=.txt&next={}",
            server.base_url, next
        ))
        .body(Body::default())
        .expect("object list next request should build");
    let list_objects_next_response = server.request(list_objects_next).await;
    assert_eq!(list_objects_next_response.status(), StatusCode::OK);
    let next_objects: ListObjectsResponse = json_body(list_objects_next_response).await;
    assert_eq!(next_objects.items.len(), 1);

    let list_versions = Request::builder()
        .method("GET")
        .uri(format!(
            "{}/admin/v1/buckets/demo/objects/versioned.txt/versions?limit=1&search=versioned",
            server.base_url
        ))
        .body(Body::default())
        .expect("version list request should build");
    let list_versions_response = server.request(list_versions).await;
    assert_eq!(list_versions_response.status(), StatusCode::OK);
    let versions: ListVersionsResponse = json_body(list_versions_response).await;
    assert_eq!(versions.items.len(), 1);
    assert!(!versions.items[0].version_id.is_empty());
    assert!(versions.next.is_some());

    let list_all_versions = Request::builder()
        .method("GET")
        .uri(format!(
            "{}/admin/v1/buckets/demo/objects/versioned.txt/versions?limit=10",
            server.base_url
        ))
        .body(Body::default())
        .expect("full version list request should build");
    let list_all_versions_response = server.request(list_all_versions).await;
    assert_eq!(list_all_versions_response.status(), StatusCode::OK);
    let all_versions: ListVersionsResponse = json_body(list_all_versions_response).await;
    assert!(all_versions.items.len() >= 2);
    assert_eq!(
        all_versions
            .items
            .iter()
            .filter(|version| version.is_latest)
            .count(),
        1
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn should_return_expected_errors_given_invalid_admin_requests_when_using_live_server() {
    let server = LiveServer::start_admin(auth_disabled()).await;

    let create_bucket = Request::builder()
        .method("POST")
        .uri(format!("{}/admin/v1/buckets", server.base_url))
        .header("content-type", "application/json")
        .body(Body::from(r#"{"name":"errors-demo"}"#))
        .expect("bucket create request should build");
    let create_bucket_response = server.request(create_bucket).await;
    assert_eq!(create_bucket_response.status(), StatusCode::CREATED);

    let duplicate_bucket = Request::builder()
        .method("POST")
        .uri(format!("{}/admin/v1/buckets", server.base_url))
        .header("content-type", "application/json")
        .body(Body::from(r#"{"name":"errors-demo"}"#))
        .expect("duplicate bucket request should build");
    let duplicate_bucket_response = server.request(duplicate_bucket).await;
    assert_eq!(duplicate_bucket_response.status(), StatusCode::CONFLICT);
    let duplicate_bucket_error: ErrorResponse = json_body(duplicate_bucket_response).await;
    assert_eq!(duplicate_bucket_error.code, "BucketAlreadyExists");
    assert_eq!(duplicate_bucket_error.error, "Bucket already exists");
    assert!(duplicate_bucket_error.details.is_none());

    let malformed_json = Request::builder()
        .method("POST")
        .uri(format!("{}/admin/v1/buckets", server.base_url))
        .header("content-type", "application/json")
        .body(Body::from("{"))
        .expect("malformed json request should build");
    let malformed_json_response = server.request(malformed_json).await;
    assert_eq!(malformed_json_response.status(), StatusCode::BAD_REQUEST);
    let malformed_json_error: ErrorResponse = json_body(malformed_json_response).await;
    assert_eq!(malformed_json_error.code, "InvalidRequest");
    assert_eq!(malformed_json_error.error, "Invalid request");
    assert!(malformed_json_error.details.is_some());

    let missing_bucket = Request::builder()
        .method("GET")
        .uri(format!(
            "{}/admin/v1/buckets/missing-bucket",
            server.base_url
        ))
        .body(Body::default())
        .expect("missing bucket request should build");
    let missing_bucket_response = server.request(missing_bucket).await;
    assert_eq!(missing_bucket_response.status(), StatusCode::NOT_FOUND);
    let missing_bucket_error: ErrorResponse = json_body(missing_bucket_response).await;
    assert_eq!(missing_bucket_error.code, "NoSuchBucket");
    assert_eq!(missing_bucket_error.error, "Bucket not found");

    let put_object = Request::builder()
        .method("PUT")
        .uri(format!(
            "{}/admin/v1/buckets/errors-demo/objects/hello.txt/content",
            server.base_url
        ))
        .header("content-type", "text/plain")
        .body(Body::from("hello"))
        .expect("object put request should build");
    let put_object_response = server.request(put_object).await;
    assert_eq!(put_object_response.status(), StatusCode::CREATED);

    let delete_non_empty_bucket = Request::builder()
        .method("DELETE")
        .uri(format!("{}/admin/v1/buckets/errors-demo", server.base_url))
        .body(Body::default())
        .expect("non-empty bucket delete request should build");
    let delete_non_empty_bucket_response = server.request(delete_non_empty_bucket).await;
    assert_eq!(
        delete_non_empty_bucket_response.status(),
        StatusCode::CONFLICT
    );
    let delete_non_empty_bucket_error: ErrorResponse =
        json_body(delete_non_empty_bucket_response).await;
    assert_eq!(delete_non_empty_bucket_error.code, "BucketNotEmpty");
    assert_eq!(delete_non_empty_bucket_error.error, "Bucket not empty");

    let missing_object = Request::builder()
        .method("GET")
        .uri(format!(
            "{}/admin/v1/buckets/errors-demo/objects/missing.txt",
            server.base_url
        ))
        .body(Body::default())
        .expect("missing object request should build");
    let missing_object_response = server.request(missing_object).await;
    assert_eq!(missing_object_response.status(), StatusCode::NOT_FOUND);
    let missing_object_error: ErrorResponse = json_body(missing_object_response).await;
    assert_eq!(missing_object_error.code, "NoSuchKey");
    assert_eq!(missing_object_error.error, "Key not found");

    let unsupported_method = Request::builder()
        .method("POST")
        .uri(format!("{}/admin/v1/buckets/errors-demo", server.base_url))
        .body(Body::default())
        .expect("unsupported method request should build");
    let unsupported_method_response = server.request(unsupported_method).await;
    assert_eq!(
        unsupported_method_response.status(),
        StatusCode::METHOD_NOT_ALLOWED
    );
    let unsupported_method_error: ErrorResponse = json_body(unsupported_method_response).await;
    assert_eq!(unsupported_method_error.code, "MethodNotAllowed");
    assert_eq!(unsupported_method_error.error, "Method not allowed");
    assert!(unsupported_method_error.details.is_some());

    let missing_route = Request::builder()
        .method("GET")
        .uri(format!("{}/admin/v1/does-not-exist", server.base_url))
        .body(Body::default())
        .expect("missing route request should build");
    let missing_route_response = server.request(missing_route).await;
    assert_eq!(missing_route_response.status(), StatusCode::NOT_FOUND);
    let missing_route_error: ErrorResponse = json_body(missing_route_response).await;
    assert_eq!(missing_route_error.code, "NotFound");
    assert_eq!(missing_route_error.error, "Route not found");
}

#[tokio::test(flavor = "multi_thread")]
async fn should_require_session_cookie_given_admin_auth_enabled_when_request_has_no_credentials() {
    let server = LiveServer::start_admin(auth_enabled("admin-key", "admin-secret")).await;

    let unauthenticated = Request::builder()
        .method("GET")
        .uri(format!("{}/admin/v1/buckets", server.base_url))
        .body(Body::default())
        .expect("unauthenticated admin request should build");
    let unauthenticated_response = server.request_without_default_auth(unauthenticated).await;
    assert_eq!(unauthenticated_response.status(), StatusCode::UNAUTHORIZED);
    let error: ErrorResponse = json_body(unauthenticated_response).await;
    assert_eq!(error.code, "Unauthorized");

    let invalid_session = Request::builder()
        .method("GET")
        .uri(format!("{}/admin/v1/auth/session", server.base_url))
        .header("cookie", "peas_admin_session=invalid")
        .body(Body::default())
        .expect("invalid cookie request should build");
    let invalid_session_response = server.request_without_default_auth(invalid_session).await;
    assert_eq!(invalid_session_response.status(), StatusCode::UNAUTHORIZED);

    let authenticated = Request::builder()
        .method("GET")
        .uri(format!("{}/admin/v1/buckets", server.base_url))
        .body(Body::default())
        .expect("authenticated admin request should build");
    let authenticated_response = server.request(authenticated).await;
    assert_eq!(authenticated_response.status(), StatusCode::OK);

    let session_request = Request::builder()
        .method("GET")
        .uri(format!("{}/admin/v1/auth/session", server.base_url))
        .body(Body::default())
        .expect("session request should build");
    let session_response = server.request(session_request).await;
    assert_eq!(session_response.status(), StatusCode::OK);
    let session: AdminSessionResponse = json_body(session_response).await;
    assert_eq!(session.mode, "session");
    assert_eq!(session.username.as_deref(), Some("admin-key"));
}

#[tokio::test(flavor = "multi_thread")]
async fn should_allow_admin_requests_without_credentials_given_admin_auth_bypass_override() {
    let server =
        LiveServer::start_admin(auth_enabled_with_admin_bypass("admin-key", "admin-secret")).await;

    let request = Request::builder()
        .method("GET")
        .uri(format!("{}/admin/v1/buckets", server.base_url))
        .body(Body::default())
        .expect("bypass admin request should build");
    let response = server.request_without_default_auth(request).await;
    assert_eq!(response.status(), StatusCode::OK);

    let session_request = Request::builder()
        .method("GET")
        .uri(format!("{}/admin/v1/auth/session", server.base_url))
        .body(Body::default())
        .expect("open session request should build");
    let session_response = server.request_without_default_auth(session_request).await;
    assert_eq!(session_response.status(), StatusCode::OK);
    let session: AdminSessionResponse = json_body(session_response).await;
    assert_eq!(session.mode, "open");
    assert!(session.username.is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn should_issue_admin_session_cookie_given_valid_login_and_authorize_admin_requests() {
    let server = LiveServer::start_admin(auth_enabled("admin-key", "admin-secret")).await;

    let login_request = Request::builder()
        .method("POST")
        .uri(format!("{}/admin/v1/auth/login", server.base_url))
        .header("content-type", "application/json")
        .body(Body::from(
            r#"{"username":"admin-key","password":"admin-secret"}"#,
        ))
        .expect("login request should build");
    let login_response = server.request_without_default_auth(login_request).await;
    assert_eq!(login_response.status(), StatusCode::OK);

    let session_cookie = login_response
        .headers()
        .get("set-cookie")
        .expect("login response should set a cookie")
        .to_str()
        .expect("set-cookie header should be valid utf-8")
        .split(';')
        .next()
        .expect("set-cookie header should contain a cookie value")
        .to_string();

    assert!(session_cookie.contains("peas_admin_session="));

    let authenticated = Request::builder()
        .method("GET")
        .uri(format!("{}/admin/v1/buckets", server.base_url))
        .header("cookie", session_cookie.clone())
        .body(Body::default())
        .expect("authenticated admin request should build");
    let authenticated_response = server.request_without_default_auth(authenticated).await;
    assert_eq!(authenticated_response.status(), StatusCode::OK);

    let session_request = Request::builder()
        .method("GET")
        .uri(format!("{}/admin/v1/auth/session", server.base_url))
        .header("cookie", session_cookie.clone())
        .body(Body::default())
        .expect("cookie session request should build");
    let session_response = server.request_without_default_auth(session_request).await;
    assert_eq!(session_response.status(), StatusCode::OK);
    let session: AdminSessionResponse = json_body(session_response).await;
    assert_eq!(session.mode, "session");
    assert_eq!(session.username.as_deref(), Some("admin-key"));

    let logout_request = Request::builder()
        .method("POST")
        .uri(format!("{}/admin/v1/auth/logout", server.base_url))
        .header("cookie", session_cookie)
        .body(Body::default())
        .expect("logout request should build");
    let logout_response = server.request_without_default_auth(logout_request).await;
    assert_eq!(logout_response.status(), StatusCode::OK);
    let logout_cookie = logout_response
        .headers()
        .get("set-cookie")
        .expect("logout response should clear the cookie")
        .to_str()
        .expect("logout cookie header should be valid utf-8");
    assert!(logout_cookie.contains("Max-Age=0"));

    let signed_out_session = Request::builder()
        .method("GET")
        .uri(format!("{}/admin/v1/auth/session", server.base_url))
        .body(Body::default())
        .expect("signed out session request should build");
    let signed_out_response = server
        .request_without_default_auth(signed_out_session)
        .await;
    assert_eq!(signed_out_response.status(), StatusCode::UNAUTHORIZED);
}
