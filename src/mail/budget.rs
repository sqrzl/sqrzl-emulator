use crate::capture::budget::{json_bytes, CaptureBudget, ENVELOPE_BYTES};
use crate::capture::RepeatabilityRecord;
use crate::error::Result;
use crate::mail::{Message, ALL_MAILBOX};
use serde::Serialize;

pub(crate) fn add_message(
    budget: &mut CaptureBudget,
    message: &impl Serialize,
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
        // Stored envelope, transaction metadata, generated identifiers and paths.
        budget.add(ENVELOPE_BYTES + target.len() as u64)?;
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
    budget.add(message.raw_mime.as_ref().map_or(0, |raw| raw.len() as u64))?;
    budget.add(ENVELOPE_BYTES + mailbox.len() as u64 + id.len() as u64)
}
