use crate::auth::{AuthConfig, AuthInfo};
use crate::body::Body;
use crate::models::policy::{AuthContext, Authorizer, PolicyEffect};
use crate::services::{bucket as bucket_service, object as object_service, xml_error_response};
use crate::storage::Storage;
use crate::utils::headers as header_utils;
use http::StatusCode;
use hyper::Response;
use std::sync::Arc;
use tracing::warn;

mod context;
mod sigv4;

use context::{build_request_headers, default_owner, parse_query_params};
pub(crate) use sigv4::{
    build_canonical_request, extract_credential_scope, extract_signed_headers,
    extract_sigv4_signature, verify_sigv4_signature,
};

#[cfg(test)]
#[allow(clippy::items_after_test_module)]
mod tests {
    use super::*;
    use crate::body::Body;
    use crate::models::policy::{
        Acl, ActionList, BucketPolicyDocument, Grant, Grantee, Permission, PolicyStatementDocument,
        Principal, ResourceList,
    };
    use crate::server::http::Request as ParsedRequest;
    use crate::storage::FilesystemStorage;
    use bytes::Bytes;
    use hyper::Request as HyperRequest;
    use hyper::StatusCode;
    use std::fs;
    use std::sync::Arc;

    fn temp_storage() -> Arc<dyn Storage> {
        let dir = std::env::temp_dir().join(format!("sqrzl-policy-test-{}", uuid::Uuid::new_v4()));
        let _ = fs::create_dir_all(&dir);
        Arc::new(FilesystemStorage::new(dir))
    }

    fn auth_config() -> AuthConfig {
        crate::config::Config {
            access_key_id: None,
            secret_access_key: None,
            enforce_auth: true,
            admin_auth_disabled: false,
            blobs_path: "./blobs".to_string(),
            lifecycle_interval: std::time::Duration::from_hours(1),
            api_port: 9000,
            ui_port: 9001,
            max_request_bytes: crate::config::DEFAULT_SQRZL_MAX_REQUEST_BYTES,
        }
    }

