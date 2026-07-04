/// XML response builders for S3-compliant responses
use crate::models::{Acl, Bucket, CannedAcl, MultipartUpload, Object, Owner, Part};
use std::collections::HashMap;
use std::fmt::Write as _;

mod parse;

pub use self::parse::{
    parse_acl_xml, parse_lifecycle_xml, parse_tagging_xml, parse_versioning_xml,
};

/// Wrap content in XML declaration
pub fn xml_declaration() -> String {
    r#"<?xml version="1.0" encoding="UTF-8"?>"#.to_string()
}

/// ListBuckets response
pub fn list_buckets_xml(buckets: &[Bucket]) -> String {
    let mut xml = format!(
        r#"{}
<ListAllMyBucketsResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/">
    <Owner>
        <ID>sqrzl-emulator</ID>
        <DisplayName>Sqrzl Emulator</DisplayName>
    </Owner>
    <Buckets>"#,
        xml_declaration()
    );

    for bucket in buckets {
        let created = bucket.created_at.to_rfc3339();
        xml.push_str(&format!(
            r#"
        <Bucket>
            <Name>{}</Name>
            <CreationDate>{}</CreationDate>
        </Bucket>"#,
            escape_xml(&bucket.name),
            created
        ));
    }

    xml.push_str(
        r#"
    </Buckets>
</ListAllMyBucketsResult>"#,
    );

    xml
}

/// Build Tagging XML response from key/value pairs
pub fn tagging_xml(tags: &HashMap<String, String>) -> String {
    let mut xml = format!(
        "{}\n<Tagging xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\">\n  <TagSet>",
        xml_declaration()
    );

    let mut entries: Vec<_> = tags.iter().collect();
    entries.sort_unstable_by_key(|(left_key, _)| *left_key);

    for (k, v) in entries {
        xml.push_str(&format!(
            "\n    <Tag><Key>{}</Key><Value>{}</Value></Tag>",
            escape_xml(k),
            escape_xml(v)
        ));
    }

    xml.push_str("\n  </TagSet>\n</Tagging>");
    xml
}

/// ListBucketResult response (list objects)
#[allow(clippy::too_many_arguments)]
pub fn list_objects_xml(
    objects: &[Object],
    common_prefixes: &[String],
    bucket: &str,
    prefix: &str,
    delimiter: Option<&str>,
    marker: Option<&str>,
    max_keys: usize,
    truncated: bool,
    next_marker: Option<&str>,
) -> String {
    let mut xml = format!(
        r#"{}
<ListBucketResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/">
  <Name>{}</Name>
  <Prefix>{}</Prefix>"#,
        xml_declaration(),
        escape_xml(bucket),
        escape_xml(prefix)
    );

    if let Some(delim) = delimiter {
        xml.push_str(&format!("\n  <Delimiter>{}</Delimiter>", escape_xml(delim)));
    }

    if let Some(m) = marker {
        xml.push_str(&format!("\n  <Marker>{}</Marker>", escape_xml(m)));
    }

    xml.push_str(&format!("\n  <MaxKeys>{}</MaxKeys>", max_keys));
    xml.push_str(&format!(
        "\n  <IsTruncated>{}</IsTruncated>",
        if truncated { "true" } else { "false" }
    ));

    for obj in objects {
        let modified = obj.last_modified.to_rfc3339();
        xml.push_str(&format!(
            r#"
  <Contents>
    <Key>{}</Key>
    <LastModified>{}</LastModified>
    <ETag>"{}"</ETag>
    <Size>{}</Size>
    <StorageClass>STANDARD</StorageClass>
  </Contents>"#,
            escape_xml(&obj.key),
            modified,
            escape_xml(&obj.etag),
            obj.size
        ));
    }

    for common_prefix in common_prefixes {
        xml.push_str(&format!(
            r#"
  <CommonPrefixes>
    <Prefix>{}</Prefix>
  </CommonPrefixes>"#,
            escape_xml(common_prefix)
        ));
    }

    if truncated {
        if let Some(nm) = next_marker {
            xml.push_str(&format!("\n  <NextMarker>{}</NextMarker>", escape_xml(nm)));
        }
    }

    xml.push_str("\n</ListBucketResult>");

    xml
}

#[derive(Debug, Clone)]
#[allow(clippy::large_enum_variant)]
pub enum ListObjectsV2Entry {
    Object(Object),
    CommonPrefix(String),
}

impl ListObjectsV2Entry {
    pub fn token(&self) -> &str {
        match self {
            ListObjectsV2Entry::Object(obj) => &obj.key,
            ListObjectsV2Entry::CommonPrefix(prefix) => prefix,
        }
    }
}

