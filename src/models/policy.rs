use serde::{Deserialize, Serialize};
use std::collections::HashMap;

pub use super::bucket::{
    Bucket, BucketPolicy, LifecycleExpiration, LifecycleRule, PolicyStatement,
};

// ============================================================================
// ACL (Access Control List) Models
// ============================================================================

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub enum CannedAcl {
    #[serde(rename = "private")]
    #[default]
    Private,
    #[serde(rename = "public-read")]
    PublicRead,
    #[serde(rename = "public-read-write")]
    PublicReadWrite,
    #[serde(rename = "authenticated-read")]
    AuthenticatedRead,
    #[serde(rename = "bucket-owner-read")]
    BucketOwnerRead,
    #[serde(rename = "bucket-owner-full-control")]
    BucketOwnerFullControl,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum Permission {
    #[serde(rename = "READ")]
    Read,
    #[serde(rename = "WRITE")]
    Write,
    #[serde(rename = "READ_ACP")]
    ReadAcp,
    #[serde(rename = "WRITE_ACP")]
    WriteAcp,
    #[serde(rename = "FULL_CONTROL")]
    FullControl,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "Type")]
pub enum Grantee {
    #[serde(rename = "CanonicalUser")]
    CanonicalUser {
        #[serde(rename = "ID")]
        id: String,
        #[serde(rename = "DisplayName", skip_serializing_if = "Option::is_none")]
        display_name: Option<String>,
    },
    #[serde(rename = "Group")]
    Group {
        #[serde(rename = "URI")]
        uri: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Grant {
    pub grantee: Grantee,
    pub permission: Permission,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct Acl {
    pub canned: CannedAcl,
    #[serde(default)]
    pub grants: Vec<Grant>,
}

impl Acl {
    /// Convert a canned ACL to explicit grants
    #[must_use]
    pub fn to_grants(&self, owner_id: &str) -> Vec<Grant> {
        if !self.grants.is_empty() {
            return self.grants.clone();
        }

        match self.canned {
            CannedAcl::Private | CannedAcl::BucketOwnerRead | CannedAcl::BucketOwnerFullControl => {
                vec![Grant {
                    grantee: Grantee::CanonicalUser {
                        id: owner_id.to_string(),
                        display_name: None,
                    },
                    permission: Permission::FullControl,
                }]
            }
            CannedAcl::PublicRead => vec![
                Grant {
                    grantee: Grantee::CanonicalUser {
                        id: owner_id.to_string(),
                        display_name: None,
                    },
                    permission: Permission::FullControl,
                },
                Grant {
                    grantee: Grantee::Group {
                        uri: "http://acs.amazonaws.com/groups/global/AllUsers".to_string(),
                    },
                    permission: Permission::Read,
                },
            ],
            CannedAcl::PublicReadWrite => vec![
                Grant {
                    grantee: Grantee::CanonicalUser {
                        id: owner_id.to_string(),
                        display_name: None,
                    },
                    permission: Permission::FullControl,
                },
                Grant {
                    grantee: Grantee::Group {
                        uri: "http://acs.amazonaws.com/groups/global/AllUsers".to_string(),
                    },
                    permission: Permission::Read,
                },
                Grant {
                    grantee: Grantee::Group {
                        uri: "http://acs.amazonaws.com/groups/global/AllUsers".to_string(),
                    },
                    permission: Permission::Write,
                },
            ],
            CannedAcl::AuthenticatedRead => vec![
                Grant {
                    grantee: Grantee::CanonicalUser {
                        id: owner_id.to_string(),
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
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct Owner {
    pub id: String,
    pub display_name: String,
}

// ============================================================================
// Authorization Context
// ============================================================================

#[derive(Debug, Clone)]
pub struct AuthContext {
    /// User/principal making the request
    pub principal: String,
    /// Whether the user is authenticated
    pub is_authenticated: bool,
    /// The action being performed (e.g., "s3:GetObject", "s3:PutObject")
    pub action: String,
    /// The resource being accessed (e.g., "`arn:aws:s3:::bucket/key`")
    pub resource: String,
    /// Bucket owner ID
    pub bucket_owner: Option<String>,
    /// Object owner ID
    pub object_owner: Option<String>,
    /// Lower-cased request headers available to condition evaluation
    pub request_headers: HashMap<String, String>,
    /// Lower-cased query parameters available to condition evaluation
    pub query_params: HashMap<String, String>,
    /// Existing object tags for object-scoped evaluations
    pub existing_object_tags: HashMap<String, String>,
}

impl AuthContext {
    #[must_use]
    pub fn anonymous(action: &str, resource: &str) -> Self {
        Self {
            principal: "*".to_string(),
            is_authenticated: false,
            action: action.to_string(),
            resource: resource.to_string(),
            bucket_owner: None,
            object_owner: None,
            request_headers: HashMap::new(),
            query_params: HashMap::new(),
            existing_object_tags: HashMap::new(),
        }
    }

    #[must_use]
    pub fn authenticated(principal: &str, action: &str, resource: &str) -> Self {
        Self {
            principal: principal.to_string(),
            is_authenticated: true,
            action: action.to_string(),
            resource: resource.to_string(),
            bucket_owner: None,
            object_owner: None,
            request_headers: HashMap::new(),
            query_params: HashMap::new(),
            existing_object_tags: HashMap::new(),
        }
    }
}

// ============================================================================
// Authorization Engine
// ============================================================================

pub struct Authorizer;

impl Authorizer {
    /// Check if an ACL permits the given action
    #[must_use]
    pub fn check_acl_permission(acl: &Acl, owner_id: &str, context: &AuthContext) -> bool {
        let grants = acl.to_grants(owner_id);

        for grant in &grants {
            if Self::grant_matches(grant, context) {
                return true;
            }
        }

        false
    }

    fn grant_matches(grant: &Grant, context: &AuthContext) -> bool {
        // Check if grantee matches
        let grantee_matches = match &grant.grantee {
            Grantee::CanonicalUser { id, .. } => {
                // Owner always matches
                context.principal == *id
            }
            Grantee::Group { uri } => {
                if uri == "http://acs.amazonaws.com/groups/global/AllUsers" {
                    true // Anyone
                } else if uri == "http://acs.amazonaws.com/groups/global/AuthenticatedUsers" {
                    context.is_authenticated
                } else {
                    false
                }
            }
        };

        if !grantee_matches {
            return false;
        }

        // Check if permission covers the action
        Self::permission_covers_action(&grant.permission, &context.action)
    }

    fn permission_covers_action(permission: &Permission, action: &str) -> bool {
        match permission {
            Permission::FullControl => true,
            Permission::Read => action.contains("GetObject") || action.contains("ListBucket"),
            Permission::Write => action.contains("PutObject") || action.contains("DeleteObject"),
            Permission::ReadAcp => {
                action.contains("GetObjectAcl") || action.contains("GetBucketAcl")
            }
            Permission::WriteAcp => {
                action.contains("PutObjectAcl") || action.contains("PutBucketAcl")
            }
        }
    }

    /// Evaluate a bucket policy document
    #[must_use]
    pub fn evaluate_policy(policy: &BucketPolicyDocument, context: &AuthContext) -> PolicyEffect {
        let mut has_allow = false;
        let mut has_deny = false;

        for statement in &policy.statement {
            if !Self::statement_applies(statement, context) {
                continue;
            }

            match statement.effect.as_str() {
                "Allow" => has_allow = true,
                "Deny" => has_deny = true,
                _ => {}
            }
        }

        // Explicit deny always wins
        if has_deny {
            PolicyEffect::Deny
        } else if has_allow {
            PolicyEffect::Allow
        } else {
            PolicyEffect::Neutral
        }
    }

    fn statement_applies(statement: &PolicyStatementDocument, context: &AuthContext) -> bool {
        // Check principal
        if !Self::principal_matches(&statement.principal, context) {
            return false;
        }

        // Check action
        if !Self::action_matches(&statement.action, &context.action) {
            return false;
        }

        // Check resource
        if !Self::resource_matches(&statement.resource, &context.resource) {
            return false;
        }

        if !Self::conditions_match(statement.condition.as_ref(), context) {
            return false;
        }

        true
    }

    fn conditions_match(condition: Option<&serde_json::Value>, context: &AuthContext) -> bool {
        let Some(condition) = condition else {
            return true;
        };

        let Some(condition_map) = condition.as_object() else {
            return false;
        };

        condition_map.iter().all(|(operator, operands)| {
            Self::condition_operator_matches(operator, operands, context)
        })
    }

    fn condition_operator_matches(
        operator: &str,
        operands: &serde_json::Value,
        context: &AuthContext,
    ) -> bool {
        let Some(operand_map) = operands.as_object() else {
            return false;
        };

        operand_map.iter().all(|(key, expected_values)| {
            Self::condition_key_matches(operator, key, expected_values, context)
        })
    }

    fn condition_key_matches(
        operator: &str,
        key: &str,
        expected_values: &serde_json::Value,
        context: &AuthContext,
    ) -> bool {
        let request_values = Self::values_for_condition_key(key, context);
        let expected_values = match Self::json_values_to_strings(expected_values) {
            Some(values) if !values.is_empty() => values,
            _ => return false,
        };

        match operator {
            "Null" => Self::null_condition_matches(&request_values, &expected_values),
            "Bool" => Self::bool_condition_matches(&request_values, &expected_values),
            "StringEquals" | "ArnEquals" => {
                Self::string_condition_matches(&request_values, &expected_values, false)
            }
            "StringNotEquals" | "ArnNotEquals" => {
                Self::string_condition_matches(&request_values, &expected_values, true)
            }
            "StringLike" | "ArnLike" => {
                Self::wildcard_condition_matches(&request_values, &expected_values, false)
            }
            "StringNotLike" | "ArnNotLike" => {
                Self::wildcard_condition_matches(&request_values, &expected_values, true)
            }
            "NumericEquals" => Self::numeric_condition_matches(
                &request_values,
                &expected_values,
                NumericCondition::Equals,
            ),
            "NumericNotEquals" => Self::numeric_condition_matches(
                &request_values,
                &expected_values,
                NumericCondition::NotEquals,
            ),
            "NumericLessThan" => Self::numeric_condition_matches(
                &request_values,
                &expected_values,
                NumericCondition::LessThan,
            ),
            "NumericLessThanEquals" => Self::numeric_condition_matches(
                &request_values,
                &expected_values,
                NumericCondition::LessThanEquals,
            ),
            "NumericGreaterThan" => Self::numeric_condition_matches(
                &request_values,
                &expected_values,
                NumericCondition::GreaterThan,
            ),
            "NumericGreaterThanEquals" => Self::numeric_condition_matches(
                &request_values,
                &expected_values,
                NumericCondition::GreaterThanEquals,
            ),
            _ => false,
        }
    }

    fn values_for_condition_key(key: &str, context: &AuthContext) -> Vec<String> {
        match key {
            "aws:PrincipalArn" | "aws:userid" => vec![context.principal.clone()],
            "aws:PrincipalType" => vec![if context.is_authenticated {
                "Authenticated".to_string()
            } else {
                "Anonymous".to_string()
            }],
            "aws:SecureTransport" => vec![Self::secure_transport_value(context).to_string()],
            "aws:SourceIp" => Self::source_ip_values(context),
            _ if key.starts_with("s3:ExistingObjectTag/") => {
                let tag_key = key.trim_start_matches("s3:ExistingObjectTag/");
                context
                    .existing_object_tags
                    .get(tag_key)
                    .cloned()
                    .into_iter()
                    .collect()
            }
            _ if key.starts_with("s3:RequestObjectTag/") => {
                let tag_key = key.trim_start_matches("s3:RequestObjectTag/");
                Self::request_object_tag_values(context, tag_key)
            }
            _ if key.starts_with("s3:") => {
                let suffix = key.trim_start_matches("s3:").to_lowercase();
                context
                    .query_params
                    .get(&suffix)
                    .or_else(|| context.request_headers.get(&suffix))
                    .cloned()
                    .into_iter()
                    .collect()
            }
            _ => {
                let lowered = key.to_lowercase();
                context
                    .request_headers
                    .get(&lowered)
                    .or_else(|| context.query_params.get(&lowered))
                    .cloned()
                    .into_iter()
                    .collect()
            }
        }
    }

    fn secure_transport_value(context: &AuthContext) -> bool {
        if let Some(value) = context.request_headers.get("x-forwarded-proto") {
            return value
                .split(',')
                .next()
                .is_some_and(|value| value.trim().eq_ignore_ascii_case("https"));
        }

        if let Some(value) = context.request_headers.get("x-forwarded-ssl") {
            return value.eq_ignore_ascii_case("on")
                || value.eq_ignore_ascii_case("true")
                || value == "1";
        }

        if let Some(value) = context.request_headers.get("x-forwarded-scheme") {
            return value.eq_ignore_ascii_case("https");
        }

        false
    }

    fn source_ip_values(context: &AuthContext) -> Vec<String> {
        context
            .request_headers
            .get("x-forwarded-for")
            .or_else(|| context.request_headers.get("x-real-ip"))
            .and_then(|value| value.split(',').next())
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
            .into_iter()
            .collect()
    }

    fn request_object_tag_values(context: &AuthContext, tag_key: &str) -> Vec<String> {
        context
            .request_headers
            .get("x-amz-tagging")
            .and_then(|tagging| Self::parse_key_value_pairs(tagging).get(tag_key).cloned())
            .into_iter()
            .collect()
    }

    fn parse_key_value_pairs(input: &str) -> HashMap<String, String> {
        let mut values = HashMap::new();

        for pair in input.split('&') {
            if pair.is_empty() {
                continue;
            }

            let Some((key, raw_value)) = pair.split_once('=') else {
                continue;
            };

            let decoded_key = urlencoding::decode(key).unwrap_or_default().to_string();
            let decoded_value = urlencoding::decode(raw_value)
                .unwrap_or_default()
                .to_string();
            values.insert(decoded_key, decoded_value);
        }

        values
    }

    fn json_values_to_strings(value: &serde_json::Value) -> Option<Vec<String>> {
        match value {
            serde_json::Value::String(value) => Some(vec![value.clone()]),
            serde_json::Value::Bool(value) => Some(vec![value.to_string()]),
            serde_json::Value::Number(value) => Some(vec![value.to_string()]),
            serde_json::Value::Array(values) => {
                let mut strings = Vec::with_capacity(values.len());
                for value in values {
                    match value {
                        serde_json::Value::String(value) => strings.push(value.clone()),
                        serde_json::Value::Bool(value) => strings.push(value.to_string()),
                        serde_json::Value::Number(value) => strings.push(value.to_string()),
                        _ => return None,
                    }
                }
                Some(strings)
            }
            _ => None,
        }
    }

    fn null_condition_matches(request_values: &[String], expected_values: &[String]) -> bool {
        let is_missing = request_values.is_empty();

        expected_values
            .iter()
            .filter_map(|value| Self::parse_bool(value))
            .any(|expected_missing| expected_missing == is_missing)
    }

    fn bool_condition_matches(request_values: &[String], expected_values: &[String]) -> bool {
        if request_values.is_empty() {
            return false;
        }

        let expected_bools: Vec<bool> = expected_values
            .iter()
            .filter_map(|value| Self::parse_bool(value))
            .collect();

        request_values.iter().any(|request_value| {
            Self::parse_bool(request_value)
                .is_some_and(|request_bool| expected_bools.contains(&request_bool))
        })
    }

    fn string_condition_matches(
        request_values: &[String],
        expected_values: &[String],
        negated: bool,
    ) -> bool {
        if request_values.is_empty() {
            return negated;
        }

        if negated {
            request_values.iter().all(|request_value| {
                expected_values
                    .iter()
                    .all(|expected| request_value != expected)
            })
        } else {
            request_values.iter().any(|request_value| {
                expected_values
                    .iter()
                    .any(|expected| request_value == expected)
            })
        }
    }

    fn wildcard_condition_matches(
        request_values: &[String],
        expected_values: &[String],
        negated: bool,
    ) -> bool {
        if request_values.is_empty() {
            return negated;
        }

        if negated {
            request_values.iter().all(|request_value| {
                expected_values
                    .iter()
                    .all(|expected| !Self::wildcard_match(expected, request_value))
            })
        } else {
            request_values.iter().any(|request_value| {
                expected_values
                    .iter()
                    .any(|expected| Self::wildcard_match(expected, request_value))
            })
        }
    }

    fn numeric_condition_matches(
        request_values: &[String],
        expected_values: &[String],
        condition: NumericCondition,
    ) -> bool {
        if request_values.is_empty() {
            return matches!(condition, NumericCondition::NotEquals);
        }

        let expected_numbers: Vec<f64> = expected_values
            .iter()
            .filter_map(|value| value.parse::<f64>().ok())
            .collect();

        if expected_numbers.is_empty() {
            return false;
        }

        match condition {
            NumericCondition::Equals => request_values.iter().any(|request_value| {
                request_value
                    .parse::<f64>()
                    .ok()
                    .is_some_and(|request_number| {
                        expected_numbers
                            .iter()
                            .any(|expected| request_number.total_cmp(expected).is_eq())
                    })
            }),
            NumericCondition::NotEquals => request_values.iter().all(|request_value| {
                request_value
                    .parse::<f64>()
                    .ok()
                    .is_some_and(|request_number| {
                        expected_numbers
                            .iter()
                            .all(|expected| !request_number.total_cmp(expected).is_eq())
                    })
            }),
            NumericCondition::LessThan => request_values.iter().any(|request_value| {
                request_value
                    .parse::<f64>()
                    .ok()
                    .is_some_and(|request_number| {
                        expected_numbers
                            .iter()
                            .any(|expected| request_number < *expected)
                    })
            }),
            NumericCondition::LessThanEquals => request_values.iter().any(|request_value| {
                request_value
                    .parse::<f64>()
                    .ok()
                    .is_some_and(|request_number| {
                        expected_numbers
                            .iter()
                            .any(|expected| request_number <= *expected)
                    })
            }),
            NumericCondition::GreaterThan => request_values.iter().any(|request_value| {
                request_value
                    .parse::<f64>()
                    .ok()
                    .is_some_and(|request_number| {
                        expected_numbers
                            .iter()
                            .any(|expected| request_number > *expected)
                    })
            }),
            NumericCondition::GreaterThanEquals => request_values.iter().any(|request_value| {
                request_value
                    .parse::<f64>()
                    .ok()
                    .is_some_and(|request_number| {
                        expected_numbers
                            .iter()
                            .any(|expected| request_number >= *expected)
                    })
            }),
        }
    }

    fn parse_bool(value: &str) -> Option<bool> {
        match value.trim().to_ascii_lowercase().as_str() {
            "true" | "1" | "on" => Some(true),
            "false" | "0" | "off" => Some(false),
            _ => None,
        }
    }

    fn wildcard_match(pattern: &str, value: &str) -> bool {
        let pattern_chars: Vec<char> = pattern.chars().collect();
        let value_chars: Vec<char> = value.chars().collect();
        let mut pattern_index = 0usize;
        let mut value_index = 0usize;
        let mut star_index: Option<usize> = None;
        let mut match_index = 0usize;

        while value_index < value_chars.len() {
            if pattern_index < pattern_chars.len()
                && (pattern_chars[pattern_index] == '?'
                    || pattern_chars[pattern_index] == value_chars[value_index])
            {
                pattern_index += 1;
                value_index += 1;
            } else if pattern_index < pattern_chars.len() && pattern_chars[pattern_index] == '*' {
                star_index = Some(pattern_index);
                match_index = value_index;
                pattern_index += 1;
            } else if let Some(star_position) = star_index {
                pattern_index = star_position + 1;
                match_index += 1;
                value_index = match_index;
            } else {
                return false;
            }
        }

        while pattern_index < pattern_chars.len() && pattern_chars[pattern_index] == '*' {
            pattern_index += 1;
        }

        pattern_index == pattern_chars.len()
    }

    fn principal_matches(principal: &Principal, context: &AuthContext) -> bool {
        match principal {
            Principal::All(s) if s == "*" => true,
            Principal::AWS(list) => match list {
                StringOrArray::Single(p) => p == &context.principal || p == "*",
                StringOrArray::Multiple(principals) => {
                    principals.contains(&context.principal) || principals.contains(&"*".to_string())
                }
            },
            Principal::All(_) => false,
        }
    }

    fn action_matches(actions: &ActionList, context_action: &str) -> bool {
        let check_action = |action: &str| -> bool {
            if action == "*" || action == "s3:*" {
                return true;
            }
            // Simple wildcard matching
            if action.ends_with('*') {
                let prefix = action.trim_end_matches('*');
                context_action.starts_with(prefix)
            } else {
                action == context_action
            }
        };

        match actions {
            ActionList::Single(action) => check_action(action),
            ActionList::Multiple(action_list) => action_list.iter().any(|a| check_action(a)),
        }
    }

    fn resource_matches(resources: &ResourceList, context_resource: &str) -> bool {
        let check_resource = |resource: &str| -> bool {
            if resource == "*" {
                return true;
            }
            // Simple wildcard matching
            if resource.ends_with('*') {
                let prefix = resource.trim_end_matches('*');
                context_resource.starts_with(prefix)
            } else {
                resource == context_resource
            }
        };

        match resources {
            ResourceList::Single(resource) => check_resource(resource),
            ResourceList::Multiple(resource_list) => {
                resource_list.iter().any(|r| check_resource(r))
            }
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum NumericCondition {
    Equals,
    NotEquals,
    LessThan,
    LessThanEquals,
    GreaterThan,
    GreaterThanEquals,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PolicyEffect {
    Allow,
    Deny,
    Neutral, // No matching statement
}

// ============================================================================
// Bucket Policy Document Models
// ============================================================================

/// Bucket policy document (JSON format)
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct BucketPolicyDocument {
    pub version: String,
    pub statement: Vec<PolicyStatementDocument>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct PolicyStatementDocument {
    pub sid: Option<String>,
    pub effect: String, // "Allow" or "Deny"
    pub principal: Principal,
    pub action: ActionList,
    pub resource: ResourceList,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub condition: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Principal {
    All(String), // "*"
    AWS(StringOrArray),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum StringOrArray {
    Single(String),
    Multiple(Vec<String>),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ActionList {
    Single(String),
    Multiple(Vec<String>),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ResourceList {
    Single(String),
    Multiple(Vec<String>),
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn base_context() -> AuthContext {
        AuthContext {
            principal: "*".to_string(),
            is_authenticated: false,
            action: "s3:ListBucket".to_string(),
            resource: "arn:aws:s3:::bucket".to_string(),
            bucket_owner: None,
            object_owner: None,
            request_headers: HashMap::new(),
            query_params: HashMap::new(),
            existing_object_tags: HashMap::new(),
        }
    }

    #[test]
    fn should_allow_policy_when_string_equals_condition_matches_query_param() {
        // Arrange
        let mut context = base_context();
        context
            .query_params
            .insert("prefix".to_string(), "allowed/".to_string());

        let policy = BucketPolicyDocument {
            version: "2012-10-17".to_string(),
            statement: vec![PolicyStatementDocument {
                sid: Some("allow-prefix".to_string()),
                effect: "Allow".to_string(),
                principal: Principal::All("*".to_string()),
                action: ActionList::Single("s3:ListBucket".to_string()),
                resource: ResourceList::Single("arn:aws:s3:::bucket".to_string()),
                condition: Some(json!({
                    "StringEquals": {
                        "s3:prefix": "allowed/"
                    }
                })),
            }],
        };

        // Act
        let effect = Authorizer::evaluate_policy(&policy, &context);

        // Assert
        assert_eq!(effect, PolicyEffect::Allow);
    }

    #[test]
    fn should_deny_policy_when_string_equals_condition_does_not_match_query_param() {
        // Arrange
        let mut context = base_context();
        context
            .query_params
            .insert("prefix".to_string(), "denied/".to_string());

        let policy = BucketPolicyDocument {
            version: "2012-10-17".to_string(),
            statement: vec![PolicyStatementDocument {
                sid: Some("allow-prefix".to_string()),
                effect: "Allow".to_string(),
                principal: Principal::All("*".to_string()),
                action: ActionList::Single("s3:ListBucket".to_string()),
                resource: ResourceList::Single("arn:aws:s3:::bucket".to_string()),
                condition: Some(json!({
                    "StringEquals": {
                        "s3:prefix": "allowed/"
                    }
                })),
            }],
        };

        // Act
        let effect = Authorizer::evaluate_policy(&policy, &context);

        // Assert
        assert_eq!(effect, PolicyEffect::Neutral);
    }

    #[test]
    fn should_allow_policy_when_secure_transport_condition_matches_forwarded_proto_header() {
        // Arrange
        let mut context = base_context();
        context
            .request_headers
            .insert("x-forwarded-proto".to_string(), "https".to_string());

        let policy = BucketPolicyDocument {
            version: "2012-10-17".to_string(),
            statement: vec![PolicyStatementDocument {
                sid: Some("secure-transport".to_string()),
                effect: "Allow".to_string(),
                principal: Principal::All("*".to_string()),
                action: ActionList::Single("s3:ListBucket".to_string()),
                resource: ResourceList::Single("arn:aws:s3:::bucket".to_string()),
                condition: Some(json!({
                    "Bool": {
                        "aws:SecureTransport": "true"
                    }
                })),
            }],
        };

        // Act
        let effect = Authorizer::evaluate_policy(&policy, &context);

        // Assert
        assert_eq!(effect, PolicyEffect::Allow);
    }
}
