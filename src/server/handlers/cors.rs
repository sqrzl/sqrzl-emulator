use super::ResponseBuilder;
use crate::body::Body;
use crate::server::RequestExt as Request;
use crate::services::bucket as bucket_service;
use crate::storage::BucketStore;
use http::StatusCode;
use hyper::Response;
use quick_xml::escape::unescape;
use quick_xml::events::Event;
use quick_xml::Reader;

const S3_CORS_XML_KEY: &str = "s3_cors_xml";

#[derive(Default)]
struct CorsRule {
    allowed_origins: Vec<String>,
    allowed_methods: Vec<String>,
    allowed_headers: Vec<String>,
    expose_headers: Vec<String>,
    max_age_seconds: Option<String>,
}

enum CorsField {
    AllowedOrigin,
    AllowedMethod,
    AllowedHeader,
    ExposeHeader,
    MaxAgeSeconds,
}

pub(super) fn is_preflight(req: &Request) -> bool {
    req.method() == http::Method::OPTIONS
        && req.header("origin").is_some()
        && req.header("access-control-request-method").is_some()
}

pub(super) fn apply_actual_request_headers(
    storage: &(impl BucketStore + ?Sized),
    bucket: &str,
    req: &Request,
    builder: ResponseBuilder,
) -> ResponseBuilder {
    let Some(origin) = req.header("origin") else {
        return builder;
    };

    let Some((rule, allow_origin)) =
        matching_rule(storage, bucket, origin, req.method().as_str(), None)
    else {
        return builder;
    };

    apply_rule_headers(builder, &rule, &allow_origin, None, false)
}

pub(super) fn apply_actual_request_headers_from_xml(
    req: &Request,
    builder: ResponseBuilder,
    cors_xml: &str,
) -> ResponseBuilder {
    let Some(origin) = req.header("origin") else {
        return builder;
    };

    let Some((rule, allow_origin)) =
        matching_rule_from_xml(cors_xml, origin, req.method().as_str(), None)
    else {
        return builder;
    };

    apply_rule_headers(builder, &rule, &allow_origin, None, false)
}

pub(super) fn preflight_response(
    storage: &(impl BucketStore + ?Sized),
    bucket: &str,
    req: &Request,
    req_id: &str,
) -> Response<Body> {
    let Some(origin) = req.header("origin") else {
        return ResponseBuilder::new(StatusCode::FORBIDDEN)
            .header("x-amz-request-id", req_id)
            .empty();
    };
    let Some(requested_method) = req.header("access-control-request-method") else {
        return ResponseBuilder::new(StatusCode::FORBIDDEN)
            .header("x-amz-request-id", req_id)
            .empty();
    };
    let requested_headers = req.header("access-control-request-headers");

    let Some((rule, allow_origin)) =
        matching_rule(storage, bucket, origin, requested_method, requested_headers)
    else {
        return ResponseBuilder::new(StatusCode::FORBIDDEN)
            .header("x-amz-request-id", req_id)
            .empty();
    };

    apply_rule_headers(
        ResponseBuilder::new(StatusCode::OK).header("x-amz-request-id", req_id),
        &rule,
        &allow_origin,
        requested_headers,
        true,
    )
    .empty()
}

fn apply_rule_headers(
    mut builder: ResponseBuilder,
    rule: &CorsRule,
    allow_origin: &str,
    requested_headers: Option<&str>,
    preflight: bool,
) -> ResponseBuilder {
    builder = builder.header("Access-Control-Allow-Origin", allow_origin);
    if allow_origin != "*" {
        builder = builder.header("Vary", "Origin");
    }

    if preflight {
        builder = builder.header(
            "Access-Control-Allow-Methods",
            &rule.allowed_methods.join(", "),
        );
        if let Some(headers) = requested_headers.filter(|value| !value.trim().is_empty()) {
            builder = builder.header("Access-Control-Allow-Headers", headers);
        } else if !rule.allowed_headers.is_empty() {
            builder = builder.header(
                "Access-Control-Allow-Headers",
                &rule.allowed_headers.join(", "),
            );
        }
        if let Some(max_age) = &rule.max_age_seconds {
            builder = builder.header("Access-Control-Max-Age", max_age);
        }
    } else if !rule.expose_headers.is_empty() {
        builder = builder.header(
            "Access-Control-Expose-Headers",
            &rule.expose_headers.join(", "),
        );
    }

    builder
}

fn matching_rule(
    storage: &(impl BucketStore + ?Sized),
    bucket: &str,
    origin: &str,
    method: &str,
    requested_headers: Option<&str>,
) -> Option<(CorsRule, String)> {
    let rules = load_rules(storage, bucket).ok()?;

    for rule in rules {
        let Some(allow_origin) = allowed_origin(&rule, origin) else {
            continue;
        };
        if !allows_method(&rule, method) {
            continue;
        }
        if !allows_headers(&rule, requested_headers) {
            continue;
        }
        return Some((rule, allow_origin.to_string()));
    }

    None
}

