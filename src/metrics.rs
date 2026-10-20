//! Passive, bounded process-local usage diagnostics.

use crate::body::Body;
use hyper::body::Body as _;
use hyper::{HeaderMap, Method, Response, Uri};
use serde::Serialize;
use std::collections::BTreeMap;
use std::sync::Mutex;

const MAX_SERIES: usize = 20_000;
const SIZE_BOUNDS: [u64; 10] = [
    256,
    1_024,
    4_096,
    16_384,
    65_536,
    262_144,
    1_048_576,
    16_777_216,
    134_217_728,
    u64::MAX,
];

#[derive(Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub struct UsageRow {
    pub provider: String,
    pub bucket: String,
    pub client: String,
    pub operation: String,
    pub requests: u64,
    pub request_bytes: u64,
    pub response_bytes: u64,
    pub list_requests: u64,
    pub list_items: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Labels(String, String, String, String);

#[derive(Clone, Copy)]
struct Record<'a> {
    provider: &'a str,
    bucket: &'a str,
    client: &'a str,
    operation: &'a str,
    request_bytes: u64,
    response_bytes: u64,
    is_list: bool,
    list_items: u64,
}

#[derive(Clone, Debug, Default)]
struct Counters {
    row: UsageRow,
    size: [u64; SIZE_BOUNDS.len()],
    size_sum: u64,
}

#[derive(Debug, Default)]
pub struct UsageMetrics {
    series: Mutex<BTreeMap<Labels, Counters>>,
}

impl UsageMetrics {
    pub fn record_http_request(
        &self,
        method: &Method,
        uri: &Uri,
        headers: &HeaderMap,
        request_bytes: u64,
        response: &Response<Body>,
        provider_hint: Option<&str>,
    ) {
        let provider = provider_hint.map_or_else(
            || provider(path(uri), host(headers), headers),
            ToOwned::to_owned,
        );
        let bucket = bucket(path(uri), host(headers), &provider);
        let operation = operation(
            method,
            path(uri),
            uri.query().unwrap_or_default(),
            &provider,
        );
        let client = headers
            .get("mcp-client-info")
            .and_then(|v| v.to_str().ok())
            .and_then(client_name)
            .or_else(|| {
                headers
                    .get("user-agent")
                    .and_then(|v| v.to_str().ok())
                    .map(|v| v.split(['/', ' ']).next().unwrap_or(v).to_string())
            })
            .unwrap_or_else(|| "unknown".to_string());
        let is_list = operation.ends_with("_list");
        let list_items = if is_list {
            response
                .body()
                .full_bytes()
                .map_or(0, |bytes| list_items(bytes))
        } else {
            0
        };
        self.record(Record {
            provider: &provider,
            bucket: &bucket,
            client: &client,
            operation: &operation,
            request_bytes,
            response_bytes: response.body().size_hint().exact().unwrap_or(0),
            is_list,
            list_items,
        });
    }

    pub fn record_mcp_tool(
        &self,
        tool: &str,
        bucket: Option<&str>,
        client: &str,
        request_bytes: u64,
        response_bytes: u64,
        list_items: u64,
    ) {
        self.record(Record {
            provider: "mcp",
            bucket: bucket.unwrap_or("none"),
            client,
            operation: tool,
            request_bytes,
            response_bytes,
            is_list: tool.contains("list"),
            list_items,
        });
    }

    fn record(&self, record: Record<'_>) {
        let provider = label(record.provider, "unknown");
        let operation = label(record.operation, "unknown");
        let bucket = label(record.bucket, "none");
        let client = label(record.client, "unknown");
        let labels = Labels(
            provider.clone(),
            bucket.clone(),
            client.clone(),
            operation.clone(),
        );
        let Ok(mut series) = self.series.lock() else {
            return;
        };
        let key = if series.contains_key(&labels) || series.len() < MAX_SERIES {
            labels
        } else {
            Labels(provider, "overflow".into(), "overflow".into(), operation)
        };
        let row_labels = key.clone();
        let counters = series.entry(key).or_default();
        counters.row.provider.clone_from(&row_labels.0);
        counters.row.bucket.clone_from(&row_labels.1);
        counters.row.client.clone_from(&row_labels.2);
        counters.row.operation.clone_from(&row_labels.3);
        counters.row.requests = counters.row.requests.saturating_add(1);
        counters.row.request_bytes = counters
            .row
            .request_bytes
            .saturating_add(record.request_bytes);
        counters.row.response_bytes = counters
            .row
            .response_bytes
            .saturating_add(record.response_bytes);
        if record.is_list {
            counters.row.list_requests = counters.row.list_requests.saturating_add(1);
            counters.row.list_items = counters.row.list_items.saturating_add(record.list_items);
        }
        if let Some((index, _)) = SIZE_BOUNDS
            .iter()
            .enumerate()
            .find(|(_, bound)| record.request_bytes <= **bound)
        {
            counters.size[index] = counters.size[index].saturating_add(1);
        }
        counters.size_sum = counters.size_sum.saturating_add(record.request_bytes);
    }

