//! Standard Model Context Protocol tools and resources over Streamable HTTP.

use crate::{
    auth::mcp::McpPrincipal,
    body::Body,
    config::{Config, McpConfig},
    mail::MailStore,
    metrics::UsageMetrics,
    sms::SmsStore,
    storage::Storage,
};
use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use http_body_util::BodyExt;
use hyper::{Method, Request, Response};
use rmcp::{
    model::{
        CacheScope, CallToolRequestParams, CallToolResult, Implementation, ListResourcesResult,
        ListToolsResult, ReadResourceRequestParams, ReadResourceResult, Resource, ResourceContents,
        ServerCapabilities, ServerConfig, Tool, ToolAnnotations,
    },
    service::{RequestContext, RoleServer},
    ErrorData as McpError, ServerHandler,
};
use serde_json::{json, Map, Value};
use std::{collections::BTreeSet, path::Path, sync::Arc, time::Duration};
use tokio::{io::AsyncReadExt, process::Command};

const MAX_TASK_OUTPUT_BYTES: usize = 16 * 1024;
const TASK_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone)]
pub struct McpState {
    pub storage: Arc<dyn Storage>,
    pub mail: Arc<dyn MailStore>,
    pub sms: Arc<dyn SmsStore>,
    pub config: Arc<Config>,
    pub metrics: Arc<UsageMetrics>,
    pub mcp_config: Arc<McpConfig>,
}

#[derive(Clone)]
pub struct McpHandler {
    state: McpState,
}

impl McpHandler {
    #[must_use]
    pub fn new(state: McpState) -> Self {
        Self { state }
    }
}

impl ServerHandler for McpHandler {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().enable_resources().build())
            .with_server_info(Implementation::new("sqrzl-emulator", env!("CARGO_PKG_VERSION")))
            .with_instructions("Use the Sqrzl tools to inspect and manage emulator storage, captured mail, and simulated text messages. Binary data is transferred as base64.")
    }

    #[allow(clippy::unused_async_trait_impl)]
    async fn list_tools(
        &self,
        _request: Option<rmcp::model::PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        Ok(ListToolsResult::with_all_items(
            tool_specs(&self.state.mcp_config.repository_tasks)
                .into_iter()
                .map(|spec| spec.to_tool())
                .collect(),
        ))
    }

    fn get_tool(&self, name: &str) -> Option<Tool> {
        tool_specs(&self.state.mcp_config.repository_tasks)
            .into_iter()
            .find(|spec| spec.name == name)
            .map(|spec| spec.to_tool())
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<rmcp::model::CallToolResponse, McpError> {
        let Some(spec) = tool_specs(&self.state.mcp_config.repository_tasks)
            .into_iter()
            .find(|spec| spec.name == request.name.as_ref())
        else {
            return Err(McpError::method_not_found::<
                rmcp::model::CallToolRequestMethod,
            >());
        };
        let args: Value = request.arguments.map_or_else(|| json!({}), Value::Object);
        if !has_scope(&context, spec.scope, self.state.config.mcp_auth_enforced()) {
            return Ok(
                CallToolResult::error(vec![rmcp::model::ContentBlock::text(format!(
                    "insufficient scope; requires {}",
                    spec.scope
                ))])
                .into(),
            );
        }
        let result = match spec.mode {
            ToolMode::Usage => Ok(self.read_usage(&args)),
            ToolMode::RepositoryTask => self.run_repository_task(&args).await,
            ToolMode::Domain => self.run_domain_tool(spec.name, &args).await,
            ToolMode::Http => self.call_shared_api(&spec, &args).await,
        };
        match result {
            Ok((value, response_bytes)) => {
                let bucket = args.get("bucket").and_then(Value::as_str);
                let list_items = value
                    .get("items")
                    .and_then(Value::as_array)
                    .map_or(0, |items| items.len() as u64);
                let (client, request_bytes) = principal(&context).map_or_else(
                    || ("unknown".to_string(), 0),
                    |value| (value.client.clone(), value.request_bytes),
                );
                self.state.metrics.record_mcp_tool(
                    spec.name,
                    bucket,
                    &client,
                    request_bytes,
                    response_bytes,
                    list_items,
                );
                if value
                    .get("status")
                    .and_then(Value::as_u64)
                    .is_some_and(|status| status >= 400)
                {
                    Ok(CallToolResult::error(vec![rmcp::model::ContentBlock::text(
                        value.to_string(),
                    )])
                    .into())
                } else {
                    Ok(CallToolResult::structured(value).into())
                }
            }
            Err(error) => {
                Ok(CallToolResult::error(vec![rmcp::model::ContentBlock::text(error)]).into())
            }
        }
    }

    #[allow(clippy::unused_async_trait_impl)]
    async fn list_resources(
        &self,
        _request: Option<rmcp::model::PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListResourcesResult, McpError> {
        Ok(ListResourcesResult::with_all_items(vec![
            Resource::new("sqrzl://health", "health").with_title("Sqrzl health").with_description("Read the emulator health status.").with_mime_type("application/json"),
            Resource::new("sqrzl://usage", "usage").with_title("Sqrzl usage").with_description("Read passive provider, bucket, client, operation, byte, and list counters. Add min_requests and min_bytes query parameters to choose thresholds.").with_mime_type("application/json"),
        ]))
    }

    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<rmcp::model::ReadResourceResponse, McpError> {
        if !has_scope(
            &context,
            "sqrzl:read",
            self.state.config.mcp_auth_enforced(),
        ) {
            return Err(McpError::invalid_params(
                "insufficient scope; requires sqrzl:read",
                None,
            ));
        }
        let contents = match request.uri.as_str() {
            "sqrzl://health" => {
                let response = crate::health::response(
                    self.state.storage.as_ref(),
                    self.state.config.as_ref(),
                );
                let bytes = response
                    .into_body()
                    .collect()
                    .await
                    .map_err(|error| McpError::internal_error(error.to_string(), None))?
                    .to_bytes();
                let value = serde_json::from_slice::<Value>(&bytes)
                    .unwrap_or_else(|_| json!({"status":"unknown"}));
                ResourceContents::text(value.to_string(), request.uri.clone())
                    .with_mime_type("application/json")
            }
            uri if uri.starts_with("sqrzl://usage") => {
                let (min_requests, min_bytes) = parse_usage_thresholds(uri);
                let rows = self.state.metrics.snapshot(min_requests, min_bytes);
                ResourceContents::text(
                    serde_json::to_string(&rows).unwrap_or_else(|_| "[]".into()),
                    request.uri.clone(),
                )
                .with_mime_type("application/json")
            }
            _ => return Err(McpError::invalid_params("unknown Sqrzl resource URI", None)),
        };
        Ok(ReadResourceResult::new(vec![contents])
            .with_cache_scope(CacheScope::Private)
            .into())
    }
}

#[derive(Clone, Copy)]
enum ToolMode {
    Http,
    Usage,
    RepositoryTask,
    Domain,
}

#[derive(Clone)]
struct ToolSpec {
    name: &'static str,
    description: &'static str,
    scope: &'static str,
    method: &'static str,
    route: &'static str,
    schema: String,
    query: &'static [&'static str],
    mode: ToolMode,
    binary_put: bool,
}

