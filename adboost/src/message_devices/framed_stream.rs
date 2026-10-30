//! Transport-neutral async driver for ADB frames over a split byte stream.
//!
//! [`FrameReadBuffer`](crate::message_devices::framed_read::FrameReadBuffer) owns
//! the sans-io part of the framing contract (header decode, `data_length` bound,
//! payload assembly, magic integrity, and the "never consume a partial frame"
//! invariant). This module owns the async part, shared by every transport that is
//! reachable as `AsyncRead + AsyncWrite`:
//!
//! - [`FrameReader`] feeds one chunk read at a time into that buffer, so a
//!   cancelled (timed-out) read can never lose bytes.
//! - [`write_frame_atomic`] implements the frame-atomic write contract (a
//!   recoverable start-gate timeout vs. a fatal truncation).
//! - [`FramedStream`] wires the two halves behind **independent** locks and owns
//!   the write-poisoning policy, so a transport cannot accidentally share one
//!   lock between reader and writer (which serializes an interactive shell behind
//!   the reader's idle timeout) or forget to poison a truncated write.
//!
//! `TcpTransport` and `StreamTransport` differ only in how they *build* a
//! connection and whether they can upgrade it (TLS); the framing invariants live
//! here, in one file.

use std::{sync::Arc, time::Duration};

use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadHalf, WriteHalf},
    sync::Mutex,
};

use crate::{
    Result, RustADBError,
    message_devices::{adb_transport_message::ADBTransportMessage, framed_read::FrameReadBuffer},
};

/// Size of one stream read issued while assembling a frame. The read fills as
/// much of this scratch buffer as the transport has available; whatever arrives
/// is appended to the persistent [`FrameReadBuffer`], so the exact value only
/// trades syscall count against transient memory and never affects correctness.
pub(crate) const READ_CHUNK_LEN: usize = 64 * 1024;

/// Once a first byte of a frame has reached the wire it MUST be finished — a
/// partially-written frame is unrecoverable for the framed peer. This is the
/// generous upper bound for draining the rest of a started frame; far longer than
/// any real flush on a healthy link, so normal backpressure never trips it.
/// Exceeding it means the peer is genuinely wedged → fatal (see
/// [`write_frame_atomic`]).
pub(crate) const WRITE_COMPLETION_TIMEOUT: Duration = Duration::from_secs(10);

/// The transport-neutral "this endpoint has no live connection" error.
pub(crate) fn not_connected() -> RustADBError {
    RustADBError::IOError(std::io::Error::new(
        std::io::ErrorKind::NotConnected,
        "not connected",
    ))
}

/// A frame reader: the stream read half plus its persistent, cancel-safe
/// accumulation buffer.
///
/// The buffer is the whole point. `read_exact` is **not cancel-safe**: on a
/// timeout the future is dropped and the bytes it had already moved into the
/// call-local buffer are lost, so the next read begins mid-frame and permanently
/// desyncs the multiplexed stream (an illegal command word → fatal
/// [`RustADBError::ConversionError`] → connection torn down). By holding a
/// [`FrameReadBuffer`] across calls and only ever timing out a *single* chunk
/// read that appends into it, a cancelled read loses nothing — every received
/// byte is already buffered.
pub(crate) struct FrameReader<R> {
    reader: R,
    buffer: FrameReadBuffer,
    /// Reusable heap scratch for one stream read. Kept on the struct (not the
    /// stack) so it does not inflate the size of the read future — a 64 KiB stack
    /// array held across the `.await` would bloat every caller's future.
    scratch: Box<[u8]>,
}

impl<R: AsyncRead + Unpin> FrameReader<R> {
    pub(crate) fn new(reader: R) -> Self {
        Self {
            reader,
            buffer: FrameReadBuffer::new(),
            scratch: vec![0_u8; READ_CHUNK_LEN].into_boxed_slice(),
        }
    }

