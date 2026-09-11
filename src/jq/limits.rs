//! Bounded, incremental jq output rendering: [`LimitedWriter`] is a generic
//! "budgeted `io::Write`" (a cumulative limit plus a per-value limit, checked
//! on every write), [`OutputWriter`] uses it to accumulate a filter's
//! rendered outputs newline-joined, and [`render_value_into`]/
//! [`write_raw_string`] do the actual value-to-bytes rendering.
//!
//! Deliberately not shared with `mcp::ByteCounter`, the other "count bytes
//! against a budget" type in this crate: `ByteCounter` discards bytes and
//! counts against one limit with no cancellation, while `LimitedWriter`
//! retains bytes into a caller-owned buffer against two limits (cumulative
//! and per-value) with cooperative cancellation and `try_reserve_exact`. The
//! overlap is the ~10 lines of bounds-checking arithmetic; sharing it would
//! cost `mcp.rs` three unused fields and its own `with_context`-attached
//! error text (see the design plan's C7 note).

use std::io::{self, Write};
use std::sync::atomic::AtomicBool;

use anyhow::{Result, anyhow, bail};
use jaq_json::Val;

use super::{MAX_OUTPUT_VALUES, MAX_RENDERED_BYTES, check_cancelled_opt, validate_value_structure};

/// A writer that bounds both the complete rendered result and the value that
/// is currently being written. It borrows the result buffer so a filter's
/// values are rendered one at a time without ever building a `Vec<Val>` or a
/// second full-size string for each value.
struct LimitedWriter<'a> {
    bytes: &'a mut Vec<u8>,
    value_start: usize,
    total_limit: usize,
    value_limit: usize,
    cancelled: Option<&'a AtomicBool>,
    exceeded: bool,
}

/// The single wording used whenever a rendered value or the accumulated
/// output crosses a configured byte limit, regardless of which of this
/// module's several checks caught it (a mid-value overflow inside
/// [`LimitedWriter::write`], the newline separator between values in
/// [`OutputWriter::render`], or the raw-string fast path in
/// [`render_value_into`]) — so the user sees the same phrasing no matter
/// which branch was taken for the same underlying condition.
fn output_limit_exceeded_message(byte_limit: usize) -> String {
    format!("jq rendered output exceeds the configured limit of {byte_limit} bytes")
}

impl LimitedWriter<'_> {
    fn new<'a>(
        bytes: &'a mut Vec<u8>,
        value_start: usize,
        total_limit: usize,
        value_limit: usize,
        cancelled: Option<&'a AtomicBool>,
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
        check_cancelled_opt(self.cancelled)
            .map_err(|error| io::Error::new(io::ErrorKind::Interrupted, error.to_string()))?;
        let Some(next_len) = self.bytes.len().checked_add(bytes.len()) else {
            self.exceeded = true;
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                output_limit_exceeded_message(self.total_limit),
            ));
        };
        let Some(value_len) = next_len.checked_sub(self.value_start) else {
            self.exceeded = true;
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                output_limit_exceeded_message(self.total_limit),
            ));
        };
        if next_len > self.total_limit || value_len > self.value_limit {
            self.exceeded = true;
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                output_limit_exceeded_message(self.total_limit),
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

/// Renders values yielded by `apply` directly into one bounded output buffer.
/// A newline separator is charged to the cumulative limit but not to the
/// per-value limit.
pub(super) struct OutputWriter<'a> {
    bytes: Vec<u8>,
    values: usize,
    cancelled: Option<&'a AtomicBool>,
}

