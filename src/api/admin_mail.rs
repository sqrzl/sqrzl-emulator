//! Admin API endpoints for inspecting captured mail.

use crate::api::admin::{
    mail_dto::{mailbox_to_info, message_summaries, stored_to_detail, stored_to_summary},
    pagination::{
        contains_search, encode_next, encode_object_next, parse_object_page_params,
        parse_page_params, PageTokenKind,
    },
};
use crate::body::Body;
use crate::error::{Error, Result};
use crate::mail::{ListMessagesParams, MailStore};
use crate::server::ResponseBuilder;
use crate::services::json_response;
use hyper::{Method, Request, Response, StatusCode};
use std::sync::Arc;

/// Path prefix routed here from `src/api/server.rs`, checked before the blob
/// admin API's `/admin/v1` catch-all.
pub const MAILBOXES_PATH_PREFIX: &str = "/admin/v1/mailboxes";

///
/// # Errors
///
/// Returns an error when the underlying emulator operation fails.
pub fn handle_request<B>(mail: &Arc<dyn MailStore>, req: &Request<B>) -> Result<Response<Body>>
where
    B: hyper::body::Body<Data = bytes::Bytes> + Send + 'static,
    B::Error: std::fmt::Display,
{
    let method = req.method().clone();
    let path = req.uri().path().to_string();
    let query = req.uri().query().unwrap_or("").to_string();
    let segments: Vec<String> = path
        .strip_prefix(MAILBOXES_PATH_PREFIX)
        .unwrap_or(&path)
        .trim_start_matches('/')
        .split('/')
        .filter(|segment| !segment.is_empty())
        .map(decode_path_segment)
        .collect::<Option<Vec<_>>>()
        .ok_or_else(|| {
            Error::InvalidRequest("invalid path encoding for mailbox endpoint".to_string())
        })?;

    match (method, segments.as_slice()) {
        (Method::GET, []) => list_mailboxes(mail, &query),
        (Method::GET, [mailbox, resource]) if *resource == "messages" => {
            list_messages(mail, mailbox, &query)
        }
        (Method::GET, [mailbox, resource, message_id]) if *resource == "messages" => {
            get_message(mail, mailbox, message_id)
        }
        (Method::GET, [mailbox, resource, message_id, resource2])
            if *resource == "messages" && resource2 == "content" =>
        {
            get_message_content(mail, mailbox, message_id)
        }
        (Method::GET, [mailbox, resource, message_id, resource2, filename])
            if *resource == "messages" && resource2 == "attachments" =>
        {
            get_message_attachment(mail, mailbox, message_id, filename)
        }
        (Method::DELETE, [mailbox]) => delete_mailbox(mail, mailbox),
        (Method::DELETE, [mailbox, resource, message_id]) if *resource == "messages" => {
            delete_message(mail, mailbox, message_id)
        }
        (method, _) => Err(Error::MethodNotAllowed(format!("{method} {path}"))),
    }
}

fn decode_path_segment(value: &str) -> Option<String> {
    urlencoding::decode(value)
        .ok()
        .map(std::borrow::Cow::into_owned)
}

fn list_mailboxes(mail: &Arc<dyn MailStore>, query: &str) -> Result<Response<Body>> {
    let page = parse_page_params(query, PageTokenKind::Mailboxes)?;
    let mut mailboxes = tokio::task::block_in_place(|| mail.list_mailboxes())?;
    mailboxes.sort_by(|left, right| left.address.cmp(&right.address));
    let items: Vec<_> = mailboxes
        .into_iter()
        .map(mailbox_to_info)
        .filter(|mailbox| contains_search(&mailbox.address, page.search.as_deref()))
        .collect();
    let (items, next) = crate::api::admin::pagination::paginate(items, &page);

    Ok(json_response(
        StatusCode::OK,
        &crate::api::models::ListMailboxesResponse {
            items,
            next: encode_next(next, PageTokenKind::Mailboxes),
        },
    ))
}

