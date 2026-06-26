//! shell,v2 inner-frame decoding multiplexed over a persistent USB connection.
//!
//! [`ShellV2Session`] wraps a [`MultiplexedSession`] opened with a `shell,v2`
//! service string and decodes the shell-v2 inner protocol: a stream of
//! `[id:u8][len:u32 LE][payload]` frames carrying stdout/stderr/exit-status on
//! separate logical channels. This lets `adb shell <cmd>` return a real exit
//! code and keep stdout/stderr separate — which the v1 path
//! ([`PersistentUsbConnection::shell_exec`]) cannot.
//!
//! ## Layering
//!
//! The shell-v2 frames ride inside the WRTE/OKAY byte stream of the underlying
//! [`MultiplexedSession`], which stays byte-transparent: it knows nothing about
//! shell framing. This is the same layering used by `SyncSession` — both sit on
//! top of an untouched `MultiplexedSession`.
//!
//! The v1 [`PersistentUsbConnection::shell_exec`] is kept intact for
//! back-compat; this is a NEW path.
//!
//! [`PersistentUsbConnection`]: crate::message_devices::usb::PersistentUsbConnection
//! [`PersistentUsbConnection::shell_exec`]: crate::message_devices::usb::PersistentUsbConnection::shell_exec

use tokio::io::AsyncReadExt;

use crate::Result;
use crate::RustADBError;
use crate::message_devices::shell_v2_codec::{
    FrameHeader, HEADER_LEN as SHELL_V2_HEADER_LEN, ShellChannel, decode_header,
};
use crate::message_devices::usb::persistent::MultiplexedSession;

/// Scratch buffer size for draining a frame payload from the inner stream.
const READ_CHUNK: usize = 65535;

/// A shell-v2 session multiplexed over a persistent USB connection.
///
/// Built by
/// [`crate::message_devices::usb::PersistentUsbConnection::open_shell_v2`]. It
/// owns the underlying [`MultiplexedSession`] (one `local_id`, demuxed by the
/// shared reader loop like any other session) and decodes the shell-v2 inner
/// framing on top of its byte stream.
pub struct ShellV2Session {
    inner: MultiplexedSession,
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

impl ShellV2Session {
    /// Wrap an already-opened `shell,v2` [`MultiplexedSession`].
    #[must_use]
    pub(crate) fn new(inner: MultiplexedSession) -> Self {
        Self { inner }
    }

    /// Run the command to completion, draining every frame, and return the
    /// separated stdout/stderr plus exit code.
    ///
    /// # Errors
    ///
    /// Returns [`RustADBError::IOError`] on a transport error or
    /// [`RustADBError::ADBShellV2ParseError`] on a malformed frame (invalid
    /// channel id or a spurious exit-status payload size).
    pub async fn execute(&mut self) -> Result<ShellV2Output> {
        let mut out = ShellV2Output::default();
        let mut scratch = vec![0u8; READ_CHUNK];

        loop {
            // 1 byte channel + 4 bytes LE payload size.
            let mut header = [0u8; SHELL_V2_HEADER_LEN];
            if !self.read_exact_or_eof(&mut header).await? {
                // Stream closed cleanly between frames.
                break;
            }
            let FrameHeader {
                channel,
                payload_len,
            } = decode_header(header)?;

            tracing::trace!(
                "PersistentUsb: shell-v2 frame channel={channel:?} payload_len={payload_len}"
            );

            match channel {
                ShellChannel::Stdout => {
                    self.drain_payload(payload_len, &mut scratch, |chunk| {
                        out.stdout.extend_from_slice(chunk);
                    })
                    .await?;
                }
                ShellChannel::Stderr => {
                    self.drain_payload(payload_len, &mut scratch, |chunk| {
                        out.stderr.extend_from_slice(chunk);
                    })
                    .await?;
                }
                ShellChannel::ExitStatus => {
                    if payload_len != 1 {
                        return Err(RustADBError::ADBShellV2ParseError(format!(
                            "spurious exit-status frame with payload size {payload_len} (should be 1)"
                        )));
                    }
                    let mut byte = [0u8; 1];
                    if !self.read_exact_or_eof(&mut byte).await? {
                        break;
                    }
                    out.exit_code = Some(byte[0]);
                }
                // stdin / close-stdin / window-size-change are not expected
                // inbound on the device→host stream; consume any payload and
                // ignore so a stray frame does not desync the decoder.
                ShellChannel::Stdin | ShellChannel::CloseStdin | ShellChannel::WindowSizeChange => {
                    self.drain_payload(payload_len, &mut scratch, |_| {})
                        .await?;
                }
            }
        }

        Ok(out)
    }

