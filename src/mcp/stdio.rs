//! The stdio transport: spawns an `mcp_servers:` entry as a child process,
//! owns its process tree until `close()`/`Drop`, and wraps its stdout in a
//! frame-size-limited reader so an unbounded line from a misbehaving server
//! cannot grow rmcp's internal line buffer without limit. `connect` (in the
//! parent module) is the only external caller: it builds a
//! [`ManagedStdioTransport`] via [`spawn`](ManagedStdioTransport::spawn) and
//! hands it to `serve_with_timeout` alongside the HTTP transport in
//! `mcp/http_client.rs`.

use std::{future::Future, io, pin::Pin, sync::Arc};

#[cfg(windows)]
use process_wrap::tokio::JobObject;
#[cfg(unix)]
use process_wrap::tokio::ProcessGroup;
use process_wrap::tokio::{ChildWrapper, CommandWrap, KillOnDrop};
use rmcp::{
    RoleClient,
    transport::{Transport, async_rw::AsyncRwTransport},
};
use tokio::{
    io::{AsyncRead, ReadBuf},
    process::{ChildStdin, ChildStdout},
};

use super::CleanupWait;

/// Maximum bytes in one newline-delimited JSON-RPC message received from an
/// MCP stdio server.  `rmcp`'s default line buffer is unbounded, so enforce a
/// limit before it can materialize an attacker-controlled frame.
const MAX_STDIO_JSON_RPC_FRAME_BYTES: usize = 16 * 1024 * 1024;

/// Stdio transport with an explicit process-tree owner.  rmcp's public
/// `TokioChildProcess` intentionally keeps the child wrapper private, so it
/// cannot be awaited by a cache eviction after the service is dropped.  This
/// small equivalent keeps the wrapped `ChildWrapper` until `close()` and
/// invokes its group/job-aware `kill()` before reporting cleanup complete.
pub(super) struct ManagedStdioTransport {
    transport: AsyncRwTransport<RoleClient, FrameLimitedReader<ChildStdout>, ChildStdin>,
    child: Option<Box<dyn ChildWrapper>>,
    cleanup: Arc<CleanupWait>,
}

/// An `AsyncRead` adapter that rejects a newline-delimited frame before the
/// unbounded `rmcp::transport::async_rw` line buffer can grow past the safety
/// limit.  Reading into a small scratch buffer also means a large underlying
/// read cannot bypass the accounting or force a large temporary allocation.
struct FrameLimitedReader<R> {
    inner: R,
    frame_bytes: usize,
    limit: usize,
}

impl<R> FrameLimitedReader<R> {
    fn new(inner: R) -> Self {
        Self::with_limit(inner, MAX_STDIO_JSON_RPC_FRAME_BYTES)
    }

    fn with_limit(inner: R, limit: usize) -> Self {
        Self {
            inner,
            frame_bytes: 0,
            limit,
        }
    }
}

impl<R> AsyncRead for FrameLimitedReader<R>
where
    R: AsyncRead + Unpin,
{
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        let destination = buf.initialize_unfilled();
        if destination.is_empty() {
            return std::task::Poll::Ready(Ok(()));
        }

        // `BufReader` normally asks for a few KiB. Keep this bound independent
        // of the caller so a future implementation cannot request an enormous
        // scratch buffer from an untrusted stream.
        const READ_CHUNK_BYTES: usize = 16 * 1024;
        let read_len = destination.len().min(READ_CHUNK_BYTES);
        let mut scratch = [0u8; READ_CHUNK_BYTES];
        let mut read_buf = ReadBuf::new(&mut scratch[..read_len]);
        match Pin::new(&mut self.inner).poll_read(cx, &mut read_buf) {
            std::task::Poll::Pending => std::task::Poll::Pending,
            std::task::Poll::Ready(Err(error)) => std::task::Poll::Ready(Err(error)),
            std::task::Poll::Ready(Ok(())) => {
                let bytes = read_buf.filled();
                for &byte in bytes {
                    if byte == b'\n' {
                        self.frame_bytes = 0;
                    } else {
                        self.frame_bytes = match self.frame_bytes.checked_add(1) {
                            Some(frame_bytes) if frame_bytes <= self.limit => frame_bytes,
                            _ => {
                                return std::task::Poll::Ready(Err(io::Error::new(
                                    io::ErrorKind::InvalidData,
                                    format!(
                                        "MCP stdio JSON-RPC frame exceeds {} bytes",
                                        self.limit
                                    ),
                                )));
                            }
                        };
                    }
                }
                buf.put_slice(bytes);
                std::task::Poll::Ready(Ok(()))
            }
        }
    }
}

