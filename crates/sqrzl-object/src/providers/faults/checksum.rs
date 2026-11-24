use crate::body::Body;
use http::HeaderValue;
use http_body_util::BodyExt;
use hyper::Response;

/// Change only the response receipt, leaving the committed bytes and metadata intact.
pub(super) async fn rewrite(
    response: Response<Body>,
    name: &str,
) -> Result<Response<Body>, String> {
    let missing = name == "checksum-receipt-missing";
    let (mut parts, body) = response.into_parts();
    let bytes = body.collect().await.map_err(|e| e.to_string())?.to_bytes();
    let mut text = String::from_utf8(bytes.to_vec()).map_err(|e| e.to_string())?;
    for header in ["x-amz-checksum-crc32c", "x-goog-hash"] {
        if let Some(value) = parts.headers.remove(header) {
            if !missing {
                let original = value.to_str().map_err(|e| e.to_string())?;
                let value = if header == "x-goog-hash" {
                    format!(
                        "crc32c={}",
                        corrupt(original.strip_prefix("crc32c=").unwrap_or(original))
                    )
                } else {
                    corrupt(original).to_string()
                };
                parts.headers.insert(
                    header,
                    HeaderValue::from_str(&value).map_err(|e| e.to_string())?,
                );
            }
        }
    }
    if let Ok(mut json) = serde_json::from_str::<serde_json::Value>(&text) {
        if let Some(object) = json.as_object_mut() {
            if let Some(original) = object.remove("crc32c") {
                if !missing {
                    object.insert(
                        "crc32c".to_string(),
                        serde_json::Value::String(
                            corrupt(original.as_str().unwrap_or_default()).to_string(),
                        ),
                    );
                }
                text = json.to_string();
            }
        }
    } else if let Some(start) = text.find("<ChecksumCRC32C>") {
        let value_start = start + "<ChecksumCRC32C>".len();
        if let Some(end) = text[value_start..].find("</ChecksumCRC32C>") {
            let end = value_start + end;
            if missing {
                text.replace_range(start..end + "</ChecksumCRC32C>".len(), "");
            } else {
                let value = corrupt(&text[value_start..end]);
                text.replace_range(value_start..end, value);
            }
        }
    }
    parts.headers.insert(
        "x-sqrzl-failpoint-applied",
        HeaderValue::from_str(name).map_err(|e| e.to_string())?,
    );
    parts.headers.insert(
        "content-length",
        HeaderValue::from_str(&text.len().to_string()).map_err(|e| e.to_string())?,
    );
    Ok(Response::from_parts(parts, Body::from(text)))
}

fn corrupt(original: &str) -> &'static str {
    if original == "AQAAAA==" {
        "AgAAAA=="
    } else {
        "AQAAAA=="
    }
}
