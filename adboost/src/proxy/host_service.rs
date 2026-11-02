//! Crate-internal smart-socket host-service framing.
//!
//! The adb server's host protocol is a sequence of `%04x`-length-prefixed
//! strings. The one-shot data queries answer `OKAY` plus one framed payload;
//! the streaming tracker services answer `OKAY` and then one framed snapshot
//! per device-set change. This module owns that byte-level contract so
//! [`crate::proxy::TCPProxyTransport`] and [`crate::tracker`] do not each
//! re-derive it.
//!
//! `FrameDecoder` is deliberately stateful and owns all partial-frame progress.
//! Dropping a [`FrameDecoder::next_frame`] future mid-frame must not lose
//! bytes; every `read()` is cancel-safe and the state survives cancellation.

use std::io;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::RustADBError;

const STATUS_LEN: usize = 4;
const LENGTH_LEN: usize = 4;

/// The status line the adb server sends before a host-service reply.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum HostStatus {
    Okay,
    Fail(String),
}

/// A host-protocol framing failure.
#[derive(Debug, thiserror::Error)]
pub(crate) enum HostServiceError {
    /// The underlying socket failed.
    #[error(transparent)]
    Io(#[from] io::Error),
    /// The peer sent bytes that do not match the host protocol.
    #[error("host service protocol error: {0}")]
    Protocol(String),
}

impl From<HostServiceError> for RustADBError {
    fn from(error: HostServiceError) -> Self {
        match error {
            HostServiceError::Io(error) => Self::IOError(error),
            HostServiceError::Protocol(detail) => Self::ADBRequestFailed(detail),
        }
    }
}

/// Write one host-service request (`%04x` length + service name).
///
/// The length prefix is four hex digits on the wire, so a service name longer
/// than `u16::MAX` cannot be represented; it is rejected rather than silently
/// emitting a malformed request.
pub(crate) async fn send_request<W>(writer: &mut W, service: &str) -> Result<(), HostServiceError>
where
    W: AsyncWrite + Unpin,
{
    let length = u16::try_from(service.len()).map_err(|_| {
        HostServiceError::Protocol(format!(
            "host service name is too long ({} bytes)",
            service.len()
        ))
    })?;
    let mut request = Vec::with_capacity(LENGTH_LEN + service.len());
    request.extend_from_slice(format!("{length:04x}").as_bytes());
    request.extend_from_slice(service.as_bytes());
    writer.write_all(&request).await?;
    Ok(())
}

/// Read the `OKAY`/`FAIL` status line.
///
/// A `FAIL` reason is itself one framed string, so the connection is left at a
/// clean request boundary and can be reused (the existing proxy transport does
/// exactly that).
pub(crate) async fn read_status<R>(reader: &mut R) -> Result<HostStatus, HostServiceError>
where
    R: AsyncRead + Unpin,
{
    let mut status = [0u8; STATUS_LEN];
    reader.read_exact(&mut status).await?;

    match &status {
        b"OKAY" => Ok(HostStatus::Okay),
        b"FAIL" => {
            let mut decoder = FrameDecoder::new();
            let reason = decoder.next_frame(reader).await?.ok_or_else(|| {
                HostServiceError::Protocol("connection closed before the FAIL reason".to_string())
            })?;
            Ok(HostStatus::Fail(
                String::from_utf8_lossy(&reason).into_owned(),
            ))
        }
        other => Err(HostServiceError::Protocol(format!(
            "unexpected host service status {:?}",
            String::from_utf8_lossy(other)
        ))),
    }
}

/// Cancel-safe decoder for the host protocol's `%04x` length-prefixed frames.
#[derive(Debug, Default)]
pub(crate) struct FrameDecoder {
    length: [u8; LENGTH_LEN],
    length_filled: usize,
    body: Vec<u8>,
    body_filled: usize,
    expected: Option<usize>,
}

impl FrameDecoder {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Read the next complete frame.
    ///
    /// `Ok(None)` means the stream ended cleanly at a frame boundary.
    /// A zero-length frame is `Ok(Some(Vec::new()))` — the tracker's
    /// authoritative empty snapshot.
    pub(crate) async fn next_frame<R>(
        &mut self,
        reader: &mut R,
    ) -> Result<Option<Vec<u8>>, HostServiceError>
    where
        R: AsyncRead + Unpin,
    {
        loop {
            if self.expected.is_none() {
                while self.length_filled < LENGTH_LEN {
                    let read = reader.read(&mut self.length[self.length_filled..]).await?;
                    if read == 0 {
                        if self.length_filled == 0 {
                            return Ok(None);
                        }
                        return Err(HostServiceError::Protocol(
                            "connection closed inside a frame length prefix".to_string(),
                        ));
                    }
                    self.length_filled += read;
                }

                let expected = parse_frame_length(self.length)?;
                self.body.clear();
                self.body.resize(expected, 0);
                self.body_filled = 0;
                self.expected = Some(expected);
            }

            let Some(expected) = self.expected else {
                continue;
            };

            while self.body_filled < expected {
                let read = reader.read(&mut self.body[self.body_filled..]).await?;
                if read == 0 {
                    return Err(HostServiceError::Protocol(format!(
                        "connection closed inside a frame body ({}/{expected} bytes)",
                        self.body_filled
                    )));
                }
                self.body_filled += read;
            }

            let frame = self.body.clone();
            self.length_filled = 0;
            self.body_filled = 0;
            self.expected = None;
            return Ok(Some(frame));
        }
    }
}

fn parse_frame_length(length: [u8; LENGTH_LEN]) -> Result<usize, HostServiceError> {
    let text = std::str::from_utf8(&length)
        .map_err(|_| HostServiceError::Protocol("frame length prefix is not ASCII".to_string()))?;
    let value = u16::from_str_radix(text, 16)
        .map_err(|_| HostServiceError::Protocol(format!("invalid frame length {text:?}")))?;
    Ok(usize::from(value))
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::pin::Pin;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::task::{Context, Poll};
    use std::time::Duration;

    use tokio::io::{AsyncRead, AsyncWriteExt, ReadBuf};
    use tokio::time::timeout;

    use super::{FrameDecoder, HostServiceError, HostStatus, read_status, send_request};

    #[tokio::test]
    async fn send_request_prefixes_the_four_hex_length() {
        let mut output = Vec::new();
        send_request(&mut output, "host:host-features")
            .await
            .expect("writing to a Vec cannot fail");
        assert_eq!(
            String::from_utf8(output).expect("request is ASCII"),
            "0012host:host-features",
            "host-service request framing changed"
        );
    }

    #[tokio::test]
    async fn read_status_distinguishes_okay_and_fail() {
        let (mut writer, mut reader) = tokio::io::duplex(64);
        writer
            .write_all(b"OKAY")
            .await
            .expect("duplex write cannot fail");
        assert_eq!(
            read_status(&mut reader).await.expect("status read"),
            HostStatus::Okay
        );

        writer
            .write_all(b"FAIL0009bad thing")
            .await
            .expect("duplex write cannot fail");
        assert_eq!(
            read_status(&mut reader).await.expect("status read"),
            HostStatus::Fail("bad thing".to_string())
        );
    }

    #[tokio::test]
    async fn frame_decoder_returns_complete_and_empty_frames() {
        let (mut writer, mut reader) = tokio::io::duplex(64);
        writer
            .write_all(b"0005hello0000")
            .await
            .expect("duplex write cannot fail");
        drop(writer);

        let mut decoder = FrameDecoder::new();
        assert_eq!(
            decoder.next_frame(&mut reader).await.expect("frame"),
            Some(b"hello".to_vec())
        );
        assert_eq!(
            decoder.next_frame(&mut reader).await.expect("frame"),
            Some(Vec::new())
        );
        assert_eq!(
            decoder.next_frame(&mut reader).await.expect("eof"),
            None,
            "a clean EOF at a frame boundary is not an error"
        );
    }

    #[tokio::test]
    async fn frame_decoder_rejects_truncated_frames() {
        let (mut writer, mut reader) = tokio::io::duplex(64);
        writer
            .write_all(b"0005he")
            .await
            .expect("duplex write cannot fail");
        drop(writer);

        let mut decoder = FrameDecoder::new();
        let error = decoder
            .next_frame(&mut reader)
            .await
            .expect_err("a partial body is fatal");
        assert!(
            matches!(error, HostServiceError::Protocol(_)),
            "truncated body must be a protocol error: {error:?}"
        );
    }

    #[tokio::test]
    async fn frame_decoder_survives_cancellation_mid_frame() {
        let body = b"hello world";
        let mut first_half = Vec::new();
        first_half.extend_from_slice(format!("{:04x}", body.len()).as_bytes());
        first_half.extend_from_slice(&body[..5]);

        let gate = Arc::new(AtomicBool::new(false));
        let mut reader = ScriptedReader::new(
            vec![
                ScriptedChunk::Data(first_half),
                ScriptedChunk::Pending,
                ScriptedChunk::Data(body[5..].to_vec()),
                ScriptedChunk::Eof,
            ],
            Arc::clone(&gate),
        );
        let mut decoder = FrameDecoder::new();

        // The first call consumes the length and half the body, then parks on
        // the Pending chunk. Dropping the timed-out future must leave the
        // partial frame in the decoder.
        let first_attempt =
            timeout(Duration::from_millis(20), decoder.next_frame(&mut reader)).await;
        assert!(
            first_attempt.is_err(),
            "the scripted reader should have parked the decode: {first_attempt:?}"
        );

        // Release the parked read; the decoder must resume at the exact byte
        // where it stopped rather than re-reading the frame from the start.
        gate.store(true, Ordering::SeqCst);
        let frame = decoder
            .next_frame(&mut reader)
            .await
            .expect("the resumed decode must not lose bytes");
        assert_eq!(frame, Some(body.to_vec()));
    }

    enum ScriptedChunk {
        Data(Vec<u8>),
        Pending,
        Eof,
    }

    struct ScriptedReader {
        chunks: VecDeque<ScriptedChunk>,
        current: Vec<u8>,
        position: usize,
        parked: bool,
        gate: Arc<AtomicBool>,
    }

    impl ScriptedReader {
        fn new(chunks: Vec<ScriptedChunk>, gate: Arc<AtomicBool>) -> Self {
            Self {
                chunks: chunks.into(),
                current: Vec::new(),
                position: 0,
                parked: false,
                gate,
            }
        }
    }

    impl AsyncRead for ScriptedReader {
        fn poll_read(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &mut ReadBuf<'_>,
        ) -> Poll<std::io::Result<()>> {
            let this = self.get_mut();
            loop {
                if this.parked {
                    if !this.gate.load(Ordering::SeqCst) {
                        return Poll::Pending;
                    }
                    this.parked = false;
                }

                if this.position < this.current.len() {
                    let available = this.current.len() - this.position;
                    let take = available.min(buf.remaining());
                    buf.put_slice(&this.current[this.position..this.position + take]);
                    this.position += take;
                    return Poll::Ready(Ok(()));
                }

                match this.chunks.pop_front() {
                    Some(ScriptedChunk::Data(data)) => {
                        this.current = data;
                        this.position = 0;
                    }
                    Some(ScriptedChunk::Pending) => {
                        this.parked = true;
                    }
                    Some(ScriptedChunk::Eof) | None => return Poll::Ready(Ok(())),
                }
            }
        }
    }
}
