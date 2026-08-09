use crate::auth::{AuthConfig, SigV4Config, SignatureVerifier};
use crate::body::Body;
use crate::mail::model::{Address, Message, SourceProtocol};
use crate::mail::providers::MailAdapter;
use crate::mail::{fan_out, MailStore};
use crate::server::{RequestExt as MailRequest, ResponseBuilder};
use hex::encode as hex_encode;
#[cfg(test)]
use hmac::{Hmac, KeyInit, Mac};
use http::{HeaderMap, Method, StatusCode, Uri};
use hyper::Response;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

pub struct SesEmailAdapter;

impl SesEmailAdapter {
    fn invalid_request_response(message: &str) -> Response<Body> {
        ResponseBuilder::new(StatusCode::BAD_REQUEST)
            .content_type("application/x-amz-json-1.1")
            .body(
                serde_json::json!({
                    "__type": "InvalidParameterValue",
                    "message": message,
                })
                .to_string()
                .into_bytes(),
            )
            .build()
    }

    fn parse_message(req: &MailRequest) -> Result<Message, String> {
        let payload = serde_json::from_slice::<Value>(&req.body)
            .map_err(|err| format!("invalid SES request body: {err}"))?;

        let from = payload
            .get("FromEmailAddress")
            .or_else(|| payload.get("fromEmailAddress"))
            .and_then(Value::as_str)
            .map_or_else(|| Address::new("unknown@localhost"), Address::new);

        let destination = payload
            .get("Destination")
            .and_then(Value::as_object)
            .cloned();
        let (to, cc, bcc) = destination.as_ref().map_or_else(
            || (Vec::new(), Vec::new(), Vec::new()),
            |value| {
                (
                    parse_addresses(value.get("ToAddresses").and_then(Value::as_array)),
                    parse_addresses(value.get("CcAddresses").and_then(Value::as_array)),
                    parse_addresses(value.get("BccAddresses").and_then(Value::as_array)),
                )
            },
        );

        let mut subject = String::new();
        let mut body_text = None;
        let mut body_html = None;
        if let Some(content) = payload.get("Content").and_then(Value::as_object) {
            if let Some(simple) = content.get("Simple").and_then(Value::as_object) {
                if let Some(subject_value) = simple.get("Subject").and_then(Value::as_object) {
                    if let Some(value) = subject_value.get("Data").and_then(Value::as_str) {
                        subject = value.to_string();
                    }
                }
                if let Some(body_value) = simple.get("Body").and_then(Value::as_object) {
                    let body_text_value = body_value
                        .get("Text")
                        .and_then(Value::as_object)
                        .and_then(|value| value.get("Data").and_then(Value::as_str))
                        .map(ToString::to_string);
                    let body_html_value = body_value
                        .get("Html")
                        .and_then(Value::as_object)
                        .and_then(|value| value.get("Data").and_then(Value::as_str))
                        .map(ToString::to_string);
                    body_text = body_text.or(body_text_value);
                    body_html = body_html.or(body_html_value);
                }
            }

            if subject.is_empty() {
                subject = payload
                    .get("FromEmailAddress")
                    .and_then(Value::as_str)
                    .unwrap_or("message")
                    .to_string();
            }
        } else {
            subject = payload
                .get("subject")
                .or_else(|| payload.get("Subject"))
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            body_text = payload
                .get("text")
                .and_then(Value::as_str)
                .map(ToString::to_string)
                .or_else(|| {
                    payload
                        .get("body")
                        .and_then(Value::as_str)
                        .map(ToString::to_string)
                });
            if body_html.is_none() {
                body_html.clone_from(&body_text);
            }
        }

        Ok(Message {
            source_protocol: SourceProtocol::Ses,
            from,
            to,
            cc,
            bcc,
            subject,
            headers: std::collections::HashMap::new(),
            body_text,
            body_html,
            attachments: Vec::new(),
            raw_mime: None,
            thread_id: None,
        })
    }