fn render_v2_value(value: &str, encoding_type: Option<&str>) -> String {
    let value = if matches!(encoding_type, Some(encoding) if encoding.eq_ignore_ascii_case("url")) {
        urlencoding::encode(value).into_owned()
    } else {
        value.to_string()
    };

    escape_xml(&value)
}

/// ListBucketResult response (ListObjectsV2)
#[allow(clippy::too_many_arguments)]
pub fn list_objects_v2_xml(
    entries: &[ListObjectsV2Entry],
    bucket: &str,
    prefix: &str,
    delimiter: Option<&str>,
    max_keys: usize,
    key_count: usize,
    truncated: bool,
    continuation_token: Option<&str>,
    next_continuation_token: Option<&str>,
    start_after: Option<&str>,
    encoding_type: Option<&str>,
    fetch_owner: bool,
) -> String {
    let mut xml = format!(
        r#"{}
<ListBucketResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/">"#,
        xml_declaration()
    );

    xml.push_str(&format!("\n  <Name>{}</Name>", escape_xml(bucket)));
    xml.push_str(&format!(
        "\n  <Prefix>{}</Prefix>",
        render_v2_value(prefix, encoding_type)
    ));

    if let Some(delim) = delimiter {
        xml.push_str(&format!(
            "\n  <Delimiter>{}</Delimiter>",
            render_v2_value(delim, encoding_type)
        ));
    }

    if let Some(token) = continuation_token {
        xml.push_str(&format!(
            "\n  <ContinuationToken>{}</ContinuationToken>",
            render_v2_value(token, encoding_type)
        ));
    }

    if let Some(start_after_value) = start_after {
        xml.push_str(&format!(
            "\n  <StartAfter>{}</StartAfter>",
            render_v2_value(start_after_value, encoding_type)
        ));
    }

    xml.push_str(&format!("\n  <MaxKeys>{}</MaxKeys>", max_keys));
    xml.push_str(&format!("\n  <KeyCount>{}</KeyCount>", key_count));
    xml.push_str(&format!(
        "\n  <IsTruncated>{}</IsTruncated>",
        if truncated { "true" } else { "false" }
    ));

    if matches!(encoding_type, Some(encoding) if encoding.eq_ignore_ascii_case("url")) {
        xml.push_str("\n  <EncodingType>url</EncodingType>");
    }

    for entry in entries {
        match entry {
            ListObjectsV2Entry::Object(obj) => {
                let modified = obj.last_modified.to_rfc3339();
                xml.push_str(&format!(
                    r#"
  <Contents>
    <Key>{}</Key>
    <LastModified>{}</LastModified>
    <ETag>\"{}\"</ETag>
    <Size>{}</Size>
    {}<StorageClass>{}</StorageClass>
  </Contents>"#,
                    render_v2_value(&obj.key, encoding_type),
                    modified,
                    escape_xml(&obj.etag),
                    obj.size,
                    if fetch_owner {
                        "<Owner><ID>sqrzl-emulator</ID><DisplayName>Sqrzl Emulator</DisplayName></Owner>\n    "
                    } else {
                        ""
                    },
                    escape_xml(&obj.storage_class)
                ));
            }
            ListObjectsV2Entry::CommonPrefix(prefix_value) => {
                xml.push_str(&format!(
                    r#"
  <CommonPrefixes>
    <Prefix>{}</Prefix>
  </CommonPrefixes>"#,
                    render_v2_value(prefix_value, encoding_type)
                ));
            }
        }
    }

    if let Some(next_token) = next_continuation_token {
        xml.push_str(&format!(
            "\n  <NextContinuationToken>{}</NextContinuationToken>",
            render_v2_value(next_token, encoding_type)
        ));
    }

    xml.push_str("\n</ListBucketResult>");
    xml
}

/// Error response
pub fn error_xml(code: &str, message: &str, request_id: &str) -> String {
    format!(
        r#"{}
<Error>
  <Code>{}</Code>
  <Message>{}</Message>
  <RequestId>{}</RequestId>
</Error>"#,
        xml_declaration(),
        escape_xml(code),
        escape_xml(message),
        escape_xml(request_id)
    )
}

/// Versioning configuration response
pub fn versioning_status_xml(status: Option<&str>) -> String {
    let status_str = status.unwrap_or("Suspended");
    format!(
        r#"{}
<VersioningConfiguration xmlns="http://s3.amazonaws.com/doc/2006-03-01/">
  <Status>{}</Status>
</VersioningConfiguration>"#,
        xml_declaration(),
        escape_xml(status_str)
    )
}

