//! Native S3 checksum admission. Multipart and trailer variants are explicit
//! unsupported boundaries until their persisted part identities are implemented.
use crate::body::Body;
use crate::models::Object;
use crate::server::http::{Request, ResponseBuilder};
use crate::services::xml_error_response;
use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use http::StatusCode;
use hyper::Response;

const ALGORITHMS: [(&str, &str); 6] = [
    ("CRC32", "crc32"),
    ("CRC32C", "crc32c"),
    ("CRC64NVME", "crc64nvme"),
    ("SHA1", "sha1"),
    ("SHA256", "sha256"),
    ("MD5", "md5"),
];

fn digest(req: &Request, algorithm: &str) -> Vec<u8> {
    match algorithm {
        "CRC32" => req.payload_crc32().to_be_bytes().to_vec(),
        "CRC32C" => req.payload_crc32c().to_be_bytes().to_vec(),
        "CRC64NVME" => req.payload_crc64_nvme().to_be_bytes().to_vec(),
        "SHA1" => req.payload_sha1().to_vec(),
        "SHA256" => req.payload_sha256().to_vec(),
        "MD5" => req.payload_md5().to_vec(),
        _ => unreachable!("only supported algorithms have digests"),
    }
}

pub(super) fn requested(req: &Request) -> bool {
    req.headers
        .keys()
        .any(|name| name.as_str().starts_with("x-amz-checksum-"))
        || req.header("x-amz-sdk-checksum-algorithm").is_some()
        || req.header("x-amz-trailer").is_some()
        || req.header("content-encoding").is_some_and(|value| {
            value
                .split(',')
                .any(|part| part.trim().eq_ignore_ascii_case("aws-chunked"))
        })
        || req
            .header("x-amz-content-sha256")
            .is_some_and(|value| value.starts_with("STREAMING-"))
}

pub(super) fn unsupported(req_id: &str) -> Response<Body> {
    xml_error_response(StatusCode::NOT_IMPLEMENTED, "NotImplemented", "This S3 checksum algorithm, multipart, copy, or trailer variant is not supported by this emulator subset.", req_id)
}

pub(super) fn validate_put(req: &Request, req_id: &str) -> Option<Response<Body>> {
    if req.header("x-amz-trailer").is_some()
        || req.header("content-encoding").is_some_and(|value| {
            value
                .split(',')
                .any(|part| part.trim().eq_ignore_ascii_case("aws-chunked"))
        })
        || req
            .header("x-amz-content-sha256")
            .is_some_and(|value| value.starts_with("STREAMING-"))
        || req.headers.keys().any(|name| {
            name.as_str().starts_with("x-amz-checksum-")
                && !ALGORITHMS
                    .iter()
                    .any(|(_, suffix)| name.as_str() == format!("x-amz-checksum-{suffix}"))
        })
    {
        return Some(unsupported(req_id));
    }
    for (name, value) in &req.headers {
        if name.as_str().starts_with("x-amz-checksum-") || name == "x-amz-sdk-checksum-algorithm" {
            if req.headers.get_all(name).iter().count() != 1 {
                return Some(xml_error_response(
                    StatusCode::BAD_REQUEST,
                    "InvalidRequest",
                    "Checksum headers must occur once.",
                    req_id,
                ));
            }
            if value.to_str().is_err() {
                return Some(xml_error_response(
                    StatusCode::BAD_REQUEST,
                    "InvalidDigest",
                    "Checksum headers must be ASCII.",
                    req_id,
                ));
            }
        }
    }
    let declared = req.header("x-amz-sdk-checksum-algorithm");
    if declared.is_some_and(|algorithm| !ALGORITHMS.iter().any(|(name, _)| *name == algorithm)) {
        return Some(unsupported(req_id));
    }
    let supplied: Vec<_> = ALGORITHMS
        .iter()
        .filter_map(|(algorithm, suffix)| {
            req.header(&format!("x-amz-checksum-{suffix}"))
                .map(|value| (*algorithm, value))
        })
        .collect();
    if supplied.len() > 1 {
        return Some(xml_error_response(
            StatusCode::BAD_REQUEST,
            "InvalidRequest",
            "Only one additional checksum algorithm may be supplied.",
            req_id,
        ));
    }
    if let Some(declared) = declared {
        if supplied.is_empty() {
            return Some(xml_error_response(
                StatusCode::BAD_REQUEST,
                "InvalidRequest",
                "The SDK checksum algorithm requires its corresponding checksum header.",
                req_id,
            ));
        }
        if supplied[0].0 != declared {
            return Some(xml_error_response(
                StatusCode::BAD_REQUEST,
                "BadDigest",
                "The checksum header does not match the SDK checksum algorithm.",
                req_id,
            ));
        }
    }
    for (algorithm, value) in supplied {
        let expected = digest(req, algorithm);
        let decoded = match BASE64.decode(value) {
            Ok(decoded) if decoded.len() == expected.len() => decoded,
            _ => {
                return Some(xml_error_response(
                    StatusCode::BAD_REQUEST,
                    "InvalidDigest",
                    "The checksum is not a valid Base64 digest for the selected algorithm.",
                    req_id,
                ))
            }
        };
        if decoded != expected {
            return Some(xml_error_response(
                StatusCode::BAD_REQUEST,
                "BadDigest",
                "The checksum does not match the request payload.",
                req_id,
            ));
        }
    }
    None
}

pub(super) fn replace_identity(req: &Request, object: &mut Object) {
    object
        .provider_metadata
        .retain(|key, _| !key.starts_with("s3_checksum_"));
    for (algorithm, suffix) in ALGORITHMS {
        if req.header(&format!("x-amz-checksum-{suffix}")).is_some() {
            object.provider_metadata.insert(
                format!("s3_checksum_{suffix}"),
                BASE64.encode(digest(req, algorithm)),
            );
            object
                .provider_metadata
                .insert("s3_checksum_type".to_string(), "FULL_OBJECT".to_string());
        }
    }
}

pub(super) fn response_headers(mut builder: ResponseBuilder, object: &Object) -> ResponseBuilder {
    for (_, suffix) in ALGORITHMS {
        if let Some(value) = object
            .provider_metadata
            .get(&format!("s3_checksum_{suffix}"))
        {
            builder = builder.header(&format!("x-amz-checksum-{suffix}"), value);
        }
    }
    if let Some(value) = object.provider_metadata.get("s3_checksum_type") {
        builder = builder.header("x-amz-checksum-type", value);
    }
    builder
}

pub(super) fn retrieval_headers(
    builder: ResponseBuilder,
    req: &Request,
    object: &Object,
) -> ResponseBuilder {
    if req.header("x-amz-checksum-mode") == Some("ENABLED") && req.header("range").is_none() {
        response_headers(builder, object)
    } else {
        builder
    }
}

pub(super) fn manifest_requests_checksum(body: &[u8]) -> bool {
    let mut reader = quick_xml::Reader::from_reader(body);
    loop {
        match reader.read_event() {
            Ok(quick_xml::events::Event::Start(tag) | quick_xml::events::Event::Empty(tag))
                if tag.local_name().as_ref().starts_with(b"Checksum") =>
            {
                return true
            }
            Ok(quick_xml::events::Event::Eof) | Err(_) => return false,
            _ => {}
        }
    }
}