    fn is_authorized(req: &MailRequest, auth_config: &AuthConfig) -> bool {
        if !auth_config.enforce_auth {
            return true;
        }

        let Some(auth_header) = req.header("authorization") else {
            return false;
        };
        if !auth_header.starts_with("AWS4-HMAC-SHA256") {
            return false;
        }

        let Some(access_key) = auth_config.access_key() else {
            return false;
        };

        let Some(signature) = parse_sigv4_signature(auth_header) else {
            return false;
        };

        let Some(signed_headers) = parse_sigv4_signed_headers(auth_header) else {
            return false;
        };
        if signed_headers.is_empty() {
            return false;
        }

        let Some((access_key_from_signature, credential_scope)) =
            parse_sigv4_credential(auth_header)
        else {
            return false;
        };

        let Some(secret_key) = auth_config.secret_key() else {
            return false;
        };

        if !credential_scope.contains("aws4_request") {
            return false;
        }

        if access_key_from_signature != access_key {
            return false;
        }

        let amz_date = req
            .header("x-amz-date")
            .or_else(|| req.header("date"))
            .unwrap_or("");
        if amz_date.is_empty() {
            return false;
        }

        let canonical_request = build_canonical_request(req, &signed_headers);
        let sigv4_config = SigV4Config {
            access_key: access_key.to_string(),
            secret_key: secret_key.to_string(),
        };
        SignatureVerifier::verify(
            &signature,
            &canonical_request,
            amz_date,
            &credential_scope,
            &sigv4_config,
        )
    }
}

impl MailAdapter for SesEmailAdapter {
    fn name(&self) -> &'static str {
        "ses"
    }

    fn matches(&self, req: &MailRequest) -> bool {
        req.path() == "/v2/email/outbound-emails"
    }

    fn matches_request_head(&self, method: &Method, uri: &Uri, headers: &HeaderMap) -> bool {
        method == Method::POST && uri.path() == "/v2/email/outbound-emails" && !headers.is_empty()
    }

    fn handle<'a>(
        &'a self,
        mail: Arc<dyn MailStore>,
        auth_config: Arc<AuthConfig>,
        req: MailRequest,
    ) -> Pin<Box<dyn Future<Output = Result<Response<Body>, String>> + Send + 'a>> {
        Box::pin(async move {
            if !Self::is_authorized(&req, auth_config.as_ref()) {
                return Ok(ResponseBuilder::new(StatusCode::UNAUTHORIZED)
                    .content_type("application/json; charset=utf-8")
                    .body(
                        serde_json::json!({"Message":"Missing Authentication Token","error":"Unauthorized"})
                            .to_string()
                            .into_bytes(),
                    )
                    .build());
            }

            let message = match Self::parse_message(&req) {
                Ok(message) => message,
                Err(message) => return Ok(Self::invalid_request_response(&message)),
            };
            let stored_messages = match fan_out(mail.as_ref(), &message) {
                Ok(stored_messages) => stored_messages,
                Err(crate::error::Error::InvalidRequest(message)) => {
                    return Ok(Self::invalid_request_response(&message));
                }
                Err(err) => return Err(err.to_string()),
            };
            let message_id = stored_messages
                .first()
                .map_or_else(crate::mail::generate_message_id, |stored| {
                    stored.message_id.clone()
                });

            Ok(ResponseBuilder::new(StatusCode::OK)
                .content_type("application/json; charset=utf-8")
                .body(
                    serde_json::json!({
                        "MessageId": message_id,
                    })
                    .to_string()
                    .into_bytes(),
                )
                .build())
        })
    }
}

fn parse_addresses(values: Option<&Vec<Value>>) -> Vec<Address> {
    values
        .unwrap_or(&Vec::new())
        .iter()
        .filter_map(|value| value.as_str())
        .map(Address::new)
        .collect()
}

fn parse_sigv4_signature(auth_header: &str) -> Option<String> {
    auth_header.split(',').map(str::trim).find_map(|part| {
        part.strip_prefix("Signature=")
            .map(std::string::ToString::to_string)
    })
}

fn parse_sigv4_credential(auth_header: &str) -> Option<(String, String)> {
    for part in auth_header.split(',') {
        let part = part.trim();
        if let Some(credential_start) = part.find("Credential=") {
            let credential = &part[credential_start + 11..];
            let mut parts = credential.split('/').map(|value| value.trim().to_string());
            let access_key = parts.next()?;
            let date = parts.next()?;
            let region = parts.next()?;
            let service = parts.next()?;
            let request = parts.next()?;
            if region.is_empty() || service.is_empty() || request.is_empty() {
                return None;
            }
            let credential_scope = format!("{date}/{region}/{service}/{request}");
            return Some((access_key, credential_scope));
        }
    }
    None
}