    #[must_use]
    pub fn snapshot(&self, min_requests: u64, min_bytes: u64) -> Vec<UsageRow> {
        let Ok(series) = self.series.lock() else {
            return Vec::new();
        };
        series
            .values()
            .map(|c| c.row.clone())
            .filter(|row| {
                row.requests >= min_requests
                    && row.request_bytes.saturating_add(row.response_bytes) >= min_bytes
            })
            .collect()
    }

    #[must_use]
    pub fn render_prometheus(&self, min_requests: u64) -> String {
        let Ok(series) = self.series.lock() else {
            return String::new();
        };
        let mut output = String::from(PROMETHEUS_HEADER);
        let hist = append_series_metrics(&series, min_requests, &mut output);
        append_size_histograms(&series, min_requests, hist, &mut output);
        output
    }
}

const PROMETHEUS_HEADER: &str = "# HELP sqrzl_requests_total Requests by provider, bucket, client, and operation.\n# TYPE sqrzl_requests_total counter\n# HELP sqrzl_request_bytes_total Request payload bytes observed.\n# TYPE sqrzl_request_bytes_total counter\n# HELP sqrzl_response_bytes_total Response payload bytes observed.\n# TYPE sqrzl_response_bytes_total counter\n# HELP sqrzl_list_requests_total List operations handled.\n# TYPE sqrzl_list_requests_total counter\n# HELP sqrzl_list_items_total Items returned by JSON list operations.\n# TYPE sqrzl_list_items_total counter\n# HELP sqrzl_request_size_bytes Request size histogram in bytes.\n# TYPE sqrzl_request_size_bytes histogram\n";

fn append_series_metrics(
    series: &BTreeMap<Labels, Counters>,
    min_requests: u64,
    output: &mut String,
) -> BTreeMap<(String, String), [u64; SIZE_BOUNDS.len()]> {
    let mut hist = BTreeMap::<(String, String), [u64; SIZE_BOUNDS.len()]>::new();
    for (labels, counters) in series
        .iter()
        .filter(|(_, counters)| counters.row.requests >= min_requests)
    {
        let common = format!(
            "provider=\"{}\",bucket=\"{}\",client=\"{}\",operation=\"{}\"",
            escape(&labels.0),
            escape(&labels.1),
            escape(&labels.2),
            escape(&labels.3)
        );
        sample(
            output,
            "sqrzl_requests_total",
            &common,
            counters.row.requests,
        );
        sample(
            output,
            "sqrzl_request_bytes_total",
            &common,
            counters.row.request_bytes,
        );
        sample(
            output,
            "sqrzl_response_bytes_total",
            &common,
            counters.row.response_bytes,
        );
        sample(
            output,
            "sqrzl_list_requests_total",
            &common,
            counters.row.list_requests,
        );
        sample(
            output,
            "sqrzl_list_items_total",
            &common,
            counters.row.list_items,
        );
        let entry = hist
            .entry((labels.0.clone(), labels.3.clone()))
            .or_default();
        for (total, count) in entry.iter_mut().zip(counters.size) {
            *total = total.saturating_add(count);
        }
    }
    hist
}

