use std::pin::Pin;
use std::task::{Context, Poll};

use tokio::io::AsyncWrite;

use crate::{ADBDeviceExt, Result, proxy::ADBProxyDevice};

/// `AsyncWrite` adapter that forwards only complete (newline-terminated) lines
/// to the wrapped writer, buffering any trailing partial line.
struct LogFilter<W: AsyncWrite + Unpin> {
    writer: W,
    buffer: Vec<u8>,
}

impl<W: AsyncWrite + Unpin> LogFilter<W> {
    pub const fn new(writer: W) -> Self {
        Self {
            writer,
            buffer: Vec::new(),
        }
    }
}

impl<W: AsyncWrite + Unpin> AsyncWrite for LogFilter<W> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        let this = self.get_mut();

        // Add newly received bytes to the internal buffer
        this.buffer.extend_from_slice(buf);

        // Find the end of the last complete line; only flush complete lines.
        let flush_until = this
            .buffer
            .iter()
            .rposition(|&b| b == b'\n')
            .map(|pos| pos + 1);

        if let Some(end) = flush_until {
            let mut written = 0;
            while written < end {
                match Pin::new(&mut this.writer).poll_write(cx, &this.buffer[written..end]) {
                    Poll::Ready(Ok(0)) => break,
                    Poll::Ready(Ok(n)) => written += n,
                    Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                    Poll::Pending => return Poll::Pending,
                }
            }
            this.buffer.drain(..written);
        }

        Poll::Ready(Ok(buf.len()))
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().writer).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().writer).poll_shutdown(cx)
    }
}

impl ADBProxyDevice {
    /// Get logs from device
    pub async fn get_logs<W: AsyncWrite + Unpin + Send>(&mut self, output: W) -> Result<()> {
        let mut filter = LogFilter::new(output);
        let _status = self
            .shell_command(&"exec logcat", Some(&mut filter), None)
            .await;
        Ok(())
    }
}