fn parse_sigv4_signed_headers(auth_header: &str) -> Option<Vec<String>> {
    auth_header.split(',').find_map(|part| {
        let part = part.trim();
        part.strip_prefix("SignedHeaders=").map(|headers| {
            headers
                .split(';')
                .map(|header| header.trim().to_lowercase())
                .filter(|header| !header.is_empty())
                .collect()
        })
    })
}

fn uri_encode(value: &str, encode_slash: bool) -> String {
    let mut out = String::new();
    for byte in value.bytes() {
        let ch = byte as char;
        let keep = ch.is_ascii_alphanumeric()
            || matches!(ch, '-' | '_' | '.' | '~')
            || (!encode_slash && ch == '/');
        if keep {
            out.push(ch);
        } else {
            let _ = std::fmt::Write::write_fmt(&mut out, format_args!("%{byte:02X}"));
        }
    }
    out
}

fn canonical_query_string(query: Option<&str>) -> String {
    let Some(query) = query else {
        return String::new();
    };

    let mut params: Vec<(String, String)> = query
        .split('&')
        .filter(|part| !part.is_empty())
        .map(|part| {
            let (raw_key, raw_value) = part.split_once('=').unwrap_or((part, ""));
            let key = urlencoding::decode(raw_key)
                .map_or_else(|_| raw_key.to_string(), std::borrow::Cow::into_owned);
            let value = urlencoding::decode(raw_value)
                .map_or_else(|_| raw_value.to_string(), std::borrow::Cow::into_owned);
            (uri_encode(&key, true), uri_encode(&value, true))
        })
        .collect();
    params.sort();

    params
        .into_iter()
        .map(|(key, value)| format!("{key}={value}"))
        .collect::<Vec<_>>()
        .join("&")
}

fn canonical_uri(path: &str) -> String {
    if path.is_empty() {
        "/".to_string()
    } else {
        uri_encode(path, false)
    }
}

fn sha256_hex(data: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(data);
    hex_encode(hasher.finalize())
}

fn build_canonical_request(req: &MailRequest, signed_headers: &[String]) -> String {
    let method = req.method();
    let canonical_uri = canonical_uri(req.path());
    let canonical_query = canonical_query_string(req.uri.query());
    let mut canonical_headers: Vec<String> = signed_headers
        .iter()
        .map(|name| {
            let value = req.header(name).unwrap_or("");
            let normalized_value = value.split_whitespace().collect::<Vec<_>>().join(" ");
            format!("{name}:{normalized_value}")
        })
        .collect();
    canonical_headers.sort();

    let canonical_headers_str = canonical_headers.join("\n");
    let signed_headers = {
        let mut names = signed_headers.to_vec();
        names.sort();
        names.join(";")
    };
    let payload_hash = req
        .header("x-amz-content-sha256")
        .filter(|value| !value.is_empty())
        .map_or_else(|| sha256_hex(&req.body), std::string::ToString::to_string);

    format!(
        "{method}\n{canonical_uri}\n{canonical_query}\n{canonical_headers_str}\n\n{signed_headers}\n{payload_hash}"
    )
}

#[cfg(test)]
fn build_signature_key(secret: &str, date: &str, region: &str, service: &str) -> Vec<u8> {
    type HmacSha256 = Hmac<Sha256>;
    let mut mac = HmacSha256::new_from_slice(format!("AWS4{secret}").as_bytes())
        .expect("signing key should be valid");
    mac.update(date.as_bytes());
    let date_key = mac.finalize().into_bytes().to_vec();

    let mut mac = HmacSha256::new_from_slice(&date_key).expect("signing key should be valid");
    mac.update(region.as_bytes());
    let region_key = mac.finalize().into_bytes().to_vec();

    let mut mac = HmacSha256::new_from_slice(&region_key).expect("signing key should be valid");
    mac.update(service.as_bytes());
    let service_key = mac.finalize().into_bytes().to_vec();

    let mut mac = HmacSha256::new_from_slice(&service_key).expect("signing key should be valid");
    mac.update(b"aws4_request");
    mac.finalize().into_bytes().to_vec()
}

