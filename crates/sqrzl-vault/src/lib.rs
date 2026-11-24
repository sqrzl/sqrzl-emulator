use std::path::Path;

use http::{uri::Authority, HeaderMap, StatusCode, Uri};
use serde::de::DeserializeOwned;
use serde_json::{json, Value};
use sqrzl_core::auth::AuthConfig;
use sqrzl_core::body::Body;
use sqrzl_core::server::{RequestExt, ResponseBuilder};
use uuid::Uuid;

mod aws;
mod azure;
mod crypto;
mod google;
mod store;

use store::Store;

pub const MAX_REQUEST_BYTES: usize = 256 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Provider {
    Aws,
    Azure,
    Google,
}

#[derive(Clone, Copy, Debug)]
enum Fault {
    Invalid(&'static str),
    Missing,
    Conflict,
    Unsupported,
    Denied,
    Ciphertext,
    WrongKey,
    Internal,
}

pub struct Vault {
    store: Store,
    azure_token: Option<String>,
    google_token: Option<String>,
}

impl Vault {
    /// Opens an encrypted filesystem vault when a key is supplied. An empty
    /// data directory can also host an in-memory vault for ephemeral tests.
    ///
    /// # Errors
    /// Returns an error if the key or saved snapshot is invalid or unreadable.
    pub fn open(root: impl AsRef<Path>, encoded_key: Option<&str>) -> sqrzl_core::Result<Self> {
        Ok(Self {
            store: Store::open(root.as_ref(), encoded_key)?,
            azure_token: None,
            google_token: None,
        })
    }

    /// Configures optional fixture bearer tokens for Azure and Google requests.
    #[must_use]
    pub fn with_bearer_tokens(mut self, azure: Option<String>, google: Option<String>) -> Self {
        self.azure_token = azure.filter(|token| !token.is_empty());
        self.google_token = google.filter(|token| !token.is_empty());
        self
    }

    #[must_use]
    pub fn handle(
        &self,
        provider: Provider,
        request: &RequestExt,
        config: &AuthConfig,
    ) -> http::Response<Body> {
        if let Some(response) = self.bearer_rejection(provider, request, config.enforce_auth) {
            return response;
        }
        let result = match provider {
            Provider::Aws => aws::handle(&self.store, request, config),
            Provider::Azure => azure::handle(&self.store, request),
            Provider::Google => google::handle(&self.store, request),
        };
        match result {
            Ok(value) => provider.response(StatusCode::OK, &value),
            Err(fault) => provider.fault(fault),
        }
    }

    fn bearer_rejection(
        &self,
        provider: Provider,
        request: &RequestExt,
        enforce_auth: bool,
    ) -> Option<http::Response<Body>> {
        let expected = match provider {
            Provider::Aws => return None,
            Provider::Azure => self.azure_token.as_deref(),
            Provider::Google => self.google_token.as_deref(),
        };
        if let Some(expected) = expected {
            let bearer = request
                .header("authorization")
                .and_then(|value| value.strip_prefix("Bearer "));
            if bearer == Some(expected) {
                return None;
            }
            let mut response = provider.error(
                StatusCode::UNAUTHORIZED,
                "Unauthorized",
                "UNAUTHENTICATED",
                "Supply the configured emulator bearer token",
            );
            let challenge = if provider == Provider::Azure {
                http::HeaderValue::from_static(
                    r#"Bearer authorization="https://login.microsoftonline.com/common", resource="https://vault.azure.net""#,
                )
            } else {
                http::HeaderValue::from_static("Bearer")
            };
            response
                .headers_mut()
                .insert(http::header::WWW_AUTHENTICATE, challenge);
            Some(response)
        } else {
            enforce_auth.then(|| provider.fault(Fault::Denied))
        }
    }
}

impl Provider {
    /// Resolve a vault protocol from the request head before storage fallback.
    #[must_use]
    pub fn resolve(uri: &Uri, headers: &HeaderMap) -> Option<Self> {
        if headers
            .get("x-amz-target")
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value.starts_with("TrentService."))
        {
            return Some(Self::Aws);
        }

        let host = authority(uri, headers).map(|authority| authority.host().to_ascii_lowercase());
        // An explicitly addressed service owns its error responses too.
        if host
            .as_deref()
            .is_some_and(|host| host.ends_with(".vault.azure.net"))
        {
            return Some(Self::Azure);
        }
        if host.as_deref() == Some("cloudkms.googleapis.com") {
            return Some(Self::Google);
        }