/// List object versions response
#[allow(clippy::too_many_arguments)]
pub fn list_versions_xml(
    bucket: &str,
    versions: &[crate::models::Object],
    prefix: &str,
    key_marker: Option<&str>,
    version_id_marker: Option<&str>,
    max_keys: usize,
    truncated: bool,
    next_key_marker: Option<&str>,
    next_version_id_marker: Option<&str>,
) -> String {
    let mut seen_keys = std::collections::HashSet::with_capacity(versions.len());
    let mut versions_xml = String::with_capacity(versions.len() * 256);
    for obj in versions {
        let version_id = obj.version_id.as_deref().unwrap_or("null");
        let last_modified = obj.last_modified.format("%Y-%m-%dT%H:%M:%S%.3fZ");
        let is_latest = seen_keys.insert(obj.key.as_str());
        versions_xml.push_str(&format!(
            r#"
  <Version>
    <Key>{}</Key>
    <VersionId>{}</VersionId>
    <IsLatest>{}</IsLatest>
    <LastModified>{}</LastModified>
    <ETag>{}</ETag>
    <Size>{}</Size>
    <Owner>
      <ID>sqrzl-emulator</ID>
      <DisplayName>Sqrzl Emulator</DisplayName>
    </Owner>
    <StorageClass>{}</StorageClass>
  </Version>"#,
            escape_xml(&obj.key),
            escape_xml(version_id),
            if is_latest { "true" } else { "false" },
            last_modified,
            escape_xml(&obj.etag),
            obj.size,
            escape_xml(&obj.storage_class)
        ));
    }

    let mut result = format!(
        r#"{}
<ListVersionsResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/">
  <Name>{}</Name>
  <Prefix>{}</Prefix>
  <KeyMarker>{}</KeyMarker>
  <VersionIdMarker>{}</VersionIdMarker>
  <MaxKeys>{}</MaxKeys>
  <IsTruncated>{}</IsTruncated>{}"#,
        xml_declaration(),
        escape_xml(bucket),
        escape_xml(prefix),
        escape_xml(key_marker.unwrap_or("")),
        escape_xml(version_id_marker.unwrap_or("")),
        max_keys,
        if truncated { "true" } else { "false" },
        versions_xml
    );

    if truncated {
        if let Some(nkm) = next_key_marker {
            write!(
                &mut result,
                "\n  <NextKeyMarker>{}</NextKeyMarker>",
                escape_xml(nkm)
            )
            .unwrap();
        }
        if let Some(nvm) = next_version_id_marker {
            write!(
                &mut result,
                "\n  <NextVersionIdMarker>{}</NextVersionIdMarker>",
                escape_xml(nvm)
            )
            .unwrap();
        }
    }

    result.push_str("</ListVersionsResult>");
    result
}

/// Location constraint response
pub fn location_xml(region: &str) -> String {
    if region == "us-east-1" {
        // AWS returns empty LocationConstraint for us-east-1
        format!(
            r#"{}
<LocationConstraint xmlns="http://s3.amazonaws.com/doc/2006-03-01/">
</LocationConstraint>"#,
            xml_declaration()
        )
    } else {
        format!(
            r#"{}
<LocationConstraint xmlns="http://s3.amazonaws.com/doc/2006-03-01/">
  {}
</LocationConstraint>"#,
            xml_declaration(),
            escape_xml(region)
        )
    }
}

/// Initiate multipart upload response
pub fn initiate_multipart_xml(bucket: &str, key: &str, upload_id: &str) -> String {
    format!(
        r#"{}
<InitiateMultipartUploadResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/">
  <Bucket>{}</Bucket>
  <Key>{}</Key>
  <UploadId>{}</UploadId>
</InitiateMultipartUploadResult>"#,
        xml_declaration(),
        escape_xml(bucket),
        escape_xml(key),
        escape_xml(upload_id)
    )
}

/// List multipart uploads response
pub fn list_multipart_uploads_xml(uploads: &[MultipartUpload], bucket: &str) -> String {
    let mut xml = format!(
        r#"{}
<ListMultipartUploadsResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/">
  <Bucket>{}</Bucket>
  <Uploads>"#,
        xml_declaration(),
        escape_xml(bucket)
    );

    for upload in uploads {
        let initiated = upload.initiated.to_rfc3339();
        xml.push_str(&format!(
            r#"
    <Upload>
      <Key>{}</Key>
      <UploadId>{}</UploadId>
      <Initiated>{}</Initiated>
      <StorageClass>STANDARD</StorageClass>
    </Upload>"#,
            escape_xml(&upload.key),
            escape_xml(&upload.upload_id),
            initiated
        ));
    }

    xml.push_str(
        r#"
  </Uploads>
  <IsTruncated>false</IsTruncated>
</ListMultipartUploadsResult>"#,
    );

    xml
}

