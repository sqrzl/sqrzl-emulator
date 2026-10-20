mod common;

use bytes::Bytes;
use common::e2e::{auth_disabled, auth_enabled, text_body, LiveServer};
use http_body_util::Full;
use hyper::{body::Incoming, Request, Response, StatusCode};
use serde_json::{json, Value};

type Body = Full<Bytes>;

async fn post_mcp(
    server: &LiveServer,
    body: Value,
    headers: &[(&str, &str)],
) -> Response<Incoming> {
    let mut request = Request::builder()
        .method("POST")
        .uri(format!("{}/mcp", server.base_url))
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream");
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    server
        .request_without_default_auth(
            request
                .body(Body::from(body.to_string()))
                .expect("MCP request should build"),
        )
        .await
}

async fn json_payload(response: Response<Incoming>) -> Value {
    let body = text_body(response).await;
    if let Ok(value) = serde_json::from_str(&body) {
        return value;
    }
    for line in body.lines() {
        if let Some(payload) = line
            .strip_prefix("data: ")
            .filter(|value| !value.is_empty())
        {
            return serde_json::from_str(payload).expect("SSE data event should be JSON");
        }
    }
    panic!("MCP response should contain a JSON message: {body:?}");
}

async fn call_tool(server: &LiveServer, id: u64, name: &str, arguments: Value) -> Value {
    let response = post_mcp(
        server,
        json!({
            "jsonrpc":"2.0",
            "id":id,
            "method":"tools/call",
            "params":{"_meta":current_meta(),"name":name,"arguments":arguments}
        }),
        &[
            ("mcp-protocol-version", "2026-07-28"),
            ("mcp-method", "tools/call"),
            ("mcp-name", name),
        ],
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let response = json_payload(response).await;
    assert!(
        !response["result"]["isError"].as_bool().unwrap_or(false),
        "{response}"
    );
    response["result"]["structuredContent"].clone()
}

fn current_meta() -> Value {
    json!({
        "io.modelcontextprotocol/protocolVersion": "2026-07-28",
        "io.modelcontextprotocol/clientInfo": {"name":"sqrzl-mcp-e2e","version":"1.0"},
        "io.modelcontextprotocol/clientCapabilities": {}
    })
}

#[tokio::test(flavor = "multi_thread")]
async fn should_enforce_origin_allowlists_for_current_mcp_clients() {
    // Arrange
    let server = LiveServer::start_admin(auth_disabled()).await;
    let origin = server.base_url.clone();
    assert_allowed_mcp_preflight(&server, &origin).await;
    let headers = [
        ("mcp-protocol-version", "2026-07-28"),
        ("mcp-method", "server/discover"),
        ("origin", origin.as_str()),
    ];
    // Act
    let response = post_mcp(
        &server,
        json!({"jsonrpc":"2.0","id":1,"method":"server/discover","params":{"_meta":current_meta()}}),
        &headers,
    )
    .await;
    // Assert
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get("access-control-allow-origin")
            .and_then(|value| value.to_str().ok()),
        Some(origin.as_str())
    );
    let discover = json_payload(response).await;
    assert!(discover["result"].is_object());

    assert_current_tool_surface(&server).await;

    let response = post_mcp(
        &server,
        json!({
            "jsonrpc":"2.0",
            "id":3,
            "method":"tools/call",
            "params":{"_meta":current_meta(),"name":"bucket_create","arguments":{"name":"current-mcp"}}
        }),
        &[
            ("mcp-protocol-version", "2026-07-28"),
            ("mcp-method", "tools/call"),
            ("mcp-name", "bucket_create"),
        ],
    )
    .await;
    if response.status() != StatusCode::OK {
        let status = response.status();
        panic!(
            "current protocol tool call returned {status}: {}",
            text_body(response).await
        );
    }
    let call = json_payload(response).await;
    assert_eq!(call["result"]["structuredContent"]["name"], "current-mcp");

    let rejected = post_mcp(
        &server,
        json!({"jsonrpc":"2.0","id":4,"method":"tools/list","params":{"_meta":current_meta()}}),
        &[
            ("mcp-protocol-version", "2026-07-28"),
            ("mcp-method", "tools/list"),
            ("origin", "https://untrusted.example"),
        ],
    )
    .await;
    assert_eq!(rejected.status(), StatusCode::FORBIDDEN);
}

