//! Conservative admission for capture buffers, before fan-out materialization.
//!
//! Counts rendered JSON, raw/media sidecars and retained payload copies. This
//! is a local aggregate admission limit, not a measured process RSS boundary.

use crate::error::{Error, Result};
use serde::Serialize;
use std::io::Write;

pub(crate) const MAX_CAPTURE_BYTES: u64 = 64 * 1024 * 1024;
pub(crate) const ENVELOPE_BYTES: u64 = 1024;

#[derive(Default)]
pub(crate) struct CaptureBudget {
    bytes: u64,
}

impl CaptureBudget {
    pub(crate) fn add(&mut self, bytes: u64) -> Result<()> {
        self.bytes = self
            .bytes
            .checked_add(bytes)
            .filter(|total| *total <= MAX_CAPTURE_BYTES)
            .ok_or(Error::CaptureTooLarge)?;
        Ok(())
    }

    pub(crate) fn copies(&mut self, bytes: u64, copies: u64) -> Result<()> {
        self.add(bytes.checked_mul(copies).ok_or(Error::CaptureTooLarge)?)
    }

    pub(crate) fn records(&mut self, records: &[super::RepeatabilityRecord]) -> Result<()> {
        for record in records {
            // The supplied record, its transaction-tagged clone and JSON buffer.
            self.copies(json_bytes(record)?, 3)?;
            self.add(ENVELOPE_BYTES)?;
        }
        Ok(())
    }
}

impl Write for CaptureBudget {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.add(bytes.len() as u64)
            .map_err(|_| std::io::Error::other("capture limit"))?;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

pub(crate) fn json_bytes(value: &impl Serialize) -> Result<u64> {
    let mut counter = CaptureBudget::default();
    serde_json::to_writer(&mut counter, value).map_err(|error| {
        if error.is_io() {
            Error::CaptureTooLarge
        } else {
            Error::InternalError(error.to_string())
        }
    })?;
    Ok(counter.bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_admit_the_capture_budget_boundary_without_allocation() {
        // Arrange a counter at the configured boundary.
        let mut budget = CaptureBudget::default();

        // Act by filling the budget and attempting an extra byte and an overflow.
        budget.add(MAX_CAPTURE_BYTES - 1).unwrap();
        budget.add(1).unwrap();
        let extra = budget.add(1);
        let overflow = CaptureBudget::default().copies(u64::MAX, 2);

        // Assert neither over-limit projection can be admitted.
        assert!(matches!(extra, Err(Error::CaptureTooLarge)));
        assert!(matches!(overflow, Err(Error::CaptureTooLarge)));
    }
}
