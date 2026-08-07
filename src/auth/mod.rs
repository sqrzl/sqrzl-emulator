// Authentication and SigV4 verification
pub mod acs_hmac;
pub mod admin_session;
pub mod authenticator;
pub mod presigned;
pub mod sigv4;

pub use acs_hmac::parse_connection_string;
pub use admin_session::{AdminLoginRequest, AdminSessionManager};
pub use authenticator::{AuthConfig, AuthInfo, HttpRequestLike};
pub use presigned::{PresignedUrl, PresignedUrlConfig};
pub use sigv4::{SigV4Config, SignatureVerifier};
