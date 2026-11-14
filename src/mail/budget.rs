use crate::capture::budget::{json_bytes, CaptureBudget, ENVELOPE_BYTES};
use crate::capture::RepeatabilityRecord;
use crate::error::Result;
use crate::mail::{Address, Message, ALL_MAILBOX};
use serde::Serialize;

/// Measures retained summary fields without copying bodies or attachments.
pub(crate) fn summary_bytes(
    from: &Address,
    to: &[Address],
    cc: &[Address],
    bcc: &[Address],
    subject: &str,
    thread_id: Option<&str>,
) -> Result<u64> {
    #[derive(Serialize)]
    struct Summary<'a> {
        from: &'a Address,
        to: &'a [Address],
        cc: &'a [Address],
        bcc: &'a [Address],
        subject: &'a str,
        thread_id: Option<&'a str>,
    }
    json_bytes(&Summary {
        from,
        to,
        cc,
        bcc,
        subject,
        thread_id,
    })
}

pub(crate) fn add_message(
    budget: &mut CaptureBudget,
    message: &impl Serialize,
    summary_bytes: u64,
    raw_bytes: u64,
    recipients: impl Iterator<Item = String>,
) -> Result<()> {
    let bytes = json_bytes(message)?;
    let mut targets = recipients.collect::<Vec<_>>();
    targets.sort();
    targets.dedup();
    // Source payload, the batch request clone, and one transient prepared copy.
    budget.copies(bytes, 3)?;
    for target in targets
        .iter()
        .map(String::as_str)
        .chain(std::iter::once(ALL_MAILBOX))
    {
        budget.add(bytes)?;
        budget.copies(summary_bytes, 2)?;
        // Stored envelope, transaction metadata, generated identifiers and paths.
        budget.add(ENVELOPE_BYTES * 3 + target.len() as u64)?;
        budget.add(raw_bytes)?;
        if target != ALL_MAILBOX {
            budget.add(bytes)?;
        }
    }
    Ok(())
}

pub(crate) fn check_messages<'a>(
    messages: impl Iterator<Item = &'a Message>,
    records: &[RepeatabilityRecord],
) -> Result<()> {
    let mut budget = CaptureBudget::default();
    for message in messages {
        add_message(
            &mut budget,
            message,
            summary_bytes(
                &message.from,
                &message.to,
                &message.cc,
                &message.bcc,
                &message.subject,
                message.thread_id.as_deref(),
            )?,
            message.raw_mime.as_ref().map_or(0, |raw| raw.len() as u64),
            message
                .recipients()
                .into_iter()
                .map(crate::mail::Address::mailbox_key),
        )?;
    }
    budget.records(records)
}

pub(crate) fn check_single_copy(message: &Message, mailbox: &str, id: &str) -> Result<()> {
    let mut budget = CaptureBudget::default();
    // Input message, prepared result and serialized JSON buffer.
    budget.copies(json_bytes(message)?, 3)?;
    budget.copies(
        summary_bytes(
            &message.from,
            &message.to,
            &message.cc,
            &message.bcc,
            &message.subject,
            message.thread_id.as_deref(),
        )?,
        2,
    )?;
    budget.add(message.raw_mime.as_ref().map_or(0, |raw| raw.len() as u64))?;
    budget.add(ENVELOPE_BYTES * 3 + mailbox.len() as u64 + id.len() as u64)
}
