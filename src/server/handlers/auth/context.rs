use crate::auth::AuthConfig;
use crate::models::Owner;
use std::collections::HashMap;

pub(super) fn default_owner(config: &AuthConfig) -> Owner {
    let owner = config
        .access_key()
        .map(|key| key.to_string())
        .unwrap_or_else(|| "sqrzl-emulator".to_string());

    Owner {
        id: owner.clone(),
        display_name: owner,
    }
}

pub(super) fn build_request_headers(
    req: &dyn crate::auth::HttpRequestLike,
) -> HashMap<String, String> {
    req.headers()
        .into_iter()
        .map(|(name, value)| (name.to_lowercase(), value))
        .collect()
}

pub(super) fn parse_query_params(query: Option<&str>) -> HashMap<String, String> {
    let mut query_params = HashMap::new();

    let Some(query) = query else {
        return query_params;
    };

    for param in query.split('&') {
        if param.is_empty() {
            continue;
        }

        if let Some((key, value)) = param.split_once('=') {
            let decoded_key = urlencoding::decode(key).unwrap_or_default().to_lowercase();
            let decoded_value = urlencoding::decode(value).unwrap_or_default().to_string();
            query_params.insert(decoded_key, decoded_value);
        } else {
            let decoded_key = urlencoding::decode(param)
                .unwrap_or_default()
                .to_lowercase();
            query_params.insert(decoded_key, String::new());
        }
    }

    query_params
}
