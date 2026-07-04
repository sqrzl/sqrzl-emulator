use crate::body::RequestBody;
use crate::error::{Error, Result};
use chrono::Utc;
use getrandom::fill;
use hyper::Request;
use jsonwebtoken::{decode, encode, Algorithm, DecodingKey, EncodingKey, Header, Validation};
use serde::{Deserialize, Serialize};
use std::time::Duration;

pub const ADMIN_LOGIN_PATH: &str = "/admin/v1/auth/login";
pub const ADMIN_LOGOUT_PATH: &str = "/admin/v1/auth/logout";
pub const ADMIN_SESSION_PATH: &str = "/admin/v1/auth/session";
pub const ADMIN_SESSION_COOKIE_NAME: &str = "sqrzl_admin_session";

const ADMIN_ISSUER: &str = "sqrzl-emulator";
const ADMIN_SESSION_TTL: Duration = Duration::from_hours(8);

#[derive(Debug, Deserialize)]
pub struct AdminLoginRequest {
    pub username: String,
    pub password: String,
}

#[derive(Debug, Clone)]
pub struct AdminSessionManager {
    signing_secret: [u8; 32],
    session_ttl: Duration,
}

#[derive(Debug, Serialize, Deserialize)]
struct AdminSessionClaims {
    sub: String,
    iss: String,
    iat: i64,
    exp: i64,
}

impl AdminSessionManager {
    ///
    /// # Errors
    ///
    /// Returns an error when the underlying emulator operation fails.
    pub fn new() -> Result<Self> {
        let mut signing_secret = [0_u8; 32];
        fill(&mut signing_secret).map_err(|e| Error::InternalError(e.to_string()))?;

        Ok(Self {
            signing_secret,
            session_ttl: ADMIN_SESSION_TTL,
        })
    }

    ///
    /// # Errors
    ///
    /// Returns an error when the underlying emulator operation fails.
    pub fn issue_session_cookie(&self, username: &str) -> Result<String> {
        let token = self.issue_token(username)?;
        Ok(format!(
            "{name}={token}; Path=/; HttpOnly; SameSite=Lax; Max-Age={max_age}",
            name = ADMIN_SESSION_COOKIE_NAME,
            token = token,
            max_age = self.session_ttl.as_secs(),
        ))
    }

    #[must_use]
    pub fn clear_session_cookie() -> String {
        format!("{ADMIN_SESSION_COOKIE_NAME}=; Path=/; HttpOnly; SameSite=Lax; Max-Age=0")
    }

    pub fn has_valid_session(&self, req: &Request<RequestBody>) -> bool {
        self.subject_from_request(req).is_some()
    }

    pub fn subject_from_request(&self, req: &Request<RequestBody>) -> Option<String> {
        req.headers()
            .get("cookie")
            .and_then(|header| header.to_str().ok())
            .and_then(|cookie_header| self.subject_from_cookie_header(cookie_header))
    }

    #[must_use]
    pub fn subject_from_cookie_header(&self, cookie_header: &str) -> Option<String> {
        let token = extract_cookie_value(cookie_header, ADMIN_SESSION_COOKIE_NAME)?;
        self.subject_from_token(token)
    }

    fn issue_token(&self, username: &str) -> Result<String> {
        let now = Utc::now().timestamp();
        let ttl_seconds = i64::try_from(self.session_ttl.as_secs()).unwrap_or(i64::MAX);
        let claims = AdminSessionClaims {
            sub: username.to_string(),
            iss: ADMIN_ISSUER.to_string(),
            iat: now,
            exp: now.saturating_add(ttl_seconds),
        };

        let encoding_key = EncodingKey::from_secret(&self.signing_secret);

        encode(&Header::new(Algorithm::HS256), &claims, &encoding_key).map_err(|err| {
            Error::InternalError(format!("failed to sign admin session token: {err}"))
        })
    }

    fn subject_from_token(&self, token: &str) -> Option<String> {
        let validation = Validation::new(Algorithm::HS256);
        let decoding_key = DecodingKey::from_secret(&self.signing_secret);
        let claims = decode::<AdminSessionClaims>(token, &decoding_key, &validation)
            .ok()?
            .claims;

        if claims.iss != ADMIN_ISSUER {
            return None;
        }

        Some(claims.sub)
    }
}

fn extract_cookie_value<'a>(cookie_header: &'a str, cookie_name: &str) -> Option<&'a str> {
    cookie_header.split(';').map(str::trim).find_map(|cookie| {
        let (name, value) = cookie.split_once('=')?;
        (name == cookie_name).then_some(value)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_issue_a_session_cookie() {
        // Arrange
        let manager = AdminSessionManager::new().expect("session manager should build");

        // Act
        let cookie = manager
            .issue_session_cookie("admin")
            .expect("cookie should build");

        // Assert
        assert!(cookie.contains(ADMIN_SESSION_COOKIE_NAME));
    }

    #[test]
    fn should_validate_an_issued_session_cookie_token() {
        // Arrange
        let manager = AdminSessionManager::new().expect("session manager should build");
        let cookie = manager
            .issue_session_cookie("admin")
            .expect("cookie should build");
        let token = cookie
            .split_once('=')
            .expect("cookie should contain token")
            .1
            .split(';')
            .next()
            .expect("cookie token should exist");

        // Act
        let subject = manager.subject_from_token(token);

        // Assert
        assert_eq!(subject.as_deref(), Some("admin"));
    }

    #[test]
    fn should_extract_cookie_value_from_cookie_header() {
        // Arrange
        let cookie_header = "foo=bar; sqrzl_admin_session=abc.def.ghi; theme=tabby";

        // Act
        let cookie_value = extract_cookie_value(cookie_header, ADMIN_SESSION_COOKIE_NAME);

        // Assert
        assert_eq!(cookie_value, Some("abc.def.ghi"));
    }

    #[test]
    fn should_build_session_clear_cookie_header() {
        // Arrange
        // Act
        let cookie = AdminSessionManager::clear_session_cookie();

        // Assert
        assert!(cookie.contains("sqrzl_admin_session="));
        assert!(cookie.contains("Max-Age=0"));
    }
}
