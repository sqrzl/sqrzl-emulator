#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    fn known_verifiers() -> HashSet<&'static str> {
        HashSet::from([
            "api::server::tests::bucket_crud_json",
            "api::server::tests::object_upload_download",
            "auth::sigv4::tests::should_verify_valid_sigv4_signature",
            "interop_auth::should_reject_invalid_signed_gcs_request_given_auth_enforced_when_signature_is_bad",
            "interop_auth::should_reject_unauthorized_azure_request_given_auth_enforced_when_listing_containers",
            "interop_auth::should_reject_unsigned_oci_request_given_auth_enforced_when_request_is_missing_signature",
            "interop_auth::should_reject_unsigned_s3_request_given_auth_enforced_when_request_is_missing_signature",
            "interop_azure::should_enforce_leases_and_retention_given_snapshot_and_immutability_operations_when_deleting_blob",
            "interop_azure::should_list_containers_and_blobs_given_stored_objects_when_querying_azure_lists",
            "interop_azure::should_persist_append_and_page_blob_writes_given_specialized_blob_types_when_uploading_content",
            "interop_azure::should_return_custom_metadata_given_blob_metadata_headers_when_requesting_blob_head",
            "interop_azure::should_return_requested_slice_given_range_header_when_reading_blob_content",
            "interop_azure::should_round_trip_block_blob_given_container_exists_when_using_basic_blob_operations",
            "interop_gcs::should_complete_resumable_upload_given_json_api_session_when_finalizing_media_object",
            "interop_gcs::should_list_matching_objects_given_existing_keys_when_querying_gcs_bucket_contents",
            "interop_gcs::should_return_custom_metadata_given_gcs_metadata_headers_when_requesting_object_head",
            "interop_gcs::should_return_requested_slice_given_range_header_when_reading_gcs_object_content",
            "interop_gcs::should_round_trip_bucket_and_object_operations_given_basic_gcs_requests_when_using_xml_api",
            "interop_oci::should_commit_multipart_object_given_uploaded_parts_when_finalizing_oci_upload",
            "interop_oci::should_list_prefixed_objects_given_nested_keys_when_querying_oci_bucket_contents",
            "interop_oci::should_return_custom_metadata_given_oci_metadata_headers_when_requesting_object_head",
            "interop_oci::should_round_trip_namespace_bucket_and_object_operations_given_basic_oci_requests_when_using_core_flows",
            "interop_s3::should_assemble_completed_object_given_uploaded_parts_when_finishing_s3_multipart_upload",
            "interop_s3::should_list_multiple_versions_given_versioning_enabled_when_object_is_overwritten",
            "interop_s3::should_round_trip_bucket_and_object_operations_given_basic_s3_requests_when_using_crud_flows",
            "providers::azure::tests::should_commit_and_list_blocks_after_adapter_restart",
            "providers::azure::tests::should_commit_block_blob_from_put_block_list",
            "providers::azure::tests::should_create_list_and_fetch_azure_blobs",
            "providers::azure::tests::should_manage_leases_snapshots_and_immutability",
            "providers::azure::tests::should_support_append_and_page_blob_writes",
            "providers::azure::tests::should_update_metadata_return_block_list_and_support_ranges",
            "providers::azure::tests::should_validate_azure_shared_key_and_sas_authorization",
            "providers::gcs::tests::should_complete_resumable_upload_after_adapter_restart",
            "providers::gcs::tests::should_handle_gcs_bucket_and_object_crud",
            "providers::gcs::tests::should_increment_generation_on_overwrite_and_patch_metageneration",
            "providers::gcs::tests::should_enforce_gcs_generation_and_metageneration_preconditions",
            "providers::gcs::tests::should_return_generation_headers_and_support_ranges",
            "providers::gcs::tests::should_support_gcs_resumable_uploads_and_signed_access",
            "providers::gcs::tests::should_support_gcs_json_api_bucket_and_media_flows",
            "providers::oci::tests::should_round_trip_oci_metadata_and_prefix_listing",
            "providers::oci::tests::should_support_oci_multipart_upload_lifecycle",
            "providers::oci::tests::should_support_oci_namespace_bucket_and_object_flows",
            "providers::oci::tests::should_validate_oci_signature_authorization",
            "server::handlers::auth::tests::should_build_standard_sigv4_canonical_request_with_sorted_query",
            "server::handlers::bucket::tests::should_accept_browser_post_uploads",
            "server::handlers::bucket::tests::should_list_version_history_when_versions_query_is_requested",
            "server::handlers::bucket::tests::should_round_trip_request_payment_website_and_cors_bucket_configs",
            "server::handlers::object::s3_contract_tests::should_block_mutation_when_object_lock_headers_are_active",
            "server::handlers::object::s3_contract_tests::should_round_trip_sse_headers_and_require_matching_sse_c_reads",
            "server::http::tests::should_route_virtual_hosted_style_bucket_requests",
            "services::object::tests::should_list_object_versions_through_service",
            "services::object::tests::should_roundtrip_object_through_service",
        ])
    }

    fn known_sdk_verifiers() -> HashSet<&'static str> {
        HashSet::from([
            "sdk-tests/test_azure_sdk.py::test_azure_block_blob_workflow",
            "sdk-tests/test_azure_sdk.py::test_azure_core_blob_workflows",
            "sdk-tests/test_gcs_sdk.py::test_gcs_core_json_workflows",
            "sdk-tests/test_gcs_sdk.py::test_gcs_resumable_upload_workflow",
            "sdk-tests/test_oci_sdk.py::test_oci_core_object_workflows",
            "sdk-tests/test_oci_sdk.py::test_oci_multipart_workflow",
            "sdk-tests/test_s3_sdk.py::test_s3_core_bucket_object_and_metadata_workflows",
            "sdk-tests/test_s3_sdk.py::test_s3_multipart_and_versioning_workflows",
        ])
    }

    #[test]
    fn should_use_allowed_status_values_in_compatibility_matrix() {
        // Arrange
        let matrix: serde_json::Value =
            serde_json::from_str(include_str!("../compatibility-matrix.json"))
                .expect("compatibility matrix should parse");
        let providers = matrix
            .get("providers")
            .and_then(|providers| providers.as_object())
            .expect("providers should be an object");

        // Act
        for (provider_name, operations) in providers {
            let operations = operations
                .as_object()
                .expect("provider operations should be an object");
            for (operation_name, operation) in operations {
                let operation = operation
                    .as_object()
                    .expect("operation entry should be an object");
                let status = operation
                    .get("status")
                    .and_then(|status| status.as_str())
                    .expect("status should be a string");
                assert!(
                    matches!(status, "pass" | "partial" | "missing" | "deferred"),
                    "unexpected compatibility status '{status}' for {provider_name}.{operation_name}"
                );
            }
        }

        // Assert
    }

    #[test]
    fn should_use_allowed_support_tiers_in_compatibility_matrix() {
        // Arrange
        let matrix: serde_json::Value =
            serde_json::from_str(include_str!("../compatibility-matrix.json"))
                .expect("compatibility matrix should parse");
        let providers = matrix
            .get("providers")
            .and_then(|providers| providers.as_object())
            .expect("providers should be an object");

        // Act
        for (provider_name, operations) in providers {
            let operations = operations
                .as_object()
                .expect("provider operations should be an object");
            for (operation_name, operation) in operations {
                let operation = operation
                    .as_object()
                    .expect("operation entry should be an object");
                let support_tier = operation
                    .get("support_tier")
                    .and_then(|support_tier| support_tier.as_str())
                    .expect("support_tier should be a string");
                assert!(
                    matches!(
                        support_tier,
                        "certified" | "partial" | "unsupported" | "deferred"
                    ),
                    "unexpected support tier '{support_tier}' for {provider_name}.{operation_name}"
                );
            }
        }

        // Assert
    }

    #[test]
    fn should_require_sdk_verifier_metadata_for_compatibility_matrix_entries() {
        // Arrange
        let matrix: serde_json::Value =
            serde_json::from_str(include_str!("../compatibility-matrix.json"))
                .expect("compatibility matrix should parse");
        let providers = matrix
            .get("providers")
            .and_then(|providers| providers.as_object())
            .expect("providers should be an object");

        // Act
        for (provider_name, operations) in providers {
            let operations = operations
                .as_object()
                .expect("provider operations should be an object");
            for (operation_name, operation) in operations {
                let operation = operation
                    .as_object()
                    .expect("operation entry should be an object");
                let support_tier = operation
                    .get("support_tier")
                    .and_then(|support_tier| support_tier.as_str())
                    .expect("support_tier should be a string");
                let sdk_verifiers = operation
                    .get("sdk_verified_by")
                    .and_then(|value| value.as_array())
                    .expect("sdk_verified_by should be an array");
                let limitations = operation
                    .get("limitations")
                    .and_then(|value| value.as_array())
                    .expect("limitations should be an array");
                if support_tier == "certified" {
                    assert!(
                        !sdk_verifiers.is_empty(),
                        "certified support tier for {provider_name}.{operation_name} must name at least one SDK verifier"
                    );
                } else {
                    assert!(
                        !limitations.is_empty(),
                        "non-certified support tier for {provider_name}.{operation_name} must document limitations"
                    );
                }
            }
        }

        // Assert
    }

    #[test]
    fn should_require_verifiers_for_pass_entries_in_compatibility_matrix() {
        // Arrange
        let matrix: serde_json::Value =
            serde_json::from_str(include_str!("../compatibility-matrix.json"))
                .expect("compatibility matrix should parse");
        let providers = matrix
            .get("providers")
            .and_then(|providers| providers.as_object())
            .expect("providers should be an object");

        // Act
        for (provider_name, operations) in providers {
            let operations = operations
                .as_object()
                .expect("provider operations should be an object");
            for (operation_name, operation) in operations {
                let operation = operation
                    .as_object()
                    .expect("operation entry should be an object");
                let status = operation
                    .get("status")
                    .and_then(|status| status.as_str())
                    .expect("status should be a string");
                let verifiers = operation
                    .get("verified_by")
                    .and_then(|value| value.as_array())
                    .expect("verified_by should be an array");
                if status == "pass" {
                    assert!(
                        !verifiers.is_empty(),
                        "pass status for {provider_name}.{operation_name} must name at least one verifier"
                    );
                    let auth_only_operation = matches!(
                        operation_name.as_str(),
                        "sigv4"
                            | "shared_key_auth"
                            | "sas_auth"
                            | "signed_url_v2"
                            | "request_signing"
                    );
                    if !auth_only_operation {
                        assert!(
                            verifiers
                                .iter()
                                .filter_map(|value| value.as_str())
                                .any(|verifier| {
                                    verifier.starts_with("interop_")
                                        || verifier.starts_with("server::")
                                }),
                            "pass status for {provider_name}.{operation_name} must include an interop or black-box verifier"
                        );
                    }
                }
            }
        }

        // Assert
    }

    #[test]
    fn should_reference_only_known_verifiers_in_compatibility_matrix() {
        // Arrange
        let matrix: serde_json::Value =
            serde_json::from_str(include_str!("../compatibility-matrix.json"))
                .expect("compatibility matrix should parse");
        let providers = matrix
            .get("providers")
            .and_then(|providers| providers.as_object())
            .expect("providers should be an object");
        let known_verifiers = known_verifiers();

        // Act
        for (provider_name, operations) in providers {
            let operations = operations
                .as_object()
                .expect("provider operations should be an object");
            for (operation_name, operation) in operations {
                let operation = operation
                    .as_object()
                    .expect("operation entry should be an object");
                let verifiers = operation
                    .get("verified_by")
                    .and_then(|value| value.as_array())
                    .expect("verified_by should be an array");
                for verifier in verifiers {
                    let verifier = verifier
                        .as_str()
                        .expect("verifier entries should be strings");
                    assert!(
                        known_verifiers.contains(verifier),
                        "unknown verifier '{verifier}' declared for {provider_name}.{operation_name}"
                    );
                }
            }
        }

        // Assert
    }

    #[test]
    fn should_reference_only_known_sdk_verifiers_in_compatibility_matrix() {
        // Arrange
        let matrix: serde_json::Value =
            serde_json::from_str(include_str!("../compatibility-matrix.json"))
                .expect("compatibility matrix should parse");
        let providers = matrix
            .get("providers")
            .and_then(|providers| providers.as_object())
            .expect("providers should be an object");
        let known_sdk_verifiers = known_sdk_verifiers();

        // Act
        for (provider_name, operations) in providers {
            let operations = operations
                .as_object()
                .expect("provider operations should be an object");
            for (operation_name, operation) in operations {
                let operation = operation
                    .as_object()
                    .expect("operation entry should be an object");
                let sdk_verifiers = operation
                    .get("sdk_verified_by")
                    .and_then(|value| value.as_array())
                    .expect("sdk_verified_by should be an array");
                for verifier in sdk_verifiers {
                    let verifier = verifier
                        .as_str()
                        .expect("SDK verifier entries should be strings");
                    assert!(
                        known_sdk_verifiers.contains(verifier),
                        "unknown SDK verifier '{verifier}' declared for {provider_name}.{operation_name}"
                    );
                }
            }
        }

        // Assert
    }
}
