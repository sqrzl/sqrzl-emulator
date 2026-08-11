#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::fs;
    use std::path::Path;

    fn collect_source(root: &Path, extensions: &[&str], output: &mut String) {
        for entry in fs::read_dir(root).expect("source directory should be readable") {
            let path = entry.expect("source entry should be readable").path();
            if path.is_dir() {
                collect_source(&path, extensions, output);
            } else if path
                .extension()
                .and_then(|extension| extension.to_str())
                .is_some_and(|extension| extensions.contains(&extension))
            {
                output.push_str(&fs::read_to_string(path).expect("source file should be readable"));
                output.push('\n');
            }
        }
    }

    fn declared_verifiers(field: &str) -> Vec<String> {
        let matrix: serde_json::Value =
            serde_json::from_str(include_str!("../compatibility-matrix.json"))
                .expect("compatibility matrix should parse");
        matrix["providers"]
            .as_object()
            .expect("providers should be an object")
            .values()
            .flat_map(|operations| {
                operations
                    .as_object()
                    .expect("operations should be an object")
                    .values()
            })
            .flat_map(|operation| {
                operation[field]
                    .as_array()
                    .expect("verifier field should be an array")
                    .iter()
            })
            .map(|value| {
                value
                    .as_str()
                    .expect("verifier should be a string")
                    .to_string()
            })
            .collect()
    }

    // Keep the complete evidence registry in one auditable literal so a matrix
    // reference cannot be hidden behind provider-specific helper composition.
    #[allow(clippy::too_many_lines)]
    fn known_verifiers() -> HashSet<&'static str> {
        HashSet::from([
            "interop_email::should_capture_message_when_sending_over_a_smtp_session",
            "interop_email::should_capture_sendgrid_send_and_fan_out_recipients",
            "interop_email::should_capture_ses_send_with_signature_authorization",
            "interop_email::should_capture_acs_send_with_recipients_and_attachments",
            "interop_email::should_apply_acs_email_repeatability_without_overwriting_or_duplicate_capture",
            "interop_email::should_reject_unsupported_mail_shapes_before_capture",
            "e2e_email::should_capture_message_when_sending_over_a_real_smtp_socket",
            "mail::providers::tests::should_render_provider_shaped_incomplete_body_responses",
            "mail::smtp::tests::should_accept_the_rfc_5321_null_reverse_path",
            "mail::tests::should_leave_no_captured_copies_when_fan_out_validation_fails",
            "mail::tests::should_roll_back_every_personalization_when_a_batch_store_fails_after_writing",
            "e2e_sms::should_simulate_list_download_and_delete_texts_through_admin_api",
            "sms::providers::tests::should_capture_twilio_repeated_media_and_return_sdk_resource",
            "sms::providers::tests::should_distinguish_sns_publish_and_sms_voice_from_ordinary_root_requests",
            "sms::providers::tests::should_store_each_acs_recipient_with_one_shared_batch_id",
            "sms::providers::tests::should_apply_sns_json_protocol_selection_and_reject_malformed_structures",
            "sms::providers::tests::should_claim_malformed_provider_requests_before_storage_routing",
            "sms::providers::tests::should_reject_invalid_twilio_sids_media_and_unsupported_fields_without_capture",
            "sms::providers::tests::should_render_provider_shaped_incomplete_body_responses",
            "sms::providers::tests::should_return_twilio_accepted_only_while_messaging_service_selects_sender",
            "sms::providers::tests::should_support_sns_get_and_acs_per_recipient_results",
            "sms::providers::tests::should_validate_acs_sms_options_and_repeatability_without_duplicate_capture",
            "sms::providers::tests::should_validate_every_aws_sms_voice_field_before_capture",
            "sms::simulator::tests::should_render_provider_specific_event_batches",
            "sms::simulator::tests::should_send_signed_twilio_callback_record_twiml_and_keep_retry_history",
            "auth::sigv4::tests::should_verify_valid_sigv4_signature",
            "interop_auth::should_authorize_gcs_v2_hmac_extension_headers_through_provider_registry",
            "interop_auth::should_reject_invalid_signed_gcs_request_given_auth_enforced_when_signature_is_bad",
            "interop_auth::should_reject_unauthorized_azure_request_given_auth_enforced_when_listing_containers",
            "interop_auth::should_reject_unsigned_oci_request_given_auth_enforced_when_request_is_missing_signature",
            "interop_auth::should_reject_unsigned_s3_request_given_auth_enforced_when_request_is_missing_signature",
            "interop_azure::should_enforce_leases_and_retention_given_snapshot_and_immutability_operations_when_deleting_blob",
            "interop_azure::should_delete_nonempty_container_given_delete_container_request",
            "interop_azure::should_list_containers_and_blobs_given_stored_objects_when_querying_azure_lists",
            "interop_azure::should_not_apply_azure_subresource_mutations_given_stale_or_weak_conditions",
            "interop_azure::should_page_azure_blob_prefixes_without_exposing_snapshot_storage_keys",
            "interop_azure::should_persist_append_and_page_blob_writes_given_specialized_blob_types_when_uploading_content",
            "interop_azure::should_read_selected_azure_version_bytes_given_a_range_request",
            "interop_azure::should_return_azure_pagination_and_container_identity_headers",
            "interop_azure::should_return_custom_metadata_given_blob_metadata_headers_when_requesting_blob_head",
            "interop_azure::should_return_requested_slice_given_range_header_when_reading_blob_content",
            "interop_azure::should_round_trip_block_blob_given_container_exists_when_using_basic_blob_operations",
            "interop_azure::should_require_content_length_for_azure_put_blob_without_mutating",
            "interop_azure::should_require_content_length_for_azure_block_append_and_page_mutations",
            "interop_azure::should_require_real_blob_type_and_specialized_blob_creation_headers",
            "providers::azure::tests::should_preserve_azure_committed_and_uncommitted_block_selectors",
            "interop_azure::should_require_snapshot_delete_mode_and_preserve_base_on_only",
            "interop_gcs::should_complete_resumable_upload_given_json_api_session_when_finalizing_media_object",
            "interop_gcs::should_list_matching_objects_given_existing_keys_when_querying_gcs_bucket_contents",
            "interop_gcs::should_return_custom_metadata_given_gcs_metadata_headers_when_requesting_object_head",
            "interop_gcs::should_return_requested_slice_given_range_header_when_reading_gcs_object_content",
            "interop_gcs::should_round_trip_bucket_and_object_operations_given_basic_gcs_requests_when_using_xml_api",
            "interop_oci::should_commit_multipart_object_given_uploaded_parts_when_finalizing_oci_upload",
            "interop_oci::should_count_oci_prefixes_toward_limit_and_keep_start_inclusive",
            "interop_oci::should_echo_oci_client_request_id_and_use_http_dates",
            "interop_oci::should_enforce_oci_object_conditions_without_mutating_on_failure",
            "interop_oci::should_inherit_and_validate_oci_storage_tiers",
            "interop_oci::should_list_prefixed_objects_given_nested_keys_when_querying_oci_bucket_contents",
            "interop_oci::should_paginate_oci_objects_with_next_start_with_body_token",
            "interop_oci::should_reject_oci_mutations_on_gcs_retained_bucket_without_committing",
            "interop_oci::should_return_oci_resource_specific_missing_errors",
            "interop_oci::should_return_custom_metadata_given_oci_metadata_headers_when_requesting_object_head",
            "interop_oci::should_round_trip_namespace_bucket_and_object_operations_given_basic_oci_requests_when_using_core_flows",
            "interop_oci::should_validate_oci_md5_and_round_trip_object_response_metadata",
            "interop_s3::should_assemble_completed_object_given_uploaded_parts_when_finishing_s3_multipart_upload",
            "interop_s3::should_return_entity_too_small_without_consuming_s3_multipart_upload",
            "interop_s3::should_list_multiple_versions_given_versioning_enabled_when_object_is_overwritten",
            "interop_s3::should_round_trip_bucket_and_object_operations_given_basic_s3_requests_when_using_crud_flows",
            "interop_s3::should_use_one_null_version_and_preserve_non_null_history_when_versioning_is_suspended",
            "lifecycle::tests::should_not_eagerly_expire_objects_from_gcs_retained_buckets",
            "lifecycle::tests::should_not_expire_gcs_soft_delete_history_through_s3_lifecycle",
            "lifecycle::tests::should_not_expire_locked_s3_noncurrent_versions",
            "provider_conformance::should_commit_before_response_loss_on_every_storage_front_door",
            "provider_conformance::should_distinguish_timeout_before_and_after_commit_on_every_front_door",
            "provider_conformance::should_expose_s3_conditional_request_conflict_without_mutation",
            "provider_conformance::should_expose_each_transient_status_before_commit_on_every_storage_front_door",
            "provider_conformance::should_enforce_azure_if_none_match_etag_on_put",
            "provider_conformance::should_enforce_gcs_xml_generation_precondition_on_delete",
            "provider_conformance::should_enforce_gcs_xml_generation_preconditions_on_create_and_update",
            "provider_conformance::should_keep_redirects_precommit_on_every_storage_front_door",
            "provider_conformance::should_keep_gcs_json_resumable_sessions_retriable_after_framing_failures",
            "provider_conformance::should_not_apply_failed_gcs_generation_not_match_upload",
            "provider_conformance::should_not_apply_failed_s3_conditional_delete",
            "provider_conformance::should_not_treat_if_none_match_as_an_s3_delete_precondition",
            "provider_conformance::should_preserve_azure_empty_mutation_statuses",
            "provider_conformance::should_preserve_gcs_json_empty_mutation_statuses",
            "provider_conformance::should_preserve_gcs_xml_empty_mutation_statuses",
            "provider_conformance::should_preserve_oci_empty_mutation_statuses",
            "provider_conformance::should_preserve_protocol_durability_lifecycle_across_cache_loss_on_every_front_door",
            "provider_conformance::should_allow_chunked_gcs_uploads_without_content_length",
            "provider_conformance::should_route_non_upload_gcs_methods_before_enforcing_upload_framing",
            "provider_conformance::should_reject_content_length_mismatch_on_every_storage_front_door",
            "provider_conformance::should_reject_gcs_json_etag_mutation_headers_without_applying_delete",
            "provider_conformance::should_reject_malformed_gcs_json_mutation_bodies_without_mutation",
            "provider_conformance::should_reject_gcs_json_upload_metageneration_precondition_without_mutation",
            "provider_conformance::should_reject_gcs_data_protection_activation_on_s3_versioned_bucket",
            "provider_conformance::should_reject_non_wildcard_s3_if_none_match_put",
            "provider_conformance::should_reject_s3_mutation_while_azure_versioning_owns_the_bucket",
            "provider_conformance::should_reject_s3_versioning_changes_on_foreign_protected_bucket",
            "provider_conformance::should_require_provider_specific_content_length_for_azure_and_gcs_uploads",
            "provider_conformance::should_require_explicit_zero_length_for_s3_object_put",
            "provider_conformance::should_render_gcs_json_payload_too_large_as_json_without_mutation",
            "provider_conformance::should_report_gcs_json_metadata_document_length_separately_from_object_size",
            "provider_conformance::should_return_every_redirect_before_mutation_on_every_storage_front_door",
            "provider_conformance::should_return_provider_shaped_throttling_without_mutation",
            "provider_conformance::should_return_gcs_json_not_found_for_missing_bucket_operations",
            "provider_conformance::should_return_gcs_xml_no_such_bucket_across_bucket_and_object_operations",
            "provider_conformance::should_return_provider_specific_missing_object_responses",
            "provider_conformance::should_return_s3_not_found_for_if_match_put_without_current_object",
            "provider_conformance::should_rewrite_malformed_and_repeated_pagination_tokens_on_every_storage_front_door",
            "provider_conformance::should_surface_truncated_response_as_body_error_on_every_storage_front_door",
            "providers::azure::tests::should_commit_and_list_blocks_after_adapter_restart",
            "providers::azure::tests::should_commit_block_blob_from_put_block_list",
            "providers::azure::tests::should_concatenate_azure_canonical_headers_and_resource_without_blank_line",
            "providers::azure::tests::should_create_list_and_fetch_azure_blobs",
            "providers::azure::tests::should_create_new_versions_when_overwriting_azure_worm_blobs",
            "providers::azure::tests::should_isolate_foreign_version_history_and_validate_local_mode_before_create",
            "providers::azure::tests::should_manage_leases_snapshots_and_immutability",
            "providers::azure::tests::should_fail_closed_for_malformed_azure_worm_metadata",
            "providers::azure::tests::should_not_schedule_or_purge_container_with_azure_worm_data",
            "providers::azure::tests::should_preserve_active_lease_across_azure_blob_overwrites",
            "providers::azure::tests::should_preserve_azure_worm_state_on_invalid_mutations",
            "providers::azure::tests::should_preserve_version_identity_for_azure_lease_and_worm_metadata",
            "providers::azure::tests::should_decode_azure_blob_paths_once_and_preserve_empty_segments",
            "providers::azure::tests::should_reject_invalid_azure_block_ids_without_staging_them",
            "providers::azure::tests::should_reject_invalid_azure_container_name_without_mutation",
            "providers::azure::tests::should_reject_malformed_azure_block_lists_without_committing",
            "providers::azure::tests::should_reject_azure_selectors_on_unsupported_mutations_without_side_effects",
            "providers::azure::tests::should_reject_unsafe_and_unsupported_azure_version_selectors_without_mutating",
            "providers::azure::tests::should_reject_unsupported_azure_subresources_before_mutation",
            "providers::azure::tests::should_reject_unsupported_azure_copy_variants_without_mutating",
            "providers::azure::tests::should_render_native_azure_incomplete_body_error",
            "providers::azure::tests::should_return_container_not_found_for_every_missing_container_blob_verb",
            "providers::azure::tests::should_support_append_and_page_blob_writes",
            "providers::azure::tests::should_update_metadata_return_block_list_and_support_ranges",
            "providers::azure::tests::should_validate_azure_shared_key_and_sas_authorization",
            "providers::gcs::tests::should_complete_resumable_upload_after_adapter_restart",
            "providers::gcs::tests::should_apply_resumable_json_metadata_and_body_name_without_silent_fields",
            "providers::gcs::tests::should_authorize_gcs_v2_hmac_request_with_canonicalized_extension_headers",
            "providers::gcs::tests::should_handle_gcs_bucket_and_object_crud",
            "providers::gcs::tests::should_increment_generation_on_overwrite_and_patch_metageneration",
            "providers::gcs::tests::should_patch_protected_object_metadata_without_creating_a_new_version",
            "providers::gcs::tests::should_reject_retained_overwrite_and_invalid_generation_preconditions",
            "providers::gcs::tests::should_reject_unsupported_or_invalid_bucket_retention_configuration_without_mutation",
            "providers::gcs::tests::should_enforce_gcs_generation_and_metageneration_preconditions",
            "providers::gcs::tests::should_return_generation_headers_and_support_ranges",
            "providers::gcs::tests::should_preserve_binary_multipart_media_bytes_exactly",
            "providers::gcs::tests::should_reject_invalid_gcs_json_media_x_goog_hash_without_mutation",
            "providers::gcs::tests::should_reject_invalid_gcs_bucket_name_without_mutation",
            "providers::gcs::tests::should_reject_mismatched_gcs_json_multipart_crc32c_without_mutation",
            "providers::gcs::tests::should_reject_mismatched_resumable_metadata_crc32c_without_mutation",
            "providers::gcs::tests::should_retain_resumable_session_after_x_goog_hash_crc32c_mismatch",
            "providers::gcs::tests::should_sort_merge_and_filter_gcs_v2_extension_headers_in_string_to_sign",
            "providers::gcs::tests::should_support_gcs_resumable_uploads_and_signed_access",
            "providers::gcs::tests::should_support_gcs_json_api_bucket_and_media_flows",
            "providers::gcs::tests::should_validate_matching_gcs_json_multipart_crc32c",
            "providers::gcs::tests::should_validate_matching_gcs_json_media_x_goog_hash_crc32c",
            "providers::gcs::tests::should_serialize_conflicting_s3_and_gcs_data_protection_activation",
            "providers::oci::tests::should_round_trip_oci_metadata_and_prefix_listing",
            "providers::oci::tests::should_decode_oci_object_paths_once_without_collapsing_key_components",
            "providers::oci::tests::should_never_commit_unlisted_or_excluded_oci_multipart_parts",
            "providers::oci::tests::should_preserve_oci_multipart_object_path_components",
            "providers::oci::tests::should_reject_invalid_oci_object_encoding_without_mutating",
            "providers::oci::tests::should_reject_invalid_oci_bucket_name_without_mutation",
            "providers::oci::tests::should_reject_invented_oci_bucket_put_alias_without_creating_bucket",
            "providers::oci::tests::should_reject_oci_version_scoped_operations_without_touching_current_object",
            "providers::oci::tests::should_reject_unsupported_oci_multipart_conditions_without_consuming_session",
            "providers::oci::tests::should_render_native_oci_incomplete_body_error",
            "providers::oci::tests::should_return_native_oci_errors_for_invalid_multipart_requests",
            "providers::oci::tests::should_support_oci_multipart_upload_lifecycle",
            "providers::oci::tests::should_support_oci_namespace_bucket_and_object_flows",
            "providers::oci::tests::should_reject_non_oci_hmac_signature_authorization",
            "providers::oci::tests::should_reject_malformed_or_wrong_algorithm_signature_authorization",
            "providers::oci::tests::should_report_valid_oci_signature_shape_as_explicitly_unsupported",
            "providers::tests::should_route_local_gcs_resumable_session_uris_back_to_gcs",
            "providers::tests::should_serialize_conflicting_azure_and_gcs_protection_creation",
            "providers::tests::should_serialize_conflicting_azure_and_s3_protection_creation",
            "server::adapter_routing_tests::should_preserve_s3_object_key_identity_through_the_http_surface",
            "server::adapter_routing_tests::should_reject_truncated_storage_request_bodies_before_any_mutation",
            "server::adapter_routing_tests::should_route_dotted_s3_virtual_host_to_the_complete_bucket_name",
            "server::handlers::auth::tests::should_build_standard_sigv4_canonical_request_with_sorted_query",
            "server::handlers::bucket::tests::should_accept_browser_post_uploads",
            "server::handlers::bucket::tests::should_fully_parse_multi_delete_before_mutating_any_object",
            "server::handlers::bucket::tests::should_reject_schema_invalid_multi_delete_nesting_without_mutating",
            "server::handlers::bucket::tests::should_require_valid_content_md5_before_multi_delete_mutation",
            "server::handlers::bucket::tests::should_report_multi_delete_lock_and_etag_failures_without_deleting",
            "server::handlers::bucket::tests::should_enable_object_lock_at_bucket_creation_and_prevent_versioning_suspension",
            "server::handlers::bucket::tests::should_list_version_history_when_versions_query_is_requested",
            "server::handlers::bucket::tests::should_round_trip_request_payment_website_and_cors_bucket_configs",
            "server::handlers::object::s3_contract_tests::should_reject_object_lock_headers_when_bucket_mode_is_not_enabled",
            "server::handlers::object::s3_contract_tests::should_not_alias_or_delete_version_data_given_unsafe_version_ids",
            "server::handlers::object::s3_contract_tests::should_reject_mismatched_content_md5_without_object_mutation",
            "server::handlers::object::s3_contract_tests::should_distinguish_missing_key_and_delete_marker_for_if_match_star_delete",
            "server::handlers::object::s3_contract_tests::should_reject_upload_part_copy_without_storing_an_empty_part",
            "server::handlers::object::s3_contract_tests::should_not_treat_weak_if_match_as_a_strong_s3_precondition",
            "server::handlers::object::s3_contract_tests::should_reject_unsupported_copy_range_and_invalid_directives_without_mutation",
            "server::handlers::object::s3_contract_tests::should_reject_malformed_copy_source_encoding_without_destination_mutation",
            "server::handlers::object::s3_contract_tests::should_reject_non_wildcard_copy_destination_if_none_match_without_mutation",
            "server::handlers::object::s3_contract_tests::should_replace_copy_content_type_and_metadata_without_changing_copy_semantics",
            "server::handlers::object::s3_contract_tests::should_return_precondition_failed_when_copy_source_was_not_modified_since",
            "server::handlers::object::s3_contract_tests::should_add_delete_marker_without_removing_locked_current_version",
            "server::handlers::object::s3_contract_tests::should_add_new_version_without_overwriting_locked_current_version",
            "server::handlers::object::s3_contract_tests::should_reject_permanent_delete_of_locked_object_version",
            "server::handlers::object::s3_contract_tests::should_explicitly_reject_unsupported_governance_bypass_without_deleting_version",
            "server::handlers::object::s3_contract_tests::should_reject_copying_delete_marker_version_without_destination_mutation",
            "server::handlers::object::s3_contract_tests::should_reject_object_lock_multipart_initiation_without_creating_a_session",
            "server::handlers::object::s3_contract_tests::should_reject_unsupported_conditional_multipart_completion_without_consuming_upload",
            "server::handlers::object::s3_contract_tests::should_preserve_locked_current_version_tags_when_tag_mutations_fail",
            "server::handlers::object::s3_contract_tests::should_reject_version_scoped_tagging_without_mutating_the_version",
            "server::handlers::object::s3_contract_tests::should_reject_orphan_or_nonfuture_object_lock_retention_headers_before_mutation",
            "server::handlers::object::s3_contract_tests::should_return_empty_not_found_for_missing_head_version",
            "server::handlers::object::s3_contract_tests::should_return_empty_not_found_for_missing_current_head",
            "server::handlers::object::s3_contract_tests::should_return_empty_method_not_allowed_for_delete_marker_version_head",
            "storage::filesystem::tests::should_replace_provider_metadata_without_changing_version_identity_or_history",
            "storage::filesystem::tests::should_reject_unsafe_version_ids_without_aliasing_current_or_historical_data",
            "server::handlers::object::s3_contract_tests::should_return_empty_not_found_with_marker_identity_for_current_delete_marker_head",
            "server::handlers::object::s3_contract_tests::should_return_method_not_allowed_for_delete_marker_version_get",
            "server::handlers::object::s3_contract_tests::should_return_not_found_with_marker_identity_for_current_delete_marker_get",
            "server::handlers::object::s3_contract_tests::should_return_not_found_for_ranged_get_of_missing_object",
            "server::handlers::object::s3_contract_tests::should_round_trip_sse_headers_and_require_matching_sse_c_reads",
            "server::http::tests::should_route_virtual_hosted_style_bucket_requests",
            "services::object::tests::should_list_object_versions_through_service",
            "services::object::tests::should_roundtrip_object_through_service",
        ])
    }

    fn known_sdk_verifiers() -> HashSet<&'static str> {
        HashSet::from([
            "sdk-tests/test_email_sdk.py::test_smtp_sdk_sends_message",
            "sdk-tests/test_email_sdk.py::test_sendgrid_sdk_send",
            "sdk-tests/test_email_sdk.py::test_ses_sdk_send",
            "sdk-tests/test_email_sdk.py::test_azure_communication_email_sdk_send",
            "sdk-tests/test_azure_sdk.py::test_azure_block_blob_workflow",
            "sdk-tests/test_azure_sdk.py::test_azure_core_blob_workflows",
            "sdk-tests/test_gcs_sdk.py::test_gcs_core_json_workflows",
            "sdk-tests/test_gcs_sdk.py::test_gcs_resumable_upload_workflow",
            "sdk-tests/test_oci_sdk.py::test_oci_core_object_workflows",
            "sdk-tests/test_oci_sdk.py::test_oci_multipart_workflow",
            "sdk-tests/test_s3_sdk.py::test_s3_core_bucket_object_and_metadata_workflows",
            "sdk-tests/test_s3_sdk.py::test_s3_multipart_and_versioning_workflows",
            "sdk-tests/test_sms_sdk.py::test_twilio_messages_sdk",
            "sdk-tests/test_sms_sdk.py::test_boto3_sns_direct_publish",
            "sdk-tests/test_sms_sdk.py::test_boto3_sms_voice_v2",
            "sdk-tests/test_sms_sdk.py::test_azure_communication_sms_sdk",
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

    #[test]
    fn should_reference_test_functions_that_exist_in_the_source_tree() {
        // Arrange
        let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
        let mut rust_source = String::new();
        collect_source(&manifest.join("src"), &["rs"], &mut rust_source);
        collect_source(&manifest.join("tests"), &["rs"], &mut rust_source);
        let mut python_source = String::new();
        collect_source(&manifest.join("sdk-tests"), &["py"], &mut python_source);

        // Act
        let rust_verifiers = declared_verifiers("verified_by");
        let sdk_verifiers = declared_verifiers("sdk_verified_by");

        // Assert
        for verifier in rust_verifiers {
            let function = verifier
                .rsplit("::")
                .next()
                .expect("Rust verifier should include a function name");
            assert!(
                rust_source.contains(&format!("fn {function}(")),
                "Rust verifier '{verifier}' does not name an existing test function"
            );
        }
        for verifier in sdk_verifiers {
            let function = verifier
                .rsplit("::")
                .next()
                .expect("SDK verifier should include a function name");
            assert!(
                python_source.contains(&format!("def {function}(")),
                "SDK verifier '{verifier}' does not name an existing test function"
            );
        }
    }
}
