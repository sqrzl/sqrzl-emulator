//! Streams large request bodies straight to disk instead of buffering them
//! in memory, for the two S3 operations whose payload size is unbounded:
//! a plain object `PUT` and a multipart `UploadPart`. Every other request
//! (control-plane XML, small object subresources like `?tagging`/`?acl`,
//! SMS/mail adapter traffic, …) keeps using the existing fully-buffered
//! [`super::http::Request::from_hyper_with_max_body`] path — those bodies
//! are always small, so buffering them costs nothing and touching their
//! handlers would only add risk.
//!
//! Spooling happens before any bytes are held in memory: the eligible route
//! is decided from the request's method, path, and headers alone (see
//! [`is_streamable_object_put`]), then the body is copied frame-by-frame
//! into a scratch file on the same volume as the eventual blob storage
//! while an MD5 and a SHA-256 digest are computed incrementally — so
//! nothing downstream (`Content-MD5` validation, the object's `ETag`, or
//! `SigV4`'s payload-hash fallback) needs to re-read the file just to hash
//! it. [`crate::storage::ObjectStore::put_object_streamed`] and
//! [`crate::storage::MultipartStore::upload_part_streamed`] then move that
//! file into its final location.

use super::http::{
    parse_query_params, CollectBodyError, Request as RequestExt, RequestParseError, RouteMatch,
    Router, SpooledPayload,
};
use crate::providers::AdapterRegistry;
use bytes::Bytes;
use http::request::Parts;
use http_body_util::BodyExt;
use md5::Context as Md5Context;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use tokio::io::{AsyncWriteExt, BufWriter};
use uuid::Uuid;

struct PartialSpoolFile {
    path: PathBuf,
    armed: bool,
}

