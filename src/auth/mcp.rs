//! OAuth bearer-token validation for the MCP resource server.

use crate::config::McpConfig;
use jsonwebtoken::{decode, decode_header, jwk::JwkSet, Algorithm, DecodingKey, Validation};
use serde_json::Value;
use std::{
    collections::BTreeSet,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::sync::{Mutex, RwLock};

const JWKS_TTL: Duration = Duration::from_secs(300);
const JWKS_REFRESH_COOLDOWN: Duration = Duration::from_secs(5);
const MAX_JWKS_BYTES: usize = 1024 * 1024;
const MAX_TOKEN_BYTES: usize = 16 * 1024;

#[derive(Clone, Debug)]
pub struct McpPrincipal {
    pub scopes: BTreeSet<String>,
    pub client: String,
    pub request_bytes: u64,
}

#[derive(Debug)]
pub enum McpAuthFailure {
    InvalidToken,
    Unavailable,
}

#[derive(Clone)]
pub struct McpAuth {
    config: Arc<McpConfig>,
    client: reqwest::Client,
    jwks: Arc<RwLock<Option<(JwkSet, Instant)>>>,
    refresh_lock: Arc<Mutex<()>>,
    last_refresh_attempt: Arc<Mutex<Option<Instant>>>,
}

impl McpAuth {
    /// Create a token validator using bounded-time HTTP access to the JWKS endpoint.
    ///
    /// # Panics
    ///
    /// Panics only if the fixed HTTP client timeout configuration is rejected by Reqwest.
    #[must_use]
    pub fn new(config: Arc<McpConfig>) -> Self {
        Self {
            config,
            client: reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(3))
                .timeout(Duration::from_secs(5))
                .build()
                .expect("static MCP JWKS HTTP client configuration should be valid"),
            jwks: Arc::new(RwLock::new(None)),
            refresh_lock: Arc::new(Mutex::new(())),
            last_refresh_attempt: Arc::new(Mutex::new(None)),
        }
    }

    /// Validate an OAuth access token against the configured issuer, audience,
    /// and rotating JWKS. Only asymmetric signing algorithms are accepted.
    ///
    /// # Errors
    ///
    /// Returns `InvalidToken` for a missing or invalid token and `Unavailable`
    /// when configured key material cannot be fetched.
    pub async fn authenticate(
        &self,
        authorization: Option<&str>,
    ) -> Result<McpPrincipal, McpAuthFailure> {
        let token = authorization
            .and_then(parse_bearer)
            .filter(|token| token.len() <= MAX_TOKEN_BYTES)
            .ok_or(McpAuthFailure::InvalidToken)?;
        let header = decode_header(token).map_err(|_| McpAuthFailure::InvalidToken)?;
        if !matches!(
            header.alg,
            Algorithm::RS256
                | Algorithm::RS384
                | Algorithm::RS512
                | Algorithm::PS256
                | Algorithm::PS384
                | Algorithm::PS512
                | Algorithm::ES256
                | Algorithm::ES384
                | Algorithm::EdDSA
        ) {
            return Err(McpAuthFailure::InvalidToken);
        }
        let kid = header.kid.as_deref().ok_or(McpAuthFailure::InvalidToken)?;
        let jwks = self.get_jwks(kid).await?;
        let jwk = jwks.find(kid).ok_or(McpAuthFailure::InvalidToken)?;
        let key = DecodingKey::from_jwk(jwk).map_err(|_| McpAuthFailure::InvalidToken)?;
        let issuer = self
            .config
            .issuer
            .as_deref()
            .ok_or(McpAuthFailure::Unavailable)?;
        let audience = self
            .config
            .audience
            .as_deref()
            .ok_or(McpAuthFailure::Unavailable)?;
        let mut validation = Validation::new(header.alg);
        validation.set_required_spec_claims(&["exp", "iss", "aud"]);
        validation.set_issuer(&[issuer]);
        validation.set_audience(&[audience]);
        validation.validate_exp = true;
        validation.validate_nbf = true;
        let claims = decode::<Value>(token, &key, &validation)
            .map_err(|_| McpAuthFailure::InvalidToken)?
            .claims;
        let mut scopes = BTreeSet::new();
        if let Some(scope) = claims.get("scope").and_then(Value::as_str) {
            scopes.extend(scope.split_ascii_whitespace().map(ToOwned::to_owned));
        }
        if let Some(values) = claims.get("scp").and_then(Value::as_array) {
            scopes.extend(
                values
                    .iter()
                    .filter_map(Value::as_str)
                    .map(ToOwned::to_owned),
            );
        } else if let Some(scope) = claims.get("scp").and_then(Value::as_str) {
            scopes.extend(scope.split_ascii_whitespace().map(ToOwned::to_owned));
        }
        let client = claims
            .get("client_id")
            .or_else(|| claims.get("azp"))
            .or_else(|| claims.get("sub"))
            .and_then(Value::as_str)
            .unwrap_or("oauth-client")
            .to_string();
        Ok(McpPrincipal {
            scopes,
            client,
            request_bytes: 0,
        })
    }

    async fn get_jwks(&self, kid: &str) -> Result<JwkSet, McpAuthFailure> {
        if let Some(set) = self.fresh_cached_set_for(kid).await {
            return Ok(set);
        }

        let _refresh = self.refresh_lock.lock().await;
        if let Some(set) = self.fresh_cached_set_for(kid).await {
            return Ok(set);
        }

        let mut last_attempt = self.last_refresh_attempt.lock().await;
        if last_attempt.is_some_and(|attempt| attempt.elapsed() < JWKS_REFRESH_COOLDOWN) {
            let cached = self.jwks.read().await;
            if let Some((set, refreshed)) = cached.as_ref() {
                if refreshed.elapsed() < JWKS_TTL {
                    return Ok(set.clone());
                }
            }
            return Err(McpAuthFailure::Unavailable);
        }
        *last_attempt = Some(Instant::now());
        drop(last_attempt);

        let url = self
            .config
            .jwks_url
            .as_deref()
            .ok_or(McpAuthFailure::Unavailable)?;
        let mut response = self
            .client
            .get(url)
            .send()
            .await
            .map_err(|_| McpAuthFailure::Unavailable)?;
        if !response.status().is_success() {
            return Err(McpAuthFailure::Unavailable);
        }
        if response
            .content_length()
            .is_some_and(|length| length > MAX_JWKS_BYTES as u64)
        {
            return Err(McpAuthFailure::Unavailable);
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| McpAuthFailure::Unavailable)?
        {
            if bytes.len().saturating_add(chunk.len()) > MAX_JWKS_BYTES {
                return Err(McpAuthFailure::Unavailable);
            }
            bytes.extend_from_slice(&chunk);
        }
        let set =
            serde_json::from_slice::<JwkSet>(&bytes).map_err(|_| McpAuthFailure::Unavailable)?;
        if set.keys.len() > 256 {
            return Err(McpAuthFailure::Unavailable);
        }
        *self.jwks.write().await = Some((set.clone(), Instant::now()));
        Ok(set)
    }

    async fn fresh_cached_set_for(&self, kid: &str) -> Option<JwkSet> {
        let cached = self.jwks.read().await;
        cached.as_ref().and_then(|(set, refreshed)| {
            (refreshed.elapsed() < JWKS_TTL && set.find(kid).is_some()).then(|| set.clone())
        })
    }
}