impl ToolSpec {
    fn to_tool(&self) -> Tool {
        let schema: Map<String, Value> =
            serde_json::from_str(&self.schema).expect("MCP tool schema should be valid JSON");
        let annotations = ToolAnnotations::new()
            .read_only(self.scope == "sqrzl:read")
            .destructive(self.name.contains("delete") || self.name.contains("abort"))
            .idempotent(self.name.ends_with("_get") || self.name.ends_with("_list"));
        Tool::new(self.name, self.description, Arc::new(schema)).with_annotations(annotations)
    }
}

fn bounded_page_schema(schema: &str) -> String {
    let Ok(mut value) = serde_json::from_str::<Value>(schema) else {
        return schema.to_string();
    };
    let Some(properties) = value.get_mut("properties").and_then(Value::as_object_mut) else {
        return schema.to_string();
    };
    properties.insert(
        "limit".to_string(),
        json!({"type":"integer","minimum":1,"maximum":500}),
    );
    serde_json::to_string(&value).unwrap_or_else(|_| schema.to_string())
}

macro_rules! tool {
    ($name:literal, $description:literal, $scope:literal, $method:literal, $route:literal, $schema:literal) => {
        ToolSpec {
            name: $name,
            description: $description,
            scope: $scope,
            method: $method,
            route: $route,
            schema: $schema.to_string(),
            query: &[],
            mode: ToolMode::Http,
            binary_put: false,
        }
    };
    ($name:literal, $description:literal, $scope:literal, $method:literal, $route:literal, $schema:literal, query $query:expr) => {
        ToolSpec {
            name: $name,
            description: $description,
            scope: $scope,
            method: $method,
            route: $route,
            schema: $schema.to_string(),
            query: $query,
            mode: ToolMode::Http,
            binary_put: false,
        }
    };
}

const PAGE: &[&str] = &["limit", "search", "next"];
const OBJECT_PAGE: &[&str] = &["limit", "search", "next", "prefix"];

