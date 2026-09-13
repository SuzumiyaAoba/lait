//! Reading a spawned command's stdout/stderr into memory under a byte limit,
//! and the result type ([`CapturedOutput`]) `run_process` returns once both
//! streams and the exit status are all in hand. Split out of the parent
//! module — this is the "capture a stream" primitive `run_process`'s
//! `select!` loop drives, not part of the loop itself.

use std::process::ExitStatus;

use anyhow::{Context, Result, bail};
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::task::JoinHandle;

/// Captures one command stream while enforcing the same 16 MiB budget used by
/// file-backed inputs. Reading in bounded chunks lets the caller observe an
/// oversized stream as soon as the first chunk crosses the limit.
async fn read_limited<R>(
    mut reader: R,
    command_kind: &'static str,
    stream_name: &'static str,
    max_output_bytes: usize,
) -> Result<Vec<u8>>
where
    R: AsyncRead + Unpin,
{
    const CHUNK_SIZE: usize = 64 * 1024;
    let mut bytes = Vec::new();
    let mut buffer = [0_u8; CHUNK_SIZE];
    loop {
        let read = reader
            .read(&mut buffer)
            .await
            .with_context(|| format!("failed to read {command_kind} {stream_name}"))?;
        if read == 0 {
            return Ok(bytes);
        }
        let Some(next_len) = bytes.len().checked_add(read) else {
            bail!("{command_kind} {stream_name} output size overflowed");
        };
        if next_len > max_output_bytes {
            bail!(
                "{command_kind} {stream_name} output exceeds the configured limit of {max_output_bytes} bytes"
            );
        }
        bytes.extend_from_slice(&buffer[..read]);
    }
}

pub(super) fn spawn_output_reader<R>(
    reader: R,
    command_kind: &'static str,
    stream_name: &'static str,
    max_output_bytes: usize,
) -> JoinHandle<Result<Vec<u8>>>
where
    R: AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(read_limited(
        reader,
        command_kind,
        stream_name,
        max_output_bytes,
    ))
}

pub(super) struct CapturedOutput {
    pub(super) status: ExitStatus,
    pub(super) stdout: Vec<u8>,
    pub(super) stderr: Vec<u8>,
}

fn join_reader_result(
    result: std::result::Result<Result<Vec<u8>>, tokio::task::JoinError>,
    command_kind: &'static str,
    stream_name: &'static str,
) -> Result<Vec<u8>> {
    result
        .with_context(|| format!("{command_kind} {stream_name} reader task panicked"))?
        .with_context(|| format!("failed to read {stream_name} from {command_kind}"))
}

pub(super) async fn join_reader_task(
    reader: &mut Option<JoinHandle<Result<Vec<u8>>>>,
    command_kind: &'static str,
    stream_name: &'static str,
) -> Result<Vec<u8>> {
    let Some(reader) = reader.as_mut() else {
        bail!("{command_kind} {stream_name} reader task was unavailable");
    };
    join_reader_result(reader.await, command_kind, stream_name)
}