fn parse_bearer(value: &str) -> Option<&str> {
    let (scheme, token) = value.split_once(' ')?;
    (scheme.eq_ignore_ascii_case("bearer") && !token.trim().is_empty()).then(|| token.trim())
}

#[cfg(test)]
mod tests {
    use super::*;
    use jsonwebtoken::{encode, jwk::Jwk, EncodingKey, Header};
    use rsa::{pkcs8::EncodePrivateKey, rand_core::OsRng, RsaPrivateKey};
    use serde_json::json;

    #[test]
    fn should_accept_only_bearer_authorization_headers() {
        // Arrange
        // Act
        // Assert
        assert_eq!(parse_bearer("Bearer abc"), Some("abc"));
        assert_eq!(parse_bearer("bearer abc"), Some("abc"));
        assert_eq!(parse_bearer("Basic abc"), None);
        assert_eq!(parse_bearer("Bearer"), None);
    }

    #[tokio::test]
    async fn should_validate_required_rsa_token_claims() {
        // Arrange
        let private = RsaPrivateKey::new(&mut OsRng, 2048).expect("RSA key should generate");
        let private_pem = private
            .to_pkcs8_pem(rsa::pkcs8::LineEnding::LF)
            .expect("private key should encode");
        let encoding_key = EncodingKey::from_rsa_pem(private_pem.as_bytes())
            .expect("jsonwebtoken should parse the RSA key");
        let mut jwk = Jwk::from_encoding_key(&encoding_key, Algorithm::RS256)
            .expect("JWK should derive from RSA key");
        jwk.common.key_id = Some("test-key".into());

        let config = Arc::new(McpConfig {
            issuer: Some("https://issuer.example".into()),
            audience: Some("https://sqrzl.example/mcp".into()),
            ..Default::default()
        });
        let auth = McpAuth::new(config);
        *auth.jwks.write().await = Some((JwkSet { keys: vec![jwk] }, Instant::now()));

        let expiration = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock should be after epoch")
            .as_secs()
            + 60;
        let mut header = Header::new(Algorithm::RS256);
        header.kid = Some("test-key".into());
        let token =
            |claims: Value| encode(&header, &claims, &encoding_key).expect("JWT should encode");

        let valid_claims = json!({
            "iss": "https://issuer.example",
            "aud": "https://sqrzl.example/mcp",
            "exp": expiration,
            "scp": ["sqrzl:read", "sqrzl:write"],
            "client_id": "integration-client"
        });
        let valid = token(valid_claims.clone());

        // Act
        let principal = auth
            .authenticate(Some(&format!("Bearer {valid}")))
            .await
            .expect("valid bearer should authenticate");

        // Assert
        assert_eq!(principal.client, "integration-client");
        assert!(principal.scopes.contains("sqrzl:read"));
        assert!(principal.scopes.contains("sqrzl:write"));

        let wrong_audience = token(
            json!({"iss":"https://issuer.example","aud":"https://other.example/mcp","exp":expiration}),
        );
        assert!(matches!(
            auth.authenticate(Some(&format!("Bearer {wrong_audience}")))
                .await,
            Err(McpAuthFailure::InvalidToken)
        ));
        let wrong_issuer = token(
            json!({"iss":"https://other.example","aud":"https://sqrzl.example/mcp","exp":expiration}),
        );
        assert!(matches!(
            auth.authenticate(Some(&format!("Bearer {wrong_issuer}")))
                .await,
            Err(McpAuthFailure::InvalidToken)
        ));
        let expired = token(
            json!({"iss":"https://issuer.example","aud":"https://sqrzl.example/mcp","exp":expiration - 300}),
        );
        assert!(matches!(
            auth.authenticate(Some(&format!("Bearer {expired}"))).await,
            Err(McpAuthFailure::InvalidToken)
        ));

        let missing_issuer = token(json!({"aud":"https://sqrzl.example/mcp","exp":expiration}));
        assert!(matches!(
            auth.authenticate(Some(&format!("Bearer {missing_issuer}")))
                .await,
            Err(McpAuthFailure::InvalidToken)
        ));
        let missing_audience = token(json!({"iss":"https://issuer.example","exp":expiration}));
        assert!(matches!(
            auth.authenticate(Some(&format!("Bearer {missing_audience}")))
                .await,
            Err(McpAuthFailure::InvalidToken)
        ));
        let future_not_before = token(json!({
            "iss":"https://issuer.example",
            "aud":"https://sqrzl.example/mcp",
            "exp":expiration,
            "nbf":expiration + 3600
        }));
        assert!(matches!(
            auth.authenticate(Some(&format!("Bearer {future_not_before}")))
                .await,
            Err(McpAuthFailure::InvalidToken)
        ));
    }

    #[tokio::test]
    async fn should_reuse_a_fresh_jwks_during_unknown_key_refresh_cooldown() {
        // Arrange
        let auth = McpAuth::new(Arc::new(McpConfig {
            jwks_url: Some("http://127.0.0.1:1/jwks".into()),
            ..Default::default()
        }));
        let cached = JwkSet { keys: Vec::new() };
        *auth.jwks.write().await = Some((cached.clone(), Instant::now()));
        *auth.last_refresh_attempt.lock().await = Some(Instant::now());

        // Act
        let result = auth.get_jwks("attacker-selected-kid").await;

        // Assert
        assert!(result.is_ok());
        assert_eq!(result.unwrap().keys.len(), 0);
    }
}