fn append_size_histograms(
    series: &BTreeMap<Labels, Counters>,
    min_requests: u64,
    hist: BTreeMap<(String, String), [u64; SIZE_BOUNDS.len()]>,
    output: &mut String,
) {
    for ((provider, operation), counts) in hist {
        let mut cumulative = 0_u64;
        for (index, upper) in SIZE_BOUNDS.iter().enumerate() {
            cumulative = cumulative.saturating_add(counts[index]);
            let bound = if *upper == u64::MAX {
                "+Inf".to_string()
            } else {
                upper.to_string()
            };
            let labels = format!(
                "provider=\"{}\",operation=\"{}\",le=\"{}\"",
                escape(&provider),
                escape(&operation),
                bound
            );
            sample(
                output,
                "sqrzl_request_size_bytes_bucket",
                &labels,
                cumulative,
            );
        }
        let labels = format!(
            "provider=\"{}\",operation=\"{}\"",
            escape(&provider),
            escape(&operation)
        );
        sample(
            output,
            "sqrzl_request_size_bytes_count",
            &labels,
            cumulative,
        );
        let size_sum = series
            .iter()
            .filter(|(labels, counters)| {
                labels.0 == provider
                    && labels.3 == operation
                    && counters.row.requests >= min_requests
            })
            .map(|(_, counters)| counters.size_sum)
            .fold(0_u64, u64::saturating_add);
        sample(output, "sqrzl_request_size_bytes_sum", &labels, size_sum);
    }
}

fn path(uri: &Uri) -> &str {
    uri.path()
}
fn host(headers: &HeaderMap) -> &str {
    headers
        .get("host")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
}
fn client_name(value: &str) -> Option<String> {
    serde_json::from_str::<serde_json::Value>(value)
        .ok()
        .and_then(|v| {
            v.get("name")
                .and_then(serde_json::Value::as_str)
                .map(ToOwned::to_owned)
        })
        .or_else(|| Some(value.split(['/', ' ']).next().unwrap_or(value).to_string()))
}
fn provider(path: &str, host: &str, headers: &HeaderMap) -> String {
    if path.starts_with("/admin/v1/mailboxes") {
        return "mail_admin".into();
    }
    if path.starts_with("/admin/v1/text-") {
        return "sms_admin".into();
    }
    if path.starts_with("/admin/v1") {
        return "admin".into();
    }
    if path == "/mcp" {
        return "mcp".into();
    }
    if headers.contains_key("x-ms-version") || path.starts_with("/devstoreaccount") {
        return "azure".into();
    }
    if host.contains("storage.googleapis.com") || path.starts_with("/storage/") {
        return "gcs".into();
    }
    if host.contains("oraclecloud") || path.starts_with("/n/") {
        return "oci".into();
    }
    if path.contains("/sms/") || path.contains("twilio") {
        return "sms".into();
    }
    if path.contains("/email/") || path.contains("/mail/") {
        return "mail".into();
    }
    "s3".into()
}
fn bucket(path: &str, host: &str, provider: &str) -> String {
    if let Some(rest) = path.strip_prefix("/admin/v1/buckets/") {
        return rest.split('/').next().unwrap_or_default().into();
    }
    match provider {
        "azure" => path
            .trim_start_matches('/')
            .split('/')
            .nth(1)
            .unwrap_or_default()
            .into(),
        "gcs" if path.starts_with("/storage/v1/b/") => path
            .trim_start_matches("/storage/v1/b/")
            .split('/')
            .next()
            .unwrap_or_default()
            .into(),
        "oci" if path.starts_with("/n/") => path
            .split("/b/")
            .nth(1)
            .and_then(|v| v.split('/').next())
            .unwrap_or_default()
            .into(),
        _ if host.contains(".s3") => host.split('.').next().unwrap_or_default().into(),
        _ => path
            .trim_start_matches('/')
            .split('/')
            .next()
            .filter(|v| !v.is_empty())
            .unwrap_or_default()
            .into(),
    }
}
fn operation(method: &Method, path: &str, query: &str, provider: &str) -> String {
    if path == "/healthz" {
        return "health".into();
    }
    if path == "/metrics" {
        return "metrics".into();
    }
    if path == "/mcp" {
        return "mcp_http".into();
    }
    if path.starts_with("/admin/v1") {
        let family = if path.starts_with("/admin/v1/mailboxes") {
            "mail"
        } else if path.starts_with("/admin/v1/text-") {
            "text"
        } else {
            "storage"
        };
        let action = if *method == Method::GET
            && (path == "/admin/v1/buckets"
                || path.ends_with("/objects")
                || path.ends_with("/versions")
                || path.ends_with("/multipart-uploads")
                || path.ends_with("/messages")
                || path.ends_with("/mailboxes")
                || path.ends_with("/text-conversations"))
        {
            "list"
        } else if path.ends_with("/content")
            || path.contains("/attachments/")
            || path.contains("/media/")
        {
            "content"
        } else if path.contains("/text-") && *method == Method::POST {
            "action"
        } else if *method == Method::DELETE {
            "delete"
        } else if *method == Method::PUT {
            "update"
        } else if *method == Method::POST {
            "create"
        } else {
            "get"
        };
        return format!("{family}_{action}");
    }
    let list = is_provider_list(method, path, query, provider);
    if list {
        return format!("{provider}_list");
    }
    let verb = match *method {
        Method::GET => "get",
        Method::HEAD => "head",
        Method::PUT => "put",
        Method::POST => "post",
        Method::DELETE => "delete",
        _ => "other",
    };
    format!("{provider}_{verb}")
}