    /// Read one complete framed message, applying `timeout` to each individual
    /// stream read (per-read idle timeout).
    ///
    /// Returns the transport-neutral [`RustADBError::ReadTimeout`] only when a
    /// read deadline elapses with **no** complete frame buffered — i.e. at a frame
    /// boundary, never mid-frame (bytes already pulled off the wire stay in the
    /// persistent buffer). Returning `ReadTimeout` (not
    /// `IOError(ErrorKind::TimedOut)`) honors the
    /// [`ADBMessageTransport::read_message_with_timeout`] contract so the
    /// transport-generic persistent reader treats an idle timeout as a
    /// keep-looping condition, not a fatal transport error.
    ///
    /// [`ADBMessageTransport::read_message_with_timeout`]: crate::message_devices::adb_message_transport::ADBMessageTransport::read_message_with_timeout
    pub(crate) async fn read_message(
        &mut self,
        timeout: Duration,
        peer: &str,
    ) -> Result<ADBTransportMessage> {
        loop {
            // Emit a frame the moment a whole one is buffered (including a frame
            // already complete from a previous call's over-read). Bytes are
            // consumed from the buffer only on a full frame, so this is the only
            // place the stream advances.
            if let Some(message) = self.buffer.try_parse()? {
                return Ok(message);
            }

            // No complete frame yet: read one more chunk. The timeout wraps just
            // this single read, so a cancellation can only ever land *between*
            // reads — with everything received so far already in `self.buffer`.
            let n = match tokio::time::timeout(timeout, self.reader.read(&mut self.scratch)).await {
                Ok(res) => res?,
                Err(_elapsed) => return Err(RustADBError::ReadTimeout),
            };
            if n == 0 {
                // Clean EOF: the peer closed the connection between frames.
                return Err(RustADBError::IOError(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    format!("{peer} connection closed by peer"),
                )));
            }
            self.buffer.push(&self.scratch[..n]);
        }
    }

    /// Give the read half back, discarding any partially buffered frame.
    ///
    /// Only a structural connection change (the TLS upgrade, which replaces the
    /// whole stream) may do this: carrying plaintext bytes across an upgrade
    /// would itself desync the encrypted stream.
    pub(crate) fn into_reader(self) -> R {
        self.reader
    }
}

/// Write one whole frame with frame-atomic timeout semantics.
///
/// `start_timeout` gates only the **start** of the frame: while zero bytes have
/// been committed, a single `write` that does not make progress within
/// `start_timeout` returns the recoverable [`RustADBError::WriteTimeout`] (the OUT
/// path is under backpressure but nothing was truncated — the persistent writer
/// keeps looping). Once any byte is committed the frame is driven to completion
/// under the looser `completion_timeout`; an IO error or that deadline elapsing
/// mid-frame is fatal (the caller poisons the write half).
///
/// A single `AsyncWrite::poll_write` is cancel-safe — it either commits `n > 0`
/// bytes or makes no progress — so wrapping ONE `write` call in `timeout` never
/// loses bytes. This is the write-side mirror of the read path's "a timeout is
/// only ever observed at a frame boundary" invariant.
///
/// Generic over the writer and parameterized on both deadlines so the branch
/// logic is unit-testable against a mock `AsyncWrite` without real sockets or
/// 10-second waits.
pub(crate) async fn write_frame_atomic<W: AsyncWrite + Unpin>(
    writer: &mut W,
    frame: &[u8],
    start_timeout: Duration,
    completion_timeout: Duration,
    peer: &str,
) -> Result<()> {
    let mut offset = 0;
    while offset < frame.len() {
        // The first write of the frame is the start gate; every subsequent write is
        // mid-frame and must complete within the (looser) completion deadline.
        let timeout = if offset == 0 {
            start_timeout
        } else {
            completion_timeout
        };

        match tokio::time::timeout(timeout, writer.write(&frame[offset..])).await {
            Ok(res) => {
                let n = res?;
                if n == 0 {
                    return Err(RustADBError::IOError(std::io::Error::new(
                        std::io::ErrorKind::WriteZero,
                        format!("{peer} write returned 0 (peer closed)"),
                    )));
                }
                offset += n;
            }
            Err(_elapsed) => {
                if offset == 0 {
                    // Nothing of this frame is on the wire: recoverable, no truncation.
                    return Err(RustADBError::WriteTimeout);
                }
                // Mid-frame stall past the completion deadline: the frame is
                // truncated and unrecoverable. Fatal.
                return Err(RustADBError::IOError(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    format!("{peer} write stalled mid-frame past the completion deadline"),
                )));
            }
        }
    }

    // Flush is a no-op for an unbuffered TcpStream/TlsStream, but keep it for
    // correctness under any buffering wrapper. A flush failure after the bytes are
    // written is an IO error (fatal), not a frame-boundary timeout.
    match tokio::time::timeout(completion_timeout, writer.flush()).await {
        Ok(res) => Ok(res?),
        Err(_elapsed) => Err(RustADBError::IOError(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            format!("{peer} flush stalled past the completion deadline"),
        ))),
    }
}