// Keep the complete public tool inventory together so discovery and scope checks share one source.
#[allow(clippy::too_many_lines)]
fn tool_specs(tasks: &[String]) -> Vec<ToolSpec> {
    let mut specs = vec![
        tool!("bucket_list", "List storage buckets with optional search and pagination.", "sqrzl:read", "GET", "/admin/v1/buckets", r#"{"type":"object","properties":{"limit":{"type":"integer","minimum":1,"maximum":1000},"search":{"type":"string"},"next":{"type":"string"}}}"#, query PAGE),
        tool!(
            "bucket_create",
            "Create a storage bucket.",
            "sqrzl:write",
            "POST",
            "/admin/v1/buckets",
            r#"{"type":"object","properties":{"name":{"type":"string"}},"required":["name"]}"#
        ),
        tool!(
            "bucket_get",
            "Read bucket details.",
            "sqrzl:read",
            "GET",
            "/admin/v1/buckets/{bucket}",
            r#"{"type":"object","properties":{"bucket":{"type":"string"}},"required":["bucket"]}"#
        ),
        tool!(
            "bucket_delete",
            "Delete an empty storage bucket.",
            "sqrzl:write",
            "DELETE",
            "/admin/v1/buckets/{bucket}",
            r#"{"type":"object","properties":{"bucket":{"type":"string"}},"required":["bucket"]}"#
        ),
        tool!(
            "bucket_versioning_get",
            "Read a bucket's versioning state.",
            "sqrzl:read",
            "GET",
            "/admin/v1/buckets/{bucket}/versioning",
            r#"{"type":"object","properties":{"bucket":{"type":"string"}},"required":["bucket"]}"#
        ),
        tool!(
            "bucket_versioning_set",
            "Set a bucket's versioning state.",
            "sqrzl:write",
            "PUT",
            "/admin/v1/buckets/{bucket}/versioning",
            r#"{"type":"object","properties":{"bucket":{"type":"string"},"enabled":{"type":"boolean"}},"required":["bucket","enabled"]}"#
        ),
        tool!(
            "bucket_acl_get",
            "Read a bucket ACL.",
            "sqrzl:read",
            "GET",
            "/admin/v1/buckets/{bucket}/acl",
            r#"{"type":"object","properties":{"bucket":{"type":"string"}},"required":["bucket"]}"#
        ),
        tool!(
            "bucket_acl_set",
            "Set a bucket ACL document.",
            "sqrzl:write",
            "PUT",
            "/admin/v1/buckets/{bucket}/acl",
            r#"{"type":"object","properties":{"bucket":{"type":"string"},"acl":{"type":"object"}},"required":["bucket","acl"]}"#
        ),
        tool!(
            "bucket_policy_get",
            "Read a bucket policy.",
            "sqrzl:read",
            "GET",
            "/admin/v1/buckets/{bucket}/policy",
            r#"{"type":"object","properties":{"bucket":{"type":"string"}},"required":["bucket"]}"#
        ),
        tool!(
            "bucket_policy_set",
            "Set a bucket policy document.",
            "sqrzl:write",
            "PUT",
            "/admin/v1/buckets/{bucket}/policy",
            r#"{"type":"object","properties":{"bucket":{"type":"string"},"policy":{"type":"object"}},"required":["bucket","policy"]}"#
        ),
        tool!(
            "bucket_policy_delete",
            "Delete a bucket policy.",
            "sqrzl:write",
            "DELETE",
            "/admin/v1/buckets/{bucket}/policy",
            r#"{"type":"object","properties":{"bucket":{"type":"string"}},"required":["bucket"]}"#
        ),
        tool!(
            "bucket_lifecycle_get",
            "Read bucket lifecycle rules.",
            "sqrzl:read",
            "GET",
            "/admin/v1/buckets/{bucket}/lifecycle",
            r#"{"type":"object","properties":{"bucket":{"type":"string"}},"required":["bucket"]}"#
        ),
        tool!(
            "bucket_lifecycle_set",
            "Set bucket lifecycle rules.",
            "sqrzl:write",
            "PUT",
            "/admin/v1/buckets/{bucket}/lifecycle",
            r#"{"type":"object","properties":{"bucket":{"type":"string"},"lifecycle":{"type":"object"}},"required":["bucket","lifecycle"]}"#
        ),
        tool!(
            "bucket_lifecycle_delete",
            "Delete bucket lifecycle rules.",
            "sqrzl:write",
            "DELETE",
            "/admin/v1/buckets/{bucket}/lifecycle",
            r#"{"type":"object","properties":{"bucket":{"type":"string"}},"required":["bucket"]}"#
        ),
        tool!("multipart_uploads_list", "List active multipart uploads for a bucket.", "sqrzl:read", "GET", "/admin/v1/buckets/{bucket}/multipart-uploads", r#"{"type":"object","properties":{"bucket":{"type":"string"},"limit":{"type":"integer"},"search":{"type":"string"},"next":{"type":"string"}},"required":["bucket"]}"#, query PAGE),
        tool!(
            "multipart_upload_get",
            "Read one multipart upload.",
            "sqrzl:read",
            "GET",
            "/admin/v1/buckets/{bucket}/multipart-uploads/{upload_id}",
            r#"{"type":"object","properties":{"bucket":{"type":"string"},"upload_id":{"type":"string"}},"required":["bucket","upload_id"]}"#
        ),
        tool!(
            "multipart_upload_abort",
            "Abort a multipart upload.",
            "sqrzl:write",
            "DELETE",
            "/admin/v1/buckets/{bucket}/multipart-uploads/{upload_id}",
            r#"{"type":"object","properties":{"bucket":{"type":"string"},"upload_id":{"type":"string"}},"required":["bucket","upload_id"]}"#
        ),
        tool!("object_list", "List objects and folders in a bucket.", "sqrzl:read", "GET", "/admin/v1/buckets/{bucket}/objects", r#"{"type":"object","properties":{"bucket":{"type":"string"},"prefix":{"type":"string"},"limit":{"type":"integer"},"search":{"type":"string"},"next":{"type":"string"}},"required":["bucket"]}"#, query OBJECT_PAGE),
        tool!(
            "object_metadata_get",
            "Read object metadata without returning its bytes.",
            "sqrzl:read",
            "GET",
            "/admin/v1/buckets/{bucket}/objects/{key}",
            r#"{"type":"object","properties":{"bucket":{"type":"string"},"key":{"type":"string"}},"required":["bucket","key"]}"#
        ),
        tool!(
            "object_delete",
            "Delete an object and its stored versions.",
            "sqrzl:write",
            "DELETE",
            "/admin/v1/buckets/{bucket}/objects/{key}",
            r#"{"type":"object","properties":{"bucket":{"type":"string"},"key":{"type":"string"}},"required":["bucket","key"]}"#
        ),
        tool!(
            "object_content_get",
            "Read an object's content as base64.",
            "sqrzl:read",
            "GET",
            "/admin/v1/buckets/{bucket}/objects/{key}/content",
            r#"{"type":"object","properties":{"bucket":{"type":"string"},"key":{"type":"string"}},"required":["bucket","key"]}"#
        ),
        ToolSpec {
            name: "object_content_put",
            description: "Write object content from base64 bytes.",
            scope: "sqrzl:write",
            method: "PUT",
            route: "/admin/v1/buckets/{bucket}/objects/{key}/content",
            schema: r#"{"type":"object","properties":{"bucket":{"type":"string"},"key":{"type":"string"},"content_base64":{"type":"string"},"content_type":{"type":"string"}},"required":["bucket","key","content_base64"]}"#.to_string(),
            query: &[],
            mode: ToolMode::Http,
            binary_put: true,
        },
        tool!("object_versions_list", "List stored object versions.", "sqrzl:read", "GET", "/admin/v1/buckets/{bucket}/objects/{key}/versions", r#"{"type":"object","properties":{"bucket":{"type":"string"},"key":{"type":"string"},"limit":{"type":"integer"},"search":{"type":"string"},"next":{"type":"string"}},"required":["bucket","key"]}"#, query PAGE),
        tool!(
            "object_version_delete",
            "Delete one stored object version.",
            "sqrzl:write",
            "DELETE",
            "/admin/v1/buckets/{bucket}/objects/{key}/versions/{version_id}",
            r#"{"type":"object","properties":{"bucket":{"type":"string"},"key":{"type":"string"},"version_id":{"type":"string"}},"required":["bucket","key","version_id"]}"#
        ),
        ToolSpec {
            name: "object_version_content_get",
            description: "Read a stored object version's bytes as base64.",
            scope: "sqrzl:read",
            method: "",
            route: "",
            schema: r#"{"type":"object","properties":{"bucket":{"type":"string"},"key":{"type":"string"},"version_id":{"type":"string"}},"required":["bucket","key","version_id"]}"#.to_string(),
            query: &[],
            mode: ToolMode::Domain,
            binary_put: false,
        },
        ToolSpec {
            name: "object_copy",
            description: "Copy an object while preserving its content type, user metadata, tags, and non-lock provider metadata.",
            scope: "sqrzl:write",
            method: "",
            route: "",
            schema: r#"{"type":"object","properties":{"source_bucket":{"type":"string"},"source_key":{"type":"string"},"destination_bucket":{"type":"string"},"destination_key":{"type":"string"}},"required":["source_bucket","source_key","destination_bucket","destination_key"]}"#.to_string(),
            query: &[],
            mode: ToolMode::Domain,
            binary_put: false,
        },
        ToolSpec {
            name: "object_delete_many",
            description: "Delete up to 1000 objects and return per-key success or error results.",
            scope: "sqrzl:write",
            method: "",
            route: "",
            schema: r#"{"type":"object","properties":{"bucket":{"type":"string"},"keys":{"type":"array","minItems":1,"maxItems":1000,"items":{"type":"string"}}},"required":["bucket","keys"]}"#.to_string(),
            query: &[],
            mode: ToolMode::Domain,
            binary_put: false,
        },
        ToolSpec {
            name: "multipart_upload_create",
            description: "Start a multipart object upload.",
            scope: "sqrzl:write",
            method: "",
            route: "",
            schema: r#"{"type":"object","properties":{"bucket":{"type":"string"},"key":{"type":"string"}},"required":["bucket","key"]}"#.to_string(),
            query: &[],
            mode: ToolMode::Domain,
            binary_put: false,
        },
        ToolSpec {
            name: "multipart_part_upload",
            description: "Upload a numbered multipart part from base64 bytes.",
            scope: "sqrzl:write",
            method: "",
            route: "",
            schema: r#"{"type":"object","properties":{"bucket":{"type":"string"},"upload_id":{"type":"string"},"part_number":{"type":"integer","minimum":1,"maximum":10000},"content_base64":{"type":"string"}},"required":["bucket","upload_id","part_number","content_base64"]}"#.to_string(),
            query: &[],
            mode: ToolMode::Domain,
            binary_put: false,
        },
        ToolSpec {
            name: "multipart_upload_complete",
            description: "Complete a multipart upload with an ordered part manifest.",
            scope: "sqrzl:write",
            method: "",
            route: "",
            schema: r#"{"type":"object","properties":{"bucket":{"type":"string"},"upload_id":{"type":"string"},"parts":{"type":"array","minItems":1,"maxItems":10000,"items":{"type":"object","properties":{"part_number":{"type":"integer","minimum":1,"maximum":10000},"etag":{"type":"string"}},"required":["part_number","etag"]}}},"required":["bucket","upload_id","parts"]}"#.to_string(),
            query: &[],
            mode: ToolMode::Domain,
            binary_put: false,
        },
        ToolSpec {
            name: "mail_send_simulate",
            description: "Capture a simulated outbound email through the shared mailbox fan-out service.",
            scope: "sqrzl:write",
            method: "",
            route: "",
            schema: r#"{"type":"object","properties":{"message":{"type":"object","properties":{"source_protocol":{"type":"string","enum":["smtp","send_grid","ses","acs"]},"from":{"type":"object","properties":{"email":{"type":"string"},"name":{"type":"string"}},"required":["email"]},"to":{"type":"array","items":{"type":"object"}},"cc":{"type":"array","items":{"type":"object"}},"bcc":{"type":"array","items":{"type":"object"}},"subject":{"type":"string"},"body_text":{"type":"string"},"body_html":{"type":"string"}},"required":["source_protocol","from","to"]}},"required":["message"]}"#.to_string(),
            query: &[],
            mode: ToolMode::Domain,
            binary_put: false,
        },
        ToolSpec {
            name: "text_send_simulate",
            description: "Capture a simulated outbound SMS using the shared SMS message store.",
            scope: "sqrzl:write",
            method: "",
            route: "",
            schema: r#"{"type":"object","properties":{"provider":{"type":"string","enum":["twilio","sns","aws-sms-voice-v2","acs"]},"from":{"type":"string"},"to":{"type":"string"},"body":{"type":"string"}},"required":["provider","from","to","body"]}"#.to_string(),
            query: &[],
            mode: ToolMode::Domain,
            binary_put: false,
        },
        tool!(
            "object_tags_get",
            "Read object tags.",
            "sqrzl:read",
            "GET",
            "/admin/v1/buckets/{bucket}/objects/{key}/tags",
            r#"{"type":"object","properties":{"bucket":{"type":"string"},"key":{"type":"string"}},"required":["bucket","key"]}"#
        ),
        tool!(
            "object_tags_set",
            "Set object tags.",
            "sqrzl:write",
            "PUT",
            "/admin/v1/buckets/{bucket}/objects/{key}/tags",
            r#"{"type":"object","properties":{"bucket":{"type":"string"},"key":{"type":"string"},"tags":{"type":"object"}},"required":["bucket","key","tags"]}"#
        ),
        tool!(
            "object_acl_get",
            "Read an object ACL.",
            "sqrzl:read",
            "GET",
            "/admin/v1/buckets/{bucket}/objects/{key}/acl",
            r#"{"type":"object","properties":{"bucket":{"type":"string"},"key":{"type":"string"}},"required":["bucket","key"]}"#
        ),
        tool!(
            "object_acl_set",
            "Set an object ACL document.",
            "sqrzl:write",
            "PUT",
            "/admin/v1/buckets/{bucket}/objects/{key}/acl",
            r#"{"type":"object","properties":{"bucket":{"type":"string"},"key":{"type":"string"},"acl":{"type":"object"}},"required":["bucket","key","acl"]}"#
        ),
        tool!("mailboxes_list", "List captured mailboxes.", "sqrzl:read", "GET", "/admin/v1/mailboxes", r#"{"type":"object","properties":{"limit":{"type":"integer"},"search":{"type":"string"},"next":{"type":"string"}}}"#, query PAGE),
        tool!(
            "mailbox_delete",
            "Delete a captured mailbox and its messages.",
            "sqrzl:write",
            "DELETE",
            "/admin/v1/mailboxes/{mailbox}",
            r#"{"type":"object","properties":{"mailbox":{"type":"string"}},"required":["mailbox"]}"#
        ),
        tool!("mail_messages_list", "List messages in a mailbox.", "sqrzl:read", "GET", "/admin/v1/mailboxes/{mailbox}/messages", r#"{"type":"object","properties":{"mailbox":{"type":"string"},"limit":{"type":"integer"},"search":{"type":"string"},"next":{"type":"string"}},"required":["mailbox"]}"#, query PAGE),
        tool!(
            "mail_message_get",
            "Read a captured email message.",
            "sqrzl:read",
            "GET",
            "/admin/v1/mailboxes/{mailbox}/messages/{message_id}",
            r#"{"type":"object","properties":{"mailbox":{"type":"string"},"message_id":{"type":"string"}},"required":["mailbox","message_id"]}"#
        ),
        tool!(
            "mail_message_delete",
            "Delete a captured email message.",
            "sqrzl:write",
            "DELETE",
            "/admin/v1/mailboxes/{mailbox}/messages/{message_id}",
            r#"{"type":"object","properties":{"mailbox":{"type":"string"},"message_id":{"type":"string"}},"required":["mailbox","message_id"]}"#
        ),
        tool!(
            "mail_message_content_get",
            "Read rendered message content as base64.",
            "sqrzl:read",
            "GET",
            "/admin/v1/mailboxes/{mailbox}/messages/{message_id}/content",
            r#"{"type":"object","properties":{"mailbox":{"type":"string"},"message_id":{"type":"string"}},"required":["mailbox","message_id"]}"#
        ),
        tool!(
            "mail_attachment_get",
            "Read a message attachment as base64.",
            "sqrzl:read",
            "GET",
            "/admin/v1/mailboxes/{mailbox}/messages/{message_id}/attachments/{filename}",
            r#"{"type":"object","properties":{"mailbox":{"type":"string"},"message_id":{"type":"string"},"filename":{"type":"string"}},"required":["mailbox","message_id","filename"]}"#
        ),
        tool!("text_conversations_list", "List simulated text conversations.", "sqrzl:read", "GET", "/admin/v1/text-conversations", r#"{"type":"object","properties":{"limit":{"type":"integer"},"search":{"type":"string"},"next":{"type":"string"}}}"#, query PAGE),
        tool!(
            "text_conversation_delete",
            "Delete a simulated text conversation.",
            "sqrzl:write",
            "DELETE",
            "/admin/v1/text-conversations/{peer}",
            r#"{"type":"object","properties":{"peer":{"type":"string"}},"required":["peer"]}"#
        ),
        tool!("text_messages_list", "List messages in a text conversation.", "sqrzl:read", "GET", "/admin/v1/text-conversations/{peer}/messages", r#"{"type":"object","properties":{"peer":{"type":"string"},"limit":{"type":"integer"},"search":{"type":"string"},"next":{"type":"string"}},"required":["peer"]}"#, query PAGE),
        tool!(
            "text_message_get",
            "Read one simulated text message.",
            "sqrzl:read",
            "GET",
            "/admin/v1/text-conversations/{peer}/messages/{message_id}",
            r#"{"type":"object","properties":{"peer":{"type":"string"},"message_id":{"type":"string"}},"required":["peer","message_id"]}"#
        ),
        tool!(
            "text_message_delete",
            "Delete one simulated text message.",
            "sqrzl:write",
            "DELETE",
            "/admin/v1/text-conversations/{peer}/messages/{message_id}",
            r#"{"type":"object","properties":{"peer":{"type":"string"},"message_id":{"type":"string"}},"required":["peer","message_id"]}"#
        ),
        tool!(
            "text_media_get",
            "Read media attached to a simulated text message as base64.",
            "sqrzl:read",
            "GET",
            "/admin/v1/text-conversations/{peer}/messages/{message_id}/media/{media_id}",
            r#"{"type":"object","properties":{"peer":{"type":"string"},"message_id":{"type":"string"},"media_id":{"type":"string"}},"required":["peer","message_id","media_id"]}"#
        ),
        tool!(
            "text_destination_get",
            "Read a provider callback destination.",
            "sqrzl:read",
            "GET",
            "/admin/v1/text-destinations/{provider}/{local_number}",
            r#"{"type":"object","properties":{"provider":{"type":"string"},"local_number":{"type":"string"}},"required":["provider","local_number"]}"#
        ),
        tool!(
            "text_destination_set",
            "Set a provider callback destination URL.",
            "sqrzl:write",
            "PUT",
            "/admin/v1/text-destinations/{provider}/{local_number}",
            r#"{"type":"object","properties":{"provider":{"type":"string"},"local_number":{"type":"string"},"callback_url":{"type":"string"}},"required":["provider","local_number","callback_url"]}"#
        ),
        tool!(
            "text_destination_delete",
            "Delete a provider callback destination.",
            "sqrzl:write",
            "DELETE",
            "/admin/v1/text-destinations/{provider}/{local_number}",
            r#"{"type":"object","properties":{"provider":{"type":"string"},"local_number":{"type":"string"}},"required":["provider","local_number"]}"#
        ),
        tool!(
            "text_delivery_transition",
            "Simulate a delivered or failed message status.",
            "sqrzl:write",
            "POST",
            "/admin/v1/text-messages/{message_id}/delivery",
            r#"{"type":"object","properties":{"message_id":{"type":"string"},"state":{"type":"string","enum":["delivered","failed"]}},"required":["message_id","state"]}"#
        ),
        tool!(
            "text_inbound_simulate",
            "Inject an inbound SMS or Twilio MMS for local testing.",
            "sqrzl:write",
            "POST",
            "/admin/v1/text-simulations/inbound",
            r#"{"type":"object","properties":{"provider":{"type":"string"},"from":{"type":"string"},"to":{"type":"string"},"body":{"type":"string"},"media":{"type":"array","items":{"type":"object"}},"metadata":{"type":"object"}},"required":["provider","from","to"]}"#
        ),
        tool!(
            "text_callback_retry",
            "Retry a captured text callback attempt.",
            "sqrzl:write",
            "POST",
            "/admin/v1/text-callback-attempts/{attempt_id}/retry",
            r#"{"type":"object","properties":{"attempt_id":{"type":"string"}},"required":["attempt_id"]}"#
        ),
        ToolSpec {
            name: "usage_query",
            description: "Read usage rows filtered by caller-selected request and byte thresholds.",
            scope: "sqrzl:read",
            method: "",
            route: "",
            schema: r#"{"type":"object","properties":{"min_requests":{"type":"integer","minimum":0},"min_bytes":{"type":"integer","minimum":0}}}"#.to_string(),
            query: &[],
            mode: ToolMode::Usage,
            binary_put: false,
        },
    ];
    if !tasks.is_empty() {
        specs.push(ToolSpec { name: "repository_task_run", description: "Run one enabled, fixed, read-only repository inspection task with bounded time and output.", scope: "sqrzl:task", method: "", route: "", schema: json!({"type":"object","properties":{"task":{"type":"string","enum":tasks}},"required":["task"]}).to_string(), query: &[], mode: ToolMode::RepositoryTask, binary_put: false });
    }
    for spec in &mut specs {
        if spec.query.contains(&"limit") {
            spec.schema = bounded_page_schema(&spec.schema);
        }
    }
    specs
}

impl McpHandler {
    fn read_usage(&self, args: &Value) -> (Value, u64) {
        let min_requests = args
            .get("min_requests")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let min_bytes = args.get("min_bytes").and_then(Value::as_u64).unwrap_or(0);
        let rows = self.state.metrics.snapshot(min_requests, min_bytes);
        let value = json!({"items":rows});
        let size = serialized_size(&value);
        (value, size)
    }

    async fn run_domain_tool(&self, name: &str, args: &Value) -> Result<(Value, u64), String> {
        let value = match name {
            "object_version_content_get" => self.object_version_content_get(args)?,
            "object_copy" => self.object_copy(args).await?,
            "object_delete_many" => self.object_delete_many(args)?,
            "multipart_upload_create" => self.multipart_upload_create(args)?,
            "multipart_part_upload" => self.multipart_part_upload(args)?,
            "multipart_upload_complete" => self.multipart_upload_complete(args)?,
            "mail_send_simulate" => self.mail_send_simulate(args)?,
            "text_send_simulate" => self.text_send_simulate(args)?,
            _ => return Err("unknown domain tool".into()),
        };
        let response_bytes = serialized_size(&value);
        Ok((value, response_bytes))
    }

    fn object_version_content_get(&self, args: &Value) -> Result<Value, String> {
        let bucket = required_string(args, "bucket")?;
        let key = required_string(args, "key")?;
        let version_id = required_string(args, "version_id")?;
        validate_tool_bucket(bucket)?;
        validate_tool_key(key)?;
        let object = crate::services::object::get_object_version(
            self.state.storage.as_ref(),
            bucket,
            key,
            version_id,
        )
        .map_err(|error| error.to_string())?;
        Ok(json!({
            "status":200,
            "content_type":object.content_type,
            "content_base64":BASE64.encode(object.data),
            "version_id":object.version_id
        }))
    }

    async fn object_copy(&self, args: &Value) -> Result<Value, String> {
        let source_bucket = required_string(args, "source_bucket")?;
        let source_key = required_string(args, "source_key")?;
        let destination_bucket = required_string(args, "destination_bucket")?;
        let destination_key = required_string(args, "destination_key")?;
        validate_tool_bucket(source_bucket)?;
        validate_tool_bucket(destination_bucket)?;
        validate_tool_key(source_key)?;
        validate_tool_key(destination_key)?;
        let gate = self.state.storage.operation_gate();
        let _owner = gate.lock().await;
        crate::api::admin::ensure_admin_object_mutable(
            &self.state.storage,
            destination_bucket,
            destination_key,
            false,
        )
        .map_err(|error| error.to_string())?;
        let mut object = crate::services::object::get_object(
            self.state.storage.as_ref(),
            source_bucket,
            source_key,
        )
        .map_err(|error| error.to_string())?;
        object.key = destination_key.to_string();
        object.acl = None;
        object.provider_metadata.retain(|name, _| {
            !name.starts_with("s3_object_lock_")
                && !name.starts_with("azure_immutability_")
                && !name.starts_with("azure_lease_")
                && name != "azure_legal_hold"
        });
        crate::services::object::put_object(
            self.state.storage.as_ref(),
            destination_bucket,
            destination_key.to_string(),
            object,
        )
        .map_err(|error| error.to_string())?;
        let copied = crate::services::object::get_object_metadata(
            self.state.storage.as_ref(),
            destination_bucket,
            destination_key,
        )
        .map_err(|error| error.to_string())?;
        Ok(json!({"status":200,"object":copied}))
    }

    fn object_delete_many(&self, args: &Value) -> Result<Value, String> {
        let bucket = required_string(args, "bucket")?;
        validate_tool_bucket(bucket)?;
        let keys = args
            .get("keys")
            .and_then(Value::as_array)
            .ok_or("keys must be an array")?;
        if keys.is_empty() || keys.len() > 1000 {
            return Err("keys must contain between 1 and 1000 items".into());
        }
        let mut deleted = Vec::new();
        let mut errors = Vec::new();
        for value in keys {
            let key = value.as_str().ok_or("every key must be a string")?;
            let result = validate_tool_key(key)
                .and_then(|()| {
                    crate::api::admin::ensure_admin_object_mutable(
                        &self.state.storage,
                        bucket,
                        key,
                        true,
                    )
                    .map_err(|error| error.to_string())
                })
                .and_then(|()| {
                    crate::services::object::delete_object(self.state.storage.as_ref(), bucket, key)
                        .map_err(|error| error.to_string())
                });
            match result {
                Ok(()) => deleted.push(key),
                Err(error) => errors.push(json!({"key":key,"error":error})),
            }
        }
        Ok(json!({"status":200,"deleted":deleted,"errors":errors}))
    }

    fn multipart_upload_create(&self, args: &Value) -> Result<Value, String> {
        let bucket = required_string(args, "bucket")?;
        let key = required_string(args, "key")?;
        validate_tool_bucket(bucket)?;
        validate_tool_key(key)?;
        crate::api::admin::ensure_admin_object_mutable(&self.state.storage, bucket, key, false)
            .map_err(|error| error.to_string())?;
        let upload = crate::services::object::create_multipart_upload(
            self.state.storage.as_ref(),
            bucket,
            key.to_string(),
        )
        .map_err(|error| error.to_string())?;
        Ok(json!({"status":201,"bucket":bucket,"upload":upload}))
    }

    fn multipart_part_upload(&self, args: &Value) -> Result<Value, String> {
        let bucket = required_string(args, "bucket")?;
        let upload_id = required_string(args, "upload_id")?;
        validate_tool_bucket(bucket)?;
        let upload = crate::services::object::get_multipart_upload(
            self.state.storage.as_ref(),
            bucket,
            upload_id,
        )
        .map_err(|error| error.to_string())?;
        validate_tool_key(&upload.key)?;
        crate::api::admin::ensure_admin_object_mutable(
            &self.state.storage,
            bucket,
            &upload.key,
            false,
        )
        .map_err(|error| error.to_string())?;
        let part_number = args
            .get("part_number")
            .and_then(Value::as_u64)
            .and_then(|number| u32::try_from(number).ok())
            .filter(|number| (1..=10_000).contains(number))
            .ok_or("part_number must be between 1 and 10000")?;
        let bytes = BASE64
            .decode(required_string(args, "content_base64")?)
            .map_err(|_| "content_base64 is invalid")?;
        let etag = crate::services::object::upload_part(
            self.state.storage.as_ref(),
            bucket,
            upload_id,
            part_number,
            bytes,
        )
        .map_err(|error| error.to_string())?;
        Ok(json!({"status":200,"part_number":part_number,"etag":etag}))
    }

    fn multipart_upload_complete(&self, args: &Value) -> Result<Value, String> {
        let bucket = required_string(args, "bucket")?;
        let upload_id = required_string(args, "upload_id")?;
        validate_tool_bucket(bucket)?;
        let upload = crate::services::object::get_multipart_upload(
            self.state.storage.as_ref(),
            bucket,
            upload_id,
        )
        .map_err(|error| error.to_string())?;
        validate_tool_key(&upload.key)?;
        crate::api::admin::ensure_admin_object_mutable(
            &self.state.storage,
            bucket,
            &upload.key,
            false,
        )
        .map_err(|error| error.to_string())?;
        let values = args
            .get("parts")
            .and_then(Value::as_array)
            .ok_or("parts must be an array")?;
        if values.is_empty() || values.len() > 10_000 {
            return Err("parts must contain between 1 and 10000 items".into());
        }
        let parts = values
            .iter()
            .map(|part| {
                let number = part
                    .get("part_number")
                    .and_then(Value::as_u64)
                    .and_then(|number| u32::try_from(number).ok())
                    .filter(|number| (1..=10_000).contains(number))
                    .ok_or("part_number must be between 1 and 10000")?;
                let etag = part
                    .get("etag")
                    .and_then(Value::as_str)
                    .ok_or("etag is required")?;
                Ok((number, etag.to_string()))
            })
            .collect::<Result<Vec<_>, &str>>()?;
        let etag = crate::services::object::complete_multipart_upload_with_parts(
            self.state.storage.as_ref(),
            bucket,
            upload_id,
            &parts,
        )
        .map_err(|error| error.to_string())?;
        Ok(json!({"status":200,"upload_id":upload_id,"etag":etag}))
    }

    fn mail_send_simulate(&self, args: &Value) -> Result<Value, String> {
        let message_value = args.get("message").cloned().ok_or("message is required")?;
        let message = serde_json::from_value::<crate::mail::Message>(message_value)
            .map_err(|error| error.to_string())?;
        let stored = crate::mail::fan_out(self.state.mail.as_ref(), &message)
            .map_err(|error| error.to_string())?;
        let message_id = stored
            .first()
            .map(|item| item.message_id.as_str())
            .ok_or("mail fan-out returned no stored messages")?;
        let mut mailboxes = stored
            .iter()
            .map(|item| item.mailbox.as_str())
            .filter(|mailbox| *mailbox != crate::mail::ALL_MAILBOX)
            .collect::<Vec<_>>();
        mailboxes.sort_unstable();
        Ok(json!({"status":202,"message_id":message_id,"mailboxes":mailboxes}))
    }

    fn text_send_simulate(&self, args: &Value) -> Result<Value, String> {
        let provider = required_string(args, "provider")?.parse::<crate::sms::SmsProvider>()?;
        let from = required_string(args, "from")?;
        let to = required_string(args, "to")?;
        if !crate::sms::model::valid_sender(from) || !crate::sms::model::is_e164(to) {
            return Err("from must be a valid sender identity and to must be E.164".into());
        }
        let message = self
            .state
            .sms
            .store_message(crate::sms::NewSmsMessage {
                batch_id: None,
                provider,
                provider_message_id: None,
                direction: crate::sms::SmsDirection::Outbound,
                channel: crate::sms::SmsChannel::Sms,
                from: from.to_string(),
                to: to.to_string(),
                body: required_string(args, "body")?.to_string(),
                media: Vec::new(),
                metadata: std::collections::HashMap::new(),
            })
            .map_err(|error| error.to_string())?;
        Ok(json!({"status":202,"message":message}))
    }

    async fn run_repository_task(&self, args: &Value) -> Result<(Value, u64), String> {
        let task = args
            .get("task")
            .and_then(Value::as_str)
            .ok_or("task is required")?;
        if !self
            .state
            .mcp_config
            .repository_tasks
            .iter()
            .any(|enabled| enabled == task)
        {
            return Err("repository task is disabled or not allowlisted".into());
        }
        execute_repository_task(
            task,
            self.state
                .mcp_config
                .repository_path
                .as_deref()
                .map(Path::new),
        )
        .await
    }

    async fn call_shared_api(&self, spec: &ToolSpec, args: &Value) -> Result<(Value, u64), String> {
        let path = build_path(spec, args)?;
        let method =
            Method::from_bytes(spec.method.as_bytes()).map_err(|error| error.to_string())?;
        let mut body = Vec::new();
        let mut builder = Request::builder()
            .method(method.clone())
            .uri(path)
            .header("host", "localhost");
        if spec.binary_put {
            let encoded = args
                .get("content_base64")
                .and_then(Value::as_str)
                .ok_or("content_base64 is required")?;
            body = BASE64
                .decode(encoded)
                .map_err(|_| "content_base64 is invalid")?;
            let content_type = args
                .get("content_type")
                .and_then(Value::as_str)
                .unwrap_or("application/octet-stream");
            builder = builder.header("content-type", content_type);
        } else if method == Method::PUT || method == Method::POST {
            let json_body = body_arguments(spec, args);
            body = serde_json::to_vec(&json_body).map_err(|error| error.to_string())?;
            builder = builder.header("content-type", "application/json");
        }
        let request = builder
            .body(Body::from(body))
            .map_err(|error| error.to_string())?;
        let response = if spec.route.starts_with("/admin/v1/mailboxes") {
            match crate::api::admin_mail::handle_request(&self.state.mail, &request) {
                Ok(response) => response,
                Err(error) => crate::api::admin::error_response(&error),
            }
        } else if crate::api::admin_text::matches_path(request.uri().path()) {
            match crate::api::admin_text::handle_request(&self.state.sms, request).await {
                Ok(response) => response,
                Err(error) => crate::api::admin::error_response(&error),
            }
        } else {
            match crate::api::admin::handle_request(self.state.storage.clone(), request).await {
                Ok(response) => response,
                Err(error) => crate::api::admin::error_response(&error),
            }
        };
        let binary_response = matches!(
            spec.name,
            "object_content_get"
                | "object_version_content_get"
                | "mail_message_content_get"
                | "mail_attachment_get"
                | "text_media_get"
        );
        response_value(response, binary_response).await
    }
}

async fn execute_repository_task(task: &str, path: Option<&Path>) -> Result<(Value, u64), String> {
    let (program, command_args) = match task {
        "git_status" => (
            "git",
            vec![
                "--no-pager",
                "-c",
                "core.fsmonitor=false",
                "-c",
                "core.untrackedCache=false",
                "status",
                "--short",
                "--branch",
            ],
        ),
        "git_diff_stat" => (
            "git",
            vec![
                "--no-pager",
                "-c",
                "core.fsmonitor=false",
                "-c",
                "diff.external=",
                "-c",
                "core.pager=cat",
                "diff",
                "--no-ext-diff",
                "--no-textconv",
                "--stat",
            ],
        ),
        _ => return Err("repository task is not allowlisted".into()),
    };
    let mut command = Command::new(program);
    command
        .args(command_args)
        .kill_on_drop(true)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_TERMINAL_PROMPT", "0");
    #[cfg(unix)]
    command.process_group(0);
    if let Some(path) = path {
        command.current_dir(path);
    }
    let mut child = command
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|error| format!("failed to start repository task: {error}"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or("repository task stdout unavailable")?;
    let stderr = child
        .stderr
        .take()
        .ok_or("repository task stderr unavailable")?;
    let mut stdout_task = tokio::spawn(read_bounded(stdout, MAX_TASK_OUTPUT_BYTES / 2));
    let mut stderr_task = tokio::spawn(read_bounded(stderr, MAX_TASK_OUTPUT_BYTES / 2));
    let completed = tokio::time::timeout(TASK_TIMEOUT, async {
        let status = child.wait().await.map_err(|error| error.to_string())?;
        let stdout = (&mut stdout_task)
            .await
            .map_err(|error| error.to_string())?
            .map_err(|error| error.to_string())?;
        let stderr = (&mut stderr_task)
            .await
            .map_err(|error| error.to_string())?
            .map_err(|error| error.to_string())?;
        Ok::<_, String>((status.code(), stdout, stderr))
    })
    .await;
    let (status, (stdout, stdout_truncated), (stderr, stderr_truncated)) = match completed {
        Ok(Ok(value)) => value,
        Ok(Err(error)) => return Err(error),
        Err(_) => {
            #[cfg(unix)]
            if let Some(pid) = child.id() {
                if let Ok(pid) = i32::try_from(pid) {
                    // SAFETY: the child was started in a new process group, so its
                    // negative PID targets only the fixed repository task and helpers.
                    let _ = unsafe { libc::kill(-pid, libc::SIGKILL) };
                }
            }
            let _ = child.kill().await;
            let _ = child.wait().await;
            stdout_task.abort();
            stderr_task.abort();
            let _ = stdout_task.await;
            let _ = stderr_task.await;
            return Err("repository task exceeded its 5 second time limit".into());
        }
    };
    let value = json!({"task":task,"exit_code":status,"stdout":String::from_utf8_lossy(&stdout),"stderr":String::from_utf8_lossy(&stderr),"output_truncated":stdout_truncated || stderr_truncated});
    let size = serde_json::to_vec(&value).map_or(0, |bytes| bytes.len() as u64);
    Ok((value, size))
}

fn required_string<'a>(args: &'a Value, name: &str) -> Result<&'a str, String> {
    args.get(name)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| format!("{name} is required"))
}

fn validate_tool_bucket(bucket: &str) -> Result<(), String> {
    crate::utils::validation::validate_bucket_name(bucket)
}

fn validate_tool_key(key: &str) -> Result<(), String> {
    crate::utils::validation::validate_blob_key(key)
}

fn serialized_size(value: &Value) -> u64 {
    serde_json::to_vec(value).map_or(0, |bytes| bytes.len() as u64)
}

async fn read_bounded<R: tokio::io::AsyncRead + Unpin>(
    mut reader: R,
    limit: usize,
) -> std::io::Result<(Vec<u8>, bool)> {
    let mut output = Vec::with_capacity(limit.min(4096));
    let mut truncated = false;
    let mut buffer = [0_u8; 4096];
    loop {
        let size = reader.read(&mut buffer).await?;
        if size == 0 {
            break;
        }
        let keep = size.min(limit.saturating_sub(output.len()));
        output.extend_from_slice(&buffer[..keep]);
        truncated |= keep < size;
    }
    Ok((output, truncated))
}

async fn response_value(
    response: Response<Body>,
    binary_response: bool,
) -> Result<(Value, u64), String> {
    let status = response.status();
    let content_type = response
        .headers()
        .get("content-type")
        .and_then(|value| value.to_str().ok())
        .unwrap_or("application/octet-stream")
        .to_string();
    let headers = response
        .headers()
        .iter()
        .filter_map(|(name, value)| {
            value
                .to_str()
                .ok()
                .map(|value| (name.as_str().to_string(), Value::String(value.to_string())))
        })
        .collect::<Map<String, Value>>();
    let bytes = response
        .into_body()
        .collect()
        .await
        .map_err(|error| error.to_string())?
        .to_bytes();
    let size = bytes.len() as u64;
    if !status.is_success() {
        if let Ok(value) = serde_json::from_slice::<Value>(&bytes) {
            return Ok((json!({"status":status.as_u16(),"error":value}), size));
        }
    } else if binary_response {
        return Ok((
            json!({"status":status.as_u16(),"content_type":content_type,"headers":headers,"content_base64":BASE64.encode(bytes)}),
            size,
        ));
    } else if bytes.is_empty() {
        return Ok((json!({"status":status.as_u16()}), size));
    } else if let Ok(value) = serde_json::from_slice::<Value>(&bytes) {
        return Ok((value, size));
    }
    Ok((
        json!({"status":status.as_u16(),"content_type":content_type,"headers":headers,"content_base64":BASE64.encode(bytes)}),
        size,
    ))
}

fn build_path(spec: &ToolSpec, args: &Value) -> Result<String, String> {
    let mut path = String::new();
    let mut rest = spec.route;
    while let Some(start) = rest.find('{') {
        path.push_str(&rest[..start]);
        let tail = &rest[start + 1..];
        let end = tail.find('}').ok_or("invalid internal tool route")?;
        let name = &tail[..end];
        let value = args
            .get(name)
            .and_then(Value::as_str)
            .ok_or_else(|| format!("{name} is required"))?;
        path.push_str(&urlencoding::encode(value));
        rest = &tail[end + 1..];
    }
    path.push_str(rest);
    let mut query = Vec::new();
    for name in spec.query {
        if let Some(value) = args.get(*name) {
            if value.is_null() {
                continue;
            }
            let text = value
                .as_str()
                .map_or_else(|| value.to_string(), ToOwned::to_owned);
            query.push(format!(
                "{}={}",
                urlencoding::encode(name),
                urlencoding::encode(&text)
            ));
        }
    }
    if !query.is_empty() {
        path.push('?');
        path.push_str(&query.join("&"));
    }
    Ok(path)
}

fn body_arguments(spec: &ToolSpec, args: &Value) -> Value {
    let Value::Object(args) = args else {
        return json!({});
    };
    if let Some(field) = spec
        .route
        .rsplit('/')
        .next()
        .filter(|field| matches!(*field, "acl" | "policy" | "lifecycle"))
    {
        if let Some(value) = args.get(field) {
            return value.clone();
        }
    }
    let path_names: BTreeSet<&str> = spec
        .route
        .split('{')
        .skip(1)
        .filter_map(|tail| tail.split_once('}').map(|(name, _)| name))
        .collect();
    let query_names: BTreeSet<&str> = spec.query.iter().copied().collect();
    Value::Object(
        args.iter()
            .filter(|(name, _)| {
                !path_names.contains(name.as_str()) && !query_names.contains(name.as_str())
            })
            .map(|(name, value)| (name.clone(), value.clone()))
            .collect(),
    )
}

fn principal(context: &RequestContext<RoleServer>) -> Option<McpPrincipal> {
    context
        .extensions
        .get::<http::request::Parts>()
        .and_then(|parts| parts.extensions.get::<McpPrincipal>().cloned())
}

fn has_scope(context: &RequestContext<RoleServer>, required: &str, protected: bool) -> bool {
    if !protected {
        return true;
    }
    principal(context).is_some_and(|principal| principal.scopes.contains(required))
}

fn parse_usage_thresholds(uri: &str) -> (u64, u64) {
    let query = uri.split_once('?').map_or("", |(_, query)| query);
    let mut min_requests = 0;
    let mut min_bytes = 0;
    for pair in query.split('&') {
        let Some((name, value)) = pair.split_once('=') else {
            continue;
        };
        let Ok(value) = value.parse::<u64>() else {
            continue;
        };
        match name {
            "min_requests" => min_requests = value,
            "min_bytes" => min_bytes = value,
            _ => {}
        }
    }
    (min_requests, min_bytes)
}

/// Required scope for the HTTP preflight. Unknown methods still pass through
/// to the SDK so it can return the protocol's normal method-not-found error.
#[must_use]
pub fn required_scope_for_request(body: &[u8]) -> Option<&'static str> {
    let request: Value = serde_json::from_slice(body).ok()?;
    let method = request.get("method").and_then(Value::as_str)?;
    match method {
        "resources/read" | "resources/list" | "tools/list" => Some("sqrzl:read"),
        "tools/call" => {
            let name = request.pointer("/params/name").and_then(Value::as_str)?;
            if name == "repository_task_run" {
                return Some("sqrzl:task");
            }
            tool_specs(&[])
                .into_iter()
                .find(|spec| spec.name == name)
                .map(|spec| spec.scope)
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use std::fs;
    use std::process::Command as StdCommand;

    #[test]
    fn should_require_scopes_for_tool_routes() {
        // Arrange
        let specs = tool_specs(&[]);

        // Act
        let required_scope = required_scope_for_request(
            br#"{"method":"tools/call","params":{"name":"object_list"}}"#,
        );

        // Assert
        assert!(specs
            .iter()
            .any(|spec| spec.name == "bucket_create" && spec.scope == "sqrzl:write"));
        assert!(specs
            .iter()
            .any(|spec| spec.name == "text_inbound_simulate"));
        assert!(!specs.iter().any(|spec| spec.name == "repository_task_run"));
        assert_eq!(
            required_scope_for_request(
                br#"{"method":"tools/call","params":{"name":"repository_task_run"}}"#
            ),
            Some("sqrzl:task")
        );
        assert_eq!(
            required_scope_for_request(
                br#"{"method":"tools/call","params":{"name":"object_list"}}"#
            ),
            required_scope
        );
        assert_eq!(
            required_scope_for_request(br#"{"method":"tools/list"}"#),
            Some("sqrzl:read")
        );
        assert_eq!(
            required_scope_for_request(
                br#"{"method":"tools/call","params":{"name":"bucket_create"}}"#
            ),
            Some("sqrzl:write")
        );
    }

    #[test]
    fn should_encode_tool_paths_with_query_limits() {
        // Arrange
        let spec = tool_specs(&[])
            .into_iter()
            .find(|spec| spec.name == "object_list")
            .unwrap();
        let args = json!({"bucket":"photos","prefix":"a/b","limit":2});

        // Act
        let path = build_path(&spec, &args).unwrap();

        // Assert
        assert_eq!(
            path,
            "/admin/v1/buckets/photos/objects?limit=2&prefix=a%2Fb"
        );
    }

    #[test]
    fn should_bound_all_paginated_tool_schemas_to_admin_limits() {
        // Arrange
        let specs = tool_specs(&[]);

        // Act
        let page_limits = specs
            .iter()
            .filter(|spec| spec.query.contains(&"limit"))
            .map(|spec| {
                serde_json::from_str::<Value>(&spec.schema)
                    .map(|schema| schema["properties"]["limit"].clone())
            })
            .collect::<std::result::Result<Vec<_>, _>>()
            .unwrap();

        // Assert
        assert_ne!(page_limits.len(), 0);
        assert!(page_limits.iter().all(|limit| limit["minimum"] == 1));
        assert!(page_limits.iter().all(|limit| limit["maximum"] == 500));
    }

    #[test]
    fn should_list_only_enabled_repository_task_names() {
        // Arrange
        let tasks = vec!["git_status".to_string()];

        // Act
        let spec = tool_specs(&tasks)
            .into_iter()
            .find(|spec| spec.name == "repository_task_run")
            .expect("opted-in repository task tool should be listed");
        let schema: Value = serde_json::from_str(&spec.schema).unwrap();

        // Assert
        assert_eq!(schema["properties"]["task"]["enum"], json!(["git_status"]));
    }

    #[tokio::test]
    async fn should_truncate_repository_task_output_at_its_configured_limit() {
        // Arrange
        let (mut writer, reader) = tokio::io::duplex(16);
        tokio::io::AsyncWriteExt::write_all(&mut writer, b"abcdef")
            .await
            .unwrap();
        drop(writer);

        // Act
        let result = read_bounded(reader, 3).await.unwrap();

        // Assert
        assert_eq!(result, (b"abc".to_vec(), true));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn should_not_run_configured_git_fsmonitor_helpers() {
        // Arrange
        let root =
            std::env::temp_dir().join(format!("sqrzl-mcp-git-task-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&root).unwrap();
        let init = StdCommand::new("git")
            .arg("-C")
            .arg(&root)
            .args(["init", "--quiet"])
            .output()
            .unwrap();
        assert!(init.status.success());
        let config = StdCommand::new("git")
            .arg("-C")
            .arg(&root)
            .args([
                "config",
                "core.fsmonitor",
                "sh -c 'touch fsmonitor-ran; echo token'",
            ])
            .output()
            .unwrap();
        assert!(config.status.success());

        // Act
        let result = execute_repository_task("git_status", Some(&root)).await;

        // Assert
        assert!(result.is_ok());
        assert!(!root.join("fsmonitor-ran").exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn should_preserve_binary_content_exactly() {
        // Arrange
        let json_bytes = Bytes::from_static(b" {\"status\":404}\n");
        let json_response = Response::builder()
            .status(200)
            .header("content-type", "application/json")
            .body(Body::from(json_bytes.clone()))
            .unwrap();
        let empty_response = Response::builder()
            .status(200)
            .header("content-type", "application/octet-stream")
            .body(Body::default())
            .unwrap();

        // Act
        let (json_value, json_size) = response_value(json_response, true).await.unwrap();
        let (empty_value, empty_size) = response_value(empty_response, true).await.unwrap();

        // Assert
        assert_eq!(json_size, json_bytes.len() as u64);
        assert_eq!(
            BASE64
                .decode(json_value["content_base64"].as_str().unwrap())
                .unwrap(),
            json_bytes
        );
        assert_eq!(empty_size, 0);
        assert_eq!(empty_value["content_base64"], "");
    }
}