async fn assert_allowed_mcp_preflight(server: &LiveServer, origin: &str) {
    let response = server
        .request_without_default_auth(
            Request::builder()
                .method("OPTIONS")
                .uri(format!("{}/mcp", server.base_url))
                .header("origin", origin)
                .header("access-control-request-method", "POST")
                .header(
                    "access-control-request-headers",
                    "authorization,content-type,mcp-protocol-version",
                )
                .body(Body::default())
                .expect("MCP CORS preflight should build"),
        )
        .await;
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    assert_eq!(
        response
            .headers()
            .get("access-control-allow-origin")
            .and_then(|value| value.to_str().ok()),
        Some(origin)
    );
}

async fn assert_current_tool_surface(server: &LiveServer) {
    let response = post_mcp(
        server,
        json!({"jsonrpc":"2.0","id":2,"method":"tools/list","params":{"_meta":current_meta()}}),
        &[
            ("mcp-protocol-version", "2026-07-28"),
            ("mcp-method", "tools/list"),
        ],
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let tools = json_payload(response).await;
    let names: Vec<&str> = tools["result"]["tools"]
        .as_array()
        .expect("tools should be listed")
        .iter()
        .filter_map(|tool| tool["name"].as_str())
        .collect();
    for name in [
        "bucket_create",
        "object_content_put",
        "text_inbound_simulate",
        "usage_query",
        "object_copy",
        "object_delete_many",
        "object_version_content_get",
        "multipart_upload_complete",
        "mail_send_simulate",
        "text_send_simulate",
    ] {
        assert!(names.contains(&name), "missing MCP tool {name}");
    }
    assert!(!names.contains(&"repository_task_run"));
}

async fn start_legacy_session(server: &LiveServer) -> String {
    let response = post_mcp(
        server,
        json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"legacy-e2e","version":"1.0"}}}),
        &[],
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let session_id = response
        .headers()
        .get("mcp-session-id")
        .expect("legacy initialization should create a session")
        .to_str()
        .expect("session id should be valid")
        .to_owned();
    let initialized = json_payload(response).await;
    assert_eq!(initialized["result"]["protocolVersion"], "2025-11-25");
    let response = post_legacy_mcp(
        server,
        &session_id,
        json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
    )
    .await;
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    session_id
}

async fn post_legacy_mcp(server: &LiveServer, session_id: &str, body: Value) -> Response<Incoming> {
    post_mcp(
        server,
        body,
        &[
            ("mcp-session-id", session_id),
            ("mcp-protocol-version", "2025-11-25"),
        ],
    )
    .await
}