/// One full-duplex framed stream, split into independently locked halves.
///
/// Clones share the halves through `Arc`s rather than duplicating the stream. The
/// read and write halves have **separate** locks, so a blocking read never stalls
/// a concurrent write — interactive `adb shell` keystrokes are not serialized
/// behind the reader's idle read timeout. (Sharing one lock between both
/// directions added a full read-timeout window of latency to every keystroke; the
/// regression is locked by a test in `tcp_transport.rs`.)
///
/// The write half is set to `None` when a write fails in a way that may have put a
/// partial frame on the wire, so a truncated frame is never followed by another
/// frame. A start-gate [`RustADBError::WriteTimeout`] is recoverable and does not
/// poison it.
pub(crate) struct FramedStream<S> {
    read_half: Arc<Mutex<Option<FrameReader<ReadHalf<S>>>>>,
    write_half: Arc<Mutex<Option<WriteHalf<S>>>>,
    /// Diagnostic label used in log/error messages (e.g. `"TCP"` or
    /// `"tbox@YTG..."`). Never contains payload bytes.
    peer: Arc<str>,
}

impl<S> Clone for FramedStream<S> {
    fn clone(&self) -> Self {
        Self {
            read_half: Arc::clone(&self.read_half),
            write_half: Arc::clone(&self.write_half),
            peer: Arc::clone(&self.peer),
        }
    }
}

impl<S> std::fmt::Debug for FramedStream<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FramedStream")
            .field("peer", &self.peer)
            .finish_non_exhaustive()
    }
}

impl<S: AsyncRead + AsyncWrite + Unpin> FramedStream<S> {
    /// Split `stream` into independently locked read/write halves.
    pub(crate) fn new(stream: S, peer: impl Into<Arc<str>>) -> Self {
        let (read, write) = tokio::io::split(stream);
        Self {
            read_half: Arc::new(Mutex::new(Some(FrameReader::new(read)))),
            write_half: Arc::new(Mutex::new(Some(write))),
            peer: peer.into(),
        }
    }

    /// The diagnostic label for this stream.
    pub(crate) fn peer(&self) -> &str {
        &self.peer
    }

    /// Read one framed message. See [`FrameReader::read_message`].
    pub(crate) async fn read_message(&self, read_timeout: Duration) -> Result<ADBTransportMessage> {
        let read_lock = Arc::clone(&self.read_half);
        let mut guard = read_lock.lock().await;
        let reader = guard.as_mut().ok_or_else(not_connected)?;
        reader.read_message(read_timeout, &self.peer).await
    }

    /// Write one framed message with the frame-atomic contract, poisoning the
    /// write half on any fatal outcome.
    pub(crate) async fn write_message(
        &self,
        message: ADBTransportMessage,
        write_timeout: Duration,
    ) -> Result<()> {
        // Serialize the whole frame (header followed by payload) into one
        // contiguous buffer so it is written as a single byte stream: the
        // start-gate timeout then applies to the frame as a unit, not to header and
        // payload separately (a header-written-then-payload-timeout would otherwise
        // be a truncation, not a clean frame boundary).
        let mut frame = message.header().as_bytes();
        frame.extend_from_slice(message.payload());

        let write_lock = Arc::clone(&self.write_half);
        let mut guard = write_lock.lock().await;
        let Some(writer) = guard.as_mut() else {
            return Err(not_connected());
        };

        let result = write_frame_atomic(
            writer,
            &frame,
            write_timeout,
            WRITE_COMPLETION_TIMEOUT,
            &self.peer,
        )
        .await;

        // Frame-atomic outcomes (see `write_frame_atomic`):
        //  - Ok: whole frame on the wire.
        //  - WriteTimeout: nothing of the frame was committed — recoverable, the
        //    stream is still aligned, so DO NOT poison (the persistent writer keeps
        //    looping and the next frame writes cleanly).
        //  - any other Err: a frame was started but not finished (truncation) or a
        //    real IO error — unrecoverable. Poison the write half so the truncated
        //    stream is never written to again; the connection is torn down.
        if matches!(result, Err(ref e) if !matches!(e, RustADBError::WriteTimeout)) {
            *guard = None;
        }
        result
    }

