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
            .header("x-amzn-errortype", "BadRequestException")
            .content_type("application/json")
            .body(
                serde_json::json!({
                    "message": message,
                })
                .to_string()
                .into_bytes(),
            )
            .build()
    }

    // Validate the complete SES document before constructing a capturable
    // message; the linear shape mirrors the provider's nested JSON schema.
    #[allow(clippy::too_many_lines)]
    fn parse_message(req: &MailRequest) -> Result<Message, String> {
        let payload = serde_json::from_slice::<Value>(&req.body)
            .map_err(|err| format!("invalid SES request body: {err}"))?;
        let payload = payload
            .as_object()
            .ok_or_else(|| "SES request body must be an object".to_string())?;
        reject_unsupported_fields(
            payload,
            &[
                "FromEmailAddress",
                "Destination",
                "ReplyToAddresses",
                "Content",
            ],
            "SES SendEmail",
        )?;

        let from = payload
            .get("FromEmailAddress")
            .and_then(Value::as_str)
            .and_then(parse_ses_address)
            .ok_or_else(|| "FromEmailAddress is required".to_string())?;

        let destination = payload
            .get("Destination")
            .and_then(Value::as_object)
            .ok_or_else(|| "Destination is required".to_string())?;
        reject_unsupported_fields(
            destination,
            &["ToAddresses", "CcAddresses", "BccAddresses"],
            "SES Destination",
        )?;
        let to = parse_addresses(destination.get("ToAddresses"))?;
        let cc = parse_addresses(destination.get("CcAddresses"))?;
        let bcc = parse_addresses(destination.get("BccAddresses"))?;
        if to.len() + cc.len() + bcc.len() == 0 {
            return Err("Destination must include at least one recipient".to_string());
        }
        if to.len() + cc.len() + bcc.len() > 50 {
            return Err("Destination supports at most 50 recipients".to_string());
        }
        let reply_to = parse_addresses(payload.get("ReplyToAddresses"))?;
        if reply_to.len() > 50 {
            return Err("ReplyToAddresses supports at most 50 addresses".to_string());
        }

        let content = payload
            .get("Content")
            .and_then(Value::as_object)
            .ok_or_else(|| "Content is required".to_string())?;
        if content.contains_key("Raw") || content.contains_key("Template") {
            return Err("only SES Content.Simple is supported by this emulator".to_string());
        }
        reject_unsupported_fields(content, &["Simple"], "SES Content")?;
        let simple = content
            .get("Simple")
            .and_then(Value::as_object)
            .ok_or_else(|| "Content.Simple is required".to_string())?;
        if simple.contains_key("Attachments") {
            return Err("SES Simple attachments are not supported by this emulator".to_string());
        }
        reject_unsupported_fields(
            simple,
            &["Subject", "Body", "Headers", "Attachments"],
            "SES Content.Simple",
        )?;
        let (subject, subject_charset) = parse_required_content_data(
            simple.get("Subject"),
            "Content.Simple.Subject.Data is required",
        )?;
        let body = simple
            .get("Body")
            .and_then(Value::as_object)
            .ok_or_else(|| "Content.Simple.Body is required".to_string())?;
        reject_unsupported_fields(body, &["Text", "Html"], "SES Content.Simple.Body")?;
        let body_text = parse_content_data(body.get("Text"))?;
        let body_html = parse_content_data(body.get("Html"))?;
        if body_text.is_none() && body_html.is_none() {
            return Err("Content.Simple.Body must include Text or Html".to_string());
        }
        let headers = parse_message_headers(simple.get("Headers"))?;
        let mut provider_metadata = std::collections::HashMap::new();
        if let Some(charset) = subject_charset {
            provider_metadata.insert("subject_charset".to_string(), Value::String(charset));
        }
        let (body_text, text_charset) = body_text.unzip();
        let (body_html, html_charset) = body_html.unzip();
        if let Some(charset) = text_charset.flatten() {
            provider_metadata.insert("text_charset".to_string(), Value::String(charset));
        }
        if let Some(charset) = html_charset.flatten() {
            provider_metadata.insert("html_charset".to_string(), Value::String(charset));
        }

        Ok(Message {
            source_protocol: SourceProtocol::Ses,
            from,
            to,
            cc,
            bcc,
            reply_to,
            subject,
            headers,
            body_text,
            body_html,
            attachments: Vec::new(),
            user_engagement_tracking_disabled: None,
            provider_metadata,
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

        let scope_parts = credential_scope.split('/').collect::<Vec<_>>();
        if scope_parts.len() != 4 || scope_parts[2] != "ses" || scope_parts[3] != "aws4_request" {
            return false;
        }

        if access_key_from_signature != access_key {
            return false;
        }

        let amz_date = req
            .header("x-amz-date")
            .or_else(|| req.header("date"))
            .unwrap_or("");
        if amz_date.is_empty() || !signed_headers.iter().any(|name| name == "host") {
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

    fn matches_request_head(&self, method: &Method, uri: &Uri, _headers: &HeaderMap) -> bool {
        method == Method::POST && uri.path() == "/v2/email/outbound-emails"
    }

    fn payload_too_large(&self, max_request_bytes: usize) -> Response<Body> {
        ResponseBuilder::new(StatusCode::PAYLOAD_TOO_LARGE)
            .header("x-amzn-errortype", "RequestEntityTooLarge")
            .content_type("application/x-amz-json-1.1")
            .body(
                serde_json::json!({"message":format!("Request body exceeds the {max_request_bytes}-byte emulator limit")})
                    .to_string()
                    .into_bytes(),
            )
            .build()
    }

    fn incomplete_body(&self) -> Response<Body> {
        Self::invalid_request_response("The request body ended before it was complete")
    }

    fn handle<'a>(
        &'a self,
        mail: Arc<dyn MailStore>,
        auth_config: Arc<AuthConfig>,
        req: MailRequest,
    ) -> Pin<Box<dyn Future<Output = Result<Response<Body>, String>> + Send + 'a>> {
        Box::pin(async move {
            if req.method() != Method::POST {
                return Ok(ResponseBuilder::new(StatusCode::METHOD_NOT_ALLOWED)
                    .header("allow", "POST")
                    .content_type("application/x-amz-json-1.1")
                    .body(
                        serde_json::json!({"message":"SendEmail requires HTTP POST"})
                            .to_string()
                            .into_bytes(),
                    )
                    .build());
            }
            if !Self::is_authorized(&req, auth_config.as_ref()) {
                return Ok(ResponseBuilder::new(StatusCode::FORBIDDEN)
                    .header("x-amzn-errortype", "MissingAuthenticationTokenException")
                    .content_type("application/x-amz-json-1.1")
                    .body(
                        serde_json::json!({"message":"Missing Authentication Token"})
                            .to_string()
                            .into_bytes(),
                    )
                    .build());
            }
            if !req
                .header("content-type")
                .and_then(|value| value.split(';').next())
                .is_some_and(|value| value.trim().eq_ignore_ascii_case("application/json"))
            {
                return Ok(ResponseBuilder::new(StatusCode::UNSUPPORTED_MEDIA_TYPE)
                    .header("x-amzn-errortype", "BadRequestException")
                    .content_type("application/json")
                    .body(
                        serde_json::json!({"message":"SendEmail requires application/json content"})
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
                .content_type("application/json")
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

fn parse_addresses(value: Option<&Value>) -> Result<Vec<Address>, String> {
    let Some(value) = value else {
        return Ok(Vec::new());
    };
    let values = value
        .as_array()
        .ok_or_else(|| "SES destination fields must be arrays".to_string())?;
    values
        .iter()
        .map(|value| {
            value
                .as_str()
                .and_then(parse_ses_address)
                .ok_or_else(|| "SES addresses must be valid email strings".to_string())
        })
        .collect()
}

fn parse_content_data(value: Option<&Value>) -> Result<Option<(String, Option<String>)>, String> {
    let Some(value) = value else {
        return Ok(None);
    };
    parse_required_content_data(Some(value), "SES message content must include Data").map(Some)
}

fn parse_required_content_data(
    value: Option<&Value>,
    missing: &str,
) -> Result<(String, Option<String>), String> {
    let object = value
        .and_then(Value::as_object)
        .ok_or_else(|| missing.to_string())?;
    reject_unsupported_fields(object, &["Data", "Charset"], "SES Content")?;
    let data = object
        .get("Data")
        .and_then(Value::as_str)
        .ok_or_else(|| missing.to_string())?
        .to_string();
    let charset = match object.get("Charset") {
        None => None,
        Some(Value::String(value)) if !value.is_empty() => Some(value.clone()),
        Some(_) => return Err("SES Content.Charset must be a non-empty string".to_string()),
    };
    Ok((data, charset))
}

fn parse_message_headers(
    value: Option<&Value>,
) -> Result<std::collections::HashMap<String, String>, String> {
    const DISALLOWED: &[&str] = &[
        "bcc",
        "cc",
        "content-disposition",
        "content-type",
        "date",
        "from",
        "message-id",
        "mime-version",
        "reply-to",
        "return-path",
        "subject",
        "to",
    ];
    let Some(value) = value else {
        return Ok(std::collections::HashMap::new());
    };
    let values = value
        .as_array()
        .ok_or_else(|| "SES Content.Simple.Headers must be an array".to_string())?;
    if values.len() > 15 {
        return Err("SES Content.Simple.Headers supports at most 15 headers".to_string());
    }
    let mut headers = std::collections::HashMap::new();
    let mut names = std::collections::HashSet::new();
    for value in values {
        let object = value
            .as_object()
            .ok_or_else(|| "SES message headers must be objects".to_string())?;
        reject_unsupported_fields(object, &["Name", "Value"], "SES message header")?;
        let name = object
            .get("Name")
            .and_then(Value::as_str)
            .filter(|name| {
                !name.is_empty()
                    && name.len() <= 126
                    && name
                        .bytes()
                        .all(|byte| (33..=126).contains(&byte) && byte != b':')
            })
            .ok_or_else(|| "SES message header Name is invalid".to_string())?;
        let header_value = object
            .get("Value")
            .and_then(Value::as_str)
            .filter(|value| {
                !value.is_empty()
                    && value.len() <= 995
                    && value.bytes().all(|byte| (32..=126).contains(&byte))
            })
            .ok_or_else(|| "SES message header Value is invalid".to_string())?;
        if name.len() + header_value.len() > 996 {
            return Err("SES message header Name and Value exceed 996 characters".to_string());
        }
        if DISALLOWED
            .iter()
            .any(|reserved| name.eq_ignore_ascii_case(reserved))
        {
            return Err(format!(
                "SES Simple custom header {name} is set by SES and cannot be supplied"
            ));
        }
        if !names.insert(name.to_ascii_lowercase()) {
            return Err(
                "duplicate SES Simple headers are not supported by this emulator".to_string(),
            );
        }
        headers.insert(name.to_string(), header_value.to_string());
    }
    Ok(headers)
}

fn reject_unsupported_fields(
    object: &serde_json::Map<String, Value>,
    allowed: &[&str],
    scope: &str,
) -> Result<(), String> {
    if let Some(name) = object.keys().find(|name| !allowed.contains(&name.as_str())) {
        return Err(format!(
            "{scope} field {name} is not supported by this emulator"
        ));
    }
    Ok(())
}

fn valid_email_address(value: &str) -> bool {
    if value.is_empty()
        || value.len() > 320
        || !value.is_ascii()
        || value
            .bytes()
            .any(|byte| byte.is_ascii_whitespace() || byte.is_ascii_control())
    {
        return false;
    }
    let mut parts = value.split('@');
    let (Some(local), Some(domain), None) = (parts.next(), parts.next(), parts.next()) else {
        return false;
    };
    !local.is_empty()
        && local.len() <= 64
        && !local.starts_with('.')
        && !local.ends_with('.')
        && !local.contains("..")
        && !domain.is_empty()
        && domain.len() <= 255
        && !domain.starts_with('.')
        && !domain.ends_with('.')
        && !domain.contains("..")
        && domain.split('.').all(|label| {
            !label.is_empty()
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        })
}

fn parse_ses_address(value: &str) -> Option<Address> {
    let value = value.trim();
    if !value.is_ascii() {
        return None;
    }
    let (email, name) = if value.ends_with('>') {
        let start = value.rfind('<')?;
        let email = value.get(start + 1..value.len() - 1)?.trim();
        let display = value[..start].trim();
        let display = display
            .strip_prefix('"')
            .and_then(|value| value.strip_suffix('"'))
            .unwrap_or(display)
            .trim();
        (email, (!display.is_empty()).then(|| display.to_string()))
    } else {
        (value, None)
    };
    valid_email_address(email).then(|| Address {
        email: email.to_string(),
        name,
    })
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
    let key = build_signature_key(secret, date, "us-east-1", "ses");
    let string_to_sign = format!(
        "AWS4-HMAC-SHA256\n{amz_date}\n{date}/us-east-1/ses/aws4_request\n{}",
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
            "AWS4-HMAC-SHA256 Credential={access_key}/20260101/us-east-1/ses/aws4_request, SignedHeaders=host;x-amz-content-sha256;x-amz-date, Signature={signature}"
        );

        crate::server::RequestExt::from_hyper(
            hyper::Request::builder()
                .method("POST")
                .uri("http://localhost/v2/email/outbound-emails")
                .header("authorization", auth_header)
                .header("host", "localhost:9000")
                .header("x-amz-date", amz_date)
                .header("x-amz-content-sha256", "UNSIGNED-PAYLOAD")
                .header("content-type", "application/json")
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
