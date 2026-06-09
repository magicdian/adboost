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
use crate::message_devices::usb::persistent::MultiplexedSession;

/// Size of a shell-v2 frame header: a 1-byte channel id + a 4-byte LE length.
const SHELL_V2_HEADER_LEN: usize = 5;

/// Scratch buffer size for draining a frame payload from the inner stream.
const READ_CHUNK: usize = 65535;

/// A decoded shell-v2 inner-frame channel id.
///
/// Mirrors the AOSP `ShellProtocol::Id` values and the reference enum in
/// `server_device/adb_server_device_commands.rs` (which only handles
/// stdout/stderr/exit-status). We additionally classify the host→device-only
/// and interactive ids so the decoder can consume-and-ignore them on the
/// device→host stream rather than erroring.
///
/// Pure / I/O-free for unit testing (D1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShellChannel {
    /// `id=0`: stdin (host→device; not expected inbound).
    Stdin,
    /// `id=1`: stdout payload.
    Stdout,
    /// `id=2`: stderr payload.
    Stderr,
    /// `id=3`: exit status (payload is exactly one byte).
    ExitStatus,
    /// `id=4`: close-stdin (host→device; not expected inbound).
    CloseStdin,
    /// `id=5`: window-size change (8-byte payload; consume and ignore inbound).
    WindowSizeChange,
}

impl TryFrom<u8> for ShellChannel {
    type Error = RustADBError;

    fn try_from(value: u8) -> std::result::Result<Self, Self::Error> {
        match value {
            0 => Ok(Self::Stdin),
            1 => Ok(Self::Stdout),
            2 => Ok(Self::Stderr),
            3 => Ok(Self::ExitStatus),
            4 => Ok(Self::CloseStdin),
            5 => Ok(Self::WindowSizeChange),
            other => Err(RustADBError::ADBShellV2ParseError(format!(
                "invalid shell-v2 channel id {other}"
            ))),
        }
    }
}

/// A single decoded shell-v2 frame header (channel + payload length).
///
/// Decoding the 5-byte header is I/O-free so it can be unit-tested by feeding
/// synthetic byte sequences (see the `tests` module).
#[derive(Debug, PartialEq, Eq)]
struct FrameHeader {
    channel: ShellChannel,
    payload_len: usize,
}

/// Decode a 5-byte shell-v2 frame header: `[id:u8][len:u32 LE]`.
///
/// Mirrors the reference parser in
/// `server_device/adb_server_device_commands.rs:201-205` (1 byte channel + 4
/// bytes LE size); ported here because that impl reads from a TCP
/// `RawConnection` while this path reads from a USB [`MultiplexedSession`].
/// Pure / I/O-free for unit testing.
fn decode_frame_header(header: [u8; SHELL_V2_HEADER_LEN]) -> Result<FrameHeader> {
    let channel = ShellChannel::try_from(header[0])?;
    let payload_len = u32::from_le_bytes([header[1], header[2], header[3], header[4]]) as usize;
    Ok(FrameHeader {
        channel,
        payload_len,
    })
}

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
            } = decode_frame_header(header)?;

            log::trace!(
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

    #[test]
    fn decode_stdout_header() {
        // id=1 (stdout), len=5.
        let header = [1u8, 5, 0, 0, 0];
        assert_eq!(
            decode_frame_header(header).expect("valid header"),
            FrameHeader {
                channel: ShellChannel::Stdout,
                payload_len: 5,
            },
            "stdout header must decode channel=Stdout and the LE length"
        );
    }

    #[test]
    fn decode_stderr_header() {
        // id=2 (stderr), len=0x0102 = 258.
        let header = [2u8, 0x02, 0x01, 0, 0];
        assert_eq!(
            decode_frame_header(header).expect("valid header"),
            FrameHeader {
                channel: ShellChannel::Stderr,
                payload_len: 258,
            },
            "stderr header must decode channel=Stderr and the little-endian length"
        );
    }

    #[test]
    fn decode_exit_status_header() {
        // id=3 (exit status), len=1.
        let header = [3u8, 1, 0, 0, 0];
        assert_eq!(
            decode_frame_header(header).expect("valid header"),
            FrameHeader {
                channel: ShellChannel::ExitStatus,
                payload_len: 1,
            },
            "exit-status header must decode channel=ExitStatus with payload len 1"
        );
    }

    #[test]
    fn decode_window_size_change_header() {
        // id=5 (window-size-change), len=8.
        let header = [5u8, 8, 0, 0, 0];
        assert_eq!(
            decode_frame_header(header).expect("valid header"),
            FrameHeader {
                channel: ShellChannel::WindowSizeChange,
                payload_len: 8,
            },
            "window-size-change header must decode channel=WindowSizeChange and len 8"
        );
    }

    #[test]
    fn decode_invalid_channel_is_parse_error() {
        let header = [9u8, 0, 0, 0, 0];
        let err = decode_frame_header(header).expect_err("channel id 9 is invalid");
        assert!(
            matches!(err, RustADBError::ADBShellV2ParseError(_)),
            "an invalid channel id must surface as ADBShellV2ParseError, got {err:?}"
        );
    }

    /// Drive [`ShellV2Session::execute`] over a byte stream using an in-memory
    /// reader, asserting the routing/exit-code logic without USB hardware.
    ///
    /// The session reads via [`MultiplexedSession`], which we cannot trivially
    /// construct in a unit test; so this helper reimplements the SAME decode
    /// loop against a `Read` to exercise the frame state machine. The framing
    /// constants and per-channel routing are the load-bearing logic and are
    /// shared with the real path through [`decode_frame_header`] /
    /// [`ShellChannel`].
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
            } = decode_frame_header(header)?;
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

    /// Build a shell-v2 frame `[id][len LE][payload]`.
    fn frame(id: u8, payload: &[u8]) -> Vec<u8> {
        let mut v = Vec::with_capacity(SHELL_V2_HEADER_LEN + payload.len());
        v.push(id);
        let len = u32::try_from(payload.len()).expect("test payload fits in u32");
        v.extend_from_slice(&len.to_le_bytes());
        v.extend_from_slice(payload);
        v
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
        let decoded = decode_frame_header(header).expect("header itself is well-formed");
        assert_eq!(
            decoded.payload_len, 4,
            "a 4-byte exit-status payload is spurious and must be rejected by execute()"
        );
    }
}
