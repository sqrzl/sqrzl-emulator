pub use sqrzl_emulator_core::config::*;

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn should_report_native_authentication_in_health() {
        // Arrange
        use crate::storage::FilesystemStorage;
        use http_body_util::BodyExt as _;
        let config = Config::from_env_with(|name| match name {
            "AZURE_ACCOUNT" => Some("account".to_string()),
            "AZURE_ACCOUNT_KEY" => Some("key".to_string()),
            _ => None,
        });
        let root = std::env::temp_dir().join(format!("sqrzl-health-{}", uuid::Uuid::new_v4()));
        let storage = FilesystemStorage::new(root);

        // Act
        let response = crate::health::response(&storage, &config);
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let health: serde_json::Value = serde_json::from_slice(&bytes).unwrap();

        // Assert
        assert_eq!(health["auth_enforced"], true);
        assert!(health["auth_enforced_providers"]
            .as_array()
            .unwrap()
            .iter()
            .any(|provider| provider == "azure-blob"));
        assert_eq!(health["admin_auth_enforced"], false);
    }
}
