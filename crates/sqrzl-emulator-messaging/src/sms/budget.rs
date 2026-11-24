use crate::capture::budget::{json_bytes, CaptureBudget, ENVELOPE_BYTES};
use crate::capture::RepeatabilityRecord;
use crate::error::Result;
use crate::sms::NewSmsMessage;

pub(crate) fn check_messages(
    messages: &[NewSmsMessage],
    records: &[RepeatabilityRecord],
) -> Result<()> {
    let mut budget = CaptureBudget::default();
    for message in messages {
        // Borrow the submitted fields; inline media is separately charged as
        // raw bytes, because stored message JSON contains only media descriptors.
        let fields = (
            &message.batch_id,
            &message.provider_message_id,
            &message.from,
            &message.to,
            &message.body,
            &message.metadata,
        );
        // Submitted payload, caller batch clone, stored result and JSON buffer.
        budget.copies(json_bytes(&fields)?, 4)?;
        budget.copies(ENVELOPE_BYTES, 4)?;
        for media in &message.media {
            budget.copies(
                json_bytes(&(&media.filename, &media.content_type, &media.external_url))?,
                4,
            )?;
            budget.add(ENVELOPE_BYTES)?;
            // Caller media and the submitted/staged sidecar bytes.
            budget.copies(
                media.content.as_ref().map_or(0, |bytes| bytes.len() as u64),
                2,
            )?;
        }
    }
    budget.records(records)
}