fn is_provider_list(method: &Method, path: &str, query: &str, provider: &str) -> bool {
    if *method != Method::GET {
        return false;
    }
    let segments = path
        .trim_matches('/')
        .split('/')
        .filter(|part| !part.is_empty())
        .count();
    match provider {
        "s3" => {
            if segments == 0 {
                return true;
            }
            segments == 1
                || query_has_any_key(query, &["list-type", "versions", "uploads", "uploadId"])
        }
        "azure" => query_has_value(query, "comp", "list"),
        "gcs" => {
            path == "/storage/v1/b"
                || path.ends_with("/o")
                || (!path.starts_with("/storage/v1/") && segments <= 1)
        }
        "oci" => path.starts_with("/n/") && path.ends_with("/o"),
        _ => false,
    }
}

fn query_has_any_key(query: &str, keys: &[&str]) -> bool {
    query.split('&').any(|part| {
        let key = part.split_once('=').map_or(part, |(key, _)| key);
        let decoded =
            urlencoding::decode(key).map_or_else(|_| key.to_string(), std::borrow::Cow::into_owned);
        keys.contains(&decoded.as_str())
    })
}

fn query_has_value(query: &str, name: &str, value: &str) -> bool {
    query.split('&').any(|part| {
        let Some((key, raw_value)) = part.split_once('=') else {
            return false;
        };
        let decoded_key =
            urlencoding::decode(key).map_or_else(|_| key.to_string(), std::borrow::Cow::into_owned);
        let decoded_value = urlencoding::decode(raw_value)
            .map_or_else(|_| raw_value.to_string(), std::borrow::Cow::into_owned);
        decoded_key == name && decoded_value == value
    })
}
fn list_items(bytes: &[u8]) -> u64 {
    if let Ok(value) = serde_json::from_slice::<serde_json::Value>(bytes) {
        return [
            "items",
            "objects",
            "buckets",
            "containers",
            "uploads",
            "versions",
            "messages",
        ]
        .iter()
        .find_map(|name| value.get(*name).and_then(serde_json::Value::as_array))
        .map_or(0, |items| items.len() as u64);
    }
    let text = String::from_utf8_lossy(bytes);
    [
        "<Contents>",
        "<Blob>",
        "<BlobPrefix>",
        "<Object>",
        "<Upload>",
        "<Bucket>",
        "<Container>",
        "<CommonPrefix>",
    ]
    .iter()
    .map(|tag| text.matches(tag).count() as u64)
    .sum()
}
fn label(value: &str, fallback: &str) -> String {
    let value: String = value
        .trim()
        .chars()
        .filter(|c| !c.is_control())
        .take(96)
        .collect();
    if value.is_empty() {
        fallback.into()
    } else {
        value
    }
}
fn escape(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
}
fn sample(output: &mut String, name: &str, labels: &str, value: u64) {
    output.push_str(name);
    output.push('{');
    output.push_str(labels);
    output.push_str("} ");
    output.push_str(&value.to_string());
    output.push('\n');
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use http::HeaderValue;

    #[test]
    fn should_record_usage_dimensions_size_and_list_activity() {
        // Arrange
        let metrics = UsageMetrics::default();
        let response = Response::builder()
            .body(Body::from(Bytes::from_static(br#"{"items":[1,2]}"#)))
            .unwrap();
        let headers = HeaderMap::from_iter([
            (
                http::header::HOST,
                HeaderValue::from_static("demo.s3.amazonaws.com"),
            ),
            (
                http::header::USER_AGENT,
                HeaderValue::from_static("aws-cli/2.0"),
            ),
        ]);
        // Act
        metrics.record_http_request(
            &Method::GET,
            &"/objects?list-type=2".parse().unwrap(),
            &headers,
            10,
            &response,
            None,
        );
        let rows = metrics.snapshot(1, 0);
        // Assert
        assert_eq!(rows.len(), 1);
        assert_eq!(
            (&*rows[0].provider, &*rows[0].bucket, &*rows[0].client),
            ("s3", "demo", "aws-cli")
        );
        assert_eq!((rows[0].list_requests, rows[0].list_items), (1, 2));
        assert!(metrics
            .render_prometheus(1)
            .contains("sqrzl_request_size_bytes_bucket"));
    }

    #[test]
    fn should_apply_caller_selected_thresholds() {
        // Arrange
        let metrics = UsageMetrics::default();

        // Act
        metrics.record_mcp_tool("bucket_list", None, "unknown", 4, 12, 0);

        // Assert
        assert_eq!(metrics.snapshot(2, 0).len(), 0);
        assert_eq!(metrics.snapshot(1, 17).len(), 0);
        assert_eq!(metrics.snapshot(1, 16).len(), 1);
    }

    #[test]
    fn should_count_provider_list_shapes_without_scanning_object_payloads() {
        // Arrange
        let metrics = UsageMetrics::default();
        let headers = HeaderMap::new();
        let list_response = Response::builder()
            .body(Body::from(Bytes::from_static(br#"{"objects":[{},{}]}"#)))
            .unwrap();
        let object_response = Response::builder()
            .body(Body::from(Bytes::from_static(br#"{"items":[1,2,3]}"#)))
            .unwrap();
        let gcs_response = Response::builder()
            .body(Body::from(Bytes::from_static(br#"{"items":[{},{}]}"#)))
            .unwrap();
        let azure_response = Response::builder()
            .body(Body::from(Bytes::from_static(
                b"<EnumerationResults><Blobs><Blob></Blob><Blob></Blob></Blobs></EnumerationResults>",
            )))
            .unwrap();

        // Act
        metrics.record_http_request(
            &Method::GET,
            &"/n/ns/b/demo/o".parse().unwrap(),
            &headers,
            0,
            &list_response,
            Some("oci"),
        );
        metrics.record_http_request(
            &Method::GET,
            &"/demo/key".parse().unwrap(),
            &headers,
            0,
            &object_response,
            Some("s3"),
        );
        metrics.record_http_request(
            &Method::GET,
            &"/storage/v1/b/demo/o".parse().unwrap(),
            &headers,
            0,
            &gcs_response,
            Some("gcs"),
        );
        let mut azure_headers = HeaderMap::new();
        azure_headers.insert("x-ms-version", HeaderValue::from_static("2023-11-03"));
        metrics.record_http_request(
            &Method::GET,
            &"/devstoreaccount1/demo?restype=container&comp=list"
                .parse()
                .unwrap(),
            &azure_headers,
            0,
            &azure_response,
            None,
        );

        // Assert
        let rows = metrics.snapshot(1, 0);
        let oci = rows.iter().find(|row| row.provider == "oci").unwrap();
        let s3 = rows.iter().find(|row| row.provider == "s3").unwrap();
        let gcs = rows.iter().find(|row| row.provider == "gcs").unwrap();
        let azure = rows.iter().find(|row| row.provider == "azure").unwrap();
        assert_eq!((oci.list_requests, oci.list_items), (1, 2));
        assert_eq!((s3.list_requests, s3.list_items), (0, 0));
        assert_eq!((gcs.list_requests, gcs.list_items), (1, 2));
        assert_eq!((azure.list_requests, azure.list_items), (1, 2));
    }

    #[test]
    fn should_classify_provider_send_endpoints_and_bucket_creation_as_mutations() {
        // Arrange
        let metrics = UsageMetrics::default();
        let response = Response::builder()
            .body(Body::from(Bytes::from_static(br#"{"accepted":true}"#)))
            .unwrap();
        let headers = HeaderMap::new();

        // Act
        metrics.record_http_request(
            &Method::POST,
            &"/emails:send?api-version=2025-09-01".parse().unwrap(),
            &headers,
            32,
            &response,
            Some("acs"),
        );
        metrics.record_http_request(
            &Method::POST,
            &"/admin/v1/buckets".parse().unwrap(),
            &headers,
            12,
            &response,
            None,
        );

        // Assert
        let rows = metrics.snapshot(1, 0);
        assert!(rows
            .iter()
            .any(|row| row.provider == "acs" && row.operation == "acs_post"));
        let create = rows
            .iter()
            .find(|row| row.operation == "storage_create")
            .unwrap();
        assert_eq!(create.list_requests, 0);
        let prometheus = metrics.render_prometheus(1);
        assert!(prometheus.contains("sqrzl_request_size_bytes_sum"));
    }
}
