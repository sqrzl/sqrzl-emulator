//! Centralized configuration management for the Peas Emulator.
//!
//! This module is the single source of truth for all environment variables
//! and configuration options. All other modules should use the types and
//! functions exported from this module rather than accessing env vars directly.

use std::env;
use std::time::Duration;

// Environment variable names
const ENV_ACCESS_KEY_ID: &str = "ACCESS_KEY_ID";
const ENV_SECRET_ACCESS_KEY: &str = "SECRET_ACCESS_KEY";
const ENV_BLOBS_PATH: &str = "BLOBS_PATH";
const ENV_LIFECYCLE_HOURS: &str = "LIFECYCLE_HOURS";
const ENV_API_PORT: &str = "API_PORT";
const ENV_UI_PORT: &str = "UI_PORT";
const ENV_ADMIN_AUTH_DISABLED: &str = "ADMIN_AUTH_DISABLED";
const ENV_MAX_REQUEST_BYTES: &str = "MAX_REQUEST_BYTES";
const ENV_PEAS_BUCKET_LIST: &str = "PEAS_BUCKET_LIST";
const ENV_PEAS_LOG_FORMAT: &str = "PEAS_LOG_FORMAT";

// Default values
const DEFAULT_BLOBS_PATH: &str = "./blobs";
const DEFAULT_LIFECYCLE_HOURS: u64 = 1;
const DEFAULT_API_PORT: u16 = 9000;
const DEFAULT_UI_PORT: u16 = 9001;
pub const DEFAULT_MAX_REQUEST_BYTES: usize = 128 * 1024 * 1024;

/// Logging format for tracing output.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LogFormat {
    /// Human-readable text logs.
    Text,
    /// Structured JSON logs.
    Json,
}

/// Global application configuration loaded from environment variables.
#[derive(Clone, Debug)]
pub struct Config {
    /// AWS access key ID for authentication
    pub access_key_id: Option<String>,
    /// AWS secret access key for authentication
    pub secret_access_key: Option<String>,
    /// Whether authentication is enforced
    pub enforce_auth: bool,
    /// Whether admin HTTP auth is bypassed even when provider auth is enabled
    pub admin_auth_disabled: bool,
    /// Path to filesystem storage directory
    pub blobs_path: String,
    /// Interval for running lifecycle rules
    pub lifecycle_interval: Duration,
    /// Port for the API server
    pub api_port: u16,
    /// Port for the UI server
    pub ui_port: u16,
    /// Maximum accepted HTTP request body size before streaming support is added
    pub max_request_bytes: usize,
}

impl Config {
    fn from_env_with<F>(mut lookup: F) -> Self
    where
        F: FnMut(&str) -> Option<String>,
    {
        let access_key_id = lookup(ENV_ACCESS_KEY_ID);
        let secret_access_key = lookup(ENV_SECRET_ACCESS_KEY);
        let blobs_path = lookup(ENV_BLOBS_PATH).unwrap_or_else(|| DEFAULT_BLOBS_PATH.to_string());

        let lifecycle_interval_hours = lookup(ENV_LIFECYCLE_HOURS)
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(DEFAULT_LIFECYCLE_HOURS);
        let api_port = lookup(ENV_API_PORT)
            .and_then(|s| s.parse::<u16>().ok())
            .unwrap_or(DEFAULT_API_PORT);
        let ui_port = lookup(ENV_UI_PORT)
            .and_then(|s| s.parse::<u16>().ok())
            .unwrap_or(DEFAULT_UI_PORT);
        let admin_auth_disabled = lookup(ENV_ADMIN_AUTH_DISABLED)
            .as_deref()
            .map(parse_bool_env)
            .unwrap_or(false);
        let max_request_bytes = lookup(ENV_MAX_REQUEST_BYTES)
            .and_then(|s| s.parse::<usize>().ok())
            .filter(|value| *value > 0)
            .unwrap_or(DEFAULT_MAX_REQUEST_BYTES);

        let enforce_auth = access_key_id.is_some() && secret_access_key.is_some();

        Self {
            access_key_id,
            secret_access_key,
            enforce_auth,
            admin_auth_disabled,
            blobs_path,
            lifecycle_interval: Duration::from_secs(lifecycle_interval_hours * 3600),
            api_port,
            ui_port,
            max_request_bytes,
        }
    }

