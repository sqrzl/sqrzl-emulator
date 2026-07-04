/// Request validation utilities for the storage APIs.
/// Validates a bucket name against the common cross-vendor subset.
/// - Must be between 3 and 63 characters
/// - Can contain lowercase letters, numbers, and hyphens
/// - Cannot start or end with a hyphen
/// - Cannot contain consecutive hyphens
///
/// # Errors
///
/// Returns an error when the underlying emulator operation fails.
pub fn validate_bucket_name(name: &str) -> Result<(), String> {
    if name.len() < 3 {
        return Err("Bucket name must be at least 3 characters long".to_string());
    }
    if name.len() > 63 {
        return Err("Bucket name cannot exceed 63 characters".to_string());
    }

    if name.starts_with('-') || name.ends_with('-') {
        return Err("Bucket name cannot start or end with a hyphen".to_string());
    }

    if !name
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    {
        return Err(
            "Bucket name can only contain lowercase letters, numbers, and hyphens".to_string(),
        );
    }

    if name.contains("--") {
        return Err("Bucket name cannot contain consecutive hyphens".to_string());
    }

    Ok(())
}

/// Validates a blob key.
/// - Must be 1 to 1024 bytes
/// - Can contain slash-separated path segments
/// - Path segments may contain ASCII letters, numbers, dots, underscores, and hyphens
/// - Path segments cannot be empty or dot segments
///
/// # Errors
///
/// Returns an error when the underlying emulator operation fails.
pub fn validate_blob_key(key: &str) -> Result<(), String> {
    let byte_len = key.len();

    if byte_len == 0 {
        return Err("Blob key cannot be empty".to_string());
    }

    if byte_len > 1024 {
        return Err(format!(
            "Blob key cannot exceed 1024 bytes (got {byte_len})"
        ));
    }

    for segment in key.split('/') {
        if segment.is_empty() {
            return Err("Blob key cannot contain empty path segments".to_string());
        }

        if segment == "." || segment == ".." {
            return Err("Blob key cannot contain dot path segments".to_string());
        }

        if !segment
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-')
        {
            return Err(
                "Blob key can only contain ASCII letters, numbers, dots, underscores, hyphens, and slashes"
                    .to_string(),
            );
        }
    }

    Ok(())
}

///
/// # Errors
///
/// Returns an error when the underlying emulator operation fails.
pub fn validate_object_key(key: &str) -> Result<(), String> {
    validate_blob_key(key)
}

/// Validates a multipart part number
/// - Must be between 1 and 10000 inclusive
///
/// # Errors
///
/// Returns an error when the underlying emulator operation fails.
pub fn validate_part_number(part_num: u32) -> Result<(), String> {
    if part_num < 1 {
        return Err("Part number must be at least 1".to_string());
    }
    if part_num > 10000 {
        return Err("Part number cannot exceed 10000".to_string());
    }
    Ok(())
}

/// Validates Content-Length header
/// - Must be non-negative
/// - Must not exceed reasonable limits (e.g., 5GB per S3 spec)
///
/// # Errors
///
/// Returns an error when the underlying emulator operation fails.
pub fn validate_content_length(content_length: u64) -> Result<(), String> {
    // S3 allows objects up to 5TB, but we'll be more conservative (5GB)
    let max_size = 5 * 1024 * 1024 * 1024u64;

    if content_length > max_size {
        return Err(format!(
            "Content-Length ({content_length} bytes) exceeds maximum allowed size"
        ));
    }

    Ok(())
}

/// Validates multipart upload ID format
/// - Should be a non-empty string (typically alphanumeric)
///
/// # Errors
///
/// Returns an error when the underlying emulator operation fails.
pub fn validate_upload_id(upload_id: &str) -> Result<(), String> {
    if upload_id.is_empty() {
        return Err("Upload ID cannot be empty".to_string());
    }

    if upload_id.len() > 1024 {
        return Err("Upload ID is too long".to_string());
    }

    Ok(())
}

