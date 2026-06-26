//! Transport-generic shell-v2 session: writable stdin, streaming reads, and a
//! back-compat buffer-until-exit `execute()`.
//!
//! The session is generic over a split read half (`R: AsyncRead`) and write half
//! (`W: AsyncWrite`) so the *same* framing logic serves both transports:
//!
//! - **USB**: `R`/`W` are [`SessionReadHalf`] / [`SessionWriteHalf`] from
//!   [`MultiplexedSession::into_split`]. Dropping the session drops both halves,
//!   which fires the underlying stream's CLSE (see `into_split`'s contract).
//! - **proxy**: `R`/`W` are the read/write halves of a `tokio::io::split` over the
//!   TCP `RawConnection`. Dropping the session closes the socket → device EOF.
//!
//! Both directions ride the shared [`shell_v2_codec`](crate::message_devices::shell_v2_codec)
//! framing. The host→device control frames (`write_stdin` / `close_stdin`) are
//! how a v2 shell gets stdin and an EOF signal — and, with a PTY-allocated
//! session, how a host-side close turns into a device-side `SIGHUP`.
//!
//! [`SessionReadHalf`]: crate::message_devices::usb::SessionReadHalf
//! [`SessionWriteHalf`]: crate::message_devices::usb::SessionWriteHalf
//! [`MultiplexedSession::into_split`]: crate::message_devices::usb::MultiplexedSession::into_split

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::Result;
use crate::RustADBError;
use crate::message_devices::shell_v2_codec::{
    FrameHeader, HEADER_LEN, ShellChannel, decode_header, encode,
};

/// A single decoded inbound shell-v2 frame: its channel and full payload.
///
/// Returned by [`ShellV2Session::read_frame`] for streaming consumers that want
/// each stdout/stderr chunk and the exit status as it arrives (rather than the
/// buffer-until-exit [`ShellV2Session::execute`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShellFrame {
    /// Which logical channel the payload arrived on.
    pub channel: ShellChannel,
    /// The frame payload bytes (already fully read off the stream).
    pub payload: Vec<u8>,
}

impl ShellFrame {
    /// The exit code, if this is an exit-status frame.
    #[must_use]
    pub fn exit_code(&self) -> Option<u8> {
        match self.channel {
            ShellChannel::ExitStatus => self.payload.first().copied(),
            _ => None,
        }
    }
}

/// The outcome of a fully-drained shell-v2 session: separated stdout/stderr and
/// the device-reported exit code (if the device sent an exit-status frame).
#[derive(Debug, Default, PartialEq, Eq)]
pub struct ShellV2Output {
    /// Bytes received on the stdout channel (`id=1`).
    pub stdout: Vec<u8>,
    /// Bytes received on the stderr channel (`id=2`).
    pub stderr: Vec<u8>,
    /// The exit code from the exit-status frame (`id=3`), if one arrived before
    /// the stream closed.
    pub exit_code: Option<u8>,
}

/// A transport-generic shell-v2 session.
///
/// Built over a split read/write pair (see the module docs). Offers three
/// surfaces:
///
/// - **streaming**: [`read_frame`](Self::read_frame) yields one [`ShellFrame`]
///   at a time and never buffers to exit, so a consumer can react to output and
///   cancel mid-stream (by dropping the session) without a panic.
/// - **writable**: [`write_stdin`](Self::write_stdin) /
///   [`close_stdin`](Self::close_stdin) drive the host→device direction.
/// - **back-compat**: [`execute`](Self::execute) drains every frame and returns
///   the separated stdout/stderr + exit code (what probe/echo callers use).
pub struct ShellV2Session<R, W> {
    reader: R,
    writer: W,
}