async fn call_legacy_tool(
    server: &LiveServer,
    session_id: &str,
    id: u64,
    name: &str,
    arguments: Value,
) -> Value {
    let response = post_mcp(
        server,
        json!({"jsonrpc":"2.0","id":id,"method":"tools/call","params":{"name":name,"arguments":arguments}}),
        &[
            ("mcp-session-id", session_id),
            ("mcp-protocol-version", "2025-11-25"),
            ("mcp-method", "tools/call"),
            ("mcp-name", name),
        ],
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    json_payload(response).await
}

#[tokio::test(flavor = "multi_thread")]
async fn should_serve_legacy_protocol_resources() {
    // Arrange
    let server = LiveServer::start_admin(auth_disabled()).await;
    let session_id = start_legacy_session(&server).await;

    // Act
    let tools = post_legacy_mcp(
        &server,
        &session_id,
        json!({"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}),
    )
    .await;
    let resources = post_legacy_mcp(
        &server,
        &session_id,
        json!({"jsonrpc":"2.0","id":3,"method":"resources/list","params":{}}),
    )
    .await;
    let health = post_legacy_mcp(
        &server,
        &session_id,
        json!({"jsonrpc":"2.0","id":4,"method":"resources/read","params":{"uri":"sqrzl://health"}}),
    )
    .await;

    // Assert
    assert_eq!(tools.status(), StatusCode::OK);
    assert!(json_payload(tools).await["result"]["tools"]
        .as_array()
        .expect("tools should be listed")
        .iter()
        .any(|tool| tool["name"] == "bucket_create"));
    assert_eq!(resources.status(), StatusCode::OK);
    assert!(json_payload(resources).await["result"]["resources"]
        .as_array()
        .expect("resources should be listed")
        .iter()
        .any(|resource| resource["uri"] == "sqrzl://health"));
    assert_eq!(health.status(), StatusCode::OK);
    assert!(json_payload(health).await["result"]["contents"][0]["text"]
        .as_str()
        .is_some_and(|text| text.contains("storage_ready")));
}

#[tokio::test(flavor = "multi_thread")]
async fn should_share_legacy_state_with_usage_metrics() {
    // Arrange
    let server = LiveServer::start_admin(auth_disabled()).await;
    let session_id = start_legacy_session(&server).await;

    // Act
    let created = call_legacy_tool(
        &server,
        &session_id,
        5,
        "bucket_create",
        json!({"name":"mcp-e2e"}),
    )
    .await;
    let put = call_legacy_tool(
        &server,
        &session_id,
        6,
        "object_content_put",
        json!({"bucket":"mcp-e2e","key":"shared.txt","content_base64":"c2hhcmVkLWZyb20tbWNw","content_type":"text/plain"}),
    )
    .await;
    let admin_read = server
        .request_without_default_auth(
            Request::builder()
                .method("GET")
                .uri(format!(
                    "{}/admin/v1/buckets/mcp-e2e/objects/shared.txt/content",
                    server.base_url
                ))
                .body(Body::default())
                .expect("admin object content request should build"),
        )
        .await;
    let usage = post_legacy_mcp(
        &server,
        &session_id,
        json!({"jsonrpc":"2.0","id":8,"method":"resources/read","params":{"uri":"sqrzl://usage?min_requests=1&min_bytes=0"}}),
    )
    .await;
    let usage_query = call_legacy_tool(
        &server,
        &session_id,
        16,
        "usage_query",
        json!({"min_requests":1}),
    )
    .await;
    let failure = call_legacy_tool(
        &server,
        &session_id,
        9,
        "bucket_get",
        json!({"bucket":"missing-mcp-bucket"}),
    )
    .await;
    let admin_buckets = server
        .request_without_default_auth(
            Request::builder()
                .method("GET")
                .uri(format!("{}/admin/v1/buckets", server.base_url))
                .body(Body::default())
                .expect("admin list request should build"),
        )
        .await;

    // Assert
    assert_eq!(created["result"]["structuredContent"]["name"], "mcp-e2e");
    assert!(!put["result"]["isError"].as_bool().unwrap_or(false));
    assert_eq!(admin_read.status(), StatusCode::OK);
    assert_eq!(text_body(admin_read).await, "shared-from-mcp");
    assert_eq!(usage.status(), StatusCode::OK);
    assert!(json_payload(usage).await["result"]["contents"][0]["text"]
        .as_str()
        .is_some_and(|text| text.contains("bucket_create")));
    assert!(usage_query["result"]["structuredContent"].is_object());
    assert!(usage_query["result"]["structuredContent"]["items"].is_array());
    assert_eq!(failure["result"]["isError"], true);
    assert!(text_body(admin_buckets).await.contains("mcp-e2e"));
    assert_prometheus_metrics(&server).await;
}

async fn assert_prometheus_metrics(server: &LiveServer) {
    let response = server
        .request_without_default_auth(
            Request::builder()
                .method("GET")
                .uri(format!("{}/metrics?min_requests=1", server.base_url))
                .body(Body::default())
                .expect("metrics request should build"),
        )
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert!(response
        .headers()
        .get("content-type")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.starts_with("text/plain; version=0.0.4")));
    let metrics = text_body(response).await;
    assert!(metrics.contains("# TYPE sqrzl_requests_total counter"));
    assert!(metrics.contains("provider=\"mcp\""));
    assert!(metrics.contains("operation=\"bucket_create\""));
    assert!(metrics.contains("sqrzl_request_size_bytes_bucket"));
}

#[tokio::test(flavor = "multi_thread")]
async fn should_advertise_oauth_metadata() {
    // Arrange
    let server = LiveServer::start_admin(auth_enabled("oauth-user", "oauth-secret")).await;
    let metadata = server
        .request_without_default_auth(
            Request::builder()
                .method("GET")
                .uri(format!(
                    "{}/.well-known/oauth-protected-resource/mcp",
                    server.base_url
                ))
                .body(Body::default())
                .expect("resource metadata request should build"),
        )
        .await;
    assert_eq!(metadata.status(), StatusCode::OK);
    let metadata: Value =
        serde_json::from_str(&text_body(metadata).await).expect("resource metadata should be JSON");
    assert_eq!(metadata["resource"], format!("{}/mcp", server.base_url));
    assert_eq!(metadata["scopes_supported"][0], "sqrzl:read");
}

#[tokio::test(flavor = "multi_thread")]
async fn should_reject_mcp_initialization_without_a_bearer_token() {
    // Arrange
    let server = LiveServer::start_admin(auth_enabled("oauth-user", "oauth-secret")).await;

    // Act
    let response = post_mcp(
        &server,
        json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"unauthenticated","version":"1.0"}}}),
        &[],
    )
    .await;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert!(response.headers().contains_key("www-authenticate"));
}