    /// Load configuration from environment variables.
    ///
    /// # Environment Variables
    ///
    /// - `ACCESS_KEY_ID`: AWS access key ID (optional)
    /// - `SECRET_ACCESS_KEY`: AWS secret access key (optional)
    /// - `BLOBS_PATH`: Path to storage directory (default: "./blobs")
    /// - `LIFECYCLE_HOURS`: Hours between lifecycle rule executions (default: 1)
    /// - `MAX_REQUEST_BYTES`: Maximum request body bytes accepted before streaming is supported (default: 128 MiB)
    /// - `ADMIN_AUTH_DISABLED`: Disable `/admin/v1` session auth even when provider auth is enabled
    /// - `PEAS_BUCKET_LIST`: Comma-delimited list of buckets to create on startup
    /// - `PEAS_LOG_FORMAT`: Logging format (`text` by default, `json` for structured logs)
    pub fn from_env() -> Self {
        Self::from_env_with(|name| env::var(name).ok())
    }

    /// Load comma-delimited startup bucket names from the environment.
    ///
    /// Empty segments are ignored, and names are trimmed of surrounding whitespace.
    pub fn startup_bucket_names_from_env() -> Vec<String> {
        Self::startup_bucket_names_from_env_with(|name| env::var(name).ok())
    }

    /// Load the configured logging format from the environment.
    ///
    /// Unknown values fall back to human-readable text logs.
    pub fn log_format_from_env() -> LogFormat {
        Self::log_format_from_env_with(|name| env::var(name).ok())
    }

    fn startup_bucket_names_from_env_with<F>(mut lookup: F) -> Vec<String>
    where
        F: FnMut(&str) -> Option<String>,
    {
        lookup(ENV_PEAS_BUCKET_LIST)
            .map(|value| {
                value
                    .split(',')
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .map(ToString::to_string)
                    .collect()
            })
            .unwrap_or_default()
    }

    fn log_format_from_env_with<F>(mut lookup: F) -> LogFormat
    where
        F: FnMut(&str) -> Option<String>,
    {
        match lookup(ENV_PEAS_LOG_FORMAT)
            .as_deref()
            .map(str::trim)
            .map(|value| value.to_ascii_lowercase())
            .as_deref()
        {
            Some("json") => LogFormat::Json,
            _ => LogFormat::Text,
        }
    }

    /// Get the access key ID if authentication is enabled.
    pub fn access_key(&self) -> Option<&str> {
        self.access_key_id.as_deref()
    }

    /// Get the secret access key if authentication is enabled.
    pub fn secret_key(&self) -> Option<&str> {
        self.secret_access_key.as_deref()
    }

    /// Validate that provided credentials match configured credentials.
    ///
    /// If authentication is not enforced, this always returns `true`.
    pub fn validate_credentials(&self, provided_key: &str, provided_secret: &str) -> bool {
        if !self.enforce_auth {
            return true;
        }

        if let (Some(key), Some(secret)) = (self.access_key(), self.secret_key()) {
            provided_key == key && provided_secret == secret
        } else {
            false
        }
    }

    /// Whether the admin API should enforce session auth.
    pub fn admin_auth_enforced(&self) -> bool {
        self.enforce_auth && !self.admin_auth_disabled
    }
}

