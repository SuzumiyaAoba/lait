//! Bounded jq output rendering: [`LimitedWriter`] is a "budgeted
//! `io::Write`" (a cumulative limit plus a per-value limit, checked on every
//! write) and [`render_value_into`] renders one yielded value into it as
//! compact JSON. Every jq evaluation produces exactly one value (see
//! `run_single_value`), so there is no multi-value output buffer here.
//!
//! Deliberately not shared with `mcp::ByteCounter`, the other "count bytes
//! against a budget" type in this crate: `ByteCounter` discards bytes and
//! counts against one limit with no cancellation, while `LimitedWriter`
//! retains bytes into a caller-owned buffer against two limits (cumulative
//! and per-value) with cooperative cancellation and `try_reserve_exact`.

use std::io;
use std::sync::atomic::AtomicBool;

use anyhow::{Result, anyhow};
use jaq_json::Val;

use super::{MAX_RENDERED_BYTES, check_cancelled, validate_value_structure};

/// A writer that bounds both the complete rendered result and the value that
/// is currently being written. It borrows the result buffer so a filter's
/// values are rendered one at a time without ever building a `Vec<Val>` or a
/// second full-size string for each value.
struct LimitedWriter<'a> {
    bytes: &'a mut Vec<u8>,
    value_start: usize,
    total_limit: usize,
    value_limit: usize,
    cancelled: &'a AtomicBool,
    exceeded: bool,
}

impl LimitedWriter<'_> {
    fn new<'a>(
        bytes: &'a mut Vec<u8>,
        value_start: usize,
        total_limit: usize,
        value_limit: usize,
        cancelled: &'a AtomicBool,
    ) -> LimitedWriter<'a> {
        LimitedWriter {
            bytes,
            value_start,
            total_limit,
            value_limit,
            cancelled,
            exceeded: false,
        }
    }
}

impl io::Write for LimitedWriter<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        check_cancelled(self.cancelled)
            .map_err(|error| io::Error::new(io::ErrorKind::Interrupted, error.to_string()))?;
        let Some(next_len) = self.bytes.len().checked_add(bytes.len()) else {
            self.exceeded = true;
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "jq rendered output exceeds the configured limit",
            ));
        };
        let Some(value_len) = next_len.checked_sub(self.value_start) else {
            self.exceeded = true;
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "jq rendered output exceeds the configured limit",
            ));
        };
        if next_len > self.total_limit || value_len > self.value_limit {
            self.exceeded = true;
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "jq rendered output exceeds the configured limit",
            ));
        }
        if self.bytes.capacity() < next_len {
            self.bytes
                .try_reserve_exact(next_len - self.bytes.len())
                .map_err(|error| io::Error::other(error.to_string()))?;
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Renders one value as compact JSON, never allowing the rendered bytes to
/// exceed `MAX_RENDERED_BYTES`.
pub(super) fn render_value_into(
    value: &Val,
    output_bytes: &mut Vec<u8>,
    cancelled: &AtomicBool,
) -> Result<()> {
    check_cancelled(cancelled)?;
    validate_value_structure(value)?;
    let mut writer = LimitedWriter::new(
        output_bytes,
        0,
        MAX_RENDERED_BYTES,
        MAX_RENDERED_BYTES,
        cancelled,
    );
    jaq_json::write::write(&mut writer, &Default::default(), 0, value).map_err(|error| {
        if writer.exceeded {
            anyhow!("jq rendered output exceeds the configured limit")
        } else {
            anyhow!("failed to render jq output: {error}")
        }
    })?;
    check_cancelled(cancelled)?;
    Ok(())
}