        let path = uri.path();
        let requested_service = headers
            .get("x-sqrzl-vault")
            .and_then(|value| value.to_str().ok());
        if (path == "/keys"
            || path.starts_with("/keys/")
            || path == "/secrets"
            || path.starts_with("/secrets/"))
            && (requested_service == Some("azure-key-vault")
                || uri.query().is_some_and(|query| {
                    query
                        .split('&')
                        .any(|pair| pair.starts_with("api-version="))
                }))
        {
            return Some(Self::Azure);
        }
        // On loopback require a Google client marker to disambiguate S3 keys
        // whose contents happen to look like Cloud KMS resource paths.
        if path.starts_with("/v1/projects/")
            && path.contains("/keyRings")
            && (requested_service == Some("google-kms")
                || headers
                    .get("x-goog-api-client")
                    .and_then(|value| value.to_str().ok())
                    .is_some_and(|value| value.contains("gapic/")))
        {
            return Some(Self::Google);
        }
        None
    }

    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Aws => "aws-kms",
            Self::Azure => "azure-key-vault",
            Self::Google => "google-kms",
        }
    }

    #[must_use]
    pub fn overloaded(self) -> http::Response<Body> {
        let mut response = self.error(
            StatusCode::SERVICE_UNAVAILABLE,
            "ServiceUnavailable",
            "UNAVAILABLE",
            "The emulator is draining or at capacity",
        );
        response.headers_mut().insert(
            http::header::RETRY_AFTER,
            http::HeaderValue::from_static("1"),
        );
        response.headers_mut().insert(
            http::header::CONNECTION,
            http::HeaderValue::from_static("close"),
        );
        response
    }

    #[must_use]
    pub fn body_error(self, too_large: bool) -> http::Response<Body> {
        if too_large {
            return self.error(
                StatusCode::PAYLOAD_TOO_LARGE,
                "RequestTooLarge",
                "RESOURCE_EXHAUSTED",
                "Vault request body exceeds the limit",
            );
        }
        self.fault(Fault::Invalid("Incomplete or invalid request body"))
    }

    #[must_use]
    pub fn internal_error(self) -> http::Response<Body> {
        self.fault(Fault::Internal)
    }

    fn fault(self, fault: Fault) -> http::Response<Body> {
        let (status, aws, azure, google, message) = match fault {
            Fault::Invalid(message) => (
                StatusCode::BAD_REQUEST,
                "ValidationException",
                "BadParameter",
                "INVALID_ARGUMENT",
                message,
            ),
            Fault::Missing => (
                StatusCode::NOT_FOUND,
                "NotFoundException",
                "NotFound",
                "NOT_FOUND",
                "The requested vault resource does not exist",
            ),
            Fault::Conflict => (
                StatusCode::CONFLICT,
                "AlreadyExistsException",
                "Conflict",
                "ALREADY_EXISTS",
                "The vault resource already exists",
            ),
            Fault::Unsupported => (
                StatusCode::NOT_IMPLEMENTED,
                "UnsupportedOperationException",
                "NotSupported",
                "UNIMPLEMENTED",
                "This operation is outside the emulated subset",
            ),
            Fault::Denied => (
                StatusCode::FORBIDDEN,
                "AccessDeniedException",
                "Forbidden",
                "PERMISSION_DENIED",
                "The request lacks the configured emulator credentials",
            ),
            Fault::Ciphertext => (
                StatusCode::BAD_REQUEST,
                "InvalidCiphertextException",
                "BadParameter",
                "INVALID_ARGUMENT",
                "Ciphertext could not be decrypted",
            ),
            Fault::WrongKey => (
                StatusCode::BAD_REQUEST,
                "IncorrectKeyException",
                "BadParameter",
                "INVALID_ARGUMENT",
                "Ciphertext does not belong to the supplied key",
            ),
            Fault::Internal => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "KMSInternalException",
                "InternalError",
                "INTERNAL",
                "The vault operation could not be completed",
            ),
        };
        let code = if self == Self::Aws { aws } else { azure };
        // AWS JSON-RPC reports the service exception code in the body/header.
        let status = if self == Self::Aws && matches!(fault, Fault::Missing | Fault::Conflict) {
            StatusCode::BAD_REQUEST
        } else {
            status
        };
        self.error(status, code, google, message)
    }

    fn error(
        self,
        status: StatusCode,
        code: &str,
        google_status: &str,
        message: &str,
    ) -> http::Response<Body> {
        let value = match self {
            Self::Aws => json!({ "__type": code, "message": message }),
            Self::Azure => json!({ "error": { "code": code, "message": message } }),
            Self::Google => {
                json!({ "error": { "code": status.as_u16(), "status": google_status, "message": message } })
            }
        };
        let mut response = self.response(status, &value);
        if self == Self::Aws {
            if let Ok(header) = http::HeaderValue::from_str(code) {
                response.headers_mut().insert("x-amzn-errortype", header);
            }
        }
        response
    }

    fn response(self, status: StatusCode, value: &Value) -> http::Response<Body> {
        let builder = ResponseBuilder::new(status).header("cache-control", "no-store");
        let builder = match self {
            Self::Aws => builder
                .content_type("application/x-amz-json-1.1")
                .header("x-amzn-requestid", &Uuid::new_v4().to_string()),
            Self::Azure => builder
                .content_type("application/json")
                .header("x-ms-request-id", &Uuid::new_v4().to_string()),
            Self::Google => builder.content_type("application/json"),
        };
        builder.body_str(&value.to_string()).build()
    }
}

fn authority(uri: &Uri, headers: &HeaderMap) -> Option<Authority> {
    uri.authority()
        .cloned()
        .or_else(|| headers.get(http::header::HOST)?.to_str().ok()?.parse().ok())
}

fn body<T: DeserializeOwned>(request: &RequestExt) -> Result<T, Fault> {
    serde_json::from_slice(&request.body)
        .map_err(|_| Fault::Invalid("Missing or malformed JSON parameters"))
}