    /// Shut down the write half. Best-effort: the error is intentionally ignored
    /// (the connection is being torn down either way).
    pub(crate) async fn shutdown(&self) {
        let write_lock = Arc::clone(&self.write_half);
        let mut guard = write_lock.lock().await;
        if let Some(writer) = guard.as_mut() {
            let _ = writer.shutdown().await;
        }
    }

    /// Disassemble the split halves back into the whole stream.
    ///
    /// Only a transport that must move the stream into another type (the TCP/TLS
    /// upgrade) needs this. Both halves are left empty; the caller MUST install a
    /// fresh stream afterwards or the transport reports [`RustADBError::IOError`]
    /// (`NotConnected`) for every subsequent read/write.
    pub(crate) async fn take_stream(&self) -> Option<S> {
        let read_lock = Arc::clone(&self.read_half);
        let write_lock = Arc::clone(&self.write_half);
        let mut read_guard = read_lock.lock().await;
        let mut write_guard = write_lock.lock().await;
        let read = read_guard.take()?;
        let write = write_guard.take()?;
        Some(read.into_reader().unsplit(write))
    }
}

#[cfg(test)]
mod tests {
    use super::{FramedStream, WRITE_COMPLETION_TIMEOUT, write_frame_atomic};
    use crate::RustADBError;
    use std::pin::Pin;
    use std::task::{Context, Poll};
    use std::time::Duration;
    use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

    /// A mock `AsyncWrite` that accepts `accept_first` bytes immediately, then
    /// stalls forever (always `Pending`). With `accept_first == 0` it models a
    /// fully back-pressured OUT path (start-gate stall); with `accept_first > 0` it
    /// models a peer that takes a frame's first bytes then wedges (mid-frame stall).
    struct StallingWriter {
        remaining_accept: usize,
        written: usize,
    }

    impl StallingWriter {
        fn new(accept_first: usize) -> Self {
            Self {
                remaining_accept: accept_first,
                written: 0,
            }
        }
    }

    impl AsyncWrite for StallingWriter {
        fn poll_write(
            mut self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<std::io::Result<usize>> {
            if self.remaining_accept == 0 {
                return Poll::Pending;
            }
            let n = buf.len().min(self.remaining_accept);
            self.remaining_accept -= n;
            self.written += n;
            Poll::Ready(Ok(n))
        }

        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }

        fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    /// A mock full-duplex stream that never produces readable bytes (its read half
    /// is irrelevant to the write-side tests) and accepts at most `accept` bytes
    /// per write, optionally failing after that.
    struct MockStream {
        accept_per_write: usize,
        fail_after: Option<usize>,
        written: usize,
    }

    impl MockStream {
        fn stalling(accept_per_write: usize) -> Self {
            Self {
                accept_per_write,
                fail_after: None,
                written: 0,
            }
        }

        fn failing_after(bytes: usize) -> Self {
            Self {
                accept_per_write: bytes,
                fail_after: Some(bytes),
                written: 0,
            }
        }
    }

    impl AsyncRead for MockStream {
        fn poll_read(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            _buf: &mut ReadBuf<'_>,
        ) -> Poll<std::io::Result<()>> {
            Poll::Pending
        }
    }

    impl AsyncWrite for MockStream {
        fn poll_write(
            mut self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<std::io::Result<usize>> {
            if let Some(limit) = self.fail_after
                && self.written >= limit
            {
                return Poll::Ready(Err(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "mock mid-frame failure",
                )));
            }
            if self.accept_per_write == 0 {
                return Poll::Pending;
            }
            let n = buf.len().min(self.accept_per_write);
            self.written += n;
            Poll::Ready(Ok(n))
        }

        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }

        fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    /// Scheme B start gate: a timeout with ZERO bytes of the frame committed is the
    /// recoverable `WriteTimeout` — NOT a fatal/truncation error. This is the case
    /// that broke `reverse_iperf3` when it was (wrongly) treated as fatal.
    #[tokio::test]
    async fn write_start_gate_timeout_is_recoverable() {
        let mut writer = StallingWriter::new(0); // back-pressured from the first byte
        let frame = vec![0xAB_u8; 64];
        let result = write_frame_atomic(
            &mut writer,
            &frame,
            Duration::from_millis(50), // start gate
            Duration::from_secs(10),   // completion (not reached)
            "test",
        )
        .await;
        assert!(
            matches!(result, Err(RustADBError::WriteTimeout)),
            "a stall before any byte is committed must be the recoverable WriteTimeout (got {result:?})"
        );
    }