impl PartialSpoolFile {
    fn new(path: PathBuf) -> Self {
        Self { path, armed: true }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for PartialSpoolFile {
    fn drop(&mut self) {
        if self.armed {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// Whether this request's body should be streamed straight to disk rather
/// than buffered: a `PUT` that the S3 adapter — specifically, not Azure,
/// GCS, or OCI, which share the same generic `/bucket/key`-shaped routing
/// and are told apart only by headers/query params inspected here via
/// [`AdapterRegistry::resolve_by_head`] — will treat as an object key write
/// (covering both a plain object write and, via its
/// `partNumber`/`uploadId` query params, a multipart `UploadPart`),
/// excluding the small subresource writes (`?tagging`, `?acl`) whose
/// handlers still read the body as in-memory XML.
pub(crate) fn is_streamable_object_put(parts: &Parts, adapters: &AdapterRegistry) -> bool {
    if parts.method != http::Method::PUT {
        return false;
    }
    let host = parts.headers.get("host").and_then(|v| v.to_str().ok());
    let route = Router::route_from_parts(&parts.method, parts.uri.path(), host);
    if !matches!(route, RouteMatch::ObjectPut(_, _)) {
        return false;
    }
    let query = parse_query_params(parts.uri.query());
    if query.contains_key("tagging") || query.contains_key("acl") {
        return false;
    }
    adapters.resolve_by_head(&parts.method, &parts.uri, &parts.headers) == Some("s3")
}

/// Streams `body` into a scratch file under `spool_dir`, enforcing
/// `max_request_bytes` as bytes arrive, and builds the resulting
/// [`RequestExt`] with an empty in-memory `body` and `spooled_body` set.
#[allow(clippy::result_large_err)]
pub(crate) async fn spool_request<B>(
    parts: Parts,
    body: B,
    max_request_bytes: usize,
    spool_dir: &Path,
) -> Result<RequestExt, RequestParseError>
where
    B: hyper::body::Body<Data = Bytes> + Unpin,
    B::Error: std::fmt::Display,
{
    match spool_body_to_disk(body, max_request_bytes, spool_dir).await {
        Ok(spooled) => Ok(RequestExt::from_parts(parts, Bytes::new(), Some(spooled))),
        Err(CollectBodyError::BodyTooLarge { max_request_bytes }) => {
            Err(RequestParseError::BodyTooLarge {
                max_request_bytes,
                method: parts.method,
                uri: parts.uri,
                headers: parts.headers,
            })
        }
        Err(CollectBodyError::BodyRead(message)) => Err(RequestParseError::BodyRead {
            message,
            method: parts.method,
            uri: parts.uri,
            headers: parts.headers,
        }),
    }
}

async fn spool_body_to_disk<B>(
    mut body: B,
    max_request_bytes: usize,
    spool_dir: &Path,
) -> Result<SpooledPayload, CollectBodyError>
where
    B: hyper::body::Body<Data = Bytes> + Unpin,
    B::Error: std::fmt::Display,
{
    tokio::fs::create_dir_all(spool_dir)
        .await
        .map_err(|e| CollectBodyError::BodyRead(format!("Failed to create spool dir: {e}")))?;
    let temp_path = spool_dir.join(format!(".spool-{}.tmp", Uuid::new_v4()));
    spool_frames(&mut body, max_request_bytes, &temp_path).await
}

async fn spool_frames<B>(
    body: &mut B,
    max_request_bytes: usize,
    temp_path: &Path,
) -> Result<SpooledPayload, CollectBodyError>
where
    B: hyper::body::Body<Data = Bytes> + Unpin,
    B::Error: std::fmt::Display,
{
    let mut partial_file = PartialSpoolFile::new(temp_path.to_path_buf());
    let file = tokio::fs::File::create(temp_path)
        .await
        .map_err(|e| CollectBodyError::BodyRead(format!("Failed to create spool file: {e}")))?;
    let mut writer = BufWriter::new(file);
    let mut md5_ctx = Md5Context::new();
    let mut sha256_ctx = Sha256::new();
    let mut total: u64 = 0;
    let max_request_bytes_u64 = max_request_bytes as u64;

    while let Some(frame) = body.frame().await {
        let frame = frame.map_err(|e| CollectBodyError::BodyRead(e.to_string()))?;
        let Some(data) = frame.data_ref() else {
            continue;
        };
        total = total.saturating_add(data.len() as u64);
        if total > max_request_bytes_u64 {
            return Err(CollectBodyError::BodyTooLarge { max_request_bytes });
        }
        writer
            .write_all(data)
            .await
            .map_err(|e| CollectBodyError::BodyRead(format!("Failed to write spool file: {e}")))?;
        md5_ctx.consume(data);
        sha256_ctx.update(data);
    }

    writer
        .flush()
        .await
        .map_err(|e| CollectBodyError::BodyRead(format!("Failed to flush spool file: {e}")))?;
    writer
        .get_ref()
        .sync_all()
        .await
        .map_err(|e| CollectBodyError::BodyRead(format!("Failed to sync spool file: {e}")))?;
    drop(writer);

    let payload = SpooledPayload::new(
        temp_path.to_path_buf(),
        total,
        md5_ctx.finalize().0,
        hex::encode(sha256_ctx.finalize()),
    );
    partial_file.disarm();
    Ok(payload)
}

/// Removes request spool files left behind by a previous interrupted process.
/// Only the request spool filename pattern is owned here; other entries are
/// preserved.
pub(crate) async fn cleanup_stale_spool_files(spool_dir: &Path) -> std::io::Result<()> {
    let mut entries = match tokio::fs::read_dir(spool_dir).await {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    while let Some(entry) = entries.next_entry().await? {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with(".spool-")
            && name.ends_with(".tmp")
            && entry.file_type().await?.is_file()
        {
            tokio::fs::remove_file(entry.path()).await?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::body::Body;
    use http::{HeaderMap, HeaderValue, Method, Uri};
    use hyper::body::{Frame, SizeHint};
    use std::convert::Infallible;
    use std::pin::Pin;
    use std::task::{Context, Poll};

    struct PendingAfterOneFrame {
        frame: Option<Bytes>,
    }

    impl hyper::body::Body for PendingAfterOneFrame {
        type Data = Bytes;
        type Error = Infallible;

        fn poll_frame(
            mut self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
        ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
            self.frame.take().map_or(Poll::Pending, |frame| {
                Poll::Ready(Some(Ok(Frame::data(frame))))
            })
        }

        fn is_end_stream(&self) -> bool {
            false
        }

        fn size_hint(&self) -> SizeHint {
            SizeHint::default()
        }
    }

    fn parts(method: Method, uri: &str, headers: &[(&str, &str)]) -> Parts {
        let mut header_map = HeaderMap::new();
        for (name, value) in headers {
            header_map.insert(
                http::HeaderName::from_bytes(name.as_bytes()).unwrap(),
                HeaderValue::from_str(value).unwrap(),
            );
        }
        let (parts, ()) = http::Request::builder()
            .method(method)
            .uri(uri.parse::<Uri>().unwrap())
            .body(())
            .unwrap()
            .into_parts();
        let mut parts = parts;
        parts.headers = header_map;
        parts
    }

    #[test]
    fn should_treat_plain_object_put_as_streamable() {
        let adapters = AdapterRegistry::default();
        let p = parts(Method::PUT, "http://localhost/bucket/key", &[]);
        assert!(is_streamable_object_put(&p, &adapters));
    }

    #[test]
    fn should_treat_multipart_upload_part_as_streamable() {
        // Arrange
        let adapters = AdapterRegistry::default();
        let p = parts(
            Method::PUT,
            "http://localhost/bucket/key?partNumber=1&uploadId=abc",
            &[],
        );

        // Act
        let streamable = is_streamable_object_put(&p, &adapters);

        // Assert
        assert!(streamable);
    }

    #[test]
    fn should_not_stream_object_tagging_or_acl_subresource_puts() {
        // Arrange
        let adapters = AdapterRegistry::default();

        // Act
        let streamable = ["?tagging", "?acl"].map(|query| {
            let p = parts(
                Method::PUT,
                &format!("http://localhost/bucket/key{query}"),
                &[],
            );
            (query, is_streamable_object_put(&p, &adapters))
        });

        // Assert
        for (query, streamable) in streamable {
            assert!(!streamable, "{query}");
        }
    }

    #[test]
    fn should_not_stream_bucket_level_or_non_put_requests() {
        // Arrange
        let adapters = AdapterRegistry::default();
        let bucket_put = parts(Method::PUT, "http://localhost/bucket", &[]);
        let object_get = parts(Method::GET, "http://localhost/bucket/key", &[]);

        // Act
        let bucket_put_streamable = is_streamable_object_put(&bucket_put, &adapters);
        let object_get_streamable = is_streamable_object_put(&object_get, &adapters);

        // Assert
        assert!(!bucket_put_streamable);
        assert!(!object_get_streamable);
    }

    #[test]
    fn should_not_stream_requests_another_provider_adapter_owns() {
        // Arrange
        // A PUT shaped exactly like an S3 object write, but carrying an
        // Azure Blob header: Azure's handler still reads `req.body`
        // directly, so this must fall back to full buffering rather than
        // being spooled out from under it.
        let adapters = AdapterRegistry::default();
        let p = parts(
            Method::PUT,
            "http://localhost/account/container/blob",
            &[("x-ms-version", "2023-11-03")],
        );

        // Act
        let streamable = is_streamable_object_put(&p, &adapters);

        // Assert
        assert!(!streamable);
    }

    #[tokio::test]
    async fn should_spool_body_computing_len_and_digests() {
        let dir = std::env::temp_dir().join(format!("sqrzl-spool-test-{}", Uuid::new_v4()));
        let payload = b"hello streaming world".repeat(1000);
        let body = Body::from(payload.clone());

        let spooled = spool_body_to_disk(body, payload.len() + 1, &dir)
            .await
            .expect("spooling should succeed");

        assert_eq!(spooled.len, payload.len() as u64);
        assert_eq!(spooled.md5, md5::compute(&payload).0);
        assert_eq!(spooled.sha256_hex, hex::encode(Sha256::digest(&payload)));
        let on_disk = std::fs::read(&spooled.path).expect("spooled file should exist");
        assert_eq!(on_disk, payload);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn should_reject_and_clean_up_spool_file_over_the_configured_limit() {
        let dir = std::env::temp_dir().join(format!("sqrzl-spool-test-{}", Uuid::new_v4()));
        let payload = vec![0u8; 4096];
        let body = Body::from(payload);

        let result = spool_body_to_disk(body, 10, &dir).await;

        assert!(matches!(
            result,
            Err(CollectBodyError::BodyTooLarge {
                max_request_bytes: 10
            })
        ));
        let leftover = std::fs::read_dir(&dir).map_or(0, Iterator::count);
        assert_eq!(leftover, 0, "oversized spool file should be cleaned up");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn should_clean_up_spool_file_when_parsed_request_is_rejected() {
        // Arrange
        let dir = std::env::temp_dir().join(format!("sqrzl-spool-test-{}", Uuid::new_v4()));
        let payload = b"request rejected after spooling".to_vec();
        let body = Body::from(payload.clone());
        let spooled = spool_body_to_disk(body, payload.len() + 1, &dir)
            .await
            .expect("spooling should succeed");
        let path = spooled.path.clone();
        assert!(path.exists());

        // Act
        drop(spooled);

        // Assert
        assert!(!path.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn should_clean_up_partial_spool_file_when_request_task_is_cancelled() {
        // Arrange
        let dir = std::env::temp_dir().join(format!("sqrzl-spool-test-{}", Uuid::new_v4()));
        let body = PendingAfterOneFrame {
            frame: Some(Bytes::from_static(b"partial request payload")),
        };
        let task_dir = dir.clone();
        let task = tokio::spawn(async move { spool_body_to_disk(body, 1024, &task_dir).await });
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            loop {
                let has_spool_file =
                    std::fs::read_dir(&dir).is_ok_and(|mut entries| entries.next().is_some());
                if has_spool_file {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("spool file should be created before cancellation");

        // Act
        task.abort();
        let _ = task.await;

        // Assert
        let leftover = std::fs::read_dir(&dir).map_or(0, Iterator::count);
        assert_eq!(leftover, 0, "cancelled spool file should be cleaned up");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
