use crate::models::policy::{Acl, CannedAcl, Grant, Grantee, Permission};
use crate::server::RequestExt as Request;
use crate::utils::xml as xml_utils;

const OWNER_ID: &str = "peas-emulator";

pub(super) fn acl_from_headers(req: &Request) -> Result<Acl, String> {
    let canned_acl_str = req.header("x-amz-acl").unwrap_or("private");
    let canned_acl: CannedAcl =
        serde_json::from_value(serde_json::json!(canned_acl_str)).unwrap_or_default();

    let mut grants = Vec::new();
    let mut saw_explicit_grant = false;

    for (header_name, permission) in [
        ("x-amz-grant-read", Permission::Read),
        ("x-amz-grant-write", Permission::Write),
        ("x-amz-grant-read-acp", Permission::ReadAcp),
        ("x-amz-grant-write-acp", Permission::WriteAcp),
        ("x-amz-grant-full-control", Permission::FullControl),
    ] {
        let Some(value) = req.header(header_name) else {
            continue;
        };
        let parsed_grantees = parse_grantee_header_value(value)?;
        if !saw_explicit_grant {
            grants.push(owner_full_control_grant());
            saw_explicit_grant = true;
        }
        for grantee in parsed_grantees {
            grants.push(Grant {
                grantee,
                permission: permission.clone(),
            });
        }
    }

    Ok(normalize_acl(Acl {
        canned: canned_acl,
        grants,
    }))
}

pub(super) fn acl_from_xml_body(body: &[u8]) -> Result<Acl, String> {
    let body =
        String::from_utf8(body.to_vec()).map_err(|err| format!("Invalid UTF-8 body: {}", err))?;
    let acl = xml_utils::parse_acl_xml(&body)?;
    Ok(normalize_acl(acl))
}

fn owner_full_control_grant() -> Grant {
    Grant {
        grantee: Grantee::CanonicalUser {
            id: OWNER_ID.to_string(),
            display_name: None,
        },
        permission: Permission::FullControl,
    }
}

fn normalize_acl(mut acl: Acl) -> Acl {
    if !acl.grants.is_empty()
        && !acl.grants.iter().any(|grant| {
            matches!(
                (&grant.grantee, &grant.permission),
                (Grantee::CanonicalUser { id, .. }, Permission::FullControl) if id == OWNER_ID
            )
        })
    {
        acl.grants.insert(0, owner_full_control_grant());
    }
    acl
}

fn parse_grantee_header_value(value: &str) -> Result<Vec<Grantee>, String> {
    let mut grantees = Vec::new();

    for raw_part in value.split(',') {
        let part = raw_part.trim();
        if part.is_empty() {
            continue;
        }

        if let Some(id) = parse_quoted_value(part, "id") {
            grantees.push(Grantee::CanonicalUser {
                id,
                display_name: None,
            });
            continue;
        }

        if let Some(uri) = parse_quoted_value(part, "uri") {
            grantees.push(Grantee::Group { uri });
            continue;
        }

        return Err(format!("Unsupported ACL grantee expression: {}", part));
    }

    if grantees.is_empty() {
        return Err("ACL grant header did not include any grantees".to_string());
    }

    Ok(grantees)
}

fn parse_quoted_value(input: &str, key: &str) -> Option<String> {
    let prefix = format!("{}=\"", key);
    let value = input.strip_prefix(&prefix)?;
    Some(value.strip_suffix('"')?.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::body::Body;
    use bytes::Bytes;
    use hyper::Request as HyperRequest;

    #[tokio::test(flavor = "multi_thread")]
    async fn should_reject_malformed_acl_grant_header_values() {
        let request = Request::from_hyper(
            HyperRequest::builder()
                .method("PUT")
                .uri("http://localhost/bucket?acl")
                .header("x-amz-grant-read", "emailAddress=\"test@example.com\"")
                .body(Body::from(Bytes::new()))
                .expect("request should build"),
        )
        .await
        .expect("request should parse");

        let error = acl_from_headers(&request).expect_err("header should be rejected");
        assert!(error.contains("Unsupported ACL grantee expression"));
    }

    #[test]
    fn should_add_owner_full_control_when_acl_xml_body_omits_it() {
        // Arrange
        let xml = br#"<?xml version="1.0" encoding="UTF-8"?>
<AccessControlPolicy xmlns="http://s3.amazonaws.com/doc/2006-03-01/">
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
        let acl = acl_from_xml_body(xml).expect("acl xml should parse");

        // Assert
        assert!(matches!(acl.grants[0].permission, Permission::FullControl));
        assert!(matches!(acl.grants[1].permission, Permission::Read));
    }
}