    /// Scheme B mid-frame: a stall AFTER the frame has started (some bytes already
    /// on the wire) past the completion deadline is FATAL — a truncated frame is
    /// unrecoverable, so it must NOT be reported as the recoverable `WriteTimeout`.
    #[tokio::test]
    async fn write_mid_frame_stall_is_fatal() {
        let mut writer = StallingWriter::new(8); // accept 8 bytes, then wedge
        let frame = vec![0xAB_u8; 64];
        let result = write_frame_atomic(
            &mut writer,
            &frame,
            Duration::from_millis(50), // start gate (passes — 8 bytes accepted)
            Duration::from_millis(50), // completion (trips mid-frame)
            "test",
        )
        .await;
        assert!(
            matches!(result, Err(RustADBError::IOError(ref e)) if e.kind() == std::io::ErrorKind::TimedOut),
            "a stall after the frame started must be a fatal IO timeout, not WriteTimeout (got {result:?})"
        );
    }

    /// A frame that fits is written whole with no error.
    #[tokio::test]
    async fn write_frame_completes_when_unblocked() {
        let mut writer = StallingWriter::new(1024); // plenty of room
        let frame = vec![0xCD_u8; 64];
        let result = write_frame_atomic(
            &mut writer,
            &frame,
            Duration::from_millis(50),
            Duration::from_secs(10),
            "test",
        )
        .await;
        assert!(
            result.is_ok(),
            "a frame that fits must write cleanly (got {result:?})"
        );
        assert_eq!(writer.written, 64, "all frame bytes must be committed");
    }

    /// Via `FramedStream`: a start-gate timeout does not poison the write half, so
    /// a later write succeeds once the peer drains.
    #[tokio::test]
    async fn framed_stream_start_gate_timeout_keeps_write_half_usable() {
        let stream = FramedStream::new(MockStream::stalling(0), "mock");
        let message = crate::message_devices::adb_transport_message::ADBTransportMessage::try_new(
            crate::message_devices::message_commands::MessageCommand::Okay,
            0,
            0,
            b"",
        )
        .expect("build frame");

        let first = stream
            .write_message(message.clone(), Duration::from_millis(50))
            .await;
        assert!(
            matches!(first, Err(RustADBError::WriteTimeout)),
            "an uncommitted stall must be WriteTimeout (got {first:?})"
        );

        // The write half is still there; the same reclaimable outcome is produced
        // again rather than the `NotConnected` a poisoned half would yield.
        let second = stream
            .write_message(message, Duration::from_millis(50))
            .await;
        assert!(
            matches!(second, Err(RustADBError::WriteTimeout)),
            "a start-gate timeout must not poison the write half (got {second:?})"
        );
    }

    /// Via `FramedStream`: a write that fails after committing bytes poisons the
    /// write half, so no second (misaligned) frame can follow.
    #[tokio::test]
    async fn framed_stream_mid_frame_error_poisons_write_half() {
        let stream = FramedStream::new(MockStream::failing_after(8), "mock");
        let message = crate::message_devices::adb_transport_message::ADBTransportMessage::try_new(
            crate::message_devices::message_commands::MessageCommand::Write,
            1,
            2,
            b"a payload long enough to need several writes",
        )
        .expect("build frame");

        let first = stream
            .write_message(message.clone(), Duration::from_secs(1))
            .await;
        assert!(
            matches!(first, Err(RustADBError::IOError(ref e)) if e.kind() == std::io::ErrorKind::BrokenPipe),
            "a mid-frame IO error must be fatal (got {first:?})"
        );

        let second = stream.write_message(message, Duration::from_secs(1)).await;
        assert!(
            matches!(second, Err(RustADBError::IOError(ref e)) if e.kind() == std::io::ErrorKind::NotConnected),
            "a fatal mid-frame write must poison the write half (got {second:?})"
        );
        assert_eq!(
            WRITE_COMPLETION_TIMEOUT,
            Duration::from_secs(10),
            "the completion deadline is a deliberate, documented constant"
        );
    }
}
