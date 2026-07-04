use crate::error::{Error, Result};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use std::collections::HashMap;
use urlencoding::decode;

#[derive(Clone, Debug)]
pub(super) struct PageParams {
    pub(super) next: usize,
    pub(super) limit: usize,
    pub(super) search: Option<String>,
}

#[derive(Clone, Debug)]
pub(super) struct ObjectPageParams {
    pub(super) next: Option<String>,
    pub(super) limit: usize,
    pub(super) prefix: Option<String>,
    pub(super) search: Option<String>,
}

#[derive(Clone, Copy, Debug)]
pub(super) enum PageTokenKind {
    Buckets,
    Objects,
    Versions,
    MultipartUploads,
}

impl PageTokenKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Buckets => "buckets",
            Self::Objects => "objects",
            Self::Versions => "versions",
            Self::MultipartUploads => "multipart-uploads",
        }
    }
}

pub(super) fn decode_component(input: &str) -> String {
    decode(input).map_or_else(|_| input.to_string(), std::borrow::Cow::into_owned)
}

fn parse_query_map(query: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
    for pair in query.split('&').filter(|pair| !pair.is_empty()) {
        if let Some((key, value)) = pair.split_once('=') {
            out.insert(decode_component(key), decode_component(value));
        }
    }
    out
}

fn parse_next_token(token: &str, kind: PageTokenKind) -> Result<usize> {
    let decoded = URL_SAFE_NO_PAD
        .decode(token)
        .map_err(|_| Error::InvalidRequest("invalid next token".into()))?;
    let decoded = String::from_utf8(decoded)
        .map_err(|_| Error::InvalidRequest("invalid next token".into()))?;
    let (token_kind, offset) = decoded
        .split_once(':')
        .ok_or_else(|| Error::InvalidRequest("invalid next token".into()))?;

    if token_kind != kind.as_str() {
        return Err(Error::InvalidRequest("invalid next token".into()));
    }

    offset
        .parse::<usize>()
        .map_err(|_| Error::InvalidRequest("invalid next token".into()))
}

pub(super) fn parse_page_params(query: &str, kind: PageTokenKind) -> Result<PageParams> {
    let params = parse_query_map(query);

    let next = params
        .get("next")
        .map(|value| parse_next_token(value, kind))
        .transpose()?
        .unwrap_or(0);

    let limit = params
        .get("limit")
        .map(|value| {
            value
                .parse::<usize>()
                .map_err(|_| Error::InvalidRequest("invalid limit".into()))
        })
        .transpose()?
        .unwrap_or(50);

    if !(1..=500).contains(&limit) {
        return Err(Error::InvalidRequest(
            "limit must be between 1 and 500".into(),
        ));
    }

    let search = params
        .get("search")
        .map(|value| value.trim().to_ascii_lowercase())
        .filter(|value| !value.is_empty());

    Ok(PageParams {
        next,
        limit,
        search,
    })
}

fn parse_object_next_token(token: &str, kind: PageTokenKind) -> Result<String> {
    let decoded = URL_SAFE_NO_PAD
        .decode(token)
        .map_err(|_| Error::InvalidRequest("invalid next token".into()))?;
    let decoded = String::from_utf8(decoded)
        .map_err(|_| Error::InvalidRequest("invalid next token".into()))?;
    let (token_kind, marker) = decoded
        .split_once(':')
        .ok_or_else(|| Error::InvalidRequest("invalid next token".into()))?;

    if token_kind != kind.as_str() {
        return Err(Error::InvalidRequest("invalid next token".into()));
    }

    Ok(marker.to_string())
}

pub(super) fn parse_object_page_params(query: &str) -> Result<ObjectPageParams> {
    let params = parse_query_map(query);

    let next = params
        .get("next")
        .map(|value| parse_object_next_token(value, PageTokenKind::Objects))
        .transpose()?;

    let limit = params
        .get("limit")
        .map(|value| {
            value
                .parse::<usize>()
                .map_err(|_| Error::InvalidRequest("invalid limit".into()))
        })
        .transpose()?
        .unwrap_or(50);

    if !(1..=500).contains(&limit) {
        return Err(Error::InvalidRequest(
            "limit must be between 1 and 500".into(),
        ));
    }

    let prefix = params.get("prefix").cloned();
    let search = params
        .get("search")
        .map(|value| value.trim().to_ascii_lowercase())
        .filter(|value| !value.is_empty());

    Ok(ObjectPageParams {
        next,
        limit,
        prefix,
        search,
    })
}

pub(super) fn paginate<T>(items: Vec<T>, page: &PageParams) -> (Vec<T>, Option<usize>) {
    let start = page.next.min(items.len());
    let end = (start + page.limit).min(items.len());
    let next = (end < items.len()).then_some(end);
    let items = items.into_iter().skip(start).take(page.limit).collect();
    (items, next)
}

pub(super) fn encode_next(next: Option<usize>, kind: PageTokenKind) -> Option<String> {
    next.map(|offset| URL_SAFE_NO_PAD.encode(format!("{}:{}", kind.as_str(), offset)))
}

pub(super) fn encode_object_next(next: Option<String>, kind: PageTokenKind) -> Option<String> {
    next.map(|marker| URL_SAFE_NO_PAD.encode(format!("{}:{}", kind.as_str(), marker)))
}

pub(super) fn contains_search(value: &str, search: Option<&str>) -> bool {
    match search {
        Some(search) => value.to_ascii_lowercase().contains(search),
        None => true,
    }
}