fn list_messages(mail: &Arc<dyn MailStore>, mailbox: &str, query: &str) -> Result<Response<Body>> {
    let page = parse_object_page_params(query, PageTokenKind::Messages)?;

    if page.search.is_none() {
        let result = tokio::task::block_in_place(|| {
            mail.list_messages(
                mailbox,
                ListMessagesParams {
                    marker: page.next,
                    limit: Some(page.limit),
                },
            )
        })?;
        let response = message_summaries(result);
        return Ok(json_response(
            StatusCode::OK,
            &crate::api::models::ListMessagesResponse {
                items: response.items,
                next: encode_object_next(response.next, PageTokenKind::Messages),
            },
        ));
    }

    let mut page_items = Vec::new();
    let mut cursor = page.next;
    let mut has_more = false;

    while page_items.len() < page.limit {
        let result = tokio::task::block_in_place(|| {
            mail.list_messages(
                mailbox,
                ListMessagesParams {
                    marker: cursor.clone(),
                    limit: Some(page.limit),
                },
            )
        })?;

        let matches = result
            .messages
            .into_iter()
            .filter(|message| message_matches_query(message, page.search.as_deref()))
            .collect::<Vec<_>>();
        page_items.extend(matches);
        if page_items.len() >= page.limit {
            has_more = page_items.len() > page.limit || result.next_marker.is_some();
            break;
        }
        let Some(next_marker) = result.next_marker else {
            break;
        };

        cursor = Some(next_marker);
    }

    if page_items.len() > page.limit {
        page_items.truncate(page.limit);
    }

    // Object markers are exclusive. When filtering spans multiple storage
    // batches, resume after the last item actually returned, not after the last
    // item fetched, or unreturned matches from the final batch are skipped.
    let next = has_more
        .then(|| page_items.last().map(|message| message.message_id.clone()))
        .flatten();

    let items = page_items
        .into_iter()
        .map(stored_to_summary)
        .collect::<Vec<_>>();
    Ok(json_response(
        StatusCode::OK,
        &crate::api::models::ListMessagesResponse {
            items,
            next: encode_object_next(next, PageTokenKind::Messages),
        },
    ))
}

fn get_message(
    mail: &Arc<dyn MailStore>,
    mailbox: &str,
    message_id: &str,
) -> Result<Response<Body>> {
    let message = tokio::task::block_in_place(|| mail.get_message(mailbox, message_id))?;
    Ok(json_response(StatusCode::OK, &stored_to_detail(message)))
}

fn get_message_content(
    mail: &Arc<dyn MailStore>,
    mailbox: &str,
    message_id: &str,
) -> Result<Response<Body>> {
    let message = tokio::task::block_in_place(|| mail.get_message(mailbox, message_id))?;
    let raw = message.message.raw_mime.ok_or(Error::MessageNotFound)?;

    Ok(ResponseBuilder::new(StatusCode::OK)
        .content_type("application/octet-stream")
        .body(raw)
        .build())
}

fn get_message_attachment(
    mail: &Arc<dyn MailStore>,
    mailbox: &str,
    message_id: &str,
    filename: &str,
) -> Result<Response<Body>> {
    let message = tokio::task::block_in_place(|| mail.get_message(mailbox, message_id))?;
    let attachment = message
        .message
        .attachments
        .into_iter()
        .find(|attachment| attachment.filename == filename)
        .ok_or(Error::MessageNotFound)?;

    Ok(ResponseBuilder::new(StatusCode::OK)
        .content_type(&attachment.content_type)
        .body(attachment.content)
        .build())
}

fn delete_message(
    mail: &Arc<dyn MailStore>,
    mailbox: &str,
    message_id: &str,
) -> Result<Response<Body>> {
    tokio::task::block_in_place(|| mail.delete_message(mailbox, message_id))?;
    Ok(ResponseBuilder::new(StatusCode::NO_CONTENT).build())
}

fn delete_mailbox(mail: &Arc<dyn MailStore>, mailbox: &str) -> Result<Response<Body>> {
    tokio::task::block_in_place(|| mail.delete_mailbox(mailbox))?;
    Ok(ResponseBuilder::new(StatusCode::NO_CONTENT).build())
}