#[tokio::test(flavor = "multi_thread")]
async fn should_route_domain_operations_over_mcp() {
    // Arrange
    let server = LiveServer::start_admin(auth_disabled()).await;

    // Act
    let created = call_tool(&server, 1, "bucket_create", json!({"name":"mcp-domain"})).await;
    assert_eq!(created["name"], "mcp-domain");
    let put = call_tool(
        &server,
        2,
        "object_content_put",
        json!({"bucket":"mcp-domain","key":"source.json","content_base64":"eyJ2YWx1ZSI6MX0=","content_type":"application/json"}),
    )
    .await;
    assert_eq!(put["key"], "source.json");
    assert_versioned_content_read(&server).await;
    call_tool(
        &server,
        3,
        "object_copy",
        json!({"source_bucket":"mcp-domain","source_key":"source.json","destination_bucket":"mcp-domain","destination_key":"copy.json"}),
    )
    .await;
    let copied = call_tool(
        &server,
        4,
        "object_content_get",
        json!({"bucket":"mcp-domain","key":"copy.json"}),
    )
    .await;
    assert_eq!(copied["content_base64"], "eyJ2YWx1ZSI6MX0=");

    assert_multipart_round_trip(&server).await;

    let mail = call_tool(
        &server,
        9,
        "mail_send_simulate",
        json!({"message":{"source_protocol":"smtp","from":{"email":"sender@example.com"},"to":[{"email":"recipient@example.com"}],"subject":"MCP test","body_text":"hello"}}),
    )
    .await;
    assert_eq!(mail["status"], 202);
    assert!(mail["message_id"].as_str().is_some());
    let text = call_tool(
        &server,
        10,
        "text_send_simulate",
        json!({"provider":"twilio","from":"+14155550100","to":"+14155550101","body":"hello"}),
    )
    .await;
    assert_eq!(text["status"], 202);
    assert_eq!(text["message"]["direction"], "outbound");

    let deleted = call_tool(
        &server,
        11,
        "object_delete_many",
        json!({"bucket":"mcp-domain","keys":["copy.json","multipart.txt"]}),
    )
    .await;
    // Assert
    assert_eq!(deleted["deleted"].as_array().map(Vec::len), Some(2));
    assert_eq!(deleted["errors"].as_array().map(Vec::len), Some(0));
}

async fn assert_versioned_content_read(server: &LiveServer) {
    call_tool(
        server,
        12,
        "bucket_versioning_set",
        json!({"bucket":"mcp-domain","enabled":true}),
    )
    .await;
    let first_version = call_tool(
        server,
        13,
        "object_content_put",
        json!({"bucket":"mcp-domain","key":"versioned.txt","content_base64":"djE=","content_type":"text/plain"}),
    )
    .await;
    call_tool(
        server,
        14,
        "object_content_put",
        json!({"bucket":"mcp-domain","key":"versioned.txt","content_base64":"djI=","content_type":"text/plain"}),
    )
    .await;
    let version = call_tool(
        server,
        15,
        "object_version_content_get",
        json!({"bucket":"mcp-domain","key":"versioned.txt","version_id":first_version["version_id"]}),
    )
    .await;
    assert_eq!(version["content_base64"], "djE=");
}

async fn assert_multipart_round_trip(server: &LiveServer) {
    let upload = call_tool(
        server,
        5,
        "multipart_upload_create",
        json!({"bucket":"mcp-domain","key":"multipart.txt"}),
    )
    .await;
    let upload_id = upload["upload"]["upload_id"]
        .as_str()
        .expect("multipart upload id should be returned");
    let part = call_tool(
        server,
        6,
        "multipart_part_upload",
        json!({"bucket":"mcp-domain","upload_id":upload_id,"part_number":1,"content_base64":"bXVsdGlwYXJ0"}),
    )
    .await;
    let completed = call_tool(
        server,
        7,
        "multipart_upload_complete",
        json!({"bucket":"mcp-domain","upload_id":upload_id,"parts":[{"part_number":1,"etag":part["etag"]}]}),
    )
    .await;
    assert_eq!(completed["status"], 200);
    let object = call_tool(
        server,
        8,
        "object_content_get",
        json!({"bucket":"mcp-domain","key":"multipart.txt"}),
    )
    .await;
    assert_eq!(object["content_base64"], "bXVsdGlwYXJ0");
}
