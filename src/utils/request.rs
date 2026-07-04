use crate::auth::HttpRequestLike;
use std::str::FromStr;

pub fn request_origin(req: &impl HttpRequestLike) -> String {
    if let Some(origin) = req.header("origin").and_then(normalize_origin) {
        return origin;
    }

    let scheme = request_scheme(req);
    let authority = request_authority(req, &scheme);
    format!("{scheme}://{authority}")
}

fn normalize_origin(origin: &str) -> Option<String> {
    if origin.eq_ignore_ascii_case("null") {
        return None;
    }

    let uri = http::Uri::from_str(origin).ok()?;
    let scheme = uri.scheme_str()?;
    let authority = uri.authority()?.as_str();
    Some(format!("{scheme}://{authority}"))
}

fn request_scheme(req: &impl HttpRequestLike) -> String {
    if let Some(value) = req.header("forwarded").and_then(forwarded_proto) {
        return value;
    }

    if let Some(value) = first_header_value(req, "x-forwarded-proto")
        .or_else(|| first_header_value(req, "x-forwarded-scheme"))
    {
        return value;
    }

    if let Some(value) = req.header("x-forwarded-ssl") {
        if matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "on" | "1" | "true" | "yes"
        ) {
            return "https".to_string();
        }
    }

    "http".to_string()
}

fn request_authority(req: &impl HttpRequestLike, scheme: &str) -> String {
    if let Some(authority) = req.header("forwarded").and_then(forwarded_host) {
        return append_forwarded_port(authority, req, scheme);
    }

    if let Some(authority) =
        first_header_value(req, "x-forwarded-host").or_else(|| first_header_value(req, "host"))
    {
        return append_forwarded_port(authority, req, scheme);
    }

    "localhost".to_string()
}

fn append_forwarded_port(authority: String, req: &impl HttpRequestLike, scheme: &str) -> String {
    if authority.contains(':') || authority.starts_with('[') {
        return authority;
    }

    let Some(port) = first_header_value(req, "x-forwarded-port") else {
        return authority;
    };

    if is_default_port(scheme, &port) {
        return authority;
    }

    format!("{authority}:{port}")
}

fn is_default_port(scheme: &str, port: &str) -> bool {
    matches!((scheme, port), ("http", "80") | ("https", "443"))
}

fn first_header_value(req: &impl HttpRequestLike, name: &str) -> Option<String> {
    req.header(name)
        .map(|value| value.split(',').next().unwrap_or(value).trim().to_string())
        .filter(|value| !value.is_empty())
}

fn forwarded_host(value: &str) -> Option<String> {
    forwarded_directive(value, "host")
}

fn forwarded_proto(value: &str) -> Option<String> {
    forwarded_directive(value, "proto")
}

fn forwarded_directive(value: &str, key: &str) -> Option<String> {
    value
        .split(',')
        .next()?
        .split(';')
        .filter_map(|part| part.trim().split_once('='))
        .find_map(|(name, value)| {
            name.trim()
                .eq_ignore_ascii_case(key)
                .then(|| value.trim().trim_matches('"').to_string())
        })
        .filter(|value| !value.is_empty())
}