/// Validates that a part number is in expected sequence
/// Expects parts to be provided in ascending order
///
/// # Errors
///
/// Returns an error when the underlying emulator operation fails.
pub fn validate_part_sequence(part_numbers: &[u32]) -> Result<(), String> {
    for (i, &part_num) in part_numbers.iter().enumerate() {
        validate_part_number(part_num)?;

        // Check that parts are in order and contiguous from start
        if i > 0 {
            let prev = part_numbers[i - 1];
            if part_num != prev + 1 {
                return Err(format!(
                    "Parts must be in contiguous sequence. Expected part {}, got {}",
                    prev + 1,
                    part_num
                ));
            }
        } else if part_num != 1 {
            return Err("Part sequence must start with part number 1".to_string());
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // Bucket name validation tests
    #[test]
    fn should_accept_valid_bucket_name() {
        assert!(validate_bucket_name("my-bucket").is_ok());
        assert!(validate_bucket_name("bucket123").is_ok());
        assert!(validate_bucket_name("a-b-c-d").is_ok());
    }

    #[test]
    fn should_reject_bucket_name_too_short() {
        assert!(validate_bucket_name("ab").is_err());
        assert!(validate_bucket_name("a").is_err());
    }

    #[test]
    fn should_reject_bucket_name_too_long() {
        let long_name = "a".repeat(64);
        assert!(validate_bucket_name(&long_name).is_err());
    }

    #[test]
    fn should_reject_bucket_name_with_uppercase() {
        assert!(validate_bucket_name("MyBucket").is_err());
        assert!(validate_bucket_name("BUCKET").is_err());
    }

    #[test]
    fn should_reject_bucket_name_starting_with_hyphen() {
        assert!(validate_bucket_name("-bucket").is_err());
    }

    #[test]
    fn should_reject_bucket_name_ending_with_hyphen() {
        assert!(validate_bucket_name("bucket-").is_err());
    }

    #[test]
    fn should_reject_bucket_name_like_ip_address() {
        assert!(validate_bucket_name("192.168.1.1").is_err());
        assert!(validate_bucket_name("10.0.0.1").is_err());
    }

    #[test]
    fn should_reject_bucket_name_with_dots() {
        assert!(validate_bucket_name("bucket.name").is_err());
    }

    #[test]
    fn should_reject_bucket_name_with_consecutive_hyphens() {
        assert!(validate_bucket_name("bucket--name").is_err());
    }

    // Blob key validation tests
    #[test]
    fn should_accept_valid_blob_key() {
        // Arrange
        // Act
        assert!(validate_blob_key("blobkey.png").is_ok());
        assert!(validate_blob_key("dir1/dir2/blobkey.png").is_ok());
        assert!(validate_blob_key("dir1/.hidden/blobkey.png").is_ok());
        assert!(validate_blob_key("dir1/dir2/blob-key_1.png").is_ok());

        // Assert
    }

    #[test]
    fn should_reject_blob_key_too_long() {
        let long_key = "a".repeat(1025);
        assert!(validate_blob_key(&long_key).is_err());
    }

    #[test]
    fn should_reject_blob_key_with_empty_segment() {
        assert!(validate_blob_key("dir1//blobkey.png").is_err());
        assert!(validate_blob_key("/blobkey.png").is_err());
        assert!(validate_blob_key("blobkey.png/").is_err());
    }

    #[test]
    fn should_reject_blob_key_with_dot_segments() {
        assert!(validate_blob_key("./blobkey.png").is_err());
        assert!(validate_blob_key("dir1/../blobkey.png").is_err());
    }

    #[test]
    fn should_reject_blob_key_with_non_url_friendly_characters() {
        assert!(validate_blob_key("🎉 emoji.txt").is_err());
        assert!(validate_blob_key("folder/blob key.txt").is_err());
    }

    // Part number validation tests
    #[test]
    fn should_accept_valid_part_number() {
        assert!(validate_part_number(1).is_ok());
        assert!(validate_part_number(5000).is_ok());
        assert!(validate_part_number(10000).is_ok());
    }

    #[test]
    fn should_reject_part_number_zero() {
        assert!(validate_part_number(0).is_err());
    }

    #[test]
    fn should_reject_part_number_too_large() {
        assert!(validate_part_number(10001).is_err());
    }

    // Content-Length validation tests
    #[test]
    fn should_accept_valid_content_length() {
        assert!(validate_content_length(1024).is_ok());
        assert!(validate_content_length(1024 * 1024).is_ok());
        assert!(validate_content_length(1024 * 1024 * 1024).is_ok());
    }

    #[test]
    fn should_reject_content_length_too_large() {
        let huge_size = 10 * 1024 * 1024 * 1024u64;
        assert!(validate_content_length(huge_size).is_err());
    }

    // Upload ID validation tests
    #[test]
    fn should_accept_valid_upload_id() {
        assert!(validate_upload_id("abc123").is_ok());
        assert!(validate_upload_id("upload-id-12345").is_ok());
    }

    #[test]
    fn should_reject_empty_upload_id() {
        assert!(validate_upload_id("").is_err());
    }

    // Part sequence validation tests
    #[test]
    fn should_accept_valid_part_sequence() {
        assert!(validate_part_sequence(&[1, 2, 3, 4, 5]).is_ok());
        assert!(validate_part_sequence(&[1]).is_ok());
    }

    #[test]
    fn should_reject_part_sequence_not_starting_at_one() {
        assert!(validate_part_sequence(&[2, 3, 4]).is_err());
    }

    #[test]
    fn should_reject_part_sequence_with_gaps() {
        assert!(validate_part_sequence(&[1, 2, 4, 5]).is_err());
    }

    #[test]
    fn should_reject_part_sequence_out_of_order() {
        assert!(validate_part_sequence(&[1, 3, 2]).is_err());
    }

    #[test]
    fn should_reject_part_sequence_with_invalid_part_number() {
        assert!(validate_part_sequence(&[0, 1, 2]).is_err());
        assert!(validate_part_sequence(&[1, 2, 10001]).is_err());
    }
}