/// ACL response
pub fn acl_xml(owner: &Owner, acl: &Acl) -> String {
    let mut grants = String::new();

    if !acl.grants.is_empty() {
        for grant in &acl.grants {
            let grantee_xml = match &grant.grantee {
                crate::models::policy::Grantee::CanonicalUser { id, display_name } => format!(
                    r#"<Grantee xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance" xsi:type="CanonicalUser"><ID>{}</ID>{}</Grantee>"#,
                    escape_xml(id),
                    display_name
                        .as_ref()
                        .map(|name| format!("<DisplayName>{}</DisplayName>", escape_xml(name)))
                        .unwrap_or_default(),
                ),
                crate::models::policy::Grantee::Group { uri } => format!(
                    r#"<Grantee xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance" xsi:type="Group"><URI>{}</URI></Grantee>"#,
                    escape_xml(uri),
                ),
            };
            let permission = match grant.permission {
                crate::models::policy::Permission::Read => "READ",
                crate::models::policy::Permission::Write => "WRITE",
                crate::models::policy::Permission::ReadAcp => "READ_ACP",
                crate::models::policy::Permission::WriteAcp => "WRITE_ACP",
                crate::models::policy::Permission::FullControl => "FULL_CONTROL",
            };
            grants.push_str(&format!(
                r#"
        <Grant>
            {}
            <Permission>{}</Permission>
        </Grant>"#,
                grantee_xml, permission
            ));
        }
    } else {
        grants.push_str(&format!(
            r#"
        <Grant>
            <Grantee xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance" xsi:type="CanonicalUser">
                <ID>{}</ID>
                <DisplayName>{}</DisplayName>
            </Grantee>
            <Permission>FULL_CONTROL</Permission>
        </Grant>"#,
            escape_xml(&owner.id),
            escape_xml(&owner.display_name)
        ));

        match acl.canned {
            CannedAcl::Private => {}
            CannedAcl::PublicRead => {
                grants.push_str(
                    r#"
        <Grant>
            <Grantee xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance" xsi:type="Group">
                <URI>http://acs.amazonaws.com/groups/global/AllUsers</URI>
            </Grantee>
            <Permission>READ</Permission>
        </Grant>"#,
                );
            }
            CannedAcl::PublicReadWrite => {
                grants.push_str(
                    r#"
        <Grant>
            <Grantee xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance" xsi:type="Group">
                <URI>http://acs.amazonaws.com/groups/global/AllUsers</URI>
            </Grantee>
            <Permission>READ</Permission>
        </Grant>
        <Grant>
            <Grantee xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance" xsi:type="Group">
                <URI>http://acs.amazonaws.com/groups/global/AllUsers</URI>
            </Grantee>
            <Permission>WRITE</Permission>
        </Grant>"#,
                );
            }
            CannedAcl::AuthenticatedRead => {
                grants.push_str(
                    r#"
        <Grant>
            <Grantee xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance" xsi:type="Group">
                <URI>http://acs.amazonaws.com/groups/global/AuthenticatedUsers</URI>
            </Grantee>
            <Permission>READ</Permission>
        </Grant>"#,
                );
            }
            CannedAcl::BucketOwnerRead => {}
            CannedAcl::BucketOwnerFullControl => {}
        }
    }

    format!(
        r#"{}
<AccessControlPolicy xmlns="http://s3.amazonaws.com/doc/2006-03-01/">
    <Owner>
        <ID>{}</ID>
        <DisplayName>{}</DisplayName>
    </Owner>
    <AccessControlList>{}
    </AccessControlList>
</AccessControlPolicy>"#,
        xml_declaration(),
        escape_xml(&owner.id),
        escape_xml(&owner.display_name),
        grants
    )
}