fn matching_rule_from_xml(
    cors_xml: &str,
    origin: &str,
    method: &str,
    requested_headers: Option<&str>,
) -> Option<(CorsRule, String)> {
    let rules = parse_cors_rules(cors_xml).ok()?;

    for rule in rules {
        let Some(allow_origin) = allowed_origin(&rule, origin) else {
            continue;
        };
        if !allows_method(&rule, method) {
            continue;
        }
        if !allows_headers(&rule, requested_headers) {
            continue;
        }
        return Some((rule, allow_origin.to_string()));
    }

    None
}

fn allowed_origin<'a>(rule: &CorsRule, origin: &'a str) -> Option<&'a str> {
    if rule.allowed_origins.iter().any(|value| value == "*") {
        return Some("*");
    }

    rule.allowed_origins
        .iter()
        .find(|value| value.eq_ignore_ascii_case(origin))
        .map(|_| origin)
}

fn allows_method(rule: &CorsRule, method: &str) -> bool {
    rule.allowed_methods.iter().any(|value| {
        value.eq_ignore_ascii_case(method)
            || (method.eq_ignore_ascii_case("HEAD") && value.eq_ignore_ascii_case("GET"))
    })
}

fn allows_headers(rule: &CorsRule, requested_headers: Option<&str>) -> bool {
    let Some(requested_headers) = requested_headers else {
        return true;
    };
    if requested_headers.trim().is_empty() {
        return true;
    }
    if rule.allowed_headers.iter().any(|value| value == "*") {
        return true;
    }

    requested_headers.split(',').all(|header| {
        let header = header.trim();
        !header.is_empty()
            && rule
                .allowed_headers
                .iter()
                .any(|allowed| allowed.eq_ignore_ascii_case(header))
    })
}

fn load_rules(
    storage: &(impl BucketStore + ?Sized),
    bucket: &str,
) -> Result<Vec<CorsRule>, crate::error::Error> {
    let bucket = bucket_service::get_bucket(storage, bucket)?;
    let Some(xml) = bucket.metadata.get(S3_CORS_XML_KEY) else {
        return Ok(Vec::new());
    };
    parse_cors_rules(xml)
}

fn parse_cors_rules(xml: &str) -> Result<Vec<CorsRule>, crate::error::Error> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);

    let mut buf = Vec::new();
    let mut rules = Vec::new();
    let mut current_rule: Option<CorsRule> = None;
    let mut current_field: Option<CorsField> = None;

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(event)) => match event.name().as_ref() {
                b"CORSRule" => current_rule = Some(CorsRule::default()),
                b"AllowedOrigin" => current_field = Some(CorsField::AllowedOrigin),
                b"AllowedMethod" => current_field = Some(CorsField::AllowedMethod),
                b"AllowedHeader" => current_field = Some(CorsField::AllowedHeader),
                b"ExposeHeader" => current_field = Some(CorsField::ExposeHeader),
                b"MaxAgeSeconds" => current_field = Some(CorsField::MaxAgeSeconds),
                _ => {}
            },
            Ok(Event::End(event)) => match event.name().as_ref() {
                b"CORSRule" => {
                    if let Some(rule) = current_rule.take() {
                        rules.push(rule);
                    }
                }
                b"AllowedOrigin" | b"AllowedMethod" | b"AllowedHeader" | b"ExposeHeader"
                | b"MaxAgeSeconds" => current_field = None,
                _ => {}
            },
            Ok(Event::Text(text)) => {
                let Some(rule) = current_rule.as_mut() else {
                    buf.clear();
                    continue;
                };
                let decoded = text
                    .decode()
                    .map_err(|err| crate::error::Error::InvalidRequest(err.to_string()))?;
                let value = unescape(&decoded)
                    .map_err(|err| crate::error::Error::InvalidRequest(err.to_string()))?
                    .to_string();
                match current_field {
                    Some(CorsField::AllowedOrigin) => rule.allowed_origins.push(value),
                    Some(CorsField::AllowedMethod) => rule.allowed_methods.push(value),
                    Some(CorsField::AllowedHeader) => rule.allowed_headers.push(value),
                    Some(CorsField::ExposeHeader) => rule.expose_headers.push(value),
                    Some(CorsField::MaxAgeSeconds) => rule.max_age_seconds = Some(value),
                    None => {}
                }
            }
            Ok(Event::Eof) => break,
            Err(err) => {
                return Err(crate::error::Error::InvalidRequest(format!(
                    "Invalid CORS XML: {err}"
                )))
            }
            _ => {}
        }
        buf.clear();
    }

    Ok(rules)
}
