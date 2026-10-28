//! The supported multipart checksum profile is CRC32C / `FULL_OBJECT`.
//! Other algorithms, composite digests, copy and streaming trailers remain explicit 501s.
use super::{checksums, xml_error_response, Body, Response, ResponseBuilder, StatusCode};
use crate::models::MultipartUpload;
use crate::server::http::Request;
use crate::storage::MultipartCompletion;
use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use quick_xml::{events::Event, Reader};

pub(super) fn enabled(upload: &MultipartUpload) -> bool {
    upload
        .provider_metadata
        .get("s3_checksum_algorithm")
        .is_some_and(|v| v == "CRC32C")
}

fn allowed(req: &Request, names: &[&str]) -> bool {
    !req.headers.keys().any(|name| {
        (name.as_str().starts_with("x-amz-checksum-")
            || name.as_str() == "x-amz-sdk-checksum-algorithm")
            && (!names.contains(&name.as_str()) || req.headers.get_all(name).iter().count() != 1)
    }) && req.header("x-amz-trailer").is_none()
        && !req.header("content-encoding").is_some_and(|v| {
            v.split(',')
                .any(|v| v.trim().eq_ignore_ascii_case("aws-chunked"))
        })
        && !req
            .header("x-amz-content-sha256")
            .is_some_and(|v| v.starts_with("STREAMING-"))
}

pub(super) fn initiate(req: &Request, req_id: &str) -> Result<bool, Box<Response<Body>>> {
    if !checksums::requested(req) {
        return Ok(false);
    }
    if !allowed(req, &["x-amz-checksum-algorithm", "x-amz-checksum-type"])
        || req.header("x-amz-checksum-algorithm") != Some("CRC32C")
        || req.header("x-amz-checksum-type") != Some("FULL_OBJECT")
    {
        return Err(Box::new(checksums::unsupported(req_id)));
    }
    Ok(true)
}

pub(super) fn validate_part(
    req: &Request,
    upload: &MultipartUpload,
    req_id: &str,
) -> Option<Response<Body>> {
    if !enabled(upload) {
        return checksums::requested(req).then(|| checksums::unsupported(req_id));
    }
    if !allowed(
        req,
        &["x-amz-checksum-crc32c", "x-amz-sdk-checksum-algorithm"],
    ) {
        return Some(checksums::unsupported(req_id));
    }
    if req.header("x-amz-sdk-checksum-algorithm").is_some()
        && req.header("x-amz-checksum-crc32c").is_none()
    {
        return Some(*error(
            "InvalidRequest",
            "The SDK checksum selector requires a checksum header.",
            req_id,
        ));
    }
    if let Some(value) = req.header("x-amz-checksum-crc32c") {
        match decode(value) {
            Some(crc) if crc == req.payload_crc32c() => {}
            Some(_) => {
                return Some(*error(
                    "BadDigest",
                    "The part checksum does not match the payload.",
                    req_id,
                ))
            }
            None => {
                return Some(*error(
                    "InvalidDigest",
                    "CRC32C must be a Base64 encoded 32-bit digest.",
                    req_id,
                ))
            }
        }
    }
    None
}

pub(super) fn part_headers(
    builder: ResponseBuilder,
    req: &Request,
    upload: &MultipartUpload,
) -> ResponseBuilder {
    if enabled(upload) {
        builder.header(
            "x-amz-checksum-crc32c",
            &BASE64.encode(req.payload_crc32c().to_be_bytes()),
        )
    } else {
        builder
    }
}

fn decode(value: &str) -> Option<u32> {
    let bytes: [u8; 4] = BASE64.decode(value).ok()?.try_into().ok()?;
    Some(u32::from_be_bytes(bytes))
}

fn error(code: &str, message: &str, req_id: &str) -> Box<Response<Body>> {
    Box::new(xml_error_response(
        StatusCode::BAD_REQUEST,
        code,
        message,
        req_id,
    ))
}