impl<R, W> ShellV2Session<R, W>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    /// Wrap a split read/write pair as a shell-v2 session.
    #[must_use]
    pub fn new(reader: R, writer: W) -> Self {
        Self { reader, writer }
    }

    /// Write a stdin (`id=0`) frame to the device.
    ///
    /// # Errors
    ///
    /// Returns [`RustADBError::IOError`] if the underlying write half errors.
    pub async fn write_stdin(&mut self, data: &[u8]) -> Result<()> {
        self.write_frame(ShellChannel::Stdin, data).await
    }

    /// Send a close-stdin (`id=4`) frame: the device reads EOF on its stdin.
    ///
    /// For a non-PTY shell this is the clean way to let a command that reads to
    /// EOF (e.g. `cat`) finish and flush; for a PTY-allocated shell, combined
    /// with a session close it is what triggers the device-side `SIGHUP`.
    ///
    /// # Errors
    ///
    /// Returns [`RustADBError::IOError`] if the underlying write half errors.
    pub async fn close_stdin(&mut self) -> Result<()> {
        self.write_frame(ShellChannel::CloseStdin, &[]).await
    }

    /// Encode and write one host→device frame, flushing it out.
    async fn write_frame(&mut self, channel: ShellChannel, payload: &[u8]) -> Result<()> {
        let frame = encode(channel, payload);
        self.writer.write_all(&frame).await?;
        self.writer.flush().await?;
        Ok(())
    }

    /// Read the next inbound frame, or `None` if the stream closed cleanly at a
    /// frame boundary.
    ///
    /// This is the streaming primitive: it reads exactly one frame's worth of
    /// bytes and returns. A consumer that stops calling it (or drops the whole
    /// session) cancels the shell — there is no buffer-until-exit.
    ///
    /// # Errors
    ///
    /// Returns [`RustADBError::ADBShellV2ParseError`] on a malformed frame
    /// (invalid channel id, a spurious exit-status payload size, or a stream
    /// that closes mid-frame) or [`RustADBError::IOError`] on a transport error.
    pub async fn read_frame(&mut self) -> Result<Option<ShellFrame>> {
        let mut header = [0u8; HEADER_LEN];
        if !self.read_exact_or_eof(&mut header).await? {
            // Clean EOF between frames.
            return Ok(None);
        }
        let FrameHeader {
            channel,
            payload_len,
        } = decode_header(header)?;

        if channel == ShellChannel::ExitStatus && payload_len != 1 {
            return Err(RustADBError::ADBShellV2ParseError(format!(
                "spurious exit-status frame with payload size {payload_len} (should be 1)"
            )));
        }

        let mut payload = vec![0u8; payload_len];
        if payload_len > 0 && !self.read_exact_or_eof(&mut payload).await? {
            return Err(RustADBError::ADBShellV2ParseError(
                "shell-v2 stream closed mid-frame".into(),
            ));
        }

        tracing::trace!("shell-v2 frame channel={channel:?} payload_len={payload_len}");
        Ok(Some(ShellFrame { channel, payload }))
    }

    /// Run the command to completion, draining every frame, and return the
    /// separated stdout/stderr plus exit code.
    ///
    /// Back-compat surface for callers (probe/echo) that just want the final
    /// output; built on top of [`read_frame`](Self::read_frame).
    ///
    /// # Errors
    ///
    /// Propagates any error from [`read_frame`](Self::read_frame).
    pub async fn execute(&mut self) -> Result<ShellV2Output> {
        let mut out = ShellV2Output::default();
        while let Some(frame) = self.read_frame().await? {
            match frame.channel {
                ShellChannel::Stdout => out.stdout.extend_from_slice(&frame.payload),
                ShellChannel::Stderr => out.stderr.extend_from_slice(&frame.payload),
                ShellChannel::ExitStatus => {
                    // read_frame already validated the 1-byte payload.
                    out.exit_code = frame.payload.first().copied();
                }
                // stdin / close-stdin / window-size are host→device (or
                // interactive) ids never expected inbound; ignore so a stray
                // frame does not corrupt the captured output.
                ShellChannel::Stdin | ShellChannel::CloseStdin | ShellChannel::WindowSizeChange => {
                }
            }
        }
        Ok(out)
    }

    /// Read exactly `buf.len()` bytes from the read half.
    ///
    /// Returns `Ok(true)` if `buf` was filled, `Ok(false)` if the stream closed
    /// cleanly with *no* bytes read (a frame boundary EOF). Closing partway
    /// through `buf` returns `Ok(false)` too — the caller turns a partial read
    /// into the appropriate "closed mid-frame" / "mid-header" error so the
    /// distinction stays at the framing layer.
    ///
    /// A split half's `read` returns whatever a single transport frame delivered,
    /// so one shell frame may span several reads — loop until full or EOF.
    async fn read_exact_or_eof(&mut self, buf: &mut [u8]) -> Result<bool> {
        let mut filled = 0;
        while filled < buf.len() {
            let n = self.reader.read(&mut buf[filled..]).await?;
            if n == 0 {
                return Ok(filled == buf.len());
            }
            filled += n;
        }
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;

    /// Build a stream of frames via the shared codec.
    fn stream(frames: &[(ShellChannel, &[u8])]) -> Vec<u8> {
        let mut v = Vec::new();
        for (ch, payload) in frames {
            v.extend_from_slice(&encode(*ch, payload));
        }
        v
    }

    /// A session over an in-memory reader and a `Vec` sink, for unit testing the
    /// framing without a real transport.
    fn session(input: Vec<u8>) -> ShellV2Session<Cursor<Vec<u8>>, Vec<u8>> {
        ShellV2Session::new(Cursor::new(input), Vec::new())
    }

    #[tokio::test]
    async fn execute_routes_stdout_stderr_and_exit() {
        let input = stream(&[
            (ShellChannel::Stdout, b"hello "),
            (ShellChannel::Stderr, b"warn"),
            (ShellChannel::Stdout, b"world"),
            (ShellChannel::ExitStatus, &[42]),
        ]);
        let out = session(input).execute().await.expect("execute");
        assert_eq!(out.stdout, b"hello world", "stdout frames must concatenate");
        assert_eq!(out.stderr, b"warn", "stderr must route separately");
        assert_eq!(out.exit_code, Some(42), "exit-status frame yields the code");
    }

    #[tokio::test]
    async fn read_frame_streams_incrementally_then_eof() {
        let input = stream(&[
            (ShellChannel::Stdout, b"a"),
            (ShellChannel::ExitStatus, &[0]),
        ]);
        let mut s = session(input);
        let f1 = s.read_frame().await.expect("frame 1").expect("some");
        assert_eq!(f1.channel, ShellChannel::Stdout, "first frame is stdout");
        assert_eq!(f1.payload, b"a");
        let f2 = s.read_frame().await.expect("frame 2").expect("some");
        assert_eq!(f2.exit_code(), Some(0), "second frame carries exit code 0");
        assert!(
            s.read_frame().await.expect("eof").is_none(),
            "a clean boundary EOF must yield None, not an error"
        );
    }

    #[tokio::test]
    async fn dropping_mid_stream_does_not_panic() {
        // Only the first of two frames is consumed, then the session is dropped
        // before the exit frame — the cancel path must not panic.
        let input = stream(&[
            (ShellChannel::Stdout, b"partial"),
            (ShellChannel::ExitStatus, &[0]),
        ]);
        let mut s = session(input);
        let f = s.read_frame().await.expect("frame").expect("some");
        assert_eq!(f.payload, b"partial");
        drop(s); // mid-stream cancel
    }

    #[tokio::test]
    async fn write_stdin_and_close_stdin_encode_correct_frames() {
        let mut s = session(Vec::new());
        s.write_stdin(b"abc").await.expect("write stdin");
        s.close_stdin().await.expect("close stdin");
        // Inspect what was written to the sink half.
        let written = s.writer;
        let mut expected = encode(ShellChannel::Stdin, b"abc");
        expected.extend_from_slice(&encode(ShellChannel::CloseStdin, &[]));
        assert_eq!(
            written, expected,
            "write_stdin then close_stdin must emit id=0 then id=4 frames"
        );
    }

    #[tokio::test]
    async fn spurious_exit_status_length_is_parse_error() {
        // exit-status (id=3) with a 4-byte payload is malformed.
        let input = encode(ShellChannel::ExitStatus, &[0, 0, 0, 0]);
        let err = session(input)
            .read_frame()
            .await
            .expect_err("a 4-byte exit-status frame must be rejected");
        assert!(
            matches!(err, RustADBError::ADBShellV2ParseError(_)),
            "got {err:?}"
        );
    }

    #[tokio::test]
    async fn frame_split_across_reads_reassembles() {
        // A reader that hands out one byte per read, to prove a frame spanning
        // many reads still reassembles.
        struct OneByte {
            data: Vec<u8>,
            pos: usize,
        }
        impl AsyncRead for OneByte {
            fn poll_read(
                mut self: std::pin::Pin<&mut Self>,
                _cx: &mut std::task::Context<'_>,
                buf: &mut tokio::io::ReadBuf<'_>,
            ) -> std::task::Poll<std::io::Result<()>> {
                if self.pos < self.data.len() && buf.remaining() > 0 {
                    let b = self.data[self.pos];
                    self.pos += 1;
                    buf.put_slice(&[b]);
                }
                std::task::Poll::Ready(Ok(()))
            }
        }
        let input = stream(&[
            (ShellChannel::Stdout, b"reassemble-me"),
            (ShellChannel::ExitStatus, &[7]),
        ]);
        let mut s = ShellV2Session::new(
            OneByte {
                data: input,
                pos: 0,
            },
            Vec::new(),
        );
        let out = s.execute().await.expect("decode across 1-byte reads");
        assert_eq!(out.stdout, b"reassemble-me", "stdout reassembles");
        assert_eq!(out.exit_code, Some(7), "exit code decodes across reads");
    }
}