#[cfg(test)]
fn sign_signature(secret: &str, canonical_request: &str, date: &str, amz_date: &str) -> String {
    type HmacSha256 = Hmac<Sha256>;
    let key = build_signature_key(secret, date, "us-east-1", "s3");
    let string_to_sign = format!(
        "AWS4-HMAC-SHA256\n{amz_date}\n{date}/us-east-1/s3/aws4_request\n{}",
        sha256_hex(canonical_request.as_bytes())
    );
    let mut mac = HmacSha256::new_from_slice(&key).expect("signing key should be valid");
    mac.update(string_to_sign.as_bytes());
    hex_encode(mac.finalize().into_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::AuthConfig;
    use crate::mail::filesystem::FilesystemMailStore;
    use crate::mail::model::ListMessagesParams;
    use crate::mail::providers::MailAdapter;
    use std::sync::Arc;

    fn temp_mail() -> Arc<dyn MailStore> {
        let dir = std::env::temp_dir().join(format!("sqrzl-mail-ses-{}", uuid::Uuid::new_v4()));
        Arc::new(FilesystemMailStore::open(dir).expect("mail store should open"))
    }

    async fn request_with_signature(body: &str, access_key: &str, secret_key: &str) -> MailRequest {
        let body_payload = body.as_bytes().to_vec();
        let amz_date = "20260101T120000Z";
        let canonical_headers = vec![
            "host".to_string(),
            "x-amz-content-sha256".to_string(),
            "x-amz-date".to_string(),
        ];
        let canonical_request = build_canonical_request(
            &crate::server::RequestExt::from_hyper(
                hyper::Request::builder()
                    .method("POST")
                    .uri("http://localhost/v2/email/outbound-emails")
                    .header("authorization", "placeholder")
                    .header("host", "localhost:9000")
                    .header("x-amz-date", amz_date)
                    .header("x-amz-content-sha256", "UNSIGNED-PAYLOAD")
                    .body(Body::from(body_payload.clone()))
                    .expect("request should build"),
            )
            .await
            .expect("request should parse"),
            &canonical_headers,
        );
        let signature = sign_signature(secret_key, &canonical_request, "20260101", amz_date);

        let auth_header = format!(
            "AWS4-HMAC-SHA256 Credential={access_key}/20260101/us-east-1/s3/aws4_request, SignedHeaders=host;x-amz-content-sha256;x-amz-date, Signature={signature}"
        );

        crate::server::RequestExt::from_hyper(
            hyper::Request::builder()
                .method("POST")
                .uri("http://localhost/v2/email/outbound-emails")
                .header("authorization", auth_header)
                .header("host", "localhost:9000")
                .header("x-amz-date", amz_date)
                .header("x-amz-content-sha256", "UNSIGNED-PAYLOAD")
                .body(Body::from(body_payload))
                .expect("request should build"),
        )
        .await
        .expect("request should parse")
    }

    #[tokio::test]
    async fn should_parse_valid_ses_request_and_store_messages() {
        let mail = temp_mail();
        let body = r#"{"FromEmailAddress":"alice@example.com","Destination":{"ToAddresses":["bob@example.com"]},"Content":{"Simple":{"Subject":{"Data":"ses test"},"Body":{"Text":{"Data":"hello"}}}}}"#;

        let req = request_with_signature(body, "test-key", "test-secret").await;
        let response = SesEmailAdapter
            .handle(
                mail.clone(),
                Arc::new(AuthConfig {
                    access_key_id: Some("test-key".to_string()),
                    secret_access_key: Some("test-secret".to_string()),
                    enforce_auth: true,
                    admin_auth_disabled: false,
                    blobs_path: "./blobs".to_string(),
                    lifecycle_interval: std::time::Duration::from_hours(1),
                    api_port: 9000,
                    ui_port: 9001,
                    max_request_bytes: crate::config::DEFAULT_SQRZL_MAX_REQUEST_BYTES,
                    smtp_port: crate::config::DEFAULT_SQRZL_SMTP_PORT,
                }),
                req,
            )
            .await
            .expect("ses adapter should handle request");

        assert_eq!(response.status(), StatusCode::OK);
        let messages = mail
            .list_messages("bob@example.com", ListMessagesParams::default())
            .expect("list should succeed");
        assert_eq!(messages.messages.len(), 1);
        assert_eq!(messages.messages[0].message.subject, "ses test");
    }
}