fn parse_bool_env(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_load_default_values_when_env_is_empty() {
        // Arrange
        // Act
        let config = Config::from_env_with(|_| None);
        let startup_buckets = Config::startup_bucket_names_from_env_with(|_| None);

        // Assert
        assert_eq!(config.access_key_id, None);
        assert_eq!(config.secret_access_key, None);
        assert!(!config.enforce_auth);
        assert!(!config.admin_auth_disabled);
        assert_eq!(config.blobs_path, DEFAULT_BLOBS_PATH);
        assert_eq!(
            config.lifecycle_interval,
            Duration::from_secs(DEFAULT_LIFECYCLE_HOURS * 3600)
        );
        assert_eq!(config.api_port, DEFAULT_API_PORT);
        assert_eq!(config.ui_port, DEFAULT_UI_PORT);
        assert_eq!(config.max_request_bytes, DEFAULT_MAX_REQUEST_BYTES);
        assert!(startup_buckets.is_empty());
    }

    #[test]
    fn should_load_custom_values_when_env_contains_all_settings() {
        // Arrange
        // Act
        let config = Config::from_env_with(|name| match name {
            ENV_ACCESS_KEY_ID => Some("test-key".to_string()),
            ENV_SECRET_ACCESS_KEY => Some("test-secret".to_string()),
            ENV_BLOBS_PATH => Some("/tmp/peas-blobs".to_string()),
            ENV_LIFECYCLE_HOURS => Some("2".to_string()),
            ENV_API_PORT => Some("9100".to_string()),
            ENV_UI_PORT => Some("9101".to_string()),
            ENV_MAX_REQUEST_BYTES => Some("1024".to_string()),
            _ => None,
        });

        // Assert
        assert_eq!(config.access_key(), Some("test-key"));
        assert_eq!(config.secret_key(), Some("test-secret"));
        assert!(config.enforce_auth);
        assert!(!config.admin_auth_disabled);
        assert_eq!(config.blobs_path, "/tmp/peas-blobs");
        assert_eq!(config.lifecycle_interval, Duration::from_secs(7200));
        assert_eq!(config.api_port, 9100);
        assert_eq!(config.ui_port, 9101);
        assert_eq!(config.max_request_bytes, 1024);
        assert!(config.validate_credentials("test-key", "test-secret"));
        assert!(!config.validate_credentials("wrong-key", "test-secret"));
    }

    #[test]
    fn should_keep_auth_disabled_when_only_one_credential_is_present() {
        // Arrange
        // Act
        let access_only = Config::from_env_with(|name| match name {
            ENV_ACCESS_KEY_ID => Some("test-key".to_string()),
            _ => None,
        });

        let secret_only = Config::from_env_with(|name| match name {
            ENV_SECRET_ACCESS_KEY => Some("test-secret".to_string()),
            _ => None,
        });

        // Assert
        assert!(!access_only.enforce_auth);
        assert!(!secret_only.enforce_auth);
        assert!(!access_only.admin_auth_disabled);
        assert!(!secret_only.admin_auth_disabled);
        assert!(access_only.validate_credentials("anything", "anything"));
        assert!(secret_only.validate_credentials("anything", "anything"));
    }

    #[test]
    fn should_fall_back_to_default_lifecycle_hours_when_env_value_is_invalid() {
        // Arrange
        // Act
        let config = Config::from_env_with(|name| match name {
            ENV_BLOBS_PATH => Some("/tmp/custom-blobs".to_string()),
            ENV_LIFECYCLE_HOURS => Some("invalid".to_string()),
            _ => None,
        });

        // Assert
        assert_eq!(config.blobs_path, "/tmp/custom-blobs");
        assert_eq!(
            config.lifecycle_interval,
            Duration::from_secs(DEFAULT_LIFECYCLE_HOURS * 3600)
        );
        assert_eq!(config.api_port, DEFAULT_API_PORT);
        assert_eq!(config.ui_port, DEFAULT_UI_PORT);
        assert_eq!(config.max_request_bytes, DEFAULT_MAX_REQUEST_BYTES);
    }

    #[test]
    fn should_allow_disabling_admin_auth_with_env_override() {
        // Arrange
        // Act
        let config = Config::from_env_with(|name| match name {
            ENV_ACCESS_KEY_ID => Some("test-key".to_string()),
            ENV_SECRET_ACCESS_KEY => Some("test-secret".to_string()),
            ENV_ADMIN_AUTH_DISABLED => Some("true".to_string()),
            _ => None,
        });

        // Assert
        assert!(config.enforce_auth);
        assert!(config.admin_auth_disabled);
        assert!(!config.admin_auth_enforced());
    }

    #[test]
    fn should_fall_back_to_default_max_request_bytes_when_env_value_is_invalid() {
        // Arrange
        // Act
        let config = Config::from_env_with(|name| match name {
            ENV_MAX_REQUEST_BYTES => Some("0".to_string()),
            _ => None,
        });

        // Assert
        assert_eq!(config.max_request_bytes, DEFAULT_MAX_REQUEST_BYTES);
    }

    #[test]
    fn should_parse_comma_delimited_startup_bucket_names() {
        // Arrange
        // Act
        let startup_buckets = Config::startup_bucket_names_from_env_with(|name| match name {
            ENV_PEAS_BUCKET_LIST => Some("alpha, beta , , gamma,delta ".to_string()),
            _ => None,
        });

        // Assert
        assert_eq!(
            startup_buckets,
            vec![
                "alpha".to_string(),
                "beta".to_string(),
                "gamma".to_string(),
                "delta".to_string(),
            ]
        );
    }

    #[test]
    fn should_load_text_log_format_by_default_when_env_is_empty() {
        // Arrange
        // Act
        let log_format = Config::log_format_from_env_with(|_| None);

        // Assert
        assert_eq!(log_format, LogFormat::Text);
    }

    #[test]
    fn should_load_json_log_format_when_env_requests_structured_logging() {
        // Arrange
        // Act
        let log_format = Config::log_format_from_env_with(|name| match name {
            ENV_PEAS_LOG_FORMAT => Some("json".to_string()),
            _ => None,
        });

        // Assert
        assert_eq!(log_format, LogFormat::Json);
    }
}
