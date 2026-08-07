use crate::api::models;
use crate::mail::model::{ListMessagesResult, MailboxInfo, StoredMessage};

pub(crate) fn mailbox_to_info(mailbox: MailboxInfo) -> models::MailboxInfo {
    models::MailboxInfo {
        address: mailbox.address,
        message_count: mailbox.message_count,
        last_received_at: mailbox
            .last_received_at
            .map(|received_at| received_at.to_rfc3339()),
    }
}

pub(crate) fn message_summaries(result: ListMessagesResult) -> models::ListMessagesResponse {
    let items = result.messages.into_iter().map(stored_to_summary).collect();

    models::ListMessagesResponse {
        items,
        next: result.next_marker,
    }
}

pub(crate) fn stored_to_summary(message: StoredMessage) -> models::MessageSummary {
    models::MessageSummary {
        message_id: message.message_id,
        from: MessageAddress {
            email: message.message.from.email,
            name: message.message.from.name,
        }
        .into(),
        subject: message.message.subject,
        received_at: message.received_at.to_rfc3339(),
        delivery_state: message.delivery_status.state,
        to: message
            .message
            .to
            .into_iter()
            .map(|to| MessageAddress {
                email: to.email,
                name: to.name,
            })
            .map(Into::into)
            .collect(),
    }
}

pub(crate) fn stored_to_detail(message: StoredMessage) -> models::MessageDetail {
    models::MessageDetail {
        message_id: message.message_id,
        mailbox: message.mailbox,
        source_protocol: message.message.source_protocol,
        from: models::MailAddress {
            email: message.message.from.email,
            name: message.message.from.name,
        },
        to: message
            .message
            .to
            .into_iter()
            .map(|recipient| models::MailAddress {
                email: recipient.email,
                name: recipient.name,
            })
            .collect(),
        cc: message
            .message
            .cc
            .into_iter()
            .map(|recipient| models::MailAddress {
                email: recipient.email,
                name: recipient.name,
            })
            .collect(),
        bcc: message
            .message
            .bcc
            .into_iter()
            .map(|recipient| models::MailAddress {
                email: recipient.email,
                name: recipient.name,
            })
            .collect(),
        subject: message.message.subject,
        received_at: message.received_at.to_rfc3339(),
        delivery_state: message.delivery_status.state,
        delivery_detail: message.delivery_status.detail,
        headers: message.message.headers,
        body_text: message.message.body_text,
        body_html: message.message.body_html,
        thread_id: message.message.thread_id,
        attachments: message
            .message
            .attachments
            .into_iter()
            .map(|attachment| models::MessageAttachmentSummary {
                filename: attachment.filename,
                content_type: attachment.content_type,
                size: attachment.content.len(),
            })
            .collect(),
    }
}

pub(crate) struct MessageAddress {
    email: String,
    name: Option<String>,
}

impl From<MessageAddress> for models::MailAddress {
    fn from(address: MessageAddress) -> Self {
        Self {
            email: address.email,
            name: address.name,
        }
    }
}