    async fn parsed_request(uri: &str, headers: &[(&str, &str)]) -> ParsedRequest {
        let mut builder = HyperRequest::builder().method("GET").uri(uri);

        for (name, value) in headers {
            builder = builder.header(*name, *value);
        }

        ParsedRequest::from_hyper(
            builder
                .body(Body::from(Bytes::new()))
                .expect("request should build"),
        )
        .await
        .expect("request should parse")
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn should_authorize_list_bucket_only_when_prefix_condition_matches() {
        // Arrange
        let storage = temp_storage();
        storage.create_bucket("bucket".to_string()).unwrap();

        let policy = BucketPolicyDocument {
            version: "2012-10-17".to_string(),
            statement: vec![PolicyStatementDocument {
                sid: Some("allow-prefix".to_string()),
                effect: "Allow".to_string(),
                principal: Principal::All("*".to_string()),
                action: ActionList::Single("s3:ListBucket".to_string()),
                resource: ResourceList::Single("arn:aws:s3:::bucket".to_string()),
                condition: Some(serde_json::json!({
                    "StringEquals": {
                        "s3:prefix": "allowed/"
                    }
                })),
            }],
        };
        storage.put_bucket_policy("bucket", policy).unwrap();

        let allowed_req = parsed_request("http://localhost/bucket?prefix=allowed%2F", &[]).await;
        let denied_req = parsed_request("http://localhost/bucket?prefix=denied%2F", &[]).await;

        // Act
        let allowed = check_authorization(
            &allowed_req,
            &auth_config(),
            &storage,
            "bucket",
            None,
            "s3:ListBucket",
        );
        let denied = check_authorization(
            &denied_req,
            &auth_config(),
            &storage,
            "bucket",
            None,
            "s3:ListBucket",
        );

        // Assert
        assert!(allowed.is_ok());
        let denied = denied.expect_err("request should be denied");
        assert_eq!(denied.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn should_build_standard_sigv4_canonical_request_with_sorted_query() {
        let req = parsed_request(
            "http://localhost/example-bucket/photos/kitten.jpg?prefix=z&list-type=2&prefix=a",
            &[
                ("Host", "example-bucket.localhost:9000"),
                ("X-Amz-Content-Sha256", "UNSIGNED-PAYLOAD"),
                ("X-Amz-Date", "20240101T120000Z"),
            ],
        )
        .await;

        let canonical = build_canonical_request(
            &req,
            &[
                "host".to_string(),
                "x-amz-content-sha256".to_string(),
                "x-amz-date".to_string(),
            ],
        );

        assert_eq!(
            canonical,
            "GET\n/example-bucket/photos/kitten.jpg\nlist-type=2&prefix=a&prefix=z\nhost:example-bucket.localhost:9000\nx-amz-content-sha256:UNSIGNED-PAYLOAD\nx-amz-date:20240101T120000Z\n\nhost;x-amz-content-sha256;x-amz-date\nUNSIGNED-PAYLOAD"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn should_authorize_authenticated_requests_via_explicit_acl_group_grants() {
        let storage = temp_storage();
        storage.create_bucket("bucket".to_string()).unwrap();
        storage
            .put_object(
                "bucket",
                "notes.txt".to_string(),
                crate::models::Object::new(
                    "notes.txt".to_string(),
                    b"payload".to_vec(),
                    "text/plain".to_string(),
                ),
            )
            .unwrap();
        object_service::put_object_acl(
            storage.as_ref(),
            "bucket",
            "notes.txt",
            Acl {
                canned: crate::models::policy::CannedAcl::default(),
                grants: vec![
                    Grant {
                        grantee: Grantee::CanonicalUser {
                            id: "integration-tester".to_string(),
                            display_name: None,
                        },
                        permission: Permission::FullControl,
                    },
                    Grant {
                        grantee: Grantee::Group {
                            uri: "http://acs.amazonaws.com/groups/global/AuthenticatedUsers"
                                .to_string(),
                        },
                        permission: Permission::Read,
                    },
                ],
            },
        )
        .unwrap();

        let auth_config = crate::config::Config {
            access_key_id: Some("integration-tester".to_string()),
            secret_access_key: Some("secret".to_string()),
            enforce_auth: true,
            admin_auth_disabled: false,
            blobs_path: "./blobs".to_string(),
            lifecycle_interval: std::time::Duration::from_hours(1),
            api_port: 9000,
            ui_port: 9001,
            max_request_bytes: crate::config::DEFAULT_SQRZL_MAX_REQUEST_BYTES,
        };

        let allowed_req = parsed_request(
            "http://localhost/bucket/notes.txt?X-Amz-Credential=integration-tester%2F20240101%2Fus-east-1%2Fs3%2Faws4_request",
            &[],
        )
        .await;
        let denied_req = parsed_request("http://localhost/bucket/notes.txt", &[]).await;

        let allowed = check_authorization(
            &allowed_req,
            &auth_config,
            &storage,
            "bucket",
            Some("notes.txt"),
            "s3:GetObject",
        );
        let denied = check_authorization(
            &denied_req,
            &auth_config,
            &storage,
            "bucket",
            Some("notes.txt"),
            "s3:GetObject",
        );

        assert!(allowed.is_ok());
        assert_eq!(
            denied
                .expect_err("anonymous request should be denied")
                .status(),
            StatusCode::FORBIDDEN
        );
    }
}

/// Verify presigned URL query parameters
#[allow(clippy::result_large_err)]
pub(crate) fn verify_presigned_url(
    req: &crate::server::http::Request,
    bucket: &str,
    key: &str,
    auth_config: &AuthConfig,
) -> Result<bool, Response<Body>> {
    if !auth_config.enforce_auth {
        return Ok(true);
    }

    let query_params = &req.query_params;

    // Check if this is a presigned URL request
    let has_presigned_query =
        query_params.contains_key("X-Amz-Signature") || query_params.contains_key("Signature");

    if !has_presigned_query {
        return Ok(true);
    }

    let req_id = header_utils::generate_request_id();

    // Parse presigned URL parameters
    match crate::auth::PresignedUrl::from_query_params(
        bucket,
        key,
        req.method().as_str(),
        query_params,
    ) {
        Ok(presigned) => {
            // Get the host from request headers
            let host = req.header("host").unwrap_or("localhost:9000").to_string();

            // Get secret key for validation
            let Some(secret_key) = auth_config.secret_key() else {
                warn!("Presigned URL validation requested but no secret key configured");
                return Ok(true);
            };

            let presigned_config = crate::auth::PresignedUrlConfig {
                access_key: auth_config
                    .access_key()
                    .unwrap_or("sqrzl-emulator")
                    .to_string(),
                secret_key: secret_key.to_string(),
            };

            // Validate the presigned URL
            if let Err(e) = presigned.validate(&host, &presigned_config) {
                warn!("Presigned URL validation failed: {}", e);
                return Err(xml_error_response(
                    StatusCode::FORBIDDEN,
                    "InvalidSignature",
                    &format!("Presigned URL validation failed: {e}"),
                    &req_id,
                ));
            }

            Ok(true)
        }
        Err(e) => {
            warn!("Failed to parse presigned URL: {}", e);
            Err(xml_error_response(
                StatusCode::BAD_REQUEST,
                "InvalidRequest",
                &format!("Invalid presigned URL parameters: {e}"),
                &req_id,
            ))
        }
    }
}

/// Check if the request is authorized to perform the action
#[allow(clippy::result_large_err)]
pub(crate) fn check_authorization(
    req: &dyn crate::auth::HttpRequestLike,
    auth_config: &AuthConfig,
    storage: &Arc<dyn Storage>,
    bucket: &str,
    key: Option<&str>,
    action: &str,
) -> Result<AuthInfo, Response<Body>> {
    verify_sigv4_signature(req, auth_config)?;

    let auth_info = AuthInfo::from_request(req, auth_config);

    if !auth_config.enforce_auth {
        return Ok(auth_info);
    }

    let resource = if let Some(k) = key {
        format!("arn:aws:s3:::{bucket}/{k}")
    } else {
        format!("arn:aws:s3:::{bucket}")
    };

    let request_headers = build_request_headers(req);
    let query_params = parse_query_params(req.query());
    let existing_object_tags = key
        .map(|object_key| {
            object_service::get_object_tags(storage.as_ref(), bucket, object_key)
                .unwrap_or_default()
        })
        .unwrap_or_default();

    let owner_id = default_owner(auth_config).id;
    let context = AuthContext {
        principal: auth_info.principal.clone(),
        is_authenticated: auth_info.is_authenticated,
        action: action.to_string(),
        resource: resource.clone(),
        bucket_owner: Some(owner_id.clone()),
        object_owner: Some(owner_id.clone()),
        request_headers,
        query_params,
        existing_object_tags,
    };

    let acl_allowed = if let Some(k) = key {
        match object_service::get_object_acl(storage.as_ref(), bucket, k) {
            Ok(acl) => Authorizer::check_acl_permission(&acl, &owner_id, &context),
            Err(_) => false,
        }
    } else {
        match bucket_service::get_bucket_acl(storage.as_ref(), bucket) {
            Ok(acl) => Authorizer::check_acl_permission(&acl, &owner_id, &context),
            Err(_) => false,
        }
    };

    let policy_result = match bucket_service::get_bucket_policy(storage.as_ref(), bucket) {
        Ok(policy) => Authorizer::evaluate_policy(&policy, &context),
        Err(_) => PolicyEffect::Neutral,
    };
    let final_decision = match policy_result {
        PolicyEffect::Deny => PolicyEffect::Deny,
        PolicyEffect::Allow => PolicyEffect::Allow,
        PolicyEffect::Neutral => {
            let is_allowed = acl_allowed
                || (auth_info.is_authenticated && auth_info.principal.contains(&owner_id));
            if is_allowed {
                PolicyEffect::Allow
            } else {
                PolicyEffect::Deny
            }
        }
    };

    if final_decision == PolicyEffect::Allow {
        Ok(auth_info)
    } else {
        warn!(
            principal = %context.principal,
            action = %action,
            resource = %resource,
            "Access denied"
        );
        let req_id = header_utils::generate_request_id();
        Err(xml_error_response(
            StatusCode::FORBIDDEN,
            "AccessDenied",
            "Access Denied",
            &req_id,
        ))
    }
}