fn message_matches_query(
    message: &crate::mail::model::StoredMessage,
    search: Option<&str>,
) -> bool {
    let Some(search) = search else {
        return true;
    };

    if contains_search(&message.message.subject, Some(search)) {
        return true;
    }
    if contains_search(&message.message.from.email, Some(search))
        || message
            .message
            .from
            .name
            .as_deref()
            .is_some_and(|name| contains_search(name, Some(search)))
    {
        return true;
    }

    let recipient_groups = [
        &message.message.to,
        &message.message.cc,
        &message.message.bcc,
    ];
    recipient_groups
        .iter()
        .flat_map(|group| group.iter())
        .any(|recipient| {
            contains_search(&recipient.email, Some(search))
                || recipient
                    .name
                    .as_deref()
                    .is_some_and(|name| contains_search(name, Some(search)))
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::body::Body as ResponseBody;
    use crate::mail::filesystem::FilesystemMailStore;
    use crate::mail::model::{Address, SourceProtocol};
    use crate::mail::{fan_out, Message};
    use http_body_util::BodyExt;
    use hyper::Request;
    use serde::de::DeserializeOwned;
    use serde_json::Value;

    fn temp_mail() -> Arc<dyn MailStore> {
        let dir = std::env::temp_dir().join(format!("sqrzl-admin-mail-{}", uuid::Uuid::new_v4()));
        Arc::new(FilesystemMailStore::open(dir).expect("store should open"))
    }

    fn sample_message(to: &str) -> Message {
        Message {
            source_protocol: SourceProtocol::Smtp,
            from: Address::new("sender@example.com"),
            to: vec![Address::new(to)],
            cc: Vec::new(),
            bcc: Vec::new(),
            subject: "hello".to_string(),
            headers: std::collections::HashMap::new(),
            body_text: Some("hi there".to_string()),
            body_html: None,
            attachments: Vec::new(),
            raw_mime: None,
            thread_id: None,
        }
    }

    fn call(req: &Request<ResponseBody>, mail: &Arc<dyn MailStore>) -> Response<Body> {
        match handle_request(mail, req) {
            Ok(resp) => resp,
            Err(err) => crate::api::admin::error_response(&err),
        }
    }

    async fn json_body<T: DeserializeOwned>(resp: Response<Body>) -> T {
        let bytes = resp
            .into_body()
            .collect()
            .await
            .expect("response body should read")
            .to_bytes();
        serde_json::from_slice(&bytes).expect("response body should deserialize")
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn should_list_mailboxes_and_messages_after_capture() {
        let mail = temp_mail();
        fan_out(mail.as_ref(), &sample_message("alice@example.com"))
            .expect("fan-out should succeed");

        let req = Request::builder()
            .method(Method::GET)
            .uri("/admin/v1/mailboxes")
            .body(ResponseBody::default())
            .unwrap();
        let resp = call(&req, &mail);
        assert_eq!(resp.status(), StatusCode::OK);
        let mailboxes: Value = json_body(resp).await;
        assert_eq!(mailboxes["items"][0]["address"], "alice@example.com");

        let req = Request::builder()
            .method(Method::GET)
            .uri("/admin/v1/mailboxes/alice@example.com/messages")
            .body(ResponseBody::default())
            .unwrap();
        let resp = call(&req, &mail);
        assert_eq!(resp.status(), StatusCode::OK);
        let messages: Value = json_body(resp).await;
        assert_eq!(messages["items"].as_array().unwrap().len(), 1);
        let message_id = messages["items"][0]["message_id"]
            .as_str()
            .unwrap()
            .to_string();

        let req = Request::builder()
            .method(Method::GET)
            .uri(format!(
                "/admin/v1/mailboxes/alice@example.com/messages/{message_id}"
            ))
            .body(ResponseBody::default())
            .unwrap();
        let resp = call(&req, &mail);
        assert_eq!(resp.status(), StatusCode::OK);
        let detail: Value = json_body(resp).await;
        assert_eq!(detail["subject"], "hello");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn should_download_raw_message_content_when_available() {
        let mail = temp_mail();
        let message_id = crate::mail::generate_message_id();
        let mut message = sample_message("alice@example.com");
        message.raw_mime = Some(b"raw message payload".to_vec());
        mail.store_message("alice@example.com", &message_id, message)
            .expect("message should be stored");

        let req = Request::builder()
            .method(Method::GET)
            .uri(format!(
                "/admin/v1/mailboxes/alice@example.com/messages/{message_id}/content"
            ))
            .body(ResponseBody::default())
            .unwrap();
        let resp = call(&req, &mail);

        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn should_report_method_not_allowed_for_unknown_routes() {
        let mail = temp_mail();

        let req = Request::builder()
            .method(Method::POST)
            .uri("/admin/v1/mailboxes")
            .body(ResponseBody::default())
            .unwrap();
        let resp = call(&req, &mail);

        assert_eq!(resp.status(), StatusCode::METHOD_NOT_ALLOWED);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn should_delete_mailbox_and_message_given_valid_path() {
        let mail = temp_mail();
        let message_id = crate::mail::generate_message_id();
        let message = sample_message("alice@example.com");
        mail.store_message("alice@example.com", &message_id, message)
            .expect("message should be stored");

        let req = Request::builder()
            .method(Method::DELETE)
            .uri(format!(
                "/admin/v1/mailboxes/alice@example.com/messages/{message_id}"
            ))
            .body(ResponseBody::default())
            .unwrap();
        let response = call(&req, &mail);
        assert_eq!(response.status(), StatusCode::NO_CONTENT);

        let req = Request::builder()
            .method(Method::DELETE)
            .uri("/admin/v1/mailboxes/alice@example.com")
            .body(ResponseBody::default())
            .unwrap();
        let response = call(&req, &mail);
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn should_filter_mailbox_messages_with_search() {
        let mail = temp_mail();
        fan_out(mail.as_ref(), &sample_message("alice@example.com"))
            .expect("fan-out should succeed");

        fan_out(mail.as_ref(), &{
            let mut message = sample_message("alice@example.com");
            message.subject = "team digest update".to_string();
            message
        })
        .expect("fan-out should succeed");

        let req = Request::builder()
            .method(Method::GET)
            .uri("/admin/v1/mailboxes/alice@example.com/messages?search=digest")
            .body(ResponseBody::default())
            .unwrap();

        let resp = call(&req, &mail);
        assert_eq!(resp.status(), StatusCode::OK);
        let messages: Value = json_body(resp).await;
        let items = messages["items"].as_array().expect("items should be array");
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["subject"], "team digest update");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn should_not_skip_filtered_matches_across_pages() {
        let mail = temp_mail();
        for subject in ["unrelated", "digest one", "digest two", "digest three"] {
            let mut message = sample_message("alice@example.com");
            message.subject = subject.to_string();
            fan_out(mail.as_ref(), &message).expect("fan-out should succeed");
        }

        let first_request = Request::builder()
            .method(Method::GET)
            .uri("/admin/v1/mailboxes/alice@example.com/messages?search=digest&limit=2")
            .body(ResponseBody::default())
            .unwrap();
        let first_response: Value = json_body(call(&first_request, &mail)).await;
        let first_items = first_response["items"]
            .as_array()
            .expect("first page items should be an array");
        assert_eq!(first_items.len(), 2);
        let next = first_response["next"]
            .as_str()
            .expect("first filtered page should have a continuation token");

        let second_request = Request::builder()
            .method(Method::GET)
            .uri(format!(
                "/admin/v1/mailboxes/alice@example.com/messages?search=digest&limit=2&next={next}"
            ))
            .body(ResponseBody::default())
            .unwrap();
        let second_response: Value = json_body(call(&second_request, &mail)).await;
        let second_items = second_response["items"]
            .as_array()
            .expect("second page items should be an array");
        assert_eq!(second_items.len(), 1);
        assert_eq!(second_items[0]["subject"], "digest three");
        assert!(second_response["next"].is_null());
    }
}
