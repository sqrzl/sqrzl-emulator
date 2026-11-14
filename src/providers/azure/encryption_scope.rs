//! Minimal Azure encryption-scope model.
//!
//! Scopes are named server-side keys in a real storage account. The emulator
//! does not encrypt anything; it records the scope name per blob, returns it on
//! reads, and rejects scope names the operator has not declared through
//! `AZURE_ENCRYPTION_SCOPES`, so an unknown scope fails as it does on
//! Azure instead of being silently dropped.

use super::{AzureBlobAdapter, Request};
use crate::body::Body;
use http::StatusCode;
use hyper::Response;

/// Provider metadata key recording the scope a blob was written with.
pub(super) const AZURE_ENCRYPTION_SCOPE_KEY: &str = "azure_encryption_scope";
const AZURE_ENCRYPTION_SCOPES_ENV: &str = "AZURE_ENCRYPTION_SCOPES";
const SCOPE_HEADER: &str = "x-ms-encryption-scope";

fn parse_declared_scopes(raw: Option<&str>) -> Vec<String> {
    raw.unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|scope| !scope.is_empty())
        .map(str::to_string)
        .collect()
}

/// The scope signed into a SAS (`ses`), when the request is SAS-authorized.
fn signed_scope(req: &Request) -> Option<&str> {
    req.query_param("sig")
        .and(req.query_param("ses"))
        .filter(|scope| !scope.is_empty())
}

/// The scope a write applies: the SAS-pinned scope wins, then the header.
///
/// Call [`validate_write_scope`] first; this does not re-validate.
pub(super) fn write_scope(req: &Request) -> Option<String> {
    signed_scope(req)
        .or_else(|| req.header(SCOPE_HEADER))
        .map(str::to_string)
}

/// Validates the encryption scope requested by a write.
///
/// # Errors
/// Returns the Azure error response for an empty header, a header that differs
/// from the SAS-pinned scope, or a scope the emulator account does not declare.
#[allow(clippy::result_large_err)]
pub(super) fn validate_write_scope(
    req: &Request,
    config: &crate::Config,
) -> Result<Option<String>, Response<Body>> {
    validate_write_scope_with(
        req,
        &parse_declared_scopes(config.vendor_credentials.azure_encryption_scopes.as_deref()),
    )
}

#[allow(clippy::result_large_err)]
fn validate_write_scope_with(
    req: &Request,
    declared: &[String],
) -> Result<Option<String>, Response<Body>> {
    let header = req.header(SCOPE_HEADER);
    if header.is_some_and(|value| value.trim().is_empty()) {
        return Err(AzureBlobAdapter::error_response(
            StatusCode::BAD_REQUEST,
            "InvalidHeaderValue",
            "The x-ms-encryption-scope header value is invalid.",
        ));
    }
    let signed = signed_scope(req);
    if let (Some(header), Some(signed)) = (header, signed) {
        if header != signed {
            return Err(AzureBlobAdapter::error_response(
                StatusCode::BAD_REQUEST,
                "InvalidHeaderValue",
                "The x-ms-encryption-scope header does not match the encryption scope signed into the SAS.",
            ));
        }
    }
    let Some(scope) = signed.or(header) else {
        return Ok(None);
    };
    if !declared.iter().any(|declared| declared == scope) {
        return Err(AzureBlobAdapter::error_response(
            StatusCode::FORBIDDEN,
            "AuthorizationFailure",
            &format!(
                "The encryption scope {scope} does not exist in this account. Declare it in {AZURE_ENCRYPTION_SCOPES_ENV}."
            ),
        ));
    }
    Ok(Some(scope.to_string()))
}

/// The scope recorded on a stored blob.
pub(super) fn recorded_scope(blob: &crate::models::Object) -> Option<&str> {
    blob.provider_metadata
        .get(AZURE_ENCRYPTION_SCOPE_KEY)
        .map(String::as_str)
}

/// Records `scope` on a blob about to be written.
pub(super) fn record_scope(blob: &mut crate::models::Object, scope: Option<&str>) {
    match scope {
        Some(scope) => {
            blob.provider_metadata
                .insert(AZURE_ENCRYPTION_SCOPE_KEY.to_string(), scope.to_string());
        }
        None => {
            blob.provider_metadata.remove(AZURE_ENCRYPTION_SCOPE_KEY);
        }
    }
}

/// Response for a write whose scope differs from the scope already bound to
/// the blob or its staged blocks.
pub(super) fn scope_mismatch(subject: &str) -> Response<Body> {
    AzureBlobAdapter::error_response(
        StatusCode::BAD_REQUEST,
        "InvalidHeaderValue",
        &format!(
            "The requested encryption scope does not match the encryption scope of {subject}."
        ),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::body::Body;
    use hyper::Request as HyperRequest;

    async fn request(uri: &str, headers: &[(&str, &str)]) -> Request {
        let mut builder = HyperRequest::builder().method("PUT").uri(uri);
        for (name, value) in headers {
            builder = builder.header(*name, *value);
        }
        Request::from_hyper(builder.body(Body::from(Vec::new())).unwrap())
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn should_resolve_encryption_scope_from_declared_sas_or_header() {
        // Arrange
        let declared = parse_declared_scopes(Some(" tenant-a , tenant-b,"));
        let sas_only = request("http://localhost/a/c/b?sig=x&ses=tenant-a", &[]).await;
        let header_only = request(
            "http://localhost/a/c/b",
            &[("x-ms-encryption-scope", "tenant-b")],
        )
        .await;
        let conflicting = request(
            "http://localhost/a/c/b?sig=x&ses=tenant-a",
            &[("x-ms-encryption-scope", "tenant-b")],
        )
        .await;
        let unknown = request(
            "http://localhost/a/c/b",
            &[("x-ms-encryption-scope", "tenant-z")],
        )
        .await;
        let unsigned_ses = request("http://localhost/a/c/b?ses=tenant-z", &[]).await;

        // Act
        let sas_scope = validate_write_scope_with(&sas_only, &declared)
            .unwrap_or_else(|_| panic!("scope should validate"));
        let header_scope = validate_write_scope_with(&header_only, &declared)
            .unwrap_or_else(|_| panic!("scope should validate"));
        let conflict = validate_write_scope_with(&conflicting, &declared).unwrap_err();
        let missing = validate_write_scope_with(&unknown, &declared).unwrap_err();
        let ignored = validate_write_scope_with(&unsigned_ses, &declared)
            .unwrap_or_else(|_| panic!("scope should validate"));

        // Assert
        assert_eq!(declared, vec!["tenant-a", "tenant-b"]);
        assert_eq!(sas_scope.as_deref(), Some("tenant-a"));
        assert_eq!(write_scope(&sas_only).as_deref(), Some("tenant-a"));
        assert_eq!(header_scope.as_deref(), Some("tenant-b"));
        assert_eq!(conflict.status(), StatusCode::BAD_REQUEST);
        assert_eq!(missing.status(), StatusCode::FORBIDDEN);
        assert_eq!(ignored, None);
    }
}