/// List parts response
pub fn list_parts_xml(bucket: &str, key: &str, upload_id: &str, parts: &[Part]) -> String {
    let mut xml = format!(
        r#"{}
<ListPartsResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/">
  <Bucket>{}</Bucket>
  <Key>{}</Key>
  <UploadId>{}</UploadId>
  <Parts>"#,
        xml_declaration(),
        escape_xml(bucket),
        escape_xml(key),
        escape_xml(upload_id)
    );

    for part in parts {
        let modified = part.last_modified.to_rfc3339();
        xml.push_str(&format!(
            r#"
    <Part>
      <PartNumber>{}</PartNumber>
      <LastModified>{}</LastModified>
      <ETag>"{}"</ETag>
      <Size>{}</Size>
    </Part>"#,
            part.part_number,
            modified,
            escape_xml(&part.etag),
            part.size
        ));
    }

    xml.push_str(
        r#"
  </Parts>
  <IsTruncated>false</IsTruncated>
</ListPartsResult>"#,
    );

    xml
}

/// Complete multipart upload response
pub fn complete_multipart_upload_xml(bucket: &str, key: &str, etag: &str) -> String {
    format!(
        r#"{}
<CompleteMultipartUploadResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/">
  <Location>http://s3.amazonaws.com/{}/{}</Location>
  <Bucket>{}</Bucket>
  <Key>{}</Key>
  <ETag>"{}"</ETag>
</CompleteMultipartUploadResult>"#,
        xml_declaration(),
        escape_xml(bucket),
        escape_xml(key),
        escape_xml(bucket),
        escape_xml(key),
        escape_xml(etag)
    )
}