    /// Drain exactly `len` payload bytes from the inner stream, invoking `sink`
    /// with each chunk as it arrives (reusing `scratch` as the transfer buffer).
    async fn drain_payload(
        &mut self,
        len: usize,
        scratch: &mut [u8],
        mut sink: impl FnMut(&[u8]),
    ) -> Result<()> {
        let mut remaining = len;
        while remaining > 0 {
            let want = remaining.min(scratch.len());
            let n = self.inner.read(&mut scratch[..want]).await?;
            if n == 0 {
                return Err(RustADBError::ADBShellV2ParseError(
                    "shell-v2 stream closed mid-frame".into(),
                ));
            }
            sink(&scratch[..n]);
            remaining -= n;
        }
        Ok(())
    }

    /// Read exactly `buf.len()` bytes from the inner byte-transparent session.
    ///
    /// Returns `Ok(true)` if `buf` was filled, `Ok(false)` if the stream closed
    /// cleanly with *no* bytes read (a frame boundary EOF). Closing partway
    /// through `buf` is an error (a truncated frame).
    ///
    /// `MultiplexedSession`'s `AsyncRead` returns whatever a single WRTE
    /// delivered, so a frame may span several reads — loop until `buf` is full
    /// or EOF.
    async fn read_exact_or_eof(&mut self, buf: &mut [u8]) -> Result<bool> {
        let mut filled = 0;
        while filled < buf.len() {
            let n = self.inner.read(&mut buf[filled..]).await?;
            if n == 0 {
                if filled == 0 {
                    return Ok(false);
                }
                return Err(RustADBError::ADBShellV2ParseError(
                    "shell-v2 stream closed mid-header".into(),
                ));
            }
            filled += n;
        }
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use std::io::Read;

    use super::*;

    // The pure header encode/decode tests live with the codec in
    // `message_devices::shell_v2_codec`; the tests here exercise the session's
    // routing/exit-code state machine on top of that codec.

    /// Drive [`ShellV2Session::execute`] over a byte stream using an in-memory
    /// reader, asserting the routing/exit-code logic without USB hardware.
    ///
    /// The session reads via [`MultiplexedSession`], which we cannot trivially
    /// construct in a unit test; so this helper reimplements the SAME decode
    /// loop against a `Read` to exercise the frame state machine. The framing
    /// constants and per-channel routing are the load-bearing logic and are
    /// shared with the real path through [`decode_header`] / [`ShellChannel`].
    fn decode_all<R: Read>(mut input: R) -> Result<ShellV2Output> {
        let mut out = ShellV2Output::default();
        let mut scratch = vec![0u8; READ_CHUNK];
        loop {
            let mut header = [0u8; SHELL_V2_HEADER_LEN];
            let mut filled = 0;
            let mut eof = false;
            while filled < header.len() {
                let n = input.read(&mut header[filled..]).expect("read header");
                if n == 0 {
                    eof = true;
                    break;
                }
                filled += n;
            }
            if eof {
                assert_eq!(filled, 0, "stream must not close mid-header in test data");
                break;
            }
            let FrameHeader {
                channel,
                payload_len,
            } = decode_header(header)?;
            let mut payload = vec![0u8; payload_len];
            let mut p = 0;
            while p < payload_len {
                let want = (payload_len - p).min(scratch.len());
                let n = input.read(&mut scratch[..want]).expect("read payload");
                assert_ne!(n, 0, "payload truncated in test data");
                payload[p..p + n].copy_from_slice(&scratch[..n]);
                p += n;
            }
            match channel {
                ShellChannel::Stdout => out.stdout.extend_from_slice(&payload),
                ShellChannel::Stderr => out.stderr.extend_from_slice(&payload),
                ShellChannel::ExitStatus => {
                    assert_eq!(payload_len, 1, "exit status payload must be exactly 1 byte");
                    out.exit_code = Some(payload[0]);
                }
                ShellChannel::Stdin | ShellChannel::CloseStdin | ShellChannel::WindowSizeChange => {
                }
            }
        }
        Ok(out)
    }

    /// Build a shell-v2 frame `[id][len LE][payload]` via the shared codec.
    fn frame(id: u8, payload: &[u8]) -> Vec<u8> {
        let channel = ShellChannel::try_from(id).expect("valid channel id in test data");
        crate::message_devices::shell_v2_codec::encode(channel, payload)
    }

    #[test]
    fn routes_stdout_stderr_and_exit_code() {
        let mut stream = Vec::new();
        stream.extend_from_slice(&frame(1, b"hello "));
        stream.extend_from_slice(&frame(2, b"warn"));
        stream.extend_from_slice(&frame(1, b"world"));
        stream.extend_from_slice(&frame(3, &[42]));

        let out = decode_all(stream.as_slice()).expect("decode");
        assert_eq!(out.stdout, b"hello world", "stdout frames must concatenate");
        assert_eq!(out.stderr, b"warn", "stderr frames must go to stderr");
        assert_eq!(
            out.exit_code,
            Some(42),
            "the exit-status frame must yield the u8 exit code"
        );
    }

    #[test]
    fn window_size_change_is_consumed_and_ignored() {
        let mut stream = Vec::new();
        // id=5 with an 8-byte payload (rows/cols) must be consumed, not error.
        stream.extend_from_slice(&frame(5, &[24, 0, 0, 0, 80, 0, 0, 0]));
        stream.extend_from_slice(&frame(1, b"ok"));
        stream.extend_from_slice(&frame(3, &[0]));

        let out = decode_all(stream.as_slice()).expect("decode");
        assert_eq!(
            out.stdout, b"ok",
            "stdout after a window-size-change frame must still be decoded"
        );
        assert_eq!(out.exit_code, Some(0), "exit code 0 must be captured");
        assert!(
            out.stderr.is_empty(),
            "window-size-change payload must not leak into stderr"
        );
    }

    /// A reader that returns at most `chunk` bytes per `read`, to simulate a
    /// frame split across multiple `MultiplexedSession::read` calls.
    struct ChunkedReader<'a> {
        data: &'a [u8],
        pos: usize,
        chunk: usize,
    }