impl ManagedStdioTransport {
    pub(super) async fn spawn(
        mut command: CommandWrap,
    ) -> std::io::Result<(Self, Arc<CleanupWait>)> {
        let mut child = command.spawn()?;
        let stdout = match child.stdout().take() {
            Some(stdout) => stdout,
            None => {
                let _ = Box::into_pin(child.kill()).await;
                return Err(std::io::Error::other("MCP child stdout was not piped"));
            }
        };
        let stdin = match child.stdin().take() {
            Some(stdin) => stdin,
            None => {
                let _ = Box::into_pin(child.kill()).await;
                return Err(std::io::Error::other("MCP child stdin was not piped"));
            }
        };
        let cleanup = CleanupWait::new();
        let transport = Self {
            transport: AsyncRwTransport::new(FrameLimitedReader::new(stdout), stdin),
            child: Some(child),
            cleanup: Arc::clone(&cleanup),
        };
        Ok((transport, cleanup))
    }

    async fn close_child(&mut self) -> std::io::Result<()> {
        let result = match self.child.take() {
            Some(mut child) => Box::into_pin(child.kill()).await,
            None => Ok(()),
        };
        self.cleanup.finish();
        result.map(|_| ())
    }
}

impl Drop for ManagedStdioTransport {
    fn drop(&mut self) {
        let Some(mut child) = self.child.take() else {
            self.cleanup.finish();
            return;
        };
        let cleanup = Arc::clone(&self.cleanup);
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                let _ = Box::into_pin(child.kill()).await;
                cleanup.finish();
            });
        } else {
            // This transport is normally dropped by rmcp on a Tokio runtime.
            // A synchronous fallback still sends the tree-wide termination
            // signal if a caller drops it outside that runtime.
            let _ = child.start_kill();
            cleanup.finish();
        }
    }
}

impl Transport<RoleClient> for ManagedStdioTransport {
    type Error = std::io::Error;

    fn send(
        &mut self,
        item: rmcp::service::TxJsonRpcMessage<RoleClient>,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send + 'static {
        self.transport.send(item)
    }

    fn receive(
        &mut self,
    ) -> impl Future<Output = Option<rmcp::service::RxJsonRpcMessage<RoleClient>>> + Send {
        self.transport.receive()
    }

    async fn close(&mut self) -> Result<(), Self::Error> {
        // Close stdin first so a cooperative MCP server may exit, then
        // kill the entire owned tree and await its reaping.  Even if the
        // pipe close reports an error, process cleanup must still happen.
        let transport_result = self.transport.close().await;
        let child_result = self.close_child().await;
        transport_result.and(child_result)
    }
}

pub(super) fn owned_process_command(command: tokio::process::Command) -> CommandWrap {
    let mut command = CommandWrap::from(command);
    // `JobObject` starts Windows children suspended while it attaches the
    // process to the job. If a later wrapper hook fails, Tokio must still kill
    // that suspended process when the temporary child is dropped.
    command.wrap(KillOnDrop);
    #[cfg(unix)]
    command.wrap(ProcessGroup::leader());
    #[cfg(windows)]
    command.wrap(JobObject);
    command
}

#[cfg(test)]
mod tests {
    use super::FrameLimitedReader;
    use tokio::io::AsyncReadExt;

    #[tokio::test]
    async fn stdio_frame_limit_rejects_an_oversized_line() {
        let input = b"12345\n".as_slice();
        let mut reader = FrameLimitedReader::with_limit(input, 4);
        let mut output = Vec::new();

        let error = reader
            .read_to_end(&mut output)
            .await
            .expect_err("an oversized MCP frame must fail");

        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("exceeds 4 bytes"));
    }

    #[tokio::test]
    async fn stdio_frame_limit_resets_at_each_newline() {
        let input = b"1234\n5678\n".as_slice();
        let mut reader = FrameLimitedReader::with_limit(input, 4);
        let mut output = Vec::new();

        reader
            .read_to_end(&mut output)
            .await
            .expect("separate frames at the limit should succeed");

        assert_eq!(output, input);
    }
}