impl OutputWriter<'_> {
    pub(super) fn new(cancelled: Option<&AtomicBool>) -> OutputWriter<'_> {
        OutputWriter {
            bytes: Vec::new(),
            values: 0,
            cancelled,
        }
    }

    pub(super) fn render(&mut self, filter_source: &str, value: &Val) -> Result<()> {
        check_cancelled_opt(self.cancelled)?;
        if self.values >= MAX_OUTPUT_VALUES {
            bail!(
                "jq filter {filter_source:?} produced more than the configured limit of {} outputs",
                MAX_OUTPUT_VALUES
            );
        }
        if self.values != 0 {
            let next_len = self.bytes.len().checked_add(1).ok_or_else(|| {
                anyhow!(
                    "jq filter {filter_source:?} rendered output: {}",
                    output_limit_exceeded_message(MAX_RENDERED_BYTES)
                )
            })?;
            if next_len > MAX_RENDERED_BYTES {
                bail!(
                    "jq filter {filter_source:?} rendered output: {}",
                    output_limit_exceeded_message(MAX_RENDERED_BYTES)
                );
            }
            self.bytes.push(b'\n');
        }
        let value_start = self.bytes.len();
        let cancelled = self.cancelled;
        render_value_into(
            value,
            true,
            &mut self.bytes,
            value_start,
            MAX_RENDERED_BYTES,
            MAX_RENDERED_BYTES,
            cancelled,
        )
        .map_err(|error| anyhow!("jq filter {filter_source:?} rendered output: {error}"))?;
        self.values += 1;
        Ok(())
    }

    pub(super) fn finish(self, filter_source: &str) -> Result<String> {
        check_cancelled_opt(self.cancelled)?;
        String::from_utf8(self.bytes).map_err(|error| {
            anyhow!("jq filter {filter_source:?} rendered output was not valid UTF-8: {error}")
        })
    }
}

/// Renders one value either in jq's raw-string mode (`apply`) or as compact
/// JSON (`apply_one` and condition-size checks), never allowing the rendered
/// bytes to exceed `MAX_RENDERED_BYTES`.
pub(super) fn render_value_into(
    value: &Val,
    raw_strings: bool,
    output_bytes: &mut Vec<u8>,
    value_start: usize,
    total_limit: usize,
    value_limit: usize,
    cancelled: Option<&AtomicBool>,
) -> Result<()> {
    check_cancelled_opt(cancelled)?;
    validate_value_structure(value)?;

    if raw_strings && let Val::TStr(string_bytes) = value {
        // JSON input and the standard jq string functions produce UTF-8,
        // but jaq also permits a TStr containing invalid bytes. Reject by
        // the source byte count before `from_utf8_lossy` can expand it.
        if string_bytes.len() > value_limit {
            bail!(output_limit_exceeded_message(value_limit));
        }
        let mut writer = LimitedWriter::new(
            output_bytes,
            value_start,
            total_limit,
            value_limit,
            cancelled,
        );
        write_raw_string(&mut writer, string_bytes).map_err(|error| {
            if writer.exceeded {
                anyhow!(output_limit_exceeded_message(writer.total_limit))
            } else {
                anyhow!("failed to render jq output: {error}")
            }
        })?;
        check_cancelled_opt(cancelled)?;
        return Ok(());
    }

    let mut writer = LimitedWriter::new(
        output_bytes,
        value_start,
        total_limit,
        value_limit,
        cancelled,
    );
    jaq_json::write::write(&mut writer, &Default::default(), 0, value).map_err(|error| {
        if writer.exceeded {
            anyhow!(output_limit_exceeded_message(writer.total_limit))
        } else {
            anyhow!("failed to render jq output: {error}")
        }
    })?;
    check_cancelled_opt(cancelled)?;
    Ok(())
}

/// Writes a jq text string without first allocating a lossily-converted copy
/// of the whole value. This matters for invalid UTF-8: `from_utf8_lossy` can
/// expand every invalid byte to a three-byte replacement character, so a
/// whole-value conversion would temporarily exceed the per-value limit.
fn write_raw_string(writer: &mut LimitedWriter<'_>, bytes: &[u8]) -> io::Result<()> {
    let mut remaining = bytes;
    while !remaining.is_empty() {
        match std::str::from_utf8(remaining) {
            Ok(valid) => {
                writer.write_all(valid.as_bytes())?;
                break;
            }
            Err(error) => {
                let valid_len = error.valid_up_to();
                if valid_len != 0 {
                    writer.write_all(&remaining[..valid_len])?;
                }
                writer.write_all("�".as_bytes())?;
                let invalid_len = error.error_len().unwrap_or(remaining.len() - valid_len);
                remaining = &remaining[valid_len + invalid_len..];
            }
        }
    }
    Ok(())
}