pub(super) fn completion(
    req: &Request,
    upload: &MultipartUpload,
    manifest: &[(u32, String)],
    req_id: &str,
) -> Result<MultipartCompletion, Box<Response<Body>>> {
    let enabled = enabled(upload);
    if !allowed(req, &["x-amz-checksum-crc32c", "x-amz-checksum-type"])
        || (!enabled
            && (checksums::requested(req) || checksums::manifest_requests_checksum(&req.body)))
    {
        return Err(Box::new(checksums::unsupported(req_id)));
    }
    if req
        .header("x-amz-checksum-type")
        .is_some_and(|v| v != "FULL_OBJECT")
    {
        return Err(error(
            "BadDigest",
            "The checksum type differs from multipart initiation.",
            req_id,
        ));
    }
    let crc32c = req
        .header("x-amz-checksum-crc32c")
        .map(|v| {
            decode(v).ok_or_else(|| {
                error(
                    "InvalidDigest",
                    "CRC32C must be a Base64 encoded 32-bit digest.",
                    req_id,
                )
            })
        })
        .transpose()?;
    let size = req
        .header("x-amz-mp-object-size")
        .map(|v| {
            v.parse::<u64>().map_err(|_| {
                error(
                    "InvalidRequest",
                    "The multipart object size must be an unsigned integer.",
                    req_id,
                )
            })
        })
        .transpose()?;
    if enabled {
        if manifest
            .iter()
            .enumerate()
            .any(|(index, (number, _))| usize::try_from(*number).ok() != Some(index + 1))
        {
            return Err(error(
                "InvalidPartOrder",
                "Checksum part numbers must start at one and be consecutive.",
                req_id,
            ));
        }
        validate_manifest(req, upload, manifest, req_id)?;
    }
    if req.header("if-none-match").is_some_and(|v| v != "*") {
        return Err(error(
            "InvalidRequest",
            "If-None-Match must be an asterisk.",
            req_id,
        ));
    }
    if req.header("if-none-match").is_some() && req.header("if-match").is_some() {
        return Err(error(
            "InvalidRequest",
            "Use one multipart publication condition.",
            req_id,
        ));
    }
    Ok(MultipartCompletion {
        crc32c,
        size,
        condition: super::helpers::mutation_condition(req),
    })
}

fn validate_manifest(
    req: &Request,
    upload: &MultipartUpload,
    manifest: &[(u32, String)],
    req_id: &str,
) -> Result<(), Box<Response<Body>>> {
    let mut reader = Reader::from_reader(req.body.as_ref());
    reader.config_mut().trim_text(true);
    let mut path = Vec::<Vec<u8>>::new();
    let mut fields = std::collections::HashSet::new();
    let mut index = 0;
    let mut root_seen = false;
    let mut checksum = String::new();
    let malformed = || {
        error(
            "MalformedXML",
            "Invalid completion manifest schema.",
            req_id,
        )
    };
    loop {
        match reader.read_event() {
            Ok(Event::Start(tag)) => {
                let name = tag.local_name().as_ref().to_vec();
                let valid = match path.len() {
                    0 => !root_seen && name == b"CompleteMultipartUpload",
                    1 => name == b"Part",
                    2 => {
                        if name.starts_with(b"Checksum") && name != b"ChecksumCRC32C" {
                            return Err(Box::new(checksums::unsupported(req_id)));
                        }
                        matches!(name.as_slice(), b"PartNumber" | b"ETag" | b"ChecksumCRC32C")
                            && fields.insert(name.clone())
                    }
                    _ => false,
                };
                if !valid {
                    return Err(malformed());
                }
                root_seen = true;
                path.push(name);
            }
            Ok(Event::Text(value)) => {
                if path.last().is_some_and(|name| name == b"ChecksumCRC32C") {
                    let decoded = value.decode().map_err(|_| malformed())?;
                    checksum
                        .push_str(&quick_xml::escape::unescape(&decoded).map_err(|_| malformed())?);
                } else if path.len() < 3 && !value.as_ref().iter().all(u8::is_ascii_whitespace) {
                    return Err(malformed());
                }
            }
            Ok(Event::GeneralRef(value)) if path.len() == 3 => {
                let reference = value.decode().map_err(|_| malformed())?;
                let encoded = format!("&{reference};");
                let decoded = quick_xml::escape::unescape(&encoded).map_err(|_| malformed())?;
                if path.last().is_some_and(|name| name == b"ChecksumCRC32C") {
                    checksum.push_str(&decoded);
                }
            }
            Ok(Event::CData(value)) if path.len() == 3 => {
                if path.last().is_some_and(|name| name == b"ChecksumCRC32C") {
                    checksum.push_str(&value.decode().map_err(|_| malformed())?);
                }
            }
            Ok(Event::End(tag)) => {
                if path.pop().as_deref() != Some(tag.local_name().as_ref()) {
                    return Err(malformed());
                }
                if tag.local_name().as_ref() == b"Part" {
                    let Some((number, _)) = manifest.get(index) else {
                        return Err(malformed());
                    };
                    if !fields.contains(b"PartNumber".as_slice())
                        || !fields.contains(b"ETag".as_slice())
                    {
                        return Err(malformed());
                    }
                    if fields.contains(b"ChecksumCRC32C".as_slice()) {
                        let expected = decode(&checksum);
                        let stored = upload
                            .parts
                            .iter()
                            .find(|p| p.part_number == *number)
                            .and_then(|p| p.crc32c);
                        if expected.is_none() || expected != stored {
                            return Err(error(
                                "InvalidPart",
                                "The part checksum differs from the stored part.",
                                req_id,
                            ));
                        }
                    }
                    index += 1;
                    fields.clear();
                    checksum.clear();
                }
            }
            Ok(Event::Eof) if root_seen && path.is_empty() && index == manifest.len() => {
                return Ok(())
            }
            Ok(Event::Comment(_) | Event::Decl(_)) => {}
            _ => return Err(malformed()),
        }
    }
}