/// Generate lifecycle configuration XML response
pub fn lifecycle_xml(config: &crate::models::lifecycle::LifecycleConfiguration) -> String {
    use crate::models::lifecycle::*;

    let mut xml = String::from("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    xml.push_str("<LifecycleConfiguration>\n");

    for rule in &config.rules {
        xml.push_str("  <Rule>\n");

        if let Some(id) = &rule.id {
            xml.push_str(&format!("    <ID>{}</ID>\n", escape_xml(id)));
        }

        xml.push_str(&format!(
            "    <Status>{}</Status>\n",
            if rule.status == Status::Enabled {
                "Enabled"
            } else {
                "Disabled"
            }
        ));

        if let Some(filter) = &rule.filter {
            xml.push_str("    <Filter>\n");
            if let Some(prefix) = &filter.prefix {
                xml.push_str(&format!("      <Prefix>{}</Prefix>\n", escape_xml(prefix)));
            }
            for tag in &filter.tags {
                xml.push_str("      <Tag>\n");
                xml.push_str(&format!("        <Key>{}</Key>\n", escape_xml(&tag.key)));
                xml.push_str(&format!(
                    "        <Value>{}</Value>\n",
                    escape_xml(&tag.value)
                ));
                xml.push_str("      </Tag>\n");
            }
            xml.push_str("    </Filter>\n");
        }

        if let Some(expiration) = &rule.expiration {
            xml.push_str("    <Expiration>\n");
            if let Some(days) = expiration.days {
                xml.push_str(&format!("      <Days>{}</Days>\n", days));
            }
            if let Some(date) = &expiration.date {
                xml.push_str(&format!("      <Date>{}</Date>\n", escape_xml(date)));
            }
            if let Some(marker) = expiration.expired_object_delete_marker {
                xml.push_str(&format!(
                    "      <ExpiredObjectDeleteMarker>{}</ExpiredObjectDeleteMarker>\n",
                    marker
                ));
            }
            xml.push_str("    </Expiration>\n");
        }

        if let Some(noncurrent_expiration) = &rule.noncurrent_version_expiration {
            xml.push_str("    <NoncurrentVersionExpiration>\n");
            xml.push_str(&format!(
                "      <NoncurrentDays>{}</NoncurrentDays>\n",
                noncurrent_expiration.noncurrent_days
            ));
            xml.push_str("    </NoncurrentVersionExpiration>\n");
        }

        for transition in &rule.transitions {
            xml.push_str("    <Transition>\n");
            if let Some(days) = transition.days {
                xml.push_str(&format!("      <Days>{}</Days>\n", days));
            }
            if let Some(date) = &transition.date {
                xml.push_str(&format!("      <Date>{}</Date>\n", escape_xml(date)));
            }
            let storage_class = match transition.storage_class {
                StorageClass::Standard => "STANDARD",
                StorageClass::Glacier => "GLACIER",
                StorageClass::DeepArchive => "DEEP_ARCHIVE",
            };
            xml.push_str(&format!(
                "      <StorageClass>{}</StorageClass>\n",
                storage_class
            ));
            xml.push_str("    </Transition>\n");
        }

        xml.push_str("  </Rule>\n");
    }

    xml.push_str("</LifecycleConfiguration>");
    xml
}

pub(crate) fn push_escaped_xml(out: &mut String, s: &str) {
    let mut start = 0;
    let bytes = s.as_bytes();

    for (index, byte) in bytes.iter().enumerate() {
        let replacement = match byte {
            b'&' => Some("&amp;"),
            b'<' => Some("&lt;"),
            b'>' => Some("&gt;"),
            b'"' => Some("&quot;"),
            b'\'' => Some("&apos;"),
            _ => None,
        };

        if let Some(replacement) = replacement {
            if start < index {
                out.push_str(&s[start..index]);
            }
            out.push_str(replacement);
            start = index + 1;
        }
    }

    if start < s.len() {
        out.push_str(&s[start..]);
    }
}

/// Helper to escape XML special characters
fn escape_xml(s: &str) -> String {
    let mut escaped = String::with_capacity(s.len());
    push_escaped_xml(&mut escaped, s);
    escaped
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::policy::{Grantee, Permission};

    fn fixed_bucket(name: &str, created_at: &str) -> Bucket {
        Bucket {
            name: name.to_string(),
            created_at: chrono::DateTime::parse_from_rfc3339(created_at)
                .expect("timestamp should parse")
                .with_timezone(&chrono::Utc),
            versioning_enabled: false,
            policy: None,
            lifecycle_rules: Vec::new(),
            metadata: HashMap::new(),
            acl: None,
        }
    }

    #[test]
    fn should_escape_ampersand_given_string_with_ampersand_when_escape_xml_called() {
        // Arrange
        let input = "test & data";
        let expected = "test &amp; data";

        // Act
        let result = escape_xml(input);

        // Assert
        assert_eq!(result, expected);
    }

    #[test]
    fn should_escape_angle_brackets_given_string_with_brackets_when_escape_xml_called() {
        // Arrange
        let input = "<tag>";
        let expected = "&lt;tag&gt;";

        // Act
        let result = escape_xml(input);

        // Assert
        assert_eq!(result, expected);
    }

    #[test]
    fn should_escape_quotes_given_string_with_quotes_when_escape_xml_called() {
        // Arrange
        let input = r#"quote"test"#;
        let expected = "quote&quot;test";

        // Act
        let result = escape_xml(input);

        // Assert
        assert_eq!(result, expected);
    }

    #[test]
    fn should_include_xml_declaration_given_any_input_when_list_buckets_xml_called() {
        // Arrange
        // Act
        let xml = list_buckets_xml(&[]);

        // Assert
        let expected = format!(
            "{}\n<ListAllMyBucketsResult xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\">\n    <Owner>\n        <ID>sqrzl-emulator</ID>\n        <DisplayName>Sqrzl Emulator</DisplayName>\n    </Owner>\n    <Buckets>\n    </Buckets>\n</ListAllMyBucketsResult>",
            xml_declaration()
        );

        assert_eq!(xml, expected);
    }

    #[test]
    fn should_include_bucket_name_given_bucket_list_when_list_buckets_xml_called() {
        // Arrange
        let buckets = vec![
            fixed_bucket("alpha", "2024-01-02T03:04:05Z"),
            fixed_bucket("beta", "2024-02-03T04:05:06Z"),
        ];

        // Act
        let xml = list_buckets_xml(&buckets);
        // Assert
        let expected = format!(
            "{}\n<ListAllMyBucketsResult xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\">\n    <Owner>\n        <ID>sqrzl-emulator</ID>\n        <DisplayName>Sqrzl Emulator</DisplayName>\n    </Owner>\n    <Buckets>\n        <Bucket>\n            <Name>alpha</Name>\n            <CreationDate>2024-01-02T03:04:05+00:00</CreationDate>\n        </Bucket>\n        <Bucket>\n            <Name>beta</Name>\n            <CreationDate>2024-02-03T04:05:06+00:00</CreationDate>\n        </Bucket>\n    </Buckets>\n</ListAllMyBucketsResult>",
            xml_declaration()
        );

        assert_eq!(xml, expected);
    }

    #[test]
    fn should_include_error_code_given_error_parameters_when_error_xml_called() {
        // Arrange
        // Act
        let xml = error_xml("NoSuchBucket", "Bucket not found", "req-12345");
        // Assert
        let expected = format!(
            "{}\n<Error>\n  <Code>NoSuchBucket</Code>\n  <Message>Bucket not found</Message>\n  <RequestId>req-12345</RequestId>\n</Error>",
            xml_declaration()
        );

        assert_eq!(xml, expected);
    }

    #[test]
    fn should_include_error_message_given_error_parameters_when_error_xml_called() {
        // Arrange
        // Act
        let xml = error_xml("NoSuchBucket", "Bucket not found", "req-12345");
        // Assert
        let expected = format!(
            "{}\n<Error>\n  <Code>NoSuchBucket</Code>\n  <Message>Bucket not found</Message>\n  <RequestId>req-12345</RequestId>\n</Error>",
            xml_declaration()
        );

        assert_eq!(xml, expected);
    }

    #[test]
    fn should_include_request_id_given_request_id_when_error_xml_called() {
        // Arrange
        // Act
        let xml = error_xml("NoSuchBucket", "Bucket not found", "req-12345");
        // Assert
        let expected = format!(
            "{}\n<Error>\n  <Code>NoSuchBucket</Code>\n  <Message>Bucket not found</Message>\n  <RequestId>req-12345</RequestId>\n</Error>",
            xml_declaration()
        );

        assert_eq!(xml, expected);
    }

    #[test]
    fn should_include_enabled_status_given_enabled_string_when_versioning_status_xml_called() {
        // Arrange
        // Act
        let xml = versioning_status_xml(Some("Enabled"));
        // Assert
        let expected = format!(
            "{}\n<VersioningConfiguration xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\">\n  <Status>Enabled</Status>\n</VersioningConfiguration>",
            xml_declaration()
        );

        assert_eq!(xml, expected);
    }

    #[test]
    fn should_default_to_suspended_status_when_no_status_is_provided() {
        // Arrange
        // Act
        let xml = versioning_status_xml(None);
        // Assert
        let expected = format!(
            "{}\n<VersioningConfiguration xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\">\n  <Status>Suspended</Status>\n</VersioningConfiguration>",
            xml_declaration()
        );

        assert_eq!(xml, expected);
    }

    #[test]
    fn should_include_empty_constraint_given_us_east_1_when_location_xml_called() {
        // Arrange
        // Act
        let xml = location_xml("us-east-1");
        // Assert
        let expected = format!(
            "{}\n<LocationConstraint xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\">\n</LocationConstraint>",
            xml_declaration()
        );

        assert_eq!(xml, expected);
    }

    #[test]
    fn should_include_region_given_non_us_east_1_when_location_xml_called() {
        // Arrange
        // Act
        let xml = location_xml("eu-central-1");
        // Assert
        let expected = format!(
            "{}\n<LocationConstraint xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\">\n  eu-central-1\n</LocationConstraint>",
            xml_declaration()
        );

        assert_eq!(xml, expected);
    }

    #[test]
    fn should_parse_tagging_xml_into_map() {
        // Arrange
        let body = r#"<?xml version="1.0" encoding="UTF-8"?>
<Tagging><TagSet><Tag><Key>env</Key><Value>dev</Value></Tag><Tag><Key>owner</Key><Value>alice</Value></Tag></TagSet></Tagging>"#;

        // Act
        let tags = parse_tagging_xml(body).expect("parse tagging xml");

        // Assert
        assert_eq!(tags.get("env"), Some(&"dev".to_string()));
        assert_eq!(tags.get("owner"), Some(&"alice".to_string()));
    }

    #[test]
    fn should_render_tagging_xml_with_entries() {
        // Arrange
        let mut tags = std::collections::HashMap::new();
        tags.insert("env".to_string(), "prod".to_string());

        // Act
        let xml = tagging_xml(&tags);
        // Assert
        let expected = format!(
            "{}\n<Tagging xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\">\n  <TagSet>\n    <Tag><Key>env</Key><Value>prod</Value></Tag>\n  </TagSet>\n</Tagging>",
            xml_declaration()
        );

        assert_eq!(xml, expected);
    }

    #[test]
    fn should_error_when_more_than_ten_tags() {
        // Arrange
        let mut body = String::from("<?xml version=\"1.0\" encoding=\"UTF-8\"?><Tagging><TagSet>");
        for i in 0..11 {
            body.push_str(&format!("<Tag><Key>k{i}</Key><Value>v{i}</Value></Tag>"));
        }
        body.push_str("</TagSet></Tagging>");

        // Act
        let result = parse_tagging_xml(&body);

        // Assert
        assert_eq!(result.unwrap_err(), "TooManyTags");
    }

    #[test]
    fn should_error_on_empty_tag_key() {
        // Arrange
        let body = "<?xml version=\"1.0\" encoding=\"UTF-8\"?><Tagging><TagSet><Tag><Key></Key><Value>v</Value></Tag></TagSet></Tagging>";
        // Act
        let result = parse_tagging_xml(body);

        // Assert
        assert_eq!(result.unwrap_err(), "InvalidTagKey");
    }

    #[test]
    fn should_round_trip_noncurrent_version_expiration_in_lifecycle_xml() {
        // Arrange
        let config = crate::models::lifecycle::LifecycleConfiguration {
            rules: vec![crate::models::lifecycle::Rule {
                id: Some("cleanup-noncurrent".to_string()),
                status: crate::models::lifecycle::Status::Enabled,
                filter: Some(crate::models::lifecycle::Filter {
                    prefix: Some("logs/".to_string()),
                    tags: vec![],
                }),
                expiration: None,
                noncurrent_version_expiration: Some(
                    crate::models::lifecycle::NoncurrentVersionExpiration { noncurrent_days: 7 },
                ),
                transitions: vec![],
            }],
        };

        // Act
        let xml = lifecycle_xml(&config);
        let parsed = parse_lifecycle_xml(&xml).expect("lifecycle xml should parse");

        // Assert
        assert!(xml.contains("<NoncurrentVersionExpiration>"));
        assert!(xml.contains("<NoncurrentDays>7</NoncurrentDays>"));
        assert_eq!(parsed.rules.len(), 1);
        assert_eq!(
            parsed.rules[0]
                .noncurrent_version_expiration
                .as_ref()
                .map(|expiration| expiration.noncurrent_days),
            Some(7)
        );
    }

    #[test]
    fn should_render_owner_grant_in_acl_xml() {
        // Arrange
        let owner = Owner {
            id: "owner-id".to_string(),
            display_name: "Owner".to_string(),
        };
        let acl = Acl {
            canned: CannedAcl::Private,
            grants: Vec::new(),
        };

        // Act
        let xml = acl_xml(&owner, &acl);

        // Assert
        assert!(xml.contains("<AccessControlPolicy"));
        assert!(xml.contains("<Permission>FULL_CONTROL</Permission>"));
        assert!(xml.contains("owner-id"));
        assert_eq!(xml.matches("<Grant>").count(), 1);
        assert_eq!(
            xml.matches("<Permission>FULL_CONTROL</Permission>").count(),
            1
        );
        assert!(!xml.contains("AllUsers"));
    }
    #[test]
    fn should_render_public_read_grant_in_acl_xml() {
        // Arrange
        let owner = Owner {
            id: "owner-id".to_string(),
            display_name: "Owner".to_string(),
        };
        let acl = Acl {
            canned: CannedAcl::PublicRead,
            grants: Vec::new(),
        };

        // Act
        let xml = acl_xml(&owner, &acl);

        // Assert
        assert!(xml.contains("AllUsers"));
        assert!(xml.contains("<Permission>READ</Permission>"));
        assert_eq!(xml.matches("<Grant>").count(), 2);
        assert_eq!(xml.matches("<Permission>READ</Permission>").count(), 1);
    }

    #[test]
    fn should_parse_acl_xml_with_canonical_user_grant() {
        // Arrange
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<AccessControlPolicy xmlns="http://s3.amazonaws.com/doc/2006-03-01/">
    <Owner>
        <ID>owner-id</ID>
        <DisplayName>Owner</DisplayName>
    </Owner>
    <AccessControlList>
        <Grant>
            <Grantee xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance" xsi:type="CanonicalUser">
                <ID>owner-id</ID>
            </Grantee>
            <Permission>FULL_CONTROL</Permission>
        </Grant>
    </AccessControlList>
</AccessControlPolicy>"#;

        // Act
        let acl = parse_acl_xml(xml).expect("acl xml should parse");

        // Assert
        assert_eq!(acl.grants.len(), 1);
        assert!(matches!(
            acl.grants[0].grantee,
            Grantee::CanonicalUser { .. }
        ));
        assert!(matches!(acl.grants[0].permission, Permission::FullControl));
    }

    #[test]
    fn should_parse_acl_xml_with_group_grant() {
        // Arrange
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<AccessControlPolicy xmlns="http://s3.amazonaws.com/doc/2006-03-01/">
    <Owner>
        <ID>owner-id</ID>
        <DisplayName>Owner</DisplayName>
    </Owner>
    <AccessControlList>
        <Grant>
            <Grantee xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance" xsi:type="Group">
                <URI>http://acs.amazonaws.com/groups/global/AllUsers</URI>
            </Grantee>
            <Permission>READ</Permission>
        </Grant>
    </AccessControlList>
</AccessControlPolicy>"#;

        // Act
        let acl = parse_acl_xml(xml).expect("acl xml should parse");

        // Assert
        assert_eq!(acl.grants.len(), 1);
        assert!(matches!(acl.grants[0].grantee, Grantee::Group { .. }));
        assert!(matches!(acl.grants[0].permission, Permission::Read));
    }
}