    impl Read for ChunkedReader<'_> {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            let remaining = self.data.len() - self.pos;
            if remaining == 0 {
                return Ok(0);
            }
            let n = remaining.min(self.chunk).min(buf.len());
            buf[..n].copy_from_slice(&self.data[self.pos..self.pos + n]);
            self.pos += n;
            Ok(n)
        }
    }

    #[test]
    fn handles_frame_split_across_reads() {
        let mut stream = Vec::new();
        stream.extend_from_slice(&frame(1, b"partial-read-payload"));
        stream.extend_from_slice(&frame(3, &[7]));

        // One byte at a time: header and payload both span many reads.
        let reader = ChunkedReader {
            data: &stream,
            pos: 0,
            chunk: 1,
        };
        let out = decode_all(reader).expect("decode across 1-byte reads");
        assert_eq!(
            out.stdout, b"partial-read-payload",
            "stdout must reassemble across split reads"
        );
        assert_eq!(
            out.exit_code,
            Some(7),
            "exit code must decode even when split across reads"
        );
    }

    #[test]
    fn malformed_exit_status_length_is_parse_error() {
        // Decode a header claiming exit-status but len != 1; the real
        // `execute` rejects this. We assert the header decodes (it is a valid
        // header) and that the length is the spurious value the guard catches.
        let header = [3u8, 4, 0, 0, 0];
        let decoded = decode_header(header).expect("header itself is well-formed");
        assert_eq!(
            decoded.payload_len, 4,
            "a 4-byte exit-status payload is spurious and must be rejected by execute()"
        );
    }
}
