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

fn admin_empty_request(server: &LiveServer, method: &str, path: &str) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(format!("{}{}", server.base_url, path))
        .body(Body::default())
        .expect("admin request should build")
}

fn admin_json_request(server: &LiveServer, method: &str, path: &str, body: &str) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(format!("{}{}", server.base_url, path))
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .expect("admin json request should build")
}

fn admin_text_request(server: &LiveServer, method: &str, path: &str, body: &str) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(format!("{}{}", server.base_url, path))
        .header("content-type", "text/plain")
        .body(Body::from(body.to_string()))
        .expect("admin text request should build")
}

#[tokio::test(flavor = "multi_thread")]
async fn should_report_health_given_live_server_when_using_admin_port() {
    let server = LiveServer::start_admin(auth_disabled()).await;

    let request = Request::builder()
        .method("GET")
        .uri(format!("{}/healthz", server.base_url))
        .body(Body::default())
        .expect("health request should build");
    let response = server.request_without_default_auth(request).await;

    assert_eq!(response.status(), StatusCode::OK);
    let body = text_body(response).await;
    assert!(body.contains(r#""status":"ok""#));
    assert!(body.contains(r#""storage_ready":true"#));
    assert!(body.contains("azure-blob"));
}

#[tokio::test(flavor = "multi_thread")]
async fn should_round_trip_admin_bucket_and_object_given_live_server_when_using_admin_api() {
    let server = LiveServer::start_admin(auth_disabled()).await;

    create_admin_bucket_and_enable_versioning(&server).await;
    verify_admin_object_metadata_tags_and_content(&server).await;
    verify_nested_admin_object(&server).await;
    delete_admin_round_trip_resources(&server).await;
}

async fn create_admin_bucket_and_enable_versioning(server: &LiveServer) {
    let response = server
        .request(admin_json_request(
            server,
            "POST",
            "/admin/v1/buckets",
            r#"{"name":"e2e-admin"}"#,
        ))
        .await;
    assert_eq!(response.status(), StatusCode::CREATED);
    let created: BucketDetails = json_body(response).await;
    assert_eq!(created.name, "e2e-admin");
    assert!(!created.versioning_enabled);

    let response = server
        .request(admin_json_request(
            server,
            "PUT",
            "/admin/v1/buckets/e2e-admin/versioning",
            r#"{"enabled":true}"#,
        ))
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    let versioning: VersioningStatus = json_body(response).await;
    assert!(versioning.enabled);
}

async fn verify_admin_object_metadata_tags_and_content(server: &LiveServer) {
    let request = Request::builder()
        .method("PUT")
        .uri(format!(
            "{}/admin/v1/buckets/e2e-admin/objects/hello.txt/content",
            server.base_url
        ))
        .header("content-type", "text/plain")
        .header("x-amz-meta-owner", "alice")
        .body(Body::from("hello over admin tcp"))
        .expect("object put request should build");
    let response = server.request(request).await;
    assert_eq!(response.status(), StatusCode::CREATED);
    let uploaded: ObjectMetadata = json_body(response).await;
    assert_eq!(uploaded.key, "hello.txt");

    let response = server
        .request(admin_empty_request(
            server,
            "GET",
            "/admin/v1/buckets/e2e-admin/objects/hello.txt",
        ))
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    let metadata: ObjectMetadata = json_body(response).await;
    assert_eq!(metadata.content_type.as_deref(), Some("text/plain"));
    assert_eq!(metadata.metadata.get("owner"), Some(&"alice".to_string()));

    let response = server
        .request(admin_json_request(
            server,
            "PUT",
            "/admin/v1/buckets/e2e-admin/objects/hello.txt/tags",
            r#"{"tags":{"env":"dev"}}"#,
        ))
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    let tags: TagsResponse = json_body(response).await;
    assert_eq!(tags.tags.get("env"), Some(&"dev".to_string()));

    let response = server
        .request(admin_empty_request(
            server,
            "GET",
            "/admin/v1/buckets/e2e-admin/objects/hello.txt/content",
        ))
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(text_body(response).await, "hello over admin tcp");
}

async fn verify_nested_admin_object(server: &LiveServer) {
    let response = server
        .request(admin_text_request(
            server,
            "PUT",
            "/admin/v1/buckets/e2e-admin/objects/docs%2Freadme.txt/content",
            "nested object",
        ))
        .await;
    assert_eq!(response.status(), StatusCode::CREATED);
    let nested_upload: ObjectMetadata = json_body(response).await;
    assert_eq!(nested_upload.key, "docs/readme.txt");

    let response = server
        .request(admin_empty_request(
            server,
            "GET",
            "/admin/v1/buckets/e2e-admin/objects/docs%2Freadme.txt",
        ))
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    let nested_metadata: ObjectMetadata = json_body(response).await;
    assert_eq!(nested_metadata.key, "docs/readme.txt");
}

async fn delete_admin_round_trip_resources(server: &LiveServer) {
    for path in [
        "/admin/v1/buckets/e2e-admin/objects/hello.txt",
        "/admin/v1/buckets/e2e-admin/objects/docs%2Freadme.txt",
        "/admin/v1/buckets/e2e-admin",
    ] {
        let response = server
            .request(admin_empty_request(server, "DELETE", path))
            .await;
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
    }
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

    create_admin_search_buckets(&server).await;
    verify_admin_bucket_pagination(&server).await;
    create_admin_demo_objects(&server).await;
    verify_admin_object_pagination(&server).await;
    verify_admin_version_pagination(&server).await;
}

async fn create_admin_search_buckets(server: &LiveServer) {
    for bucket in ["alpha", "beta", "gamma"] {
        let response = server
            .request(admin_json_request(
                server,
                "POST",
                "/admin/v1/buckets",
                &format!(r#"{{"name":"{bucket}"}}"#),
            ))
            .await;
        assert_eq!(response.status(), StatusCode::CREATED);
    }
}

async fn verify_admin_bucket_pagination(server: &LiveServer) {
    let list_buckets_response = server
        .request(admin_empty_request(
            server,
            "GET",
            "/admin/v1/buckets?limit=1&search=a",
        ))
        .await;
    assert_eq!(list_buckets_response.status(), StatusCode::OK);
    let buckets: ListBucketsResponse = json_body(list_buckets_response).await;
    assert_eq!(buckets.items.len(), 1);
    assert!(buckets.items[0].name.contains('a'));
    let next = buckets
        .next
        .clone()
        .expect("bucket list should have next token");
    assert!(next.parse::<usize>().is_err());

    let path = format!("/admin/v1/buckets?limit=1&search=a&next={next}");
    let list_buckets_next_response = server
        .request(admin_empty_request(server, "GET", &path))
        .await;
    assert_eq!(list_buckets_next_response.status(), StatusCode::OK);
    let next_buckets: ListBucketsResponse = json_body(list_buckets_next_response).await;
    assert_eq!(next_buckets.items.len(), 1);

    let invalid_next_response = server
        .request(admin_empty_request(
            server,
            "GET",
            "/admin/v1/buckets?next=not-a-valid-token",
        ))
        .await;
    assert_eq!(invalid_next_response.status(), StatusCode::BAD_REQUEST);
    let invalid_next_error: ErrorResponse = json_body(invalid_next_response).await;
    assert_eq!(invalid_next_error.code, "InvalidRequest");
    assert_eq!(invalid_next_error.error, "Invalid request");
    assert_eq!(
        invalid_next_error.details.as_deref(),
        Some("invalid next token")
    );

    let invalid_limit_response = server
        .request(admin_empty_request(
            server,
            "GET",
            "/admin/v1/buckets?limit=0",
        ))
        .await;
    assert_eq!(invalid_limit_response.status(), StatusCode::BAD_REQUEST);
    let invalid_limit_error: ErrorResponse = json_body(invalid_limit_response).await;
    assert_eq!(invalid_limit_error.code, "InvalidRequest");
    assert_eq!(
        invalid_limit_error.details.as_deref(),
        Some("limit must be between 1 and 500")
    );
}

async fn create_admin_demo_objects(server: &LiveServer) {
    let response = server
        .request(admin_json_request(
            server,
            "POST",
            "/admin/v1/buckets",
            r#"{"name":"demo"}"#,
        ))
        .await;
    assert_eq!(response.status(), StatusCode::CREATED);

    let response = server
        .request(admin_json_request(
            server,
            "PUT",
            "/admin/v1/buckets/demo/versioning",
            r#"{"enabled":true}"#,
        ))
        .await;
    assert_eq!(response.status(), StatusCode::OK);

    for key in ["alpha.txt", "beta.txt", "gamma.bin"] {
        let path = format!("/admin/v1/buckets/demo/objects/{key}/content");
        let response = server
            .request(admin_text_request(server, "PUT", &path, key))
            .await;
        assert!(matches!(
            response.status(),
            StatusCode::CREATED | StatusCode::OK
        ));
    }

    for body in ["v1", "v2"] {
        let response = server
            .request(admin_text_request(
                server,
                "PUT",
                "/admin/v1/buckets/demo/objects/versioned.txt/content",
                body,
            ))
            .await;
        assert!(matches!(
            response.status(),
            StatusCode::CREATED | StatusCode::OK
        ));
    }
}

async fn verify_admin_object_pagination(server: &LiveServer) {
    let list_objects_response = server
        .request(admin_empty_request(
            server,
            "GET",
            "/admin/v1/buckets/demo/objects?limit=1&search=.txt",
        ))
        .await;
    assert_eq!(list_objects_response.status(), StatusCode::OK);
    let objects: ListObjectsResponse = json_body(list_objects_response).await;
    assert_eq!(objects.items.len(), 1);
    assert!(std::path::Path::new(&objects.items[0].key)
        .extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("txt")));
    let next = objects
        .next
        .clone()
        .expect("object list should have next token");
    assert!(next.parse::<usize>().is_err());

    let path = format!("/admin/v1/buckets/demo/objects?limit=1&search=.txt&next={next}");
    let list_objects_next_response = server
        .request(admin_empty_request(server, "GET", &path))
        .await;
    assert_eq!(list_objects_next_response.status(), StatusCode::OK);
    let next_objects: ListObjectsResponse = json_body(list_objects_next_response).await;
    assert_eq!(next_objects.items.len(), 1);
}

async fn verify_admin_version_pagination(server: &LiveServer) {
    let list_versions_response = server
        .request(admin_empty_request(
            server,
            "GET",
            "/admin/v1/buckets/demo/objects/versioned.txt/versions?limit=1&search=versioned",
        ))
        .await;
    assert_eq!(list_versions_response.status(), StatusCode::OK);
    let versions: ListVersionsResponse = json_body(list_versions_response).await;
    assert_eq!(versions.items.len(), 1);
    assert!(!versions.items[0].version_id.is_empty());
    assert!(versions.next.is_some());

    let list_all_versions_response = server
        .request(admin_empty_request(
            server,
            "GET",
            "/admin/v1/buckets/demo/objects/versioned.txt/versions?limit=10",
        ))
        .await;
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

    create_admin_error_bucket(&server).await;
    verify_admin_bucket_errors(&server).await;
    verify_admin_object_errors(&server).await;
    verify_admin_route_errors(&server).await;
}

async fn create_admin_error_bucket(server: &LiveServer) {
    let response = server
        .request(admin_json_request(
            server,
            "POST",
            "/admin/v1/buckets",
            r#"{"name":"errors-demo"}"#,
        ))
        .await;
    assert_eq!(response.status(), StatusCode::CREATED);
}

async fn verify_admin_bucket_errors(server: &LiveServer) {
    let response = server
        .request(admin_json_request(
            server,
            "POST",
            "/admin/v1/buckets",
            r#"{"name":"errors-demo"}"#,
        ))
        .await;
    assert_eq!(response.status(), StatusCode::CONFLICT);
    let error: ErrorResponse = json_body(response).await;
    assert_eq!(error.code, "BucketAlreadyExists");
    assert_eq!(error.error, "Bucket already exists");
    assert!(error.details.is_none());

    let response = server
        .request(admin_json_request(server, "POST", "/admin/v1/buckets", "{"))
        .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let error: ErrorResponse = json_body(response).await;
    assert_eq!(error.code, "InvalidRequest");
    assert_eq!(error.error, "Invalid request");
    assert!(error.details.is_some());

    let response = server
        .request(admin_empty_request(
            server,
            "GET",
            "/admin/v1/buckets/missing-bucket",
        ))
        .await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let error: ErrorResponse = json_body(response).await;
    assert_eq!(error.code, "NoSuchBucket");
    assert_eq!(error.error, "Bucket not found");
}

async fn verify_admin_object_errors(server: &LiveServer) {
    let response = server
        .request(admin_text_request(
            server,
            "PUT",
            "/admin/v1/buckets/errors-demo/objects/hello.txt/content",
            "hello",
        ))
        .await;
    assert_eq!(response.status(), StatusCode::CREATED);

    let response = server
        .request(admin_empty_request(
            server,
            "DELETE",
            "/admin/v1/buckets/errors-demo",
        ))
        .await;
    assert_eq!(response.status(), StatusCode::CONFLICT);
    let error: ErrorResponse = json_body(response).await;
    assert_eq!(error.code, "BucketNotEmpty");
    assert_eq!(error.error, "Bucket not empty");

    let response = server
        .request(admin_empty_request(
            server,
            "GET",
            "/admin/v1/buckets/errors-demo/objects/missing.txt",
        ))
        .await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let error: ErrorResponse = json_body(response).await;
    assert_eq!(error.code, "NoSuchKey");
    assert_eq!(error.error, "Key not found");
}

async fn verify_admin_route_errors(server: &LiveServer) {
    let response = server
        .request(admin_empty_request(
            server,
            "POST",
            "/admin/v1/buckets/errors-demo",
        ))
        .await;
    assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
    let error: ErrorResponse = json_body(response).await;
    assert_eq!(error.code, "MethodNotAllowed");
    assert_eq!(error.error, "Method not allowed");
    assert!(error.details.is_some());

    let response = server
        .request(admin_empty_request(
            server,
            "GET",
            "/admin/v1/does-not-exist",
        ))
        .await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let error: ErrorResponse = json_body(response).await;
    assert_eq!(error.code, "NotFound");
    assert_eq!(error.error, "Route not found");
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
        .header("cookie", "sqrzl_admin_session=invalid")
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

    assert!(session_cookie.contains("sqrzl_admin_session="));

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
